//! One shape for "fetch this input's playlist", whatever kind of provider it is.
//!
//! The three provider families in this crate were modelled three different ways — M3U as
//! free functions, Xtream as free functions with a different arity, Stalker as a struct
//! client whose orchestration lives in `tuliprox-processing` — and each returned a
//! differently-shaped tuple. The dispatcher paid for that: a ninety-line `match` whose
//! arms each hand-assembled a six-element tuple, padding the fields their provider did not
//! produce with literal zeros and `false`s, and then destructured the lot positionally.
//! Two of those six elements were dead on arrival.
//!
//! [`PlaylistFetch`] is that result with names, and [`PlaylistProvider`] is the one method
//! every family can implement. Providers are constructed per-input with whatever they
//! individually need — an event sink, a refresh mode — so `fetch` takes only what all of
//! them take.
//!
//! Dispatch stays a `match` and stays statically dispatched: the win is that each arm now
//! names one type instead of assembling a tuple by position.

use shared::{
    error::TuliproxError,
    model::{PlaylistGroup, XtreamCluster},
};
use std::{future::Future, sync::Arc};
use tuliprox_core::model::{AppConfig, Config, ConfigInput};

/// What a provider produced for one input.
///
/// `errors` is not exclusive with `groups`: a provider that fetched two of three clusters
/// reports both, and the dispatcher decides what that means for the input's cache status.
#[derive(Debug, Default)]
pub struct PlaylistFetch {
    pub groups: Vec<PlaylistGroup>,
    pub errors: Vec<TuliproxError>,
    /// The provider wrote the playlist to disk itself, so the caller must not.
    pub persisted: bool,
    /// The fetch stopped part-way and should be resumed rather than treated as a result.
    /// Only the resumable Stalker refresh sets this.
    pub partial: bool,
}

impl PlaylistFetch {
    /// A successful fetch of `groups`.
    #[must_use]
    pub fn groups(groups: Vec<PlaylistGroup>) -> Self {
        Self { groups, ..Self::default() }
    }

    /// A fetch that produced nothing and failed for one reason.
    #[must_use]
    pub fn failed(error: TuliproxError) -> Self {
        Self { errors: vec![error], ..Self::default() }
    }

    /// A provider that has nothing to do for this input — a batch input whose members are
    /// fetched individually, say. Distinct from a failure.
    #[must_use]
    pub fn nothing_to_do() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_errors(mut self, errors: Vec<TuliproxError>) -> Self {
        self.errors = errors;
        self
    }

    #[must_use]
    pub fn persisted(mut self, persisted: bool) -> Self {
        self.persisted = persisted;
        self
    }

    #[must_use]
    pub fn partial(mut self, partial: bool) -> Self {
        self.partial = partial;
        self
    }

    /// Whether the fetch completed without errors. A fetch that produced no groups and no
    /// errors counts as successful — an empty catalog is a legitimate answer.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty() && !self.partial
    }
}

/// Everything every provider needs, and nothing that only one of them does.
///
/// Provider-specific inputs — the event sink, the Stalker refresh mode — belong to the
/// provider value, which the dispatcher builds per input.
pub struct PlaylistFetchRequest<'a> {
    pub app_config: &'a Arc<AppConfig>,
    pub config: &'a Arc<Config>,
    pub client: &'a reqwest::Client,
    pub input: &'a ConfigInput,
    /// Which Xtream clusters still need fetching. `None` means "whatever the input
    /// allows"; providers that have no clusters ignore it.
    pub xtream_clusters: Option<&'a [XtreamCluster]>,
}

/// Fetch one input's playlist.
pub trait PlaylistProvider {
    /// A short name for this provider, for logs and errors.
    fn name(&self) -> &'static str;

    fn fetch(&self, request: &PlaylistFetchRequest<'_>) -> impl Future<Output = PlaylistFetch> + Send;
}

/// Plain M3U playlist over HTTP or from a file.
#[derive(Debug, Clone, Copy, Default)]
pub struct M3uProvider;

impl PlaylistProvider for M3uProvider {
    fn name(&self) -> &'static str {
        "m3u"
    }

    async fn fetch(&self, request: &PlaylistFetchRequest<'_>) -> PlaylistFetch {
        let (groups, errors) =
            crate::m3u::download_m3u_playlist(request.app_config, request.client, request.config, request.input).await;
        PlaylistFetch::groups(groups).with_errors(errors)
    }
}

/// Xtream Codes player API. Carries the event sink because account state — expiry,
/// suspension — is discovered during login and published as it is found.
pub struct XtreamProvider<'e, E> {
    events: &'e E,
}

impl<'e, E> XtreamProvider<'e, E> {
    pub fn new(events: &'e E) -> Self {
        Self { events }
    }
}

impl<E: shared::model::EventSink> PlaylistProvider for XtreamProvider<'_, E> {
    fn name(&self) -> &'static str {
        "xtream"
    }

    async fn fetch(&self, request: &PlaylistFetchRequest<'_>) -> PlaylistFetch {
        let (groups, errors, persisted) = crate::xtream::download_xtream_playlist(
            request.app_config,
            request.client,
            self.events,
            request.input,
            request.xtream_clusters,
        )
        .await;
        PlaylistFetch::groups(groups).with_errors(errors).persisted(persisted)
    }
}

/// A provider that is configured but cannot fetch — a catalog importer that does not exist
/// yet, or a staged input that reached the dispatcher without being resolved.
///
/// Modelled as a provider rather than as a `match` arm returning a hand-built error so
/// that "this input type produces nothing, and here is why" is one thing with one shape.
pub struct UnsupportedProvider {
    name: &'static str,
    reason: String,
}

impl UnsupportedProvider {
    #[must_use]
    pub fn new(name: &'static str, reason: impl Into<String>) -> Self {
        Self { name, reason: reason.into() }
    }
}

impl PlaylistProvider for UnsupportedProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn fetch(&self, _request: &PlaylistFetchRequest<'_>) -> PlaylistFetch {
        PlaylistFetch::failed(TuliproxError::Download(self.reason.clone()))
    }
}

/// A provider whose members are fetched individually elsewhere; fetching the container
/// itself is a no-op rather than an error.
#[derive(Debug, Clone, Copy, Default)]
pub struct BatchContainerProvider;

impl PlaylistProvider for BatchContainerProvider {
    fn name(&self) -> &'static str {
        "batch"
    }

    async fn fetch(&self, _request: &PlaylistFetchRequest<'_>) -> PlaylistFetch {
        PlaylistFetch::nothing_to_do()
    }
}

#[cfg(test)]
mod tests {
    use super::PlaylistFetch;
    use shared::error::TuliproxError;

    #[test]
    fn an_empty_catalog_is_a_success_not_a_failure() {
        let fetch = PlaylistFetch::groups(Vec::new());
        assert!(fetch.is_ok());
        assert!(fetch.groups.is_empty());
    }

    #[test]
    fn a_partial_fetch_is_not_reported_as_ok_even_without_errors() {
        // The resumable Stalker refresh returns rows it has, with more to come; treating
        // that as a complete result would cache a half-catalog.
        assert!(!PlaylistFetch::groups(Vec::new()).partial(true).is_ok());
    }

    #[test]
    fn a_failed_fetch_carries_its_reason() {
        let fetch = PlaylistFetch::failed(TuliproxError::Download("portal unreachable".to_string()));
        assert!(!fetch.is_ok());
        assert_eq!(fetch.errors.len(), 1);
        assert!(!fetch.persisted);
    }

    #[test]
    fn nothing_to_do_is_distinct_from_failure() {
        let fetch = PlaylistFetch::nothing_to_do();
        assert!(fetch.is_ok(), "a batch container producing nothing must not fail its input");
    }
}
