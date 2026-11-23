use super::{
    promotion::recording_grouping, PersistedRecordingTask, RecordingTaskState, MAX_COLLISION_PROBES,
    RECORDING_TASK_ID_COUNTER,
};
use crate::recording::{recording_path, recording_transition};
use log::warn;
use serde::{Deserialize, Serialize};
use shared::{
    model::{
        xtream_const::RECORDING_VOD_ID_BASE, Claims, QueueRevision, RecordingKind, RecordingMetadata, RecordingTaskDto,
        TaskPriorityDto, UserId,
    },
    utils::{sanitize_filename_chars, FILENAME_TRIM_PATTERNS},
};
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc},
    time::{SystemTime, UNIX_EPOCH},
};
use tuliprox_core::model::RecordingConfig;

/// One completed recording as the Xtream VOD listing needs it.
///
/// Copies only the fields a listing publishes, so a catalogue request does not
/// clone whole tasks including their source URLs and file paths.
pub struct RecordingVodEntry {
    pub uuid: String,
    pub vod_id: u32,
    pub filename: String,
    pub title: String,
    /// Length of the recorded window in seconds, when the recording knows it.
    pub duration_secs: Option<u64>,
    /// Unix timestamp the recording completed, when it did.
    pub added: Option<i64>,
    /// Owner, visibility and source: everything the visibility rule judges.
    pub recording: RecordingMetadata,
}

/// A recording task in memory. Internal shape — never serialized to a
/// client. Use [`RecordingTask::to_owner_view`] for the public projection.
#[derive(Clone, Debug)]
pub struct RecordingTask {
    /// uuid of the task for identification.
    pub uuid: String,
    /// Stream id this recording answers to in the Xtream VOD namespace of its owner.
    /// Assigned once by the queue and never reused.
    pub vod_id: u32,
    /// Server-resolved media kind. Decides the execution strategy.
    pub kind: RecordingKind,
    /// `file_dir` is the directory where the file should be placed.
    pub file_dir: PathBuf,
    /// `file_path` is the complete path including the filename.
    pub file_path: PathBuf,
    /// filename is the filename.
    pub filename: String,
    /// Server-resolved source url.
    pub url: reqwest::Url,
    /// finished is true when the task reached a terminal state.
    pub finished: bool,
    /// Bytes transferred so far.
    pub size: u64,
    /// Total size in bytes (from the Content-Length header), when known.
    pub total_size: Option<u64>,
    /// Paused state. Only VOD/Series can be paused.
    pub paused: bool,
    /// Optional error if something goes wrong while running the task.
    pub error: Option<String>,
    /// Task state.
    pub state: RecordingTaskState,
    /// The input source name used to acquire a provider connection.
    pub input_name: Option<Arc<str>>,
    /// Priority for provider connection preemption (lower = higher priority).
    pub priority: i8,
    /// Consecutive retry attempts for transient failures.
    pub retry_attempts: u8,
    /// Unix timestamp of the next retry attempt while waiting.
    pub next_retry_at: Option<i64>,
    /// Recording metadata. Every task carries it; the programme window is
    /// populated for `RecordingKind::Live` only.
    pub recording: RecordingMetadata,
    /// When this task is a clip, its enclosing time window and byte offsets.
    pub clip: Option<tuliprox_repository::recording_repository::RecordingClip>,
    /// Pre-resolved or enclosing media identity.
    pub media_identity: Option<String>,
    /// Marked when the enclosing recording was cancelled by its owner while attached clips still need data.
    pub cancelled_by_owner: bool,
}

/// Mark a recording task as a completed clip with the given byte offsets and size.
pub fn complete_clip(
    task: &mut PersistedRecordingTask,
    start_offset: u64,
    end_offset: u64,
    header_bytes: Option<Vec<u8>>,
    now_ts: i64,
) {
    let (start_ts, end_ts) = task.clip.as_ref().map_or((0, 0), |c| (c.start_ts, c.end_ts));
    task.clip = Some(tuliprox_repository::recording_repository::RecordingClip {
        start_ts,
        end_ts,
        start_offset: Some(start_offset),
        end_offset: Some(end_offset),
        header_bytes,
    });
    let clip_size = (end_offset.saturating_sub(start_offset)).saturating_add(crate::recording::CLIP_HEADER_LEN as u64);
    task.size = clip_size;
    task.total_size = Some(clip_size);
    task.finished = true;
    task.paused = false;
    task.error = None;
    task.next_retry_at = None;
    task.retry_attempts = 0;
    task.state = RecordingTaskState::Completed;
    task.recording.measured_bytes = clip_size;
    task.recording.reserved_bytes = 0;
    task.recording.completed_at = Some(now_ts);
}

/// Highest recording VOD id players can address.
///
/// A stream id travels as a signed 32-bit integer, so `i32::MAX` is the last usable
/// value; an id past it is not published at all.
const RECORDING_VOD_ID_LIMIT: u32 = 0x7fff_ffff;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PersistedRecordingQueue {
    pub queue: Vec<PersistedRecordingTask>,
    pub scheduled: Vec<PersistedRecordingTask>,
    pub active: Vec<PersistedRecordingTask>,
    pub finished: Vec<PersistedRecordingTask>,
    /// Monotonic revision. Increments once per committed queue mutation.
    /// The in-memory `RecordingQueue` mirrors this counter via an `AtomicU64`.
    #[serde(default)]
    pub revision: QueueRevision,
    /// Next recording VOD id to hand out. Persisted with the metadata record so a
    /// restart neither reuses nor renumbers ids.
    #[serde(default)]
    pub next_vod_id: u32,
}

impl PersistedRecordingQueue {
    /// Give every task that has no id yet the next recording VOD id.
    ///
    /// Runs inside a queue mutation, so the insert that produced a task, the scan
    /// for unassigned tasks and the assignment are one critical section. Ids are
    /// never taken back: the counter only moves forward.
    pub(super) fn assign_missing_vod_ids(&mut self) -> (u32, bool) {
        // The counter starts above every id already handed out, in any partition, so
        // a new id never equals an existing one regardless of the partition order.
        let highest_assigned = self.all_tasks().map(|task| task.vod_id).max().unwrap_or(0);
        let mut next = self.next_vod_id.max(RECORDING_VOD_ID_BASE).max(highest_assigned.saturating_add(1));
        let mut assigned = false;
        for task in self
            .queue
            .iter_mut()
            .chain(self.scheduled.iter_mut())
            .chain(self.active.iter_mut())
            .chain(self.finished.iter_mut())
            .filter(|task| task.vod_id == 0)
        {
            // A task that cannot get an id keeps 0 and stays unlisted; the counter
            // never saturates, so ids are never reused.
            if next > RECORDING_VOD_ID_LIMIT {
                warn!("Recording VOD id space is exhausted at {}; further recordings stay unlisted", i32::MAX);
                break;
            }
            task.vod_id = next;
            next = next.saturating_add(1);
            assigned = true;
        }
        self.next_vod_id = next;
        (next, assigned)
    }

    fn all_tasks(&self) -> impl Iterator<Item = &PersistedRecordingTask> {
        self.queue.iter().chain(self.scheduled.iter()).chain(self.active.iter()).chain(self.finished.iter())
    }
}

fn generate_recording_task_id() -> String {
    let now_nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| duration.as_nanos());
    let counter = RECORDING_TASK_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{now_nanos:032x}{counter:016x}")
}

impl RecordingTask {
    /// Build a task for a server-resolved source. Live tasks start in
    /// `Scheduled`; VOD/Series tasks start in `Queued`.
    pub fn new(
        kind: RecordingKind,
        req_url: &str,
        req_filename: &str,
        recording_cfg: &RecordingConfig,
        input_name: Option<Arc<str>>,
        priority: i8,
        recording: RecordingMetadata,
    ) -> Option<Self> {
        let url = reqwest::Url::parse(req_url).ok()?;
        let tmp_filename = sanitize_filename_chars(req_filename, true).replace("__", "_").replace("_-_", "-");
        let filename_path = Path::new(&tmp_filename);
        let file_stem =
            filename_path.file_stem().and_then(OsStr::to_str).unwrap_or("").trim_matches(FILENAME_TRIM_PATTERNS);
        let file_ext = filename_path.extension().and_then(OsStr::to_str).unwrap_or("");

        let base_filename = if file_ext.is_empty() { file_stem.to_string() } else { format!("{file_stem}.{file_ext}") };
        let root = Path::new(recording_cfg.directory.as_str());
        let grouping = recording_grouping(kind, &recording, recording_cfg, file_stem);
        let mut relative = recording_path::build_relative_path(
            kind,
            recording_cfg.organize_into_directories,
            &grouping,
            &base_filename,
        )
        .ok()?;
        // A file already on disk under this name belongs to an earlier
        // recording; the repository reservation resolves logical collisions,
        // this only avoids clobbering something already written. Bounded: each
        // probe is a filesystem call on the caller's thread.
        let base = relative.clone();
        let mut index = 0;
        while root.join(&relative).is_file() {
            index += 1;
            if index > MAX_COLLISION_PROBES {
                return None;
            }
            relative = recording_path::with_collision_suffix(&base, index);
        }
        let file_path = recording_path::resolve_under_root(root, &relative).ok()?;
        let file_dir = file_path.parent().map(Path::to_path_buf)?;
        let filename = relative.file_name().and_then(|name| name.to_str())?.to_string();

        file_path.to_str()?;
        let mut recording = recording;
        recording.relative_path = Some(relative.to_string_lossy().into_owned());

        Some(Self {
            uuid: generate_recording_task_id(),
            vod_id: 0,
            kind,
            file_dir,
            file_path,
            filename,
            url,
            finished: false,
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state: if kind.is_scheduled() { RecordingTaskState::Scheduled } else { RecordingTaskState::Queued },
            input_name,
            priority,
            retry_attempts: 0,
            next_retry_at: None,
            recording,
            clip: None,
            media_identity: None,
            cancelled_by_owner: false,
        })
    }

    /// Padded start of a scheduled Live window.
    pub fn scheduled_start(&self) -> Option<i64> { self.recording.scheduled_start }

    /// Padded end of a scheduled Live window.
    pub fn scheduled_end(&self) -> Option<i64> { self.recording.scheduled_end }

    /// Length of the padded Live window, when both bounds are known.
    pub fn scheduled_duration_secs(&self) -> Option<u64> {
        let (start, end) = self.recording.scheduled_start.zip(self.recording.scheduled_end)?;
        u64::try_from(end.saturating_sub(start)).ok()
    }

    pub fn owner_id(&self) -> &UserId { self.recording.owner_id() }

    /// Public projection for one viewer. Returns `None` when the viewer is
    /// neither the owner nor entitled to see a shared task; the owner id is
    /// disclosed only to the owner itself.
    pub fn to_owner_view(&self, claims: &Claims, shared_visible: bool) -> Option<RecordingTaskDto> {
        let is_owner = claims.subject_id.as_ref().is_some_and(|subject| subject == self.owner_id());
        if self.cancelled_by_owner && is_owner {
            return None;
        }
        if !is_owner && !shared_visible {
            return None;
        }
        Some(self.to_view(is_owner))
    }

    /// Projection without a viewer check. Callers that already authorized the
    /// viewer pass `is_owner` explicitly.
    pub fn to_view(&self, is_owner: bool) -> RecordingTaskDto {
        let meta = &self.recording;
        RecordingTaskDto {
            id: self.uuid.clone(),
            title: meta.program_title.clone().unwrap_or_else(|| self.filename.clone()),
            kind: self.kind,
            priority: match self.priority.cmp(&0) {
                std::cmp::Ordering::Less => TaskPriorityDto::High,
                std::cmp::Ordering::Equal => TaskPriorityDto::Normal,
                std::cmp::Ordering::Greater => TaskPriorityDto::Background,
            },
            status: self.state.into(),
            retry_attempts: self.retry_attempts,
            transferred_bytes: self.size,
            total_bytes: self.total_size,
            next_retry_at: self.next_retry_at,
            error: self.error.clone(),
            restart_from_beginning_required: self.state == RecordingTaskState::Failed
                && self.error.as_deref() == Some(super::super::recording_transfer::RANGE_UNSUPPORTED_ERROR),
            owner_id: is_owner.then(|| meta.owner_id().clone()),
            visibility: meta.visibility,
            channel_id: meta.channel_id.clone(),
            channel_name: meta.channel_name.clone(),
            program_title: meta.program_title.clone(),
            program_start: meta.program_start,
            program_end: meta.program_end,
            scheduled_start: meta.scheduled_start,
            scheduled_end: meta.scheduled_end,
            pre_roll_secs: meta.pre_roll_secs,
            post_roll_secs: meta.post_roll_secs,
            completed_at: meta.completed_at,
            filename: meta.filename().map(str::to_string),
            epg: meta.epg.clone(),
            rule_id: meta.provenance.rule_id.clone(),
            occurrence_key: meta.provenance.occurrence_key.clone(),
            allowed_actions: recording_transition::allowed_actions(self.kind, self.state),
        }
    }
}
