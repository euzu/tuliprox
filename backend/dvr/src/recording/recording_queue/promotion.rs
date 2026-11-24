use super::{
    mutate_optional, PersistedRecordingQueue, PersistedRecordingTask, RecordingQueue, RecordingTask,
    RecordingTaskState, RECORDING_SOURCE_GAP, RECORDING_SOURCE_RECORDING_FAILED, RECORDING_WINDOW_EXPIRED_ERR,
};
use crate::recording::recording_path;
use chrono::Utc;
use shared::{
    model::{RecordingKind, RecordingMetadata},
    utils::CONSTANTS,
};
use tuliprox_core::model::RecordingConfig;

/// What the organised layout groups this recording under.
///
/// Every kind is filed under its playlist group. Without one, Live groups by
/// channel and VOD by title. A series groups by its series name, which falls
/// back to the programme title and then to the episode pattern applied to the
/// filename stem, which is how the layout was derived before the metadata
/// carried it.
pub(super) fn recording_grouping(
    kind: RecordingKind,
    recording: &RecordingMetadata,
    recording_cfg: &RecordingConfig,
    file_stem: &str,
) -> recording_path::RecordingGrouping {
    let grouping = match kind {
        RecordingKind::Live => recording_path::RecordingGrouping::live(
            recording.channel_name.clone().unwrap_or_else(|| stem_group(recording_cfg, file_stem)),
        ),
        RecordingKind::Vod => recording_path::RecordingGrouping::vod(
            recording.program_title.clone().unwrap_or_else(|| stem_group(recording_cfg, file_stem)),
        ),
        RecordingKind::Series => recording_path::RecordingGrouping::series(
            recording
                .series_name
                .clone()
                .or_else(|| recording.program_title.clone())
                .unwrap_or_else(|| stem_group(recording_cfg, file_stem)),
        ),
    };
    grouping.in_group(recording.group.clone())
}

/// The pre-metadata grouping rule: strip the episode marker and any trailing
/// filename decoration from the stem.
///
/// The stem is cut before a named `episode` group when the pattern has one,
/// otherwise before the whole match. The default pattern names its groups per
/// branch (`episode_s`, `episode_n`, ...), so it relies on the whole match.
fn stem_group(recording_cfg: &RecordingConfig, file_stem: &str) -> String {
    let cut = recording_cfg.episode_pattern.as_ref().and_then(|re| re.captures(file_stem)).and_then(|captures| {
        let marker = captures.name("episode").or_else(|| captures.get(0))?;
        (!marker.as_str().is_empty()).then_some(marker.start())
    });
    let stem = cut.map_or(file_stem, |end| &file_stem[..end]);
    CONSTANTS.re_remove_filename_ending.replace(stem, "").into_owned()
}

/// What to do with a queued task whose media another entry may already hold.
///
/// Several library entries can reference one physical file. Promoting each of
/// them would start a second transfer to the same path, so a queued entry that
/// is not the one producing the file must attach to it or wait for it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromotionDecision {
    /// Nothing else holds this media; run the transfer.
    Execute,
    /// A completed entry at this index in `finished` already holds it.
    AttachTo(usize),
    /// Another active entry is producing this media; wait for its file.
    Wait,
}

/// Two tasks refer to the same media when they carry the same identity.
///
/// An empty identity never matches, including another empty one: it means the
/// identity could not be resolved, and guessing that two unidentified requests
/// are the same recording would merge two different files.
fn same_media(left: &PersistedRecordingTask, right: &PersistedRecordingTask) -> bool {
    !left.media_identity.is_empty() && left.media_identity == right.media_identity
}

fn same_task_media(left: &RecordingTask, right: &RecordingTask) -> bool {
    match (&left.media_identity, &right.media_identity) {
        (Some(l), Some(r)) => !l.is_empty() && l == r,
        _ => false,
    }
}

/// Every task in the candidate, whichever partition it sits in.
fn all_tasks(candidate: &PersistedRecordingQueue) -> impl Iterator<Item = &PersistedRecordingTask> {
    candidate
        .queue
        .iter()
        .chain(candidate.scheduled.iter())
        .chain(candidate.active.iter())
        .chain(candidate.finished.iter())
}

/// Whether an entry other than `uuid` still holds the same file.
///
/// Entries already mid-deletion are not counted. Two concurrent deletions
/// that each saw the other as a holder would both decline to unlink and
/// leave the file behind with nothing pointing at it.
pub fn media_is_still_referenced(candidate: &PersistedRecordingQueue, uuid: &str) -> bool {
    let Some(subject) = all_tasks(candidate).find(|task| task.uuid == uuid) else {
        return false;
    };
    media_held_by_another(
        all_tasks(candidate).map(|task| {
            (task.uuid.as_str(), task.media_identity.as_str(), task.recording.deleting_previous_state.is_some())
        }),
        uuid,
        &subject.media_identity,
    )
}

/// The reference rule itself, over `(uuid, media identity, being deleted)`
/// entries, so the persisted queue and an in-memory snapshot apply the same
/// one. An entry being deleted holds nothing, and an empty identity matches
/// nothing.
pub fn media_held_by_another<'a>(
    entries: impl IntoIterator<Item = (&'a str, &'a str, bool)>,
    uuid: &str,
    media: &str,
) -> bool {
    !media.is_empty()
        && entries
            .into_iter()
            .any(|(other_uuid, other_media, deleting)| other_uuid != uuid && !deleting && other_media == media)
}

pub fn promotion_decision(candidate: &PersistedRecordingQueue, task: &PersistedRecordingTask) -> PromotionDecision {
    if candidate.active.iter().any(|active| same_media(active, task)) {
        return PromotionDecision::Wait;
    }
    if let Some(index) =
        candidate.finished.iter().position(|done| done.state == RecordingTaskState::Completed && same_media(done, task))
    {
        return PromotionDecision::AttachTo(index);
    }
    PromotionDecision::Execute
}

/// Add the next runnable entry to the active tasks, attaching any entry whose
/// file another entry already produced.
///
/// Every path that activates a task goes through here. Taking the head of
/// the queue directly would re-download a file that was just completed by the
/// entry ahead of it.
pub fn promote_from_queue(candidate: &mut PersistedRecordingQueue) -> Option<(String, String)> {
    let mut index = 0;
    while index < candidate.queue.len() {
        match promotion_decision(candidate, &candidate.queue[index]) {
            PromotionDecision::Execute => {
                // Bulk transfers stay serial; live captures can run alongside them.
                if candidate.queue[index].kind != RecordingKind::Live
                    && candidate.active.iter().any(|task| task.kind != RecordingKind::Live)
                {
                    index += 1;
                    continue;
                }
                let next = candidate.queue.remove(index);
                let promoted = (next.uuid.clone(), next.filename.clone());
                candidate.active.push(next);
                return Some(promoted);
            }
            PromotionDecision::AttachTo(completed) => {
                let source = candidate.finished[completed].clone();
                let mut attached = candidate.queue.remove(index);
                attach_to_completed(&mut attached, &source);
                candidate.finished.push(attached);
                // The queue shrank; re-examine this position.
            }
            PromotionDecision::Wait => index += 1,
        }
    }
    None
}

/// Adopt an already-produced file instead of transferring it again.
///
/// Only the physical result is copied. The owner, visibility and quota belong
/// to the entry and must survive: this is one user's link to a file another
/// user's request happened to produce.
pub fn attach_to_completed(task: &mut PersistedRecordingTask, source: &PersistedRecordingTask) {
    task.file_dir.clone_from(&source.file_dir);
    task.file_path.clone_from(&source.file_path);
    task.filename.clone_from(&source.filename);
    task.size = source.size;
    task.total_size = source.total_size;
    task.finished = true;
    task.paused = false;
    task.error = None;
    task.state = RecordingTaskState::Completed;
    task.next_retry_at = None;
    task.retry_attempts = 0;
    task.recording.relative_path.clone_from(&source.recording.relative_path);
    task.recording.partial_relative_path = None;
    task.recording.measured_bytes = source.recording.measured_bytes;
    task.recording.completed_at = source.recording.completed_at;
    // The reservation is released: the bytes are already on disk and charged
    // to this entry as measured, not reserved.
    task.recording.reserved_bytes = 0;
}

enum ClipPlan {
    Complete { start_offset: u64, end_offset: u64, header_bytes: Option<Vec<u8>> },
    Fail(String),
}

impl RecordingQueue {
    #[allow(clippy::too_many_lines)]
    pub async fn promote_due_scheduled(&self, now_ts: i64) -> usize {
        const GAP: i64 = crate::recording::recording_sidecar::MAX_INDEX_SAMPLE_GAP_SECS;

        let (scheduled_clips, has_active_clips, identities, has_due_non_clips) = {
            let scheduled = self.scheduled.read().await;
            let active = self.active.read().await;
            let has_active_clips = active.iter().any(|t| t.clip.is_some());
            let has_any_scheduled_clips = scheduled.iter().any(|t| t.clip.is_some());
            let mut due_non_clips = false;
            for t in scheduled.iter() {
                if t.clip.is_none()
                    && (t.recording.scheduled_start.is_some_and(|start| start <= now_ts)
                        || (t.kind == RecordingKind::Live
                            && t.recording.scheduled_end.is_some_and(|end| now_ts >= end)))
                {
                    due_non_clips = true;
                    break;
                }
            }

            if !has_any_scheduled_clips && !has_active_clips && !due_non_clips {
                return 0;
            }

            let mut clips = Vec::new();
            let mut identities = std::collections::HashSet::new();
            if has_any_scheduled_clips {
                let needed_identities: std::collections::HashSet<&str> = scheduled
                    .iter()
                    .filter_map(|t| {
                        if t.clip.is_some() {
                            t.media_identity.as_deref().filter(|m| !m.is_empty())
                        } else {
                            None
                        }
                    })
                    .collect();

                let terminal_enclosings: std::collections::HashSet<String> = if needed_identities.is_empty() {
                    std::collections::HashSet::new()
                } else {
                    let finished = self.finished.read().await;
                    finished
                        .iter()
                        .filter_map(|e| {
                            if e.clip.is_none() {
                                e.media_identity.as_ref().filter(|m| needed_identities.contains(m.as_str())).cloned()
                            } else {
                                None
                            }
                        })
                        .collect()
                };

                for t in scheduled.iter() {
                    if let Some(ref clip) = t.clip {
                        let enclosing_terminal =
                            t.media_identity.as_deref().is_some_and(|m| terminal_enclosings.contains(m));
                        let near_or_due = clip.end_ts <= now_ts + GAP;
                        if near_or_due || enclosing_terminal {
                            clips.push(t.clone());
                            if let Some(ref m) = t.media_identity {
                                if !m.is_empty() {
                                    identities.insert(m.clone());
                                }
                            }
                        }
                    }
                }
            }

            for t in active.iter() {
                if t.clip.is_some() {
                    if let Some(ref m) = t.media_identity {
                        if !m.is_empty() {
                            identities.insert(m.clone());
                        }
                    }
                }
            }
            (clips, has_active_clips, identities, due_non_clips)
        };

        if scheduled_clips.is_empty() && !has_active_clips && !has_due_non_clips {
            return 0;
        }

        let (active_enclosing, finished_enclosing, active_clips_snapshot) =
            if scheduled_clips.is_empty() && !has_active_clips {
                (Vec::new(), Vec::new(), Vec::new())
            } else {
                let active = self.active.read().await;
                let active_tasks: Vec<_> = active
                    .iter()
                    .filter(|t| t.clip.is_none() && t.media_identity.as_ref().is_some_and(|m| identities.contains(m)))
                    .cloned()
                    .collect();

                let active_clips: Vec<_> = active.iter().filter(|t| t.clip.is_some()).cloned().collect();

                let finished = self.finished.read().await;
                let finished_tasks: Vec<_> = finished
                    .iter()
                    .filter(|t| t.clip.is_none() && t.media_identity.as_ref().is_some_and(|m| identities.contains(m)))
                    .cloned()
                    .collect();

                (active_tasks, finished_tasks, active_clips)
            };

        let mut candidate_actions = std::collections::HashMap::new();
        let mut index_cache: std::collections::HashMap<
            std::path::PathBuf,
            Vec<crate::recording::recording_sidecar::TimeIndexEntry>,
        > = std::collections::HashMap::new();

        // 1. Evaluate scheduled clips outside the lock
        let scheduled_enclosing: Vec<&RecordingTask> =
            active_enclosing.iter().chain(finished_enclosing.iter()).collect();
        let finished_enclosing_refs: Vec<&RecordingTask> = finished_enclosing.iter().collect();
        evaluate_clip_candidates(
            &scheduled_clips,
            &scheduled_enclosing,
            false,
            now_ts,
            &mut index_cache,
            &mut candidate_actions,
        )
        .await;

        // 2. Evaluate active clips outside the lock
        evaluate_clip_candidates(
            &active_clips_snapshot,
            &finished_enclosing_refs,
            true,
            now_ts,
            &mut index_cache,
            &mut candidate_actions,
        )
        .await;

        if !has_due_non_clips && candidate_actions.is_empty() {
            return 0;
        }

        let mut workers_to_stop = Vec::new();
        let result = mutate_optional(self, |candidate| {
            let mut due_tasks = Vec::new();
            let mut missed_recordings = Vec::new();
            let mut completed_or_failed_clips = Vec::new();

            candidate.scheduled.retain(|task| {
                if task.clip.is_some() {
                    if let Some(plan) = candidate_actions.get(&task.uuid) {
                        match plan {
                            ClipPlan::Complete { start_offset, end_offset, header_bytes } => {
                                let mut done = task.clone();
                                crate::recording::complete_clip(
                                    &mut done,
                                    *start_offset,
                                    *end_offset,
                                    header_bytes.clone(),
                                    now_ts,
                                );
                                completed_or_failed_clips.push(done);
                                return false;
                            }
                            ClipPlan::Fail(reason) => {
                                let mut failed = task.clone();
                                failed.finished = true;
                                failed.paused = false;
                                failed.state = RecordingTaskState::Failed;
                                failed.error = Some(reason.clone());
                                failed.recording.reserved_bytes = 0;
                                completed_or_failed_clips.push(failed);
                                return false;
                            }
                        }
                    }
                    return true;
                }

                let scheduled_start = task.recording.scheduled_start;
                let is_missed =
                    task.kind == RecordingKind::Live && task.recording.scheduled_end.is_some_and(|end| now_ts >= end);
                if is_missed {
                    let mut missed = task.clone();
                    missed.finished = true;
                    missed.paused = false;
                    missed.state = RecordingTaskState::Failed;
                    missed.error = Some(RECORDING_WINDOW_EXPIRED_ERR.to_string());
                    missed.recording.reserved_bytes = 0;
                    missed_recordings.push(missed);
                    return false;
                }
                let is_due = scheduled_start.is_some_and(|start_at| start_at <= now_ts);
                if is_due {
                    let mut queued = task.clone();
                    queued.state = RecordingTaskState::Queued;
                    queued.paused = false;
                    queued.finished = false;
                    queued.error = None;
                    queued.size = 0;
                    queued.total_size = None;
                    queued.retry_attempts = 0;
                    queued.next_retry_at = None;
                    due_tasks.push(queued);
                }
                !is_due
            });

            let mut completed_or_failed_active = Vec::new();
            candidate.active.retain(|active_task| {
                if active_task.clip.is_some() {
                    if let Some(plan) = candidate_actions.get(&active_task.uuid) {
                        match plan {
                            ClipPlan::Complete { start_offset, end_offset, header_bytes } => {
                                let mut done = active_task.clone();
                                crate::recording::complete_clip(
                                    &mut done,
                                    *start_offset,
                                    *end_offset,
                                    header_bytes.clone(),
                                    now_ts,
                                );
                                completed_or_failed_active.push(done.clone());
                                completed_or_failed_clips.push(done);
                                return false;
                            }
                            ClipPlan::Fail(reason) => {
                                let mut failed = active_task.clone();
                                failed.finished = true;
                                failed.paused = false;
                                failed.state = RecordingTaskState::Failed;
                                failed.error = Some(reason.clone());
                                failed.recording.reserved_bytes = 0;
                                completed_or_failed_active.push(failed.clone());
                                completed_or_failed_clips.push(failed);
                                return false;
                            }
                        }
                    }
                    return true;
                }
                true
            });

            for active_done in &completed_or_failed_active {
                let media = &active_done.media_identity;
                let needed = !media.is_empty()
                    && (candidate.active.iter().any(|t| t.media_identity == *media)
                        || candidate.scheduled.iter().any(|t| t.media_identity == *media));
                if !needed {
                    workers_to_stop.push(active_done.uuid.clone());
                }
            }

            if due_tasks.is_empty() && missed_recordings.is_empty() && completed_or_failed_clips.is_empty() {
                return Ok(None);
            }

            let changed_count = due_tasks.len() + missed_recordings.len() + completed_or_failed_clips.len();

            for c in &completed_or_failed_clips {
                if c.clip.is_some() && !c.media_identity.is_empty() {
                    if let Some(worker_uuid) = Self::update_cancelled_enclosing_stop_time(candidate, &c.media_identity)
                    {
                        workers_to_stop.push(worker_uuid);
                    }
                }
            }

            candidate.finished.extend(missed_recordings);
            candidate.finished.extend(completed_or_failed_clips);
            candidate.queue.splice(0..0, due_tasks);

            let enclosing_actions = Self::cleanup_settled_cancelled_enclosings(candidate);

            Ok(Some((changed_count, workers_to_stop, enclosing_actions)))
        })
        .await;

        match result {
            Ok(Some((promoted, workers_to_stop, enclosing_actions))) => {
                for worker_uuid in workers_to_stop {
                    self.stop_worker_early(&worker_uuid).await;
                }
                crate::recording::recording_deletion::apply_cancelled_enclosing_action(self, enclosing_actions).await;
                promoted
            }
            Ok(None) => 0,
            Err(err) => {
                log::error!("Promoting due scheduled recordings failed: {err}");
                0
            }
        }
    }

    pub async fn promote_due_scheduled_now(&self) -> usize { self.promote_due_scheduled(Utc::now().timestamp()).await }
}

async fn evaluate_clip_candidates(
    candidates: &[RecordingTask],
    enclosing_candidates: &[&RecordingTask],
    // Active clips judge their own file's index; scheduled clips judge the enclosing recording's file.
    use_own_file: bool,
    now_ts: i64,
    index_cache: &mut std::collections::HashMap<
        std::path::PathBuf,
        Vec<crate::recording::recording_sidecar::TimeIndexEntry>,
    >,
    candidate_actions: &mut std::collections::HashMap<String, ClipPlan>,
) {
    const GAP: i64 = crate::recording::recording_sidecar::MAX_INDEX_SAMPLE_GAP_SECS;

    for task in candidates {
        let Some(clip) = &task.clip else {
            continue;
        };
        let enclosing =
            enclosing_candidates.iter().find(|t| t.uuid != task.uuid && same_task_media(t, task) && t.clip.is_none());

        if let Some(a) = enclosing {
            if a.state == RecordingTaskState::Failed {
                candidate_actions
                    .insert(task.uuid.clone(), ClipPlan::Fail(RECORDING_SOURCE_RECORDING_FAILED.to_string()));
                continue;
            }
        }

        let file_path = if use_own_file { Some(&task.file_path) } else { enclosing.map(|a| &a.file_path) };

        if let Some(path) = file_path {
            let index = match index_cache.entry(path.clone()) {
                std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
                std::collections::hash_map::Entry::Vacant(v) => {
                    let idx_path = crate::recording::recording_sidecar::time_index_path(path);
                    let idx = crate::recording::recording_sidecar::read_time_index(&idx_path).await.unwrap_or_default();
                    v.insert(idx)
                }
            };
            let index_passed = index.iter().any(|e| e.unix_secs + GAP >= clip.end_ts);
            if index_passed {
                if crate::recording::recording_sidecar::has_clip_time_gap(index, clip.start_ts, clip.end_ts) {
                    candidate_actions.insert(task.uuid.clone(), ClipPlan::Fail(RECORDING_SOURCE_GAP.to_string()));
                    continue;
                }
                if let Ok((s, e)) =
                    crate::recording::recording_worker::compute_clip_offsets(path, clip.start_ts, clip.end_ts).await
                {
                    let header_bytes = crate::recording::recording_worker::read_clip_header(path).await.ok();
                    candidate_actions
                        .insert(task.uuid.clone(), ClipPlan::Complete { start_offset: s, end_offset: e, header_bytes });
                }
            } else if enclosing.is_some_and(|a| a.state.is_terminal()) {
                candidate_actions.insert(task.uuid.clone(), ClipPlan::Fail(RECORDING_SOURCE_GAP.to_string()));
            } else if use_own_file && enclosing.is_none() && now_ts >= clip.end_ts {
                candidate_actions
                    .insert(task.uuid.clone(), ClipPlan::Fail(RECORDING_SOURCE_RECORDING_FAILED.to_string()));
            }
        } else if now_ts >= clip.end_ts {
            candidate_actions.insert(task.uuid.clone(), ClipPlan::Fail(RECORDING_SOURCE_RECORDING_FAILED.to_string()));
        }
    }
}

#[cfg(test)]
mod stem_group_tests {
    use super::stem_group;
    use shared::{model::RecordingConfigDto, utils::EPISODE_PATTERN};
    use tuliprox_core::model::RecordingConfig;

    fn config(pattern: &str) -> RecordingConfig {
        RecordingConfig::from(&RecordingConfigDto {
            directory: Some("/tmp".to_string()),
            episode_pattern: Some(pattern.to_string()),
            ..Default::default()
        })
    }

    #[test]
    fn default_pattern_cuts_every_episode_shape() {
        let cfg = config(EPISODE_PATTERN);
        for stem in ["Show S01E02", "Show 1x02", "Show Season 1 Episode 2", "Show Episode 2", "Show.S01E02.1080p"] {
            assert_eq!(stem_group(&cfg, stem), "Show", "stem {stem:?}");
        }
    }

    #[test]
    fn named_episode_group_still_cuts_before_that_group() {
        let cfg = config(r"Part (?P<episode>\d+)");
        assert_eq!(stem_group(&cfg, "Show Part 3"), "Show Part");
    }

    #[test]
    fn stem_without_episode_marker_is_kept() {
        let cfg = config(EPISODE_PATTERN);
        assert_eq!(stem_group(&cfg, "Documentary"), "Documentary");
    }
}
