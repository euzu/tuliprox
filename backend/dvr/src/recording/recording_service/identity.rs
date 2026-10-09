use crate::{
    recording::recording_queue::{PersistedRecordingQueue, PersistedRecordingTask, RecordingTask},
    recording_quota,
};
use shared::model::recording::RecordingMetadata;

/// What makes two recording requests "the same thing".
///
/// The previous key was `(url, start_at, duration_secs)` OR `file_path`,
/// and neither half worked:
/// - `file_path` is disambiguated with a `_N` suffix by
///   `reserve_recording_relative_path`, so it *never* matches an
///   existing task and the whole disjunct was dead.
/// - `start_at` is `now.max(scheduled_start)`, so for a
///   currently-airing programme every request inside the window
///   produces a different value and the same programme could be
///   booked over and over.
///
/// The identity is now derived from what the user actually asked for.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
pub enum RecordingIdentity<S = String> {
    /// Materialization of one rule occurrence. Two tasks with the same
    /// `(rule_id, occurrence_key)` are the same recording by
    /// definition, whatever their window looks like.
    Occurrence { rule_id: S, occurrence_key: S },
    /// A concrete programme on a concrete source. Deliberately free of any
    /// owner: two users asking for the same programme are asking for the same
    /// file, and it is recorded once and linked twice.
    Programme { target_id: S, virtual_id: S, program_start: i64, program_end: i64 },
    /// No programme metadata at all: every VOD and series transfer, and a
    /// Live capture whose programme window is unknown. Falls back to the
    /// resolved URL plus the *scheduled* (padded) window, which — unlike
    /// `start_at` — is stable across requests inside a currently-airing
    /// window.
    ///
    /// Owner-free for the same reason `Programme` is: the same URL over the
    /// same window is the same bytes, whoever asked for them.
    Url { url: S, scheduled_start: Option<i64>, scheduled_end: Option<i64> },
}

/// A stable, field-named key for the media a request refers to.
///
/// Two tasks with the same key are the same recording, so they share one
/// physical file. The key is persisted, so it has to be stable across builds:
/// that is why it is serialised rather than formatted with `Debug`.
pub fn recording_identity_key(meta: &RecordingMetadata, url: &str) -> String {
    serde_json::to_string(&recording_identity(meta, url)).unwrap_or_else(|_| format!("url:{url}"))
}

pub(crate) fn recording_identity<'a>(meta: &'a RecordingMetadata, url: &'a str) -> RecordingIdentity<&'a str> {
    if let (Some(rule_id), Some(occurrence_key)) =
        (meta.provenance.rule_id.as_deref(), meta.provenance.occurrence_key.as_deref())
    {
        return RecordingIdentity::Occurrence { rule_id, occurrence_key };
    }
    if let (Some(program_start), Some(program_end)) = (meta.program_start, meta.program_end) {
        return RecordingIdentity::Programme {
            target_id: &meta.source.target_id,
            virtual_id: &meta.source.virtual_id,
            program_start,
            program_end,
        };
    }
    RecordingIdentity::Url { url, scheduled_start: meta.scheduled_start, scheduled_end: meta.scheduled_end }
}

fn persisted_recording_identity(task: &PersistedRecordingTask) -> RecordingIdentity<&str> {
    recording_identity(&task.recording, &task.url)
}

pub(super) fn candidate_has_duplicate_recording(candidate: &PersistedRecordingQueue, task: &RecordingTask) -> bool {
    let meta = &task.recording;
    let identity = recording_identity(meta, task.url.as_str());
    let pool = recording_quota::quota_pool_for_task(task);
    // Only the *same* principal asking twice is a duplicate. A different
    // principal asking for the same media gets their own library entry, which
    // attaches to the one physical file rather than producing a second.
    let pending_match = candidate
        .queue
        .iter()
        .chain(candidate.scheduled.iter())
        .chain(candidate.active.iter())
        .filter(|existing| recording_quota::quota_pool_for_task(*existing) == pool)
        .map(persisted_recording_identity)
        .any(|existing| existing == identity);
    if pending_match {
        return true;
    }
    // Terminal tasks do not block a fresh request: after a failed or
    // cancelled attempt the user must be able to try again, and after a
    // successful one they may legitimately want a second copy. The one
    // exception is a rule occurrence — re-materializing an occurrence
    // that already ran would duplicate it on every scheduler tick.
    if !matches!(identity, RecordingIdentity::Occurrence { .. }) {
        return false;
    }
    candidate
        .finished
        .iter()
        .filter(|existing| recording_quota::quota_pool_for_task(*existing) == pool)
        .map(persisted_recording_identity)
        .any(|existing| existing == identity)
}

/// Where a recording lives in the candidate snapshot. The first scan
/// produces one of these so the second access (mut borrow for writes)
/// is O(1) instead of repeating the linear search.
#[derive(Debug, Clone, Copy)]
pub(super) enum RecordingLocation {
    Scheduled(usize),
    Queue(usize),
    Active(usize),
    Finished(usize),
}

/// Single linear scan that locates a recording anywhere in the
/// candidate snapshot. The returned `RecordingLocation` lets the
/// caller re-acquire the same task for a mutable borrow without a
/// second search.
pub(super) fn locate_recording(candidate: &PersistedRecordingQueue, uuid: &str) -> Option<RecordingLocation> {
    let matches_uuid = |task: &PersistedRecordingTask| task.uuid == uuid;
    if let Some(i) = candidate.scheduled.iter().position(matches_uuid) {
        return Some(RecordingLocation::Scheduled(i));
    }
    if let Some(i) = candidate.queue.iter().position(matches_uuid) {
        return Some(RecordingLocation::Queue(i));
    }
    if let Some(i) = candidate.active.iter().position(matches_uuid) {
        return Some(RecordingLocation::Active(i));
    }
    if let Some(i) = candidate.finished.iter().position(matches_uuid) {
        return Some(RecordingLocation::Finished(i));
    }
    None
}

/// Resolve a recording to a mutable borrow using a remembered
/// location. The location must have come from the same candidate;
/// callers obtain it via [`locate_recording`].
pub(super) fn recording_mut_at(
    candidate: &mut PersistedRecordingQueue,
    location: RecordingLocation,
) -> Option<&mut PersistedRecordingTask> {
    match location {
        RecordingLocation::Scheduled(i) => candidate.scheduled.get_mut(i),
        RecordingLocation::Queue(i) => candidate.queue.get_mut(i),
        RecordingLocation::Active(i) => candidate.active.get_mut(i),
        // Must be the located index, not element 0: returning the first
        // finished task would silently edit an unrelated recording.
        RecordingLocation::Finished(i) => candidate.finished.get_mut(i),
    }
}
