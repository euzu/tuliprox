use super::{
    promotion::recording_grouping, PersistedRecordingTask, RecordingTaskState, MAX_COLLISION_PROBES,
    RECORDING_TASK_ID_COUNTER,
};
use crate::recording::{recording_path, recording_transition};
use serde::{Deserialize, Serialize};
use shared::{
    model::{Claims, QueueRevision, RecordingKind, RecordingMetadata, RecordingTaskDto, TaskPriorityDto, UserId},
    utils::{sanitize_filename_chars, FILENAME_TRIM_PATTERNS},
};
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc},
    time::{SystemTime, UNIX_EPOCH},
};
use tuliprox_core::model::RecordingConfig;

/// A recording task in memory. Internal shape — never serialized to a
/// client. Use [`RecordingTask::to_owner_view`] for the public projection.
#[derive(Clone, Debug)]
pub struct RecordingTask {
    /// uuid of the task for identification.
    pub uuid: String,
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
}

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
