//! The background boot update .
//!
//! [`run_boot_update`] runs once per start, after the plugins loaded, when
//! `plugins.auto_update_on_boot` is on. It looks only at installed plugins
//! that came from the marketplace, are enabled and have the `auto` policy;
//! it syncs only their sources, then for each plugin, at most once:
//!
//! ```text
//! check -> download -> stage -> evaluate -> eligible? -> drain tasks -> activate
//!                                   `-> no: stays staged, "awaiting approval"
//! ```
//!
//! `plugins.boot_update_budget` bounds the run: the syncs, the downloads and
//! the drain wait are cut at the deadline, and no new step starts after it.
//! Two steps are never cut once started, because they hold the install lock
//! and change files: staging (a local unpack and validation) and the
//! activation swap. Both run on a spawned task the budget does not cancel. A
//! failure, an unreachable source or the budget keep the installed package;
//! the next start tries again.
//!
//! The run's report is `<data_dir>/marketplace/.boot-report.json` (the dot
//! keeps it apart from the `<source-id>.json` catalog caches), together
//! with the plugins whose activation the startup recovery rolled back
//! ([`record_recovered`]).

use crate::{
    activation::{write_atomic, RecoveredActivation},
    marketplace::{sync_source, FetchPolicy, UpdateCandidate},
    system::PluginSystem,
};
use shared::model::{
    PluginBootReportDto, PluginBootUpdateEntryDto, PluginBootUpdateOutcomeDto, PluginUpdateEvent, PluginUpdateOutcome,
    PluginUpdatePolicy,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::time::Instant;
use tuliprox_core::model::PluginsConfig;

/// The report lives beside the catalog caches.
const REPORT_DIR: &str = "marketplace";
const REPORT_FILE: &str = ".boot-report.json";

/// The reason given to every plugin the budget cut off.
const OUT_OF_BUDGET: &str = "the boot update budget ran out; retried on the next start";

/// `<data_dir>/marketplace/.boot-report.json`.
#[must_use]
pub fn boot_report_path(data_dir: &Path) -> PathBuf { data_dir.join(REPORT_DIR).join(REPORT_FILE) }

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
}

/// The stored report; an empty one when there is none or it cannot be read.
/// Blocking.
fn load_report(data_dir: &Path) -> PluginBootReportDto {
    fs::read(boot_report_path(data_dir)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

fn store_report(data_dir: &Path, report: &PluginBootReportDto) {
    let written = serde_json::to_vec_pretty(report)
        .map_err(std::io::Error::other)
        .and_then(|bytes| write_atomic(&boot_report_path(data_dir), &bytes));
    if let Err(err) = written {
        log::error!("cannot write the plugin boot update report: {err}");
    }
}

/// `GET /api/v1/plugins/updates/boot-report`: the last boot update run and the
/// last start's recovered activations. An empty report before the first run.
pub async fn read_boot_report(data_dir: &Path) -> PluginBootReportDto {
    let data_dir = data_dir.to_path_buf();
    tokio::task::spawn_blocking(move || load_report(&data_dir)).await.unwrap_or_default()
}

/// Put this start's recovered activations (see
/// [`crate::recover_activations`]) into the report, replacing the previous
/// start's. Blocking: call it on the blocking pool next to the recovery.
pub fn record_recovered(data_dir: &Path, recovered: &[RecoveredActivation]) {
    let ids: BTreeSet<String> = recovered.iter().map(|item| item.id.clone()).collect();
    let mut report = load_report(data_dir);
    let ids: Vec<String> = ids.into_iter().collect();
    if report.recovered == ids {
        return;
    }
    report.recovered = ids;
    store_report(data_dir, &report);
}

impl PluginSystem {
    /// [`read_boot_report`] of this system's data dir.
    pub async fn boot_report(&self) -> PluginBootReportDto { read_boot_report(&self.marketplace_data_dir()).await }
}

/// One plugin the run looks at.
struct Target {
    id: String,
    from: String,
    source: String,
}

/// How the work on one plugin ended.
enum Step {
    UpToDate,
    Applied,
    /// Staged; an admin decides. Carries why.
    Awaiting(String),
    Failed(String),
    /// Not tried this time; carries why, and whether the release is staged
    /// (then it waits like an update an admin must apply).
    Skipped {
        reason: String,
        staged: bool,
    },
    /// The deadline passed before the next budgeted step.
    OutOfBudget,
}

/// The report and the events of one run.
struct Run<'a, E: Fn(PluginUpdateEvent)> {
    report: PluginBootReportDto,
    emit: &'a E,
}

impl<E: Fn(PluginUpdateEvent)> Run<'_, E> {
    /// Add `target`'s entry and emit its event: one per plugin that has a
    /// release `to` (an applied, staged, skipped-while-staged or failed one).
    fn record(&mut self, target: &Target, to: Option<&str>, step: Step) {
        let (outcome, reason, event) = match step {
            Step::UpToDate => (PluginBootUpdateOutcomeDto::UpToDate, None, None),
            Step::Applied => (PluginBootUpdateOutcomeDto::Applied, None, Some(PluginUpdateOutcome::Applied)),
            Step::Awaiting(reason) => {
                (PluginBootUpdateOutcomeDto::AwaitingApproval, Some(reason), Some(PluginUpdateOutcome::Available))
            }
            Step::Skipped { reason, staged } => {
                (PluginBootUpdateOutcomeDto::Skipped, Some(reason), staged.then_some(PluginUpdateOutcome::Available))
            }
            Step::Failed(reason) => {
                (PluginBootUpdateOutcomeDto::Failed, Some(reason), Some(PluginUpdateOutcome::Failed))
            }
            Step::OutOfBudget => {
                self.report.budget_exceeded = true;
                (PluginBootUpdateOutcomeDto::Failed, Some(OUT_OF_BUDGET.to_string()), Some(PluginUpdateOutcome::Failed))
            }
        };
        match outcome {
            PluginBootUpdateOutcomeDto::UpToDate => {}
            PluginBootUpdateOutcomeDto::Applied => {
                log::info!("boot update: plugin `{}` {} -> {}", target.id, target.from, to.unwrap_or_default());
            }
            other => log::warn!(
                "boot update: plugin `{}` {}: {}",
                target.id,
                match other {
                    PluginBootUpdateOutcomeDto::AwaitingApproval => "awaits approval",
                    PluginBootUpdateOutcomeDto::Skipped => "skipped",
                    _ => "failed",
                },
                reason.as_deref().unwrap_or_default()
            ),
        }
        let event = event.zip(to).map(|(kind, to)| PluginUpdateEvent {
            outcome: kind,
            plugin: target.id.clone(),
            from: target.from.clone(),
            to: to.to_string(),
            source: target.source.clone(),
            reason: reason.clone(),
        });
        self.report.entries.push(PluginBootUpdateEntryDto {
            id: target.id.clone(),
            from: target.from.clone(),
            to: to.map(ToString::to_string),
            source: Some(target.source.clone()),
            outcome,
            reason,
            attempted_at: now_secs(),
        });
        if let Some(event) = event {
            (self.emit)(event);
        }
    }
}

const fn policy_name(policy: PluginUpdatePolicy) -> &'static str {
    match policy {
        PluginUpdatePolicy::Auto => "auto",
        PluginUpdatePolicy::Manual => "manual",
        PluginUpdatePolicy::Pinned => "pinned",
    }
}

/// The boot update run (see the module docs). `config` is the plugins config
/// with every entry's settings **as written** (`${env:…}` unresolved), as the
/// update evaluation and the activation reload need; its repositories are the
/// runtime ones. `client` and `policy` are those of a marketplace download.
/// `emit` gets one [`PluginUpdateEvent`] per plugin that has a release to
/// report (`available`, `applied` or `failed`).
///
/// Returns the report it stored. Does nothing (and returns the stored report)
/// when `auto_update_on_boot` is off.
pub async fn run_boot_update(
    system: &PluginSystem,
    config: &PluginsConfig,
    client: &reqwest::Client,
    policy: FetchPolicy,
    emit: impl Fn(PluginUpdateEvent),
) -> PluginBootReportDto {
    let data_dir = system.marketplace_data_dir();
    if !config.auto_update_on_boot {
        return read_boot_report(&data_dir).await;
    }
    let budget = config.boot_update_budget.as_duration();
    let deadline = Instant::now() + budget;
    let previous = read_boot_report(&data_dir).await;
    let mut run = Run {
        report: PluginBootReportDto {
            started_at: Some(now_secs()),
            finished_at: None,
            budget_ms: config.boot_update_budget.get(),
            budget_exceeded: false,
            recovered: previous.recovered,
            entries: Vec::new(),
        },
        emit: &emit,
    };
    log::info!("boot update: checking marketplace plugins (budget {} ms)", run.report.budget_ms);

    let targets = select_targets(system, config, &mut run);
    let unreachable = sync_sources(&data_dir, config, client, policy, &targets, deadline).await;
    let candidates: BTreeMap<String, UpdateCandidate> = if Instant::now() < deadline {
        // Per id the newest comes first and, when the approval does not cover
        // it, the newest release it does cover comes second: keeping the last
        // one prefers the update that needs no review (the newest then waits
        // for the next start).
        system.update_candidates(config).await.into_iter().map(|candidate| (candidate.id.clone(), candidate)).collect()
    } else {
        BTreeMap::new()
    };

    for target in &targets {
        let candidate = candidates.get(&target.id);
        let to = candidate.map(|candidate| candidate.version.as_str());
        if run.report.budget_exceeded || Instant::now() >= deadline {
            run.record(target, to, Step::OutOfBudget);
            continue;
        }
        if let Some(Some(error)) = unreachable.get(&target.source) {
            run.record(target, None, Step::Failed(format!("source `{}` is unreachable: {error}", target.source)));
            continue;
        }
        if unreachable.get(&target.source).is_some_and(Option::is_none) {
            // The sync of this source was cut by the budget.
            run.record(target, None, Step::OutOfBudget);
            continue;
        }
        let Some(candidate) = candidate else {
            run.record(target, None, Step::UpToDate);
            continue;
        };
        let step = update_one(system, config, client, policy, candidate, deadline).await;
        run.record(target, to, step);
    }

    run.report.finished_at = Some(now_secs());
    let report = run.report;
    let stored = report.clone();
    let dir = data_dir.clone();
    if let Err(err) = tokio::task::spawn_blocking(move || store_report(&dir, &stored)).await {
        log::error!("cannot write the plugin boot update report: {err}");
    }
    log::info!(
        "boot update: done ({} plugins looked at{})",
        report.entries.len(),
        if report.budget_exceeded { ", budget exceeded" } else { "" }
    );
    report
}

/// The installed marketplace plugins the run may update; the others that came
/// from the marketplace are recorded as skipped with the reason.
fn select_targets<E: Fn(PluginUpdateEvent)>(
    system: &PluginSystem,
    config: &PluginsConfig,
    run: &mut Run<'_, E>,
) -> Vec<Target> {
    let installed: BTreeMap<_, _> = system.marketplace_installed().into_iter().collect();
    let mut targets = Vec::new();
    for (id, installed) in installed {
        let Some(origin) = installed.origin else { continue };
        let target = Target { id: id.clone(), from: installed.version.unwrap_or_default(), source: origin.source };
        let entry = config.entry(&id);
        let skip = match entry {
            None => Some("not enabled".to_string()),
            Some(entry) if !entry.enabled => Some("not enabled".to_string()),
            Some(entry) if !entry.update_policy.is_auto() => {
                Some(format!("update policy `{}`", policy_name(entry.update_policy)))
            }
            Some(_) if !config.repositories.iter().any(|repo| repo.id == target.source && repo.enabled) => {
                Some(format!("source `{}` is not configured or not enabled", target.source))
            }
            Some(_) => None,
        };
        match skip {
            Some(reason) => run.record(&target, None, Step::Skipped { reason, staged: false }),
            None => targets.push(target),
        }
    }
    targets
}

/// Sync the sources of `targets`, each once. Answers the sources that did not
/// sync: `Some(error)` when it failed, `None` when the budget cut it.
async fn sync_sources(
    data_dir: &Path,
    config: &PluginsConfig,
    client: &reqwest::Client,
    policy: FetchPolicy,
    targets: &[Target],
    deadline: Instant,
) -> BTreeMap<String, Option<String>> {
    let sources: BTreeSet<&str> = targets.iter().map(|target| target.source.as_str()).collect();
    let mut unreachable = BTreeMap::new();
    for source in sources {
        let Some(repository) = config.repositories.iter().find(|repo| repo.id == source && repo.enabled) else {
            continue;
        };
        match tokio::time::timeout_at(deadline, sync_source(data_dir, client, policy, repository)).await {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => {
                log::warn!("boot update: source `{source}` cannot be synced: {err}");
                unreachable.insert(source.to_string(), Some(err.to_string()));
            }
            Err(_) => {
                unreachable.insert(source.to_string(), None);
            }
        }
    }
    unreachable
}

/// Download, then stage, evaluate, drain and activate one update. The
/// download is cut at the deadline; the rest runs on its own task (see
/// [`apply_update`]), which this awaits to the end.
async fn update_one(
    system: &PluginSystem,
    config: &PluginsConfig,
    client: &reqwest::Client,
    policy: FetchPolicy,
    candidate: &UpdateCandidate,
    deadline: Instant,
) -> Step {
    let package = match tokio::time::timeout_at(deadline, system.download_update(client, policy, candidate)).await {
        Ok(Ok(package)) => package,
        Ok(Err(err)) => return Step::Failed(format!("download failed: {err}")),
        Err(_) => return Step::OutOfBudget,
    };
    let job = tokio::spawn(apply_update(system.clone(), config.clone(), candidate.clone(), package, deadline));
    job.await.unwrap_or_else(|err| Step::Failed(format!("the update task failed: {err}")))
}

/// Stage, evaluate, drain and activate under the install lock. Each step
/// that may wait checks the deadline first; staging and the activation are
/// never cut once started (a dropped future would release the lock while
/// their blocking work goes on). Runs on a spawned task, so cancelling the
/// boot run does not cut it either.
async fn apply_update(
    system: PluginSystem,
    config: PluginsConfig,
    candidate: UpdateCandidate,
    package: Vec<u8>,
    deadline: Instant,
) -> Step {
    let Ok(guard) = tokio::time::timeout_at(deadline, system.install_lock()).await else {
        return Step::OutOfBudget;
    };
    match system.stage_update(&guard, &candidate, package).await {
        Ok(_) => {}
        // The staging slot holds an admin's upload: never replace it.
        Err(rejection) if rejection.status == http::StatusCode::CONFLICT => {
            let reason = format!("an upload is staged for this plugin: {}", rejection.body.error);
            return Step::Skipped { reason, staged: false };
        }
        Err(rejection) => return Step::Failed(format!("staging failed: {}", rejection.body.error)),
    }
    if Instant::now() >= deadline {
        return Step::OutOfBudget;
    }
    let eligibility = system.evaluate_update(&config, &candidate).await;
    if !eligibility.is_eligible() {
        return Step::Awaiting(eligibility.reason().unwrap_or_else(|| "it needs a review".to_string()));
    }
    if !eligibility.verified {
        return Step::Awaiting("the staged package could not be verified; it needs a review".to_string());
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Step::OutOfBudget;
    }
    if !system.wait_for_idle_tasks(remaining).await {
        let reason =
            "running plugin tasks did not finish within the budget; the update stays staged until the next start";
        return Step::Skipped { reason: reason.to_string(), staged: true };
    }
    // From here on the budget no longer applies: a swap runs to its end.
    match system.activate_staged_unattended(&guard, &candidate.id, &candidate.version, &config).await {
        Ok(_) => Step::Applied,
        Err(rejection) => Step::Failed(rejection.body.error),
    }
}
