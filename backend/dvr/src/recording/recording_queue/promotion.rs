use super::{
    mutate_optional, PersistedRecordingQueue, PersistedRecordingTask, RecordingQueue, RecordingTaskState,
    RECORDING_WINDOW_EXPIRED_ERR,
};
use crate::recording::recording_path;
use chrono::Utc;
use log::error;
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
fn stem_group(recording_cfg: &RecordingConfig, file_stem: &str) -> String {
    let mut stem = file_stem;
    if let Some(re) = &recording_cfg.episode_pattern {
        if let Some(captures) = re.captures(stem) {
            if let Some(episode) = captures.name("episode") {
                if !episode.as_str().is_empty() {
                    stem = &stem[..episode.start()];
                }
            }
        }
    }
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

impl RecordingQueue {
    pub async fn promote_due_scheduled(&self, now_ts: i64) -> usize {
        let due = self.scheduled.read().await.iter().any(|task| {
            task.recording.scheduled_start.is_some_and(|start| start <= now_ts)
                || (task.kind == RecordingKind::Live && task.recording.scheduled_end.is_some_and(|end| now_ts >= end))
        });
        if !due {
            return 0;
        }
        let result = mutate_optional(self, |candidate| {
            let mut due_tasks = Vec::new();
            let mut missed_recordings = Vec::new();
            candidate.scheduled.retain(|task| {
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

            if due_tasks.is_empty() && missed_recordings.is_empty() {
                return Ok(None);
            }

            let due_count = due_tasks.len();
            let missed_count = missed_recordings.len();
            candidate.finished.extend(missed_recordings);
            candidate.queue.splice(0..0, due_tasks);
            Ok(Some(if due_count == 0 { missed_count } else { due_count }))
        })
        .await;

        match result {
            Ok(Some(promoted)) => promoted,
            Ok(None) => 0,
            Err(err) => {
                error!("Promoting due scheduled recordings failed: {err}");
                0
            }
        }
    }

    pub async fn promote_due_scheduled_now(&self) -> usize { self.promote_due_scheduled(Utc::now().timestamp()).await }
}
