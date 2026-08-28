//! Rule materialization scheduler.
//!
//! Pure: enumerate the (rule, occurrence) pairs the scheduler
//! should materialize. The caller applies the actions under the
//! queue-mutation boundary and the fixed cross-store lock order:
//!
//! ```text
//! queue mutation boundary -> rule repository mutation
//! ```
//!
//! The pure planner enumerates candidates; the runtime runner loads
//! rules from disk and writes materialized tasks through
//! `RecordingService`.

use super::{
    recording_ctx::RecordingCtx,
    recording_occurrence::{
        candidate_channel_key, candidate_episode_key, matches_new_episode, next_weekly_occurrence, occurrence_key,
    },
    recording_reconciliation::{ReconcilableTask, MIN_TOMBSTONE_HORIZON_SECS},
    recording_service::{CreateRecordingInput, RecordingService, RecordingSourceInput},
};
use crate::download::{DownloadState, FileDownload};
use chrono::{DateTime, Utc};
use log::{debug, error};
use shared::model::{
    recording::{RecordingProvenance, RecordingVisibility},
    recording_rule::{RecordingRule, RuleBody, RuleSource, RuleVisibility, TombstoneSet},
    Claims, EpgProgramme, EventSink, Permission, PermissionSet, RoleSet, XtreamCluster,
    CURRENT_PERMISSION_SCHEMA_VERSION,
};
use tokio_util::sync::CancellationToken;
use tuliprox_repository::recording_rule_repository::RecordingRuleRepository;

/// Maximum look-ahead for a weekly rule without an EPG horizon.
pub const WEEKLY_FALLBACK_HORIZON_SECS: i64 = MIN_TOMBSTONE_HORIZON_SECS;
const RULE_SCHEDULER_INTERVAL_SECS: u64 = 60;

/// A single (rule, occurrence) pair the scheduler should
/// materialize. The `programme` is the EPG match for `NewEpisode`
/// rules; for `WeeklyTimeslot` it is `None` and the caller fills
/// the programme metadata from the rule's body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializationCandidate {
    pub rule_id: String,
    pub rule_owner_id: shared::model::UserId,
    pub rule_visibility: RuleVisibility,
    pub source: RuleSource,
    pub channel_id: Option<String>,
    pub channel_name: Option<String>,
    pub pre_roll_secs: u64,
    pub post_roll_secs: u64,
    pub programme_start: i64,
    pub programme_end: i64,
    pub programme_title: String,
    pub occurrence_key: String,
}

/// Pure: enumerate the candidates the scheduler should create.
/// `rules` is the rule repository's current rule list.
/// `epg_programmes` is the EPG horizon (the slice of EPG that
/// covers `[now, epg_horizon_end]`).
/// `tasks` is the current set of tasks (for dedup / `rule_id`
/// provenance).
/// `tombstones` is the current tombstone set (for dedup).
/// `now` is the current server time (Unix seconds).
/// `epg_horizon_end` is the end of the available EPG horizon. The
/// scheduler only considers programmes that start on or before
/// this instant. When `None`, weekly rules fall back to the
/// `WEEKLY_FALLBACK_HORIZON_SECS` window.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn plan_materializations(
    rules: &[RecordingRule],
    epg_programmes: &[EpgProgramme],
    tasks: &[ReconcilableTask],
    tombstones: &TombstoneSet,
    now: i64,
    epg_horizon_end: Option<i64>,
) -> Vec<MaterializationCandidate> {
    let mut out: Vec<MaterializationCandidate> = Vec::new();

    // Index existing (rule_id, occurrence_key) so the scheduler
    // never produces a duplicate.
    let mut existing: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::with_capacity(tasks.len() + tombstones.tombstones.len());
    for task in tasks {
        if let (Some(rule_id), Some(key)) = (task.rule_id.as_deref(), task.occurrence_key.as_deref()) {
            existing.insert((rule_id.to_string(), key.to_string()));
        }
    }
    for t in &tombstones.tombstones {
        if t.expires_at > now {
            existing.insert((t.rule_id.clone(), t.occurrence_key.clone()));
        }
    }

    for rule in rules {
        if !rule.enabled {
            continue;
        }
        match &rule.body {
            RuleBody::NewEpisode { series_id, title_pattern, exclude_repeat } => {
                let _ = (series_id, title_pattern, exclude_repeat);
                // For each EPG programme, evaluate the match. The
                // rule's local channel_id is the channel key when
                // the EPG programme carries an id; otherwise we
                // fall back to the EPG programme's id / title.
                for prog in epg_programmes {
                    if prog.start < now {
                        continue;
                    }
                    if let Some(end) = epg_horizon_end {
                        if prog.start > end {
                            continue;
                        }
                    }
                    let airing_is_repeat =
                        matches!(prog.airing_status(), shared::model::recording::AiringStatus::Repeat);
                    let programme_title = prog.title.as_deref();
                    let m = matches_new_episode(&rule.body, None, programme_title, airing_is_repeat);
                    if !matches!(m, crate::recording_occurrence::NewEpisodeMatch::NewEpisode) {
                        continue;
                    }
                    let channel = candidate_channel_key(Some(prog.get_transient_channel_id().as_ref()), None);
                    let episode = candidate_episode_key(None, None, None, None, None, programme_title);
                    let key = occurrence_key(&rule.id, &rule.source, &channel, prog.start, &episode);
                    if existing.contains(&(rule.id.clone(), key.clone())) {
                        continue;
                    }
                    out.push(MaterializationCandidate {
                        rule_id: rule.id.clone(),
                        rule_owner_id: rule.owner_id.clone(),
                        rule_visibility: rule.visibility,
                        source: rule.source.clone(),
                        channel_id: Some(prog.get_transient_channel_id().to_string()),
                        channel_name: None,
                        pre_roll_secs: rule.pre_roll_secs,
                        post_roll_secs: rule.post_roll_secs,
                        programme_start: prog.start,
                        programme_end: prog.stop,
                        programme_title: programme_title.unwrap_or("Untitled").to_string(),
                        occurrence_key: key,
                    });
                }
            }
            RuleBody::WeeklyTimeslot { duration_secs, .. } => {
                let Some(fallback_horizon_end) = now.checked_add(WEEKLY_FALLBACK_HORIZON_SECS) else {
                    continue;
                };
                let horizon_end = epg_horizon_end.unwrap_or(fallback_horizon_end).min(fallback_horizon_end);
                let Some(now_utc) = DateTime::<Utc>::from_timestamp(now, 0) else {
                    continue;
                };
                let Some(start) = next_weekly_occurrence(&rule.body, now_utc) else {
                    continue;
                };
                if start > horizon_end {
                    continue;
                }
                let Ok(duration_secs) = i64::try_from(*duration_secs) else {
                    continue;
                };
                let Some(programme_end) = start.checked_add(duration_secs) else {
                    continue;
                };
                let channel = candidate_channel_key(rule.channel_id.as_deref(), None);
                let episode = String::new();
                let key = occurrence_key(&rule.id, &rule.source, &channel, start, &episode);
                if existing.contains(&(rule.id.clone(), key.clone())) {
                    continue;
                }
                out.push(MaterializationCandidate {
                    rule_id: rule.id.clone(),
                    rule_owner_id: rule.owner_id.clone(),
                    rule_visibility: rule.visibility,
                    source: rule.source.clone(),
                    channel_id: rule.channel_id.clone(),
                    channel_name: None,
                    pre_roll_secs: rule.pre_roll_secs,
                    post_roll_secs: rule.post_roll_secs,
                    programme_start: start,
                    programme_end,
                    programme_title: format!("Weekly slot at {start}"),
                    occurrence_key: key,
                });
            }
        }
    }

    out
}

pub fn spawn_recording_rule_scheduler<E: EventSink + Clone + 'static>(
    ctx: &RecordingCtx<E>,
    cancel_token: &CancellationToken,
) {
    let ctx = ctx.clone();
    let cancel_token = cancel_token.clone();
    tokio::spawn(async move {
        loop {
            if let Err(err) = materialize_due_rules(&ctx).await {
                error!("Recording rule scheduler failed: {err}");
            }
            tokio::select! {
                () = cancel_token.cancelled() => break,
                () = tokio::time::sleep(std::time::Duration::from_secs(RULE_SCHEDULER_INTERVAL_SECS)) => {}
            }
        }
    });
}

async fn materialize_due_rules<E: EventSink + Clone + 'static>(ctx: &RecordingCtx<E>) -> Result<(), String> {
    // `recording.enabled: false` has to stop the scheduler too. Without
    // this the routes refuse every request while this loop keeps quietly
    // materializing tasks the worker will then run — the worst of both
    // states, and invisible to the operator who just switched the DVR
    // off. Checked per tick, not at spawn time, so a config reload takes
    // effect without a restart.
    if !super::recording_supervisor::recording_enabled(&ctx.app_config) {
        return Ok(());
    }
    let storage_dir = ctx.app_config.config.load().storage_dir.clone();
    let file = RecordingRuleRepository::new(storage_dir).load().await.map_err(|err| err.to_string())?;
    let tasks = reconcilable_tasks(ctx).await;
    // The EPG horizon this tick. `plan_materializations` matches
    // `NewEpisode` rules by walking programmes, so an empty slice means no
    // `NewEpisode` rule can ever produce a candidate — only
    // `WeeklyTimeslot` rules, which need no EPG, still work.
    //
    // Nothing supplies the horizon yet: the runner has no resolution path
    // from a rule's `(target_id, virtual_id)` to that channel's stored
    // programmes. Until one exists, say so out loud rather than looking
    // like a scheduler that simply found nothing — a silently inert rule
    // is indistinguishable from a rule that matched no programmes.
    let epg_programmes: &[EpgProgramme] = &[];
    warn_about_inert_new_episode_rules(&file.rules, epg_programmes);
    let candidates =
        plan_materializations(&file.rules, epg_programmes, &tasks, &file.tombstones, Utc::now().timestamp(), None);
    if candidates.is_empty() {
        return Ok(());
    }
    let service = RecordingService::from_ctx(ctx);
    for candidate in candidates {
        let claims = scheduler_claims(candidate.rule_owner_id.clone(), candidate.rule_visibility);
        let input = CreateRecordingInput {
            source: RecordingSourceInput {
                target_id: candidate.source.target_id,
                virtual_id: candidate.source.virtual_id,
                cluster: XtreamCluster::Live,
                input_name: candidate.source.input_name,
            },
            program_title: candidate.programme_title,
            program_start: candidate.programme_start,
            program_end: candidate.programme_end,
            pre_roll_secs: candidate.pre_roll_secs,
            post_roll_secs: candidate.post_roll_secs,
            visibility: match candidate.rule_visibility {
                RuleVisibility::Private => RecordingVisibility::Private,
                RuleVisibility::Shared => RecordingVisibility::Shared,
            },
            channel_id: candidate.channel_id,
            channel_name: candidate.channel_name,
            provenance: RecordingProvenance {
                rule_id: Some(candidate.rule_id),
                occurrence_key: Some(candidate.occurrence_key),
            },
            epg: None,
        };
        match service.create_recording(&claims, &input).await {
            Ok(view) => debug!("Materialized recording rule task {}", view.uuid),
            Err(err) if err.code() == "recording_invalid_state" => {}
            Err(err) => error!("Failed to materialize recording rule: {err}"),
        }
    }
    Ok(())
}

/// Summarize every queue-resident recording for the reconciliation and
/// materialization planners. Shared with
/// [`recording_supervisor`](super::recording_supervisor), which needs the
/// same view at startup.
/// Warn when the operator has enabled `NewEpisode` rules that cannot
/// fire because no EPG horizon is available.
///
/// Rate-limited to once per process: this runs every tick, and a warning
/// per minute per rule would bury the log it is trying to make visible.
fn warn_about_inert_new_episode_rules(rules: &[RecordingRule], epg_programmes: &[EpgProgramme]) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !epg_programmes.is_empty() {
        return;
    }
    let inert = rules.iter().filter(|rule| rule.enabled && matches!(rule.body, RuleBody::NewEpisode { .. })).count();
    if inert == 0 {
        return;
    }
    if WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    log::warn!(
        "{inert} enabled NewEpisode recording rule(s) cannot match: the scheduler has no EPG \
         horizon, so only WeeklyTimeslot rules materialize. Use a WeeklyTimeslot rule, or record \
         individual programmes from the EPG view, until the EPG horizon is wired into the scheduler."
    );
}

pub async fn reconcilable_tasks<E: EventSink + Clone + 'static>(ctx: &RecordingCtx<E>) -> Vec<ReconcilableTask> {
    let mut tasks = Vec::new();
    for task in ctx.downloads.queue.lock().await.iter() {
        push_reconcilable(&mut tasks, task);
    }
    for task in ctx.downloads.scheduled.read().await.iter() {
        push_reconcilable(&mut tasks, task);
    }
    if let Some(task) = ctx.downloads.active.read().await.as_ref() {
        push_reconcilable(&mut tasks, task);
    }
    for task in ctx.downloads.finished.read().await.iter() {
        push_reconcilable(&mut tasks, task);
    }
    tasks
}

fn push_reconcilable(tasks: &mut Vec<ReconcilableTask>, task: &FileDownload) {
    let Some(meta) = task.recording.as_ref() else {
        return;
    };
    tasks.push(ReconcilableTask {
        uuid: task.uuid.clone(),
        rule_id: meta.provenance.rule_id.clone(),
        occurrence_key: meta.provenance.occurrence_key.clone(),
        terminal: matches!(task.state, DownloadState::Completed | DownloadState::Failed | DownloadState::Cancelled),
        active: matches!(task.state, DownloadState::Downloading),
        editable: matches!(task.state, DownloadState::Scheduled | DownloadState::Queued | DownloadState::Paused),
    });
}

fn scheduler_claims(owner_id: shared::model::UserId, visibility: RuleVisibility) -> Claims {
    let mut permissions = PermissionSet::new();
    permissions.set(Permission::RecordingWrite);
    let roles = if visibility == RuleVisibility::Shared { RoleSet::ADMIN } else { RoleSet::new() };
    let now = Utc::now().timestamp();
    Claims {
        username: "recording-scheduler".to_string(),
        iss: "tuliprox".to_string(),
        iat: now,
        exp: now + 3600,
        roles,
        permissions,
        pwd_version: 0,
        subject_id: Some(owner_id),
        permission_schema_version: CURRENT_PERMISSION_SCHEMA_VERSION,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording_reconciliation::ReconcilableTask;
    use shared::{
        model::{
            recording_rule::{RuleSource, RuleVisibility},
            EpgProgramme, UserId,
        },
        utils::Internable,
    };

    fn user() -> UserId {
        UserId::from("web:alice")
    }
    fn source() -> RuleSource {
        RuleSource::new("tgt", "virt", "input")
    }
    fn new_episode_rule() -> RecordingRule {
        RecordingRule {
            id: "r1".into(),
            owner_id: user(),
            visibility: RuleVisibility::Private,
            enabled: true,
            source: source(),
            channel_id: None,
            body: RuleBody::NewEpisode { series_id: None, title_pattern: Some("My Show".into()), exclude_repeat: true },
            pre_roll_secs: 0,
            post_roll_secs: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn weekly_rule() -> RecordingRule {
        RecordingRule {
            id: "r2".into(),
            owner_id: user(),
            visibility: RuleVisibility::Private,
            enabled: true,
            source: source(),
            channel_id: Some("ch-1".into()),
            body: RuleBody::WeeklyTimeslot {
                weekday: 7, // Sunday
                local_start_time: "20:00".into(),
                duration_secs: 1800,
                timezone: "UTC".into(),
            },
            pre_roll_secs: 0,
            post_roll_secs: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn epg_programme(title: &str, start: i64, stop: i64) -> EpgProgramme {
        let mut p = EpgProgramme::new(start, stop, "ch-1".intern());
        p.title = Some(title.intern());
        p
    }

    fn task(rule_id: &str, key: &str) -> ReconcilableTask {
        ReconcilableTask {
            uuid: format!("{rule_id}-{key}"),
            rule_id: Some(rule_id.into()),
            occurrence_key: Some(key.into()),
            terminal: false,
            active: false,
            editable: true,
        }
    }

    #[test]
    fn new_episode_matches_and_materializes() {
        let rules = vec![new_episode_rule()];
        let programmes = vec![epg_programme("My Show", 1_000, 2_000), epg_programme("Other", 1_000, 2_000)];
        let out = plan_materializations(&rules, &programmes, &[], &TombstoneSet::default(), 0, Some(10_000));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].programme_title, "My Show");
    }

    #[test]
    fn new_episode_dedupes_against_existing_task() {
        let rules = vec![new_episode_rule()];
        let programmes = vec![epg_programme("My Show", 1_000, 2_000)];
        let key = occurrence_key("r1", &source(), "ch-1", 1_000, "t:my show");
        let tasks = vec![task("r1", &key)];
        let out = plan_materializations(&rules, &programmes, &tasks, &TombstoneSet::default(), 0, Some(10_000));
        assert!(out.is_empty());
    }

    #[test]
    fn new_episode_skips_programs_in_the_past() {
        let rules = vec![new_episode_rule()];
        let programmes = vec![epg_programme("My Show", -1, 100)];
        let out = plan_materializations(&rules, &programmes, &[], &TombstoneSet::default(), 0, Some(10_000));
        assert!(out.is_empty());
    }

    #[test]
    fn new_episode_skips_programs_outside_epg_horizon() {
        let rules = vec![new_episode_rule()];
        let programmes = vec![epg_programme("My Show", 5_000, 6_000)];
        let out = plan_materializations(&rules, &programmes, &[], &TombstoneSet::default(), 0, Some(2_000));
        assert!(out.is_empty());
    }

    #[test]
    fn new_episode_skips_explicit_repeat() {
        let rules = vec![new_episode_rule()];
        let mut p = epg_programme("My Show", 1_000, 2_000);
        p.previously_shown = true;
        let programmes = vec![p];
        let out = plan_materializations(&rules, &programmes, &[], &TombstoneSet::default(), 0, Some(10_000));
        assert!(out.is_empty());
    }

    #[test]
    fn disabled_rule_does_not_materialize() {
        let mut r = new_episode_rule();
        r.enabled = false;
        let rules = vec![r];
        let programmes = vec![epg_programme("My Show", 1_000, 2_000)];
        let out = plan_materializations(&rules, &programmes, &[], &TombstoneSet::default(), 0, Some(10_000));
        assert!(out.is_empty());
    }

    #[test]
    fn weekly_rule_uses_fallback_horizon_without_epg() {
        let rules = vec![weekly_rule()];
        // Pick a Sunday 20:00 UTC; the next occurrence from
        // 1970-01-01 (Thursday) is the next Sunday.
        let now = 0; // 1970-01-01
        let out = plan_materializations(&rules, &[], &[], &TombstoneSet::default(), now, None);
        assert_eq!(out.len(), 1);
        let cand = &out[0];
        assert_eq!(cand.rule_id, "r2");
        assert!(cand.programme_start > now);
        assert!(cand.programme_start - now <= WEEKLY_FALLBACK_HORIZON_SECS);
    }

    #[test]
    fn weekly_rule_skipped_when_outside_horizon() {
        let rules = vec![weekly_rule()];
        let now = 0;
        // Set the EPG horizon to 1 hour — the next Sunday is
        // days away.
        let out = plan_materializations(&rules, &[], &[], &TombstoneSet::default(), now, Some(3_600));
        assert!(out.is_empty());
    }

    #[test]
    fn weekly_rule_skips_duration_that_overflows_end_timestamp() {
        let mut rule = weekly_rule();
        if let RuleBody::WeeklyTimeslot { duration_secs, .. } = &mut rule.body {
            *duration_secs = i64::MAX as u64;
        }

        let out = plan_materializations(&[rule], &[], &[], &TombstoneSet::default(), 0, None);

        assert!(out.is_empty());
    }
}
