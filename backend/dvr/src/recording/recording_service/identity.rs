use crate::{
    recording::recording_queue::{PersistedRecordingQueue, PersistedRecordingTask, RecordingTask},
    recording_quota,
};
use shared::model::recording::RecordingMetadata;

/// What makes two recording requests "the same thing".
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
pub enum RecordingIdentity<S = String> {
    /// Live, single channel: `input_name` + provider stream id + PADDED window
    LiveChannel { input_name: S, provider_stream_id: S, padded_start: Option<i64>, padded_end: Option<i64> },
    /// Live, group channel (source vid is a failover group): `target_id` + group vid + padded window
    LiveGroup { target_id: S, group_vid: S, padded_start: Option<i64>, padded_end: Option<i64> },
    /// VOD and series episodes: `input_name` + cluster + provider stream id, no time
    Media { input_name: S, cluster: shared::model::XtreamCluster, provider_stream_id: S },
    /// Fallback url
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
    let padded_window = {
        if let (Some(start), Some(end)) = (meta.program_start, meta.program_end) {
            let pre = i64::try_from(meta.pre_roll_secs).unwrap_or(i64::MAX);
            let post = i64::try_from(meta.post_roll_secs).unwrap_or(i64::MAX);
            (Some(start.saturating_sub(pre)), Some(end.saturating_add(post)))
        } else {
            (meta.scheduled_start, meta.scheduled_end)
        }
    };

    match meta.source.cluster {
        shared::model::XtreamCluster::Video | shared::model::XtreamCluster::Series => {
            if let Some(ref provider_id) = meta.source.provider_id {
                RecordingIdentity::Media {
                    input_name: &meta.source.input_name,
                    cluster: meta.source.cluster,
                    provider_stream_id: provider_id.as_str(),
                }
            } else {
                RecordingIdentity::Url { url, scheduled_start: meta.scheduled_start, scheduled_end: meta.scheduled_end }
            }
        }
        shared::model::XtreamCluster::Live => {
            if meta.source.is_group {
                RecordingIdentity::LiveGroup {
                    target_id: &meta.source.target_id,
                    group_vid: &meta.source.virtual_id,
                    padded_start: padded_window.0,
                    padded_end: padded_window.1,
                }
            } else if let Some(ref provider_id) = meta.source.provider_id {
                RecordingIdentity::LiveChannel {
                    input_name: &meta.source.input_name,
                    provider_stream_id: provider_id.as_str(),
                    padded_start: padded_window.0,
                    padded_end: padded_window.1,
                }
            } else {
                RecordingIdentity::Url { url, scheduled_start: padded_window.0, scheduled_end: padded_window.1 }
            }
        }
    }
}

/// Whether two requests share the same live stream source (F11 clip source identity).
///
/// For Live recordings:
/// - Group channels match on `target_id` and `virtual_id` (the group VID).
/// - Provider channels match on `input_name` and `provider_id`.
/// - Fallback matches on the stream URL.
pub fn same_live_source(
    left_meta: &RecordingMetadata,
    left_url: &str,
    right_meta: &RecordingMetadata,
    right_url: &str,
) -> bool {
    if left_meta.source.cluster != shared::model::XtreamCluster::Live
        || right_meta.source.cluster != shared::model::XtreamCluster::Live
    {
        return false;
    }
    if left_meta.source.is_group && right_meta.source.is_group {
        left_meta.source.target_id == right_meta.source.target_id
            && left_meta.source.virtual_id == right_meta.source.virtual_id
    } else if !left_meta.source.is_group && !right_meta.source.is_group {
        match (&left_meta.source.provider_id, &right_meta.source.provider_id) {
            (Some(left_pid), Some(right_pid)) => {
                left_meta.source.input_name == right_meta.source.input_name && left_pid == right_pid
            }
            (None, None) => left_url == right_url,
            _ => false,
        }
    } else {
        false
    }
}

fn persisted_recording_identity(task: &PersistedRecordingTask) -> RecordingIdentity<&str> {
    recording_identity(&task.recording, &task.url)
}

pub(super) fn candidate_has_duplicate_recording(candidate: &PersistedRecordingQueue, task: &RecordingTask) -> bool {
    let meta = &task.recording;
    let pool = recording_quota::quota_pool_for_task(task);

    // Rule occurrence duplicate check: the same occurrence is not scheduled twice by its rule.
    if let (Some(rule_id), Some(occurrence_key)) =
        (meta.provenance.rule_id.as_deref(), meta.provenance.occurrence_key.as_deref())
    {
        return candidate
            .queue
            .iter()
            .chain(candidate.scheduled.iter())
            .chain(candidate.active.iter())
            .chain(candidate.finished.iter())
            .any(|existing| {
                recording_quota::quota_pool_for_task(existing) == pool
                    && existing.recording.provenance.rule_id.as_deref() == Some(rule_id)
                    && existing.recording.provenance.occurrence_key.as_deref() == Some(occurrence_key)
            });
    }

    let identity = recording_identity(meta, task.url.as_str());
    candidate
        .queue
        .iter()
        .chain(candidate.scheduled.iter())
        .chain(candidate.active.iter())
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
