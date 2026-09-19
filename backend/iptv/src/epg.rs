//! One shape for "get this input's EPG", whatever kind of provider it is.
//!
//! EPG acquisition was wired per provider family and never met in the middle. Stalker has
//! three EPG calls in this crate, streaming programme records straight into the Stalker
//! repository. M3U and Xtream have none here at all: their XMLTV download lives in
//! `tuliprox-processing`, produces files on disk, and is reached from a completely
//! separate call site. Neither knew the other existed.
//!
//! What they genuinely have in common is the question — *does this input have an EPG, and
//! what did fetching it produce?* — so that is what [`EpgProvider`] unifies. What they do
//! not have in common is the answer's shape, and pretending otherwise would mean either
//! materialising a multi-hundred-megabyte XMLTV document into records in memory, or
//! teaching the Stalker client to write XMLTV it has no reason to write. [`EpgOutcome`]
//! says which of the two happened instead of forcing one into the other.

use crate::stalker::{
    client::StalkerApiClient, error::StalkerError, profile::StalkerHandshake, transport::StalkerTransport,
};
use shared::error::TuliproxError;
use std::{
    future::Future,
    sync::atomic::{AtomicU64, Ordering},
};
/// A single programme.
///
/// Aliased rather than redefined: this is already the workspace's only programme record,
/// and it lives in `tuliprox-core` rather than in any provider's module. The name it was
/// given there is the one thing about it that is Stalker-specific.
pub use tuliprox_core::model::StalkerProgramRecord as EpgProgramRecord;
use tuliprox_core::{model::ConfigInput, utils::Clock};

/// Where a provider delivers programme records.
///
/// Batched rather than one-at-a-time because both the portal and the store on the other
/// end work in batches, and because a bulk EPG response can carry hundreds of thousands
/// of programmes — the whole reason this is a sink and not a `Vec` return.
pub trait EpgRecordSink: Send {
    fn accept(&mut self, batch: Vec<EpgProgramRecord>) -> impl Future<Output = Result<(), TuliproxError>> + Send;
}

/// Collects everything in memory. For small windows — a single channel's short EPG — and
/// for tests.
#[derive(Debug, Default)]
pub struct CollectEpgSink {
    records: Vec<EpgProgramRecord>,
}

impl CollectEpgSink {
    #[must_use]
    pub fn new() -> Self { Self::default() }

    #[must_use]
    pub fn into_records(self) -> Vec<EpgProgramRecord> { self.records }
}

impl EpgRecordSink for CollectEpgSink {
    fn accept(&mut self, mut batch: Vec<EpgProgramRecord>) -> impl Future<Output = Result<(), TuliproxError>> + Send {
        self.records.append(&mut batch);
        async { Ok(()) }
    }
}

/// Counts without keeping. Useful when the interesting effect is elsewhere — a provider
/// that has already written each batch as it passed.
#[derive(Debug, Default)]
pub struct CountingEpgSink {
    count: u64,
}

impl CountingEpgSink {
    #[must_use]
    pub fn new() -> Self { Self::default() }

    #[must_use]
    pub fn count(&self) -> u64 { self.count }
}

impl EpgRecordSink for CountingEpgSink {
    fn accept(&mut self, batch: Vec<EpgProgramRecord>) -> impl Future<Output = Result<(), TuliproxError>> + Send {
        self.count = self.count.saturating_add(u64::try_from(batch.len()).unwrap_or(u64::MAX));
        async { Ok(()) }
    }
}

/// What fetching an input's EPG produced.
///
/// `G` is the provider's own guide handle — whatever the caller needs to read the files a
/// file-based provider left behind. Providers that stream records have no guide and use
/// `()`.
#[derive(Debug)]
pub enum EpgOutcome<G> {
    /// The provider streamed `count` programme records into the sink.
    Records { count: u64 },
    /// The provider produced a guide over files on disk; the caller parses them lazily.
    Guide(G),
    /// The input has no EPG configured. Not a failure.
    NotConfigured,
}

impl<G> EpgOutcome<G> {
    /// Whether anything was produced at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Records { count } => *count == 0,
            Self::Guide(_) => false,
            Self::NotConfigured => true,
        }
    }

    /// The guide, if this provider produced one.
    #[must_use]
    pub fn into_guide(self) -> Option<G> {
        match self {
            Self::Guide(guide) => Some(guide),
            _ => None,
        }
    }
}

/// Everything every EPG provider needs.
pub struct EpgFetchRequest<'a> {
    pub input: &'a ConfigInput,
    /// How many hours ahead to fetch. Providers that fetch whole documents ignore it.
    pub period_hours: u32,
    /// Records per batch handed to the sink.
    pub batch_size: usize,
}

impl<'a> EpgFetchRequest<'a> {
    #[must_use]
    pub fn new(input: &'a ConfigInput) -> Self { Self { input, period_hours: 24, batch_size: 512 } }

    #[must_use]
    pub fn period_hours(mut self, hours: u32) -> Self {
        self.period_hours = hours;
        self
    }

    #[must_use]
    pub fn batch_size(mut self, size: usize) -> Self {
        self.batch_size = size.max(1);
        self
    }
}

/// Fetch one input's EPG.
pub trait EpgProvider {
    /// The guide handle this provider hands back, or `()` when it streams records.
    type Guide;

    fn name(&self) -> &'static str;

    fn fetch(
        &self,
        request: &EpgFetchRequest<'_>,
        sink: &mut impl EpgRecordSink,
    ) -> impl Future<Output = Result<EpgOutcome<Self::Guide>, TuliproxError>> + Send;
}

/// Stalker's bulk EPG, streamed straight through to the sink.
pub struct StalkerEpgProvider<'a, Tr: StalkerTransport, C: Clock> {
    client: &'a StalkerApiClient<Tr, C>,
    handshake: &'a StalkerHandshake,
}

impl<'a, Tr: StalkerTransport, C: Clock> StalkerEpgProvider<'a, Tr, C> {
    pub fn new(client: &'a StalkerApiClient<Tr, C>, handshake: &'a StalkerHandshake) -> Self {
        Self { client, handshake }
    }
}

impl<Tr: StalkerTransport, C: Clock> EpgProvider for StalkerEpgProvider<'_, Tr, C> {
    type Guide = ();

    fn name(&self) -> &'static str { "stalker" }

    async fn fetch(
        &self,
        request: &EpgFetchRequest<'_>,
        sink: &mut impl EpgRecordSink,
    ) -> Result<EpgOutcome<()>, TuliproxError> {
        // The callback holds the sink across an await, so it needs an async-aware lock;
        // and the count is tracked here rather than asked of the sink afterwards, because
        // a sink that discards its input still has to report what passed through it.
        let sink = tokio::sync::Mutex::new(sink);
        let delivered = AtomicU64::new(0);
        self.client
            .stream_bulk_epg(self.handshake, request.period_hours, request.batch_size, |batch| {
                let taken = u64::try_from(batch.len()).unwrap_or(u64::MAX);
                let sink = &sink;
                let delivered = &delivered;
                async move {
                    sink.lock()
                        .await
                        .accept(batch)
                        .await
                        .map_err(|err| StalkerError::Io(std::io::Error::other(err.to_string())))?;
                    delivered.fetch_add(taken, Ordering::Relaxed);
                    Ok(())
                }
            })
            .await
            .map_err(|err| crate::error::stalker_error_to_tuliprox(&err))?;
        Ok(EpgOutcome::Records { count: delivered.load(Ordering::Relaxed) })
    }
}

#[cfg(test)]
mod tests {
    use super::{CollectEpgSink, CountingEpgSink, EpgFetchRequest, EpgOutcome, EpgProgramRecord, EpgRecordSink};

    fn program(title: &str) -> EpgProgramRecord {
        EpgProgramRecord {
            channel_id: Some("1".to_string()),
            title: title.to_string(),
            start_epoch: Some(1_700_000_000),
            stop_epoch: Some(1_700_003_600),
            description: None,
            category: None,
        }
    }

    #[tokio::test]
    async fn a_collecting_sink_keeps_every_batch_in_order() -> Result<(), shared::error::TuliproxError> {
        let mut sink = CollectEpgSink::new();
        sink.accept(vec![program("A"), program("B")]).await?;
        sink.accept(vec![program("C")]).await?;

        let titles: Vec<String> = sink.into_records().into_iter().map(|record| record.title).collect();
        assert_eq!(titles, vec!["A", "B", "C"]);
        Ok(())
    }

    #[tokio::test]
    async fn a_counting_sink_reports_throughput_without_retaining_it() -> Result<(), shared::error::TuliproxError> {
        let mut sink = CountingEpgSink::new();
        sink.accept(vec![program("A"), program("B")]).await?;
        sink.accept(Vec::new()).await?;
        assert_eq!(sink.count(), 2);
        Ok(())
    }

    #[test]
    fn an_unconfigured_epg_is_empty_but_not_a_failure() {
        let outcome: EpgOutcome<()> = EpgOutcome::NotConfigured;
        assert!(outcome.is_empty());
        assert!(outcome.into_guide().is_none());
    }

    #[test]
    fn a_guide_outcome_hands_the_guide_back() {
        let outcome = EpgOutcome::Guide("guide-handle");
        assert!(!outcome.is_empty());
        assert_eq!(outcome.into_guide(), Some("guide-handle"));
    }

    #[test]
    fn a_record_outcome_is_empty_only_when_nothing_arrived() {
        assert!(EpgOutcome::<()>::Records { count: 0 }.is_empty());
        assert!(!EpgOutcome::<()>::Records { count: 1 }.is_empty());
    }

    #[test]
    fn a_batch_size_of_zero_is_refused_rather_than_looping_forever() {
        let input = tuliprox_core::model::ConfigInput::default();
        assert_eq!(EpgFetchRequest::new(&input).batch_size(0).batch_size, 1);
    }
}
