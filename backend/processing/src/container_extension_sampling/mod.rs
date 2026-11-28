//! Opt-in learning of Stalker VOD `container_extension` values.
//!
//! A Stalker portal states a movie's container only in its playback answer, so the delivery path
//! publishes the input's configured default. This module closes that gap for operators who accept the
//! cost: a background run after a playlist update asks the portal for the items that still have no
//! extension, and remembers the answer in a ledger.
//!
//! The run is bounded by design, because it is the only feature that sends portal requests beyond the
//! import: a per-update call budget, a minimum interval between calls, and a stop after a run of auth
//! or rate refusals. It never opens a media stream, only `create_link` is called.

mod ledger;
#[cfg(test)]
mod tests;

use crate::processor::stalker::{playback_candidates, resolve_stalker_link_for_sampling, StalkerLinkFailure};
pub use ledger::{ContainerExtensionLedger, CONTAINER_EXTENSION_LEDGER_FILE};
use log::{debug, info, warn};
use shared::{
    error::TuliproxError,
    model::{stalker::StalkerStreamKind, stalker_item::StalkerPlaylistItem},
    utils::{
        clear_sampled_container_extensions, extract_extension_from_url, normalize_container_extension,
        publish_sampled_container_extensions,
    },
};
use std::{collections::HashSet, future::Future, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use tuliprox_core::model::{AppConfig, ConfigInput, MaintenanceGate};
use tuliprox_iptv::stalker::error::StalkerError;
use tuliprox_repository::{
    build_input_storage_path,
    stalker_generation_repository::readable_active_manifest,
    stalker_repository::{ensure_stalker_storage_path, get_stalker_storage_path, load_stalker_items_at},
};

/// The limits an input opted into with `options.container_extension_sampling`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SamplingLimits {
    /// Portal calls one playlist update may spend.
    pub max_calls_per_update: u16,
    /// Minimum pause between two calls.
    pub min_interval_secs: u16,
    /// Consecutive auth or rate refusals that end the run.
    pub stop_after_refusals: u8,
}

impl SamplingLimits {
    /// `None` when the input did not opt in, which is every input by default.
    #[must_use]
    pub fn from_input(input: &ConfigInput) -> Option<Self> {
        input.container_extension_sampling().map(|sampling| Self {
            max_calls_per_update: sampling.max_calls_per_update,
            min_interval_secs: sampling.min_interval_secs,
            stop_after_refusals: sampling.stop_after_refusals,
        })
    }
}

/// What one portal answer was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SamplingAnswer {
    /// The portal answered with a playable URL that carries this container extension.
    Resolved(Arc<str>),
    /// Auth or rate refusal: the portal rejected the account or asked us to slow down.
    Refused,
    /// Any other failure. It spent budget, but it is not a refusal.
    Failed,
}

/// The single portal call the sampler makes, so a test can answer it without a portal.
///
/// The call resolves one item through the input's own Stalker path; the stream id is all the answer
/// needs, because the command and the account come from the published catalog and the configuration.
pub trait ContainerExtensionPortal {
    fn resolve(&self, stream_id: u32) -> impl Future<Output = SamplingAnswer> + Send;
}

/// What one sampling run did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SamplingReport {
    /// Items that passed the eligibility filter, whether or not the run reached them.
    pub candidates: u32,
    /// Portal calls made.
    pub calls: u32,
    pub resolved: u32,
    pub refused: u32,
    pub failed: u32,
    /// Ledger entries dropped because the catalog no longer holds those items.
    pub pruned: u32,
    /// The call budget was used up.
    pub budget_exhausted: bool,
    /// The run stopped after the configured number of consecutive refusals.
    pub stopped_after_refusals: bool,
    /// The run stopped between two calls: the input entered maintenance or the task was cancelled.
    pub aborted: bool,
}

/// Samples the candidates in order, within the limits.
///
/// `should_stop` is asked before every call, so an input that enters maintenance or a cancelled token
/// ends the run between two calls rather than inside one. The interval only separates calls; the first
/// one starts immediately.
pub async fn run_sampling<P: ContainerExtensionPortal>(
    portal: &P,
    candidates: &[u32],
    limits: SamplingLimits,
    ledger: &mut ContainerExtensionLedger,
    cancel: &CancellationToken,
    pause_gate: Option<&Arc<tokio::sync::RwLock<()>>>,
    should_stop: &(dyn Fn() -> bool + Sync),
) -> SamplingReport {
    let mut report =
        SamplingReport { candidates: u32::try_from(candidates.len()).unwrap_or(u32::MAX), ..SamplingReport::default() };
    let mut consecutive_refusals: u8 = 0;
    let interval = Duration::from_secs(u64::from(limits.min_interval_secs));

    for &stream_id in candidates {
        if report.calls >= u32::from(limits.max_calls_per_update) {
            report.budget_exhausted = true;
            break;
        }
        if should_stop() {
            report.aborted = true;
            break;
        }
        let mut did_wait = false;
        if report.calls > 0 && !interval.is_zero() {
            did_wait = true;
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    report.aborted = true;
                    break;
                }
                () = tokio::time::sleep(interval) => {}
            }
        }
        // Acquire the update pause gate only for a single portal call and release it before sleeping,
        // so an operator or scheduled playlist update never blocks behind a multi-minute sleep loop.
        let pause_guard = if let Some(gate) = pause_gate {
            if let Ok(guard) = gate.clone().try_read_owned() {
                Some(guard)
            } else {
                did_wait = true;
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        report.aborted = true;
                        break;
                    }
                    guard = gate.clone().read_owned() => Some(guard),
                }
            }
        } else {
            None
        };
        if did_wait && should_stop() {
            report.aborted = true;
            break;
        }
        report.calls += 1;
        let answer = portal.resolve(stream_id).await;
        drop(pause_guard);
        match answer {
            SamplingAnswer::Resolved(extension) => {
                report.resolved += 1;
                consecutive_refusals = 0;
                if ledger.insert(stream_id, &extension) {
                    debug!("Sampled container extension '{extension}' for Stalker stream {stream_id}");
                }
            }
            SamplingAnswer::Refused => {
                report.refused += 1;
                consecutive_refusals = consecutive_refusals.saturating_add(1);
                if consecutive_refusals >= limits.stop_after_refusals {
                    report.stopped_after_refusals = true;
                    break;
                }
            }
            SamplingAnswer::Failed => {
                report.failed += 1;
                consecutive_refusals = 0;
            }
        }
    }
    report
}

/// Items worth a call: a Stalker movie without a provider extension, without a URL extension, without
/// a ledger entry, and with a command the portal can resolve.
///
/// The URL is checked as well because delivery prefers it over the ledger, so sampling such an item
/// would spend budget on an answer nobody uses. An item with neither a descriptor candidate nor a
/// stored command cannot resolve at all and is skipped for the same reason.
fn sampling_candidates(items: &[StalkerPlaylistItem], ledger: &ContainerExtensionLedger) -> Vec<u32> {
    items
        .iter()
        .filter(|item| item.stream_kind == StalkerStreamKind::Movie)
        .filter(|item| !states_extension(item.container_extension.as_deref()))
        .filter(|item| !states_extension(extract_extension_from_url(&item.stream_url)))
        .filter(|item| !ledger.contains(item.stream_id))
        .filter(|item| has_sampling_command(item))
        .map(|item| item.stream_id)
        .collect()
}

/// `true` when the playback path has a command for the item, asked through the same candidate rule the
/// playback resolution uses.
fn has_sampling_command(item: &StalkerPlaylistItem) -> bool { !playback_candidates(item).is_empty() }

fn states_extension(raw: Option<&str>) -> bool { raw.is_some_and(|raw| normalize_container_extension(raw).is_some()) }

/// Samples one input's Stalker VOD catalog, then publishes and persists what it learned.
///
/// `Ok(None)` means there was nothing to do: sampling is off, the input is not a Stalker input, it is
/// in maintenance, it has no Stalker configuration, no VOD catalog is published, or every item already
/// states an extension. Nothing is requested from the portal in any of those cases.
pub async fn sample_container_extensions(
    app_config: &Arc<AppConfig>,
    http_client: &reqwest::Client,
    input: &ConfigInput,
    maintenance: &dyn MaintenanceGate,
    cancel: &CancellationToken,
    pause_gate: Option<&Arc<tokio::sync::RwLock<()>>>,
) -> Result<Option<SamplingReport>, TuliproxError> {
    let Some(limits) = SamplingLimits::from_input(input) else { return Ok(None) };
    if !input.input_type.is_stalker() || maintenance.is_in_maintenance(&input.name) {
        return Ok(None);
    }
    // The catalog, its storage and the ledger belong to the input's own account. A run through an
    // alias would read a different catalog than the items it publishes for.
    let Some(stalker_cfg) = input.stalker.as_ref() else { return Ok(None) };
    let portal_url = input.resolve_url(&input.url)?;
    let storage_path = ensure_stalker_storage_path(app_config, &input.name).await?;
    let ledger_path = storage_path.join(CONTAINER_EXTENSION_LEDGER_FILE);
    let (mut ledger, candidates) = {
        let _pause_guard = if let Some(gate) = pause_gate {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(None),
                guard = gate.clone().read_owned() => Some(guard),
            }
        } else {
            None
        };
        if cancel.is_cancelled() || maintenance.is_in_maintenance(&input.name) {
            return Ok(None);
        }
        let (manifest, published) =
            readable_active_manifest(&storage_path, stalker_cfg.identity_fingerprint(portal_url.as_ref())).await?;
        if !published {
            debug!("Stalker container extension sampling skipped for input '{}': no published catalog", input.name);
            return Ok(None);
        }
        let Some(vod_files) = manifest.vod.as_ref() else { return Ok(None) };
        let items = load_stalker_items_at(app_config, &vod_files.data).await?;

        let mut ledger = ContainerExtensionLedger::load(&ledger_path).await.unwrap_or_else(|err| {
            warn!("{err}; sampling starts from an empty container extension ledger");
            ContainerExtensionLedger::default()
        });
        let existing: HashSet<u32> = items.iter().map(|item| item.stream_id).collect();
        let pruned = ledger.prune_to(&existing);
        let candidates = sampling_candidates(&items, &ledger);
        if candidates.is_empty() {
            ledger.store(&ledger_path).await?;
            publish_sampled_container_extensions(Arc::clone(&input.name), ledger.to_map());
            return Ok(None);
        }
        (ledger, (candidates, pruned))
    };
    let (candidates, pruned) = candidates;

    let portal = StalkerSamplingPortal { app_config, http_client, input };
    let should_stop = || cancel.is_cancelled() || maintenance.is_in_maintenance(&input.name);
    let mut report = run_sampling(&portal, &candidates, limits, &mut ledger, cancel, pause_gate, &should_stop).await;
    report.pruned = u32::try_from(pruned).unwrap_or(u32::MAX);

    ledger.store(&ledger_path).await?;
    publish_sampled_container_extensions(Arc::clone(&input.name), ledger.to_map());
    info!(
        "Stalker container extension sampling for input '{}': {} of {} candidates resolved, {} refused, {} failed in {} portal calls{}",
        input.name,
        report.resolved,
        report.candidates,
        report.refused,
        report.failed,
        report.calls,
        if report.stopped_after_refusals {
            " (stopped after consecutive refusals)"
        } else if report.aborted {
            " (stopped before the budget was spent)"
        } else {
            ""
        }
    );
    Ok(Some(report))
}

/// Loads and publishes every Stalker input's persisted container-extension ledger on startup.
///
/// Runs regardless of whether `container_extension_sampling.enabled` is currently `true` or the
/// input is in maintenance, so previously learned extensions remain available across restarts.
pub async fn load_persisted_container_extension_ledgers(app_config: &AppConfig) {
    let storage_dir = app_config.config.load().storage_dir.clone();
    let inputs: Vec<Arc<str>> = {
        let sources = app_config.sources.load();
        sources
            .inputs
            .iter()
            .filter(|input| input.input_type.is_stalker())
            .map(|input| Arc::clone(&input.name))
            .collect()
    };
    for input_name in inputs {
        let input_root = build_input_storage_path(&input_name, &storage_dir);
        let ledger_path = get_stalker_storage_path(&input_root).join(CONTAINER_EXTENSION_LEDGER_FILE);
        match ContainerExtensionLedger::load(&ledger_path).await {
            Ok(ledger) if !ledger.is_empty() => {
                publish_sampled_container_extensions(input_name, ledger.to_map());
            }
            Ok(_) => {
                clear_sampled_container_extensions(&input_name);
            }
            Err(err) => {
                warn!("{err}; starting without persisted container extension ledger for input '{input_name}'");
            }
        }
    }
}

/// The production portal: the input's own Stalker resolution path, one item at a time.
struct StalkerSamplingPortal<'a> {
    app_config: &'a Arc<AppConfig>,
    http_client: &'a reqwest::Client,
    input: &'a ConfigInput,
}

impl ContainerExtensionPortal for StalkerSamplingPortal<'_> {
    async fn resolve(&self, stream_id: u32) -> SamplingAnswer {
        let account_name = self.input.name.as_ref();
        match resolve_stalker_link_for_sampling(self.app_config, self.http_client, self.input, account_name, stream_id)
            .await
        {
            Ok(Some(url)) => extract_extension_from_url(&url).and_then(normalize_container_extension).map_or_else(
                || {
                    debug!("Stalker create_link answer for stream {stream_id} carries no container extension");
                    SamplingAnswer::Failed
                },
                |extension| SamplingAnswer::Resolved(extension.as_ref().into()),
            ),
            Ok(None) => SamplingAnswer::Failed,
            Err(StalkerLinkFailure::Handshake(err) | StalkerLinkFailure::Commands(err)) => {
                classify_failure(&err, stream_id)
            }
            Err(StalkerLinkFailure::Lookup(err)) => {
                debug!("Stalker container extension sampling lookup failed for stream {stream_id}: {err}");
                SamplingAnswer::Failed
            }
        }
    }
}

fn classify_failure(err: &StalkerError, stream_id: u32) -> SamplingAnswer {
    if err.is_rate_or_auth_refusal() {
        warn!("Stalker container extension sampling refused for stream {stream_id}: {err}");
        SamplingAnswer::Refused
    } else {
        debug!("Stalker container extension sampling failed for stream {stream_id}: {err}");
        SamplingAnswer::Failed
    }
}
