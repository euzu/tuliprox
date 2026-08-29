//! The persisted recording queue.
//!
//! One queue holds every recording task — scheduled Live captures and
//! immediate VOD/Series transfers alike. Three deliberately separate shapes
//! exist:
//!
//! * [`RecordingTask`] — the internal, mutable, in-memory task. Carries the
//!   resolved source URL, owner, path state and transfer progress.
//! * [`PersistedRecordingTask`] — the on-disk shape. Never serialized to a
//!   client.
//! * [`shared::model::RecordingTaskDto`] — the owner-safe public projection,
//!   produced only through [`RecordingTask::to_owner_view`].

use chrono::Utc;
use log::error;
use serde::{Deserialize, Serialize};
use shared::{
    model::{
        Claims, QueueRevision, RecordingKind, RecordingMetadata, RecordingTaskDto, TaskPriorityDto, TransferStatusDto,
        UserId,
    },
    utils::{deunicode_string, CONSTANTS, FILENAME_TRIM_PATTERNS},
};
use std::{
    collections::VecDeque,
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    fs,
    sync::{Mutex, Notify, RwLock},
};
use tuliprox_core::{
    model::RecordingConfig,
    utils::{file_exists_async, write_json_atomic},
};

const RECORDING_WINDOW_EXPIRED_ERR: &str = "Recording window already expired";
/// Schema of the canonical recording state file. There is no upgrade path:
/// an unknown version fails the load instead of being reinterpreted.
const RECORDING_QUEUE_SCHEMA_VERSION: u16 = 1;
static RECORDING_TASK_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Reason a persisted entry cannot be converted back to its in-memory form
/// during the commit step. Surfaced to the caller so a corrupt persisted file
/// fails closed instead of silently dropping entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistedError {
    /// The persisted URL could not be parsed.
    InvalidUrl(String),
    /// The state file carries a schema version this build cannot read.
    UnsupportedSchema(u16),
}

impl std::fmt::Display for PersistedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl(s) => write!(f, "persisted url is invalid: {s}"),
            Self::UnsupportedSchema(version) => {
                write!(f, "persisted recording queue schema version {version} is not supported")
            }
        }
    }
}

impl std::error::Error for PersistedError {}

/// Typed error returned from the queue mutation boundary. Every
/// `mutate` closure that fails must return a known variant; the
/// `Other` variant is an escape hatch for dynamically-formatted
/// messages that have no stable wire code.
#[derive(Debug)]
pub enum QueueMutationError {
    UnknownRecording,
    StateNotEditable,
    Forbidden,
    InvalidInterval,
    InvalidQuotaPool,
    InvalidPath,
    PaddingLimitExceeded,
    QuotaExceeded,
    Duplicate,
    NotInTerminalState,
    DiskFull,
    MutationSkipped,
    /// Escape hatch for dynamically-formatted validation messages
    /// that have no stable wire code. Prefer the typed variants.
    Other(String),
    Io(std::io::Error),
}

impl QueueMutationError {
    /// Escape-hatch constructor for messages that cannot be expressed
    /// as a typed variant. Prefer the typed `Self::X` constructors.
    pub fn new(message: impl Into<String>) -> Self { Self::Other(message.into()) }

    pub fn from_io(err: std::io::Error) -> Self { Self::Io(err) }

    /// Stable display message for logging and HTTP error rendering.
    pub fn message(&self) -> &'static str {
        match self {
            Self::UnknownRecording => "recording unknown",
            Self::StateNotEditable => "recording state not editable",
            Self::Forbidden => "recording forbidden",
            Self::InvalidInterval => "recording invalid interval",
            Self::InvalidQuotaPool => "recording invalid quota pool",
            Self::InvalidPath => "recording invalid path",
            Self::PaddingLimitExceeded => "recording_padding_limit_exceeded",
            Self::QuotaExceeded => "recording quota exceeded",
            Self::Duplicate => "recording duplicate",
            Self::NotInTerminalState => "recording not in terminal state",
            Self::DiskFull => "disk full",
            Self::MutationSkipped => "mutation unexpectedly skipped",
            Self::Other(_) => "queue mutation failed",
            Self::Io(_) => "queue mutation persistence failed",
        }
    }

    pub fn source_io(&self) -> Option<&std::io::Error> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl std::fmt::Display for QueueMutationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Other(s) => f.write_str(s),
            Self::Io(e) => std::fmt::Display::fmt(e, f),
            other => f.write_str(other.message()),
        }
    }
}

impl std::error::Error for QueueMutationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err as &(dyn std::error::Error + 'static)),
            _ => None,
        }
    }
}

/// Lock ordering for the queue mutation boundary:
///
/// 1. `mutation_guard` (`Mutex`) — outermost for persisted mutations and ordered control publication.
/// 2. `queue` (`Mutex`) — always taken before persisted state locks.
/// 3. `scheduled` / `active` / `finished` (`RwLock`, write) — taken after the queue.
/// 4. `control_signal` (`RwLock`) and `control_notify` (`Notify`) — taken after the
///    queue locks; they signal runtime state, not persisted state.
/// 5. `revision` (`AtomicU64`) — no lock; swapped atomically with the
///    commit step.
///
/// The queue mutation boundary (`mutate`) holds the queue locks only while
/// building the candidate snapshot. It does **not** hold any queue lock
/// while running the user closure or while persisting. Persisting holds
/// the `state_file` (filesystem only).
///
/// Rule repository mutations are acquired strictly after the queue boundary
/// has committed; never inside it.
///
/// Apply a single transactional queue mutation. The closure receives an
/// owned `PersistedRecordingQueue` candidate cloned from the current state
/// and returns either a value or a [`QueueMutationError`]. On success the
/// candidate is persisted atomically, then swapped into the in-memory state,
/// then the `QueueRevision` is incremented. On any failure — closure error
/// or persist error — the in-memory state, the persisted file, and the
/// revision are all unchanged.
pub async fn mutate<F, R>(this: &RecordingQueue, op: F) -> Result<R, QueueMutationError>
where
    F: FnOnce(&mut PersistedRecordingQueue) -> Result<R, QueueMutationError>,
{
    match mutate_optional(this, |candidate| op(candidate).map(Some)).await? {
        Some(result) => Ok(result),
        None => Err(QueueMutationError::MutationSkipped),
    }
}

pub async fn mutate_optional<F, R>(this: &RecordingQueue, op: F) -> Result<Option<R>, QueueMutationError>
where
    F: FnOnce(&mut PersistedRecordingQueue) -> Result<Option<R>, QueueMutationError>,
{
    let _mutation = this.mutation_guard.lock().await;
    mutate_optional_locked(this, op).await
}

async fn mutate_optional_locked<F, R>(this: &RecordingQueue, op: F) -> Result<Option<R>, QueueMutationError>
where
    F: FnOnce(&mut PersistedRecordingQueue) -> Result<Option<R>, QueueMutationError>,
{
    let next_revision = this.revision.load(Ordering::SeqCst).saturating_add(1);

    // 2. Build candidate under the queue locks.
    let mut candidate = this.snapshot_current(QueueRevision(next_revision)).await;

    // 3. Apply the mutation to a candidate snapshot. The closure can do
    //    arbitrary validation and refer back to the candidate's prior
    //    state.
    let Some(result) = op(&mut candidate)? else {
        return Ok(None);
    };

    let content = this
        .state_file
        .as_ref()
        .map(|_| serde_json::to_vec_pretty(&candidate))
        .transpose()
        .map_err(|err| QueueMutationError::from_io(std::io::Error::other(err)))?;

    let PersistedRecordingQueue {
        schema_version: _,
        queue: candidate_queue,
        scheduled: candidate_scheduled,
        active: candidate_active,
        finished: candidate_finished,
        revision: _,
    } = candidate;

    // 4. Validate every persisted entry into its in-memory form before
    // swapping. A single corrupt entry must abort the commit so the
    // persisted file and the in-memory state stay identical. Without this,
    // a bad URL would be silently dropped from memory while the file still
    // listed it, desyncing the two.
    let mut queue: VecDeque<RecordingTask> = VecDeque::with_capacity(candidate_queue.len());
    for p in candidate_queue {
        queue.push_back(
            RecordingQueue::from_persisted(p)
                .map_err(|e| QueueMutationError::new(format!("persisted queue entry invalid: {e}")))?,
        );
    }
    let mut scheduled: Vec<RecordingTask> = Vec::with_capacity(candidate_scheduled.len());
    for p in candidate_scheduled {
        scheduled.push(
            RecordingQueue::from_persisted(p)
                .map_err(|e| QueueMutationError::new(format!("persisted scheduled entry invalid: {e}")))?,
        );
    }
    let active = match candidate_active {
        Some(p) => Some(
            RecordingQueue::from_persisted(p)
                .map_err(|e| QueueMutationError::new(format!("persisted active entry invalid: {e}")))?,
        ),
        None => None,
    };
    let mut finished: Vec<RecordingTask> = Vec::with_capacity(candidate_finished.len());
    for p in candidate_finished {
        finished.push(
            RecordingQueue::from_persisted(p)
                .map_err(|e| QueueMutationError::new(format!("persisted finished entry invalid: {e}")))?,
        );
    }

    // 5. Persist only after the complete candidate has been validated.
    if let (Some(state_file), Some(content)) = (this.state_file.as_ref(), content) {
        if let Some(parent) = state_file.parent() {
            fs::create_dir_all(parent).await.map_err(QueueMutationError::from_io)?;
        }
        let tmp_path = state_file.with_extension(format!("json.tmp.{next_revision}"));
        fs::write(&tmp_path, &content).await.map_err(QueueMutationError::from_io)?;
        fs::rename(&tmp_path, state_file).await.map_err(QueueMutationError::from_io)?;
    }

    // 6. Commit. Swap the validated in-memory state from the persisted
    // candidate.
    let mut queue_lock = this.queue.lock().await;
    let mut scheduled_lock = this.scheduled.write().await;
    let mut active_lock = this.active.write().await;
    let mut finished_lock = this.finished.write().await;
    *queue_lock = queue;
    *scheduled_lock = scheduled;
    *active_lock = active;
    *finished_lock = finished;
    this.revision.store(next_revision, Ordering::SeqCst);
    Ok(Some(result))
}

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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistedRecordingTask {
    pub uuid: String,
    pub kind: RecordingKind,
    pub file_dir: PathBuf,
    pub file_path: PathBuf,
    pub filename: String,
    pub url: String,
    pub finished: bool,
    pub size: u64,
    pub total_size: Option<u64>,
    pub paused: bool,
    pub error: Option<String>,
    pub state: RecordingTaskState,
    #[serde(default)]
    pub input_name: Option<String>,
    #[serde(default)]
    pub priority: i8,
    #[serde(default)]
    pub retry_attempts: u8,
    #[serde(default)]
    pub next_retry_at: Option<i64>,
    pub recording: RecordingMetadata,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PersistedRecordingQueue {
    pub schema_version: u16,
    pub queue: Vec<PersistedRecordingTask>,
    pub scheduled: Vec<PersistedRecordingTask>,
    pub active: Option<PersistedRecordingTask>,
    pub finished: Vec<PersistedRecordingTask>,
    /// Monotonic revision. Increments once per committed queue mutation.
    /// The in-memory `RecordingQueue` mirrors this counter via an `AtomicU64`.
    #[serde(default)]
    pub revision: QueueRevision,
}

impl Default for PersistedRecordingQueue {
    fn default() -> Self {
        Self {
            schema_version: RECORDING_QUEUE_SCHEMA_VERSION,
            queue: Vec::new(),
            scheduled: Vec::new(),
            active: None,
            finished: Vec::new(),
            revision: QueueRevision::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum RecordingTaskState {
    #[default]
    Queued,
    Scheduled,
    WaitingForCapacity,
    RetryWaiting,
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl RecordingTaskState {
    pub fn is_terminal(self) -> bool { matches!(self, Self::Completed | Self::Failed | Self::Cancelled) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RecordingControl {
    #[default]
    None,
    Pause,
    Cancel,
    Restart,
}

/// Returns the directory for the recording file.
/// If option `organize_into_directories` is set, the root directory is determined.
/// - For series, the episode pattern is used to determine the sub directory for the series.
/// - For vod files, the title is used to determine the sub directory.
///
/// # Arguments
/// * `recording_cfg` the recording configuration
/// * `filestem` the prepared filestem to use as sub directory
fn recording_target_directory(recording_cfg: &RecordingConfig, filestem: &str) -> PathBuf {
    if recording_cfg.organize_into_directories {
        let mut stem = filestem;
        if let Some(re) = &recording_cfg.episode_pattern {
            if let Some(captures) = re.captures(stem) {
                if let Some(episode) = captures.name("episode") {
                    if !episode.as_str().is_empty() {
                        stem = &stem[..episode.start()];
                    }
                }
            }
        }
        let dir_name = CONSTANTS.re_remove_filename_ending.replace(stem, "");
        let file_dir: PathBuf = [recording_cfg.directory.as_str(), dir_name.as_ref()].iter().collect();
        file_dir
    } else {
        PathBuf::from(recording_cfg.directory.as_str())
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
        let tmp_filename = CONSTANTS
            .re_filename
            .replace_all(&deunicode_string(req_filename).replace(' ', "_"), "")
            .replace("__", "_")
            .replace("_-_", "-");
        let filename_path = Path::new(&tmp_filename);
        let file_stem =
            filename_path.file_stem().and_then(OsStr::to_str).unwrap_or("").trim_matches(FILENAME_TRIM_PATTERNS);
        let file_ext = filename_path.extension().and_then(OsStr::to_str).unwrap_or("");

        let mut filename = if file_ext.is_empty() { file_stem.to_string() } else { format!("{file_stem}.{file_ext}") };
        let file_dir = recording_target_directory(recording_cfg, file_stem);
        let mut file_path: PathBuf = file_dir.clone();
        file_path.push(&filename);
        let mut x: usize = 1;
        while file_path.is_file() {
            filename =
                if file_ext.is_empty() { format!("{file_stem}_{x}") } else { format!("{file_stem}_{x}.{file_ext}") };
            file_path.clone_from(&file_dir);
            file_path.push(&filename);
            x += 1;
        }

        file_path.to_str()?;

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
        }
    }

    fn matches_existing_task(&self, other: &Self) -> bool {
        if self.kind != other.kind {
            return false;
        }
        match self.kind {
            RecordingKind::Vod | RecordingKind::Series => self.url == other.url || self.file_path == other.file_path,
            RecordingKind::Live => {
                (self.url == other.url
                    && self.scheduled_start() == other.scheduled_start()
                    && self.scheduled_end() == other.scheduled_end())
                    || self.file_path == other.file_path
            }
        }
    }
}

impl From<RecordingTaskState> for TransferStatusDto {
    fn from(value: RecordingTaskState) -> Self {
        match value {
            RecordingTaskState::Queued => Self::Queued,
            RecordingTaskState::Scheduled => Self::Scheduled,
            RecordingTaskState::WaitingForCapacity => Self::WaitingForCapacity,
            RecordingTaskState::RetryWaiting => Self::RetryWaiting,
            RecordingTaskState::Running => Self::Running,
            RecordingTaskState::Paused => Self::Paused,
            RecordingTaskState::Completed => Self::Completed,
            RecordingTaskState::Failed => Self::Failed,
            RecordingTaskState::Cancelled => Self::Cancelled,
        }
    }
}

/// Priority-aware wait queue for provider connection slots.
/// When the provider is at capacity, tasks register here and are
/// woken one-at-a-time in descending priority order (lowest i8 = highest priority).
struct RecordingWaiter {
    id: u64,
    input_name: Option<Arc<str>>,
    priority: i8,
    notify: Arc<Notify>,
}

type RecordingWaiters = Arc<Mutex<Vec<RecordingWaiter>>>;

#[derive(Clone)]
pub struct RecordingWaiterSnapshot {
    pub id: u64,
    pub input_name: Option<Arc<str>>,
    pub priority: i8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingWaitOutcome {
    Signalled,
    Paused,
    Cancelled,
    Restarted,
}

pub struct RecordingSlotWaitQueue {
    waiters: RecordingWaiters,
    next_waiter_id: AtomicU64,
}

impl Default for RecordingSlotWaitQueue {
    fn default() -> Self { Self::new() }
}

impl RecordingSlotWaitQueue {
    pub fn new() -> Self { Self { waiters: Arc::new(Mutex::new(Vec::new())), next_waiter_id: AtomicU64::new(1) } }

    async fn remove_waiter(&self, waiter_id: u64) { self.waiters.lock().await.retain(|waiter| waiter.id != waiter_id); }

    /// Register and block until this task is signalled or control flow requests pause/cancel.
    pub async fn wait(
        &self,
        input_name: Option<Arc<str>>,
        priority: i8,
        control_signal: &RwLock<RecordingControl>,
        control_notify: &Notify,
    ) -> RecordingWaitOutcome {
        let waiter_id = self.next_waiter_id.fetch_add(1, Ordering::Relaxed);
        let notify = Arc::new(Notify::new());
        self.waiters.lock().await.push(RecordingWaiter {
            id: waiter_id,
            input_name,
            priority,
            notify: Arc::clone(&notify),
        });

        if let Some(outcome) = self.control_outcome(waiter_id, control_signal).await {
            return outcome;
        }

        loop {
            tokio::select! {
                () = notify.notified() => return RecordingWaitOutcome::Signalled,
                () = control_notify.notified() => {
                    if let Some(outcome) = self.control_outcome(waiter_id, control_signal).await {
                        return outcome;
                    }
                }
            }
        }
    }

    /// Map the current control signal to a wait outcome, deregistering the
    /// waiter when the wait ends.
    async fn control_outcome(
        &self,
        waiter_id: u64,
        control_signal: &RwLock<RecordingControl>,
    ) -> Option<RecordingWaitOutcome> {
        let outcome = match *control_signal.read().await {
            RecordingControl::Pause => RecordingWaitOutcome::Paused,
            RecordingControl::Cancel => RecordingWaitOutcome::Cancelled,
            RecordingControl::Restart => RecordingWaitOutcome::Restarted,
            RecordingControl::None => return None,
        };
        self.remove_waiter(waiter_id).await;
        Some(outcome)
    }

    pub async fn snapshots(&self) -> Vec<RecordingWaiterSnapshot> {
        self.waiters
            .lock()
            .await
            .iter()
            .map(|waiter| RecordingWaiterSnapshot {
                id: waiter.id,
                input_name: waiter.input_name.clone(),
                priority: waiter.priority,
            })
            .collect()
    }

    /// Wake a specific waiter by id.
    pub async fn signal_waiter(&self, waiter_id: u64) -> bool {
        let mut waiters = self.waiters.lock().await;
        if let Some(idx) = waiters.iter().position(|waiter| waiter.id == waiter_id) {
            let notify = Arc::clone(&waiters[idx].notify);
            waiters.remove(idx);
            notify.notify_one();
            true
        } else {
            false
        }
    }
}

pub struct RecordingQueue {
    pub queue: Arc<Mutex<VecDeque<RecordingTask>>>,
    pub scheduled: Arc<RwLock<Vec<RecordingTask>>>,
    pub active: Arc<RwLock<Option<RecordingTask>>>,
    pub finished: Arc<RwLock<Vec<RecordingTask>>>,
    pub control_signal: Arc<RwLock<RecordingControl>>,
    pub control_notify: Arc<Notify>,
    pub worker_running: Arc<RwLock<bool>>,
    pub state_file: Option<PathBuf>,
    /// Priority-aware waiter queue for provider connection slots.
    pub slot_waiters: Arc<RecordingSlotWaitQueue>,
    /// In-memory mirror of the persisted queue revision. Incremented
    /// once per committed mutation.
    pub revision: Arc<AtomicU64>,
    mutation_guard: Arc<Mutex<()>>,
}

impl Default for RecordingQueue {
    fn default() -> Self { Self::new() }
}

impl RecordingQueue {
    pub async fn mutate_optional_and_clear_control<F, R>(
        &self,
        expected: RecordingControl,
        op: F,
    ) -> Result<Option<R>, QueueMutationError>
    where
        F: FnOnce(&mut PersistedRecordingQueue) -> Result<Option<R>, QueueMutationError>,
    {
        let _mutation = self.mutation_guard.lock().await;
        let result = mutate_optional_locked(self, op).await?;
        if result.is_some() {
            let mut control = self.control_signal.write().await;
            if *control == expected {
                *control = RecordingControl::None;
            }
        }
        Ok(result)
    }

    async fn snapshot_current(&self, revision: QueueRevision) -> PersistedRecordingQueue {
        let queue = self.queue.lock().await;
        let scheduled = self.scheduled.read().await;
        let active = self.active.read().await;
        let finished = self.finished.read().await;

        PersistedRecordingQueue {
            schema_version: RECORDING_QUEUE_SCHEMA_VERSION,
            queue: queue.iter().map(Self::to_persisted).collect(),
            scheduled: scheduled.iter().map(Self::to_persisted).collect(),
            active: active.as_ref().map(Self::to_persisted),
            finished: finished.iter().map(Self::to_persisted).collect(),
            revision,
        }
    }

    pub async fn committed_snapshot(&self) -> (QueueRevision, Vec<RecordingTask>) {
        let _mutation = self.mutation_guard.lock().await;
        let revision = QueueRevision(self.revision.load(Ordering::SeqCst));
        let queue = self.queue.lock().await;
        let scheduled = self.scheduled.read().await;
        let active = self.active.read().await;
        let finished = self.finished.read().await;
        let mut tasks =
            Vec::with_capacity(queue.len() + scheduled.len() + finished.len() + usize::from(active.is_some()));
        tasks.extend(queue.iter().cloned());
        tasks.extend(scheduled.iter().cloned());
        tasks.extend(active.iter().cloned());
        tasks.extend(finished.iter().cloned());
        (revision, tasks)
    }

    pub async fn committed_partitioned_snapshot(
        &self,
    ) -> (Vec<RecordingTask>, Option<RecordingTask>, Vec<RecordingTask>) {
        let _mutation = self.mutation_guard.lock().await;
        let queue = self.queue.lock().await;
        let scheduled = self.scheduled.read().await;
        let active = self.active.read().await;
        let finished = self.finished.read().await;
        let mut queued = Vec::with_capacity(queue.len() + scheduled.len());
        queued.extend(queue.iter().cloned());
        queued.extend(scheduled.iter().cloned());
        (queued, active.clone(), finished.clone())
    }

    fn finalize_missed_recording(mut task: RecordingTask) -> RecordingTask {
        task.finished = true;
        task.paused = false;
        task.state = RecordingTaskState::Failed;
        task.error = Some(RECORDING_WINDOW_EXPIRED_ERR.to_string());
        task
    }

    fn recording_start_missed_window(task: &RecordingTask, now_ts: i64) -> bool {
        task.kind == RecordingKind::Live
            && task.scheduled_start().zip(task.scheduled_duration_secs()).is_some_and(|(start_at, duration_secs)| {
                shared::model::recording_math::window_elapsed(start_at, duration_secs, now_ts)
            })
    }

    pub fn new() -> Self { Self::new_with_state_file(None) }

    pub fn new_with_state_file(state_file: Option<PathBuf>) -> Self {
        Self {
            queue: Arc::from(Mutex::new(VecDeque::new())),
            scheduled: Arc::from(RwLock::new(Vec::new())),
            active: Arc::from(RwLock::new(None)),
            finished: Arc::from(RwLock::new(Vec::new())),
            control_signal: Arc::from(RwLock::new(RecordingControl::None)),
            control_notify: Arc::new(Notify::new()),
            worker_running: Arc::from(RwLock::new(false)),
            state_file,
            slot_waiters: Arc::new(RecordingSlotWaitQueue::new()),
            revision: Arc::new(AtomicU64::new(0)),
            mutation_guard: Arc::new(Mutex::new(())),
        }
    }

    pub fn to_persisted(task: &RecordingTask) -> PersistedRecordingTask {
        PersistedRecordingTask {
            uuid: task.uuid.clone(),
            kind: task.kind,
            file_dir: task.file_dir.clone(),
            file_path: task.file_path.clone(),
            filename: task.filename.clone(),
            url: task.url.to_string(),
            finished: task.finished,
            size: task.size,
            total_size: task.total_size,
            paused: task.paused,
            error: task.error.clone(),
            state: task.state,
            input_name: task.input_name.as_ref().map(std::string::ToString::to_string),
            priority: task.priority,
            retry_attempts: task.retry_attempts,
            next_retry_at: task.next_retry_at,
            recording: task.recording.clone(),
        }
    }

    pub fn from_persisted(task: PersistedRecordingTask) -> Result<RecordingTask, PersistedError> {
        let url = reqwest::Url::parse(&task.url).map_err(|e| PersistedError::InvalidUrl(e.to_string()))?;
        Ok(RecordingTask {
            uuid: task.uuid,
            kind: task.kind,
            file_dir: task.file_dir,
            file_path: task.file_path,
            filename: task.filename,
            url,
            finished: task.finished,
            size: task.size,
            total_size: task.total_size,
            paused: task.paused,
            error: task.error,
            state: task.state,
            input_name: task.input_name.map(|s| Arc::from(s.as_str())),
            priority: task.priority,
            retry_attempts: task.retry_attempts,
            next_retry_at: task.next_retry_at,
            recording: task.recording,
        })
    }

    pub async fn persist_to_disk(&self) -> std::io::Result<()> {
        let result = self.try_persist_to_disk().await;
        // Callers discard the result; log here so persistence failures are never silent
        if let Err(err) = &result {
            error!("Failed to persist recording queue: {err}");
        }
        result
    }

    async fn try_persist_to_disk(&self) -> std::io::Result<()> {
        let Some(state_file) = self.state_file.as_ref() else {
            return Ok(());
        };

        let queue = self.queue.lock().await.iter().map(Self::to_persisted).collect::<Vec<_>>();
        let scheduled = self.scheduled.read().await.iter().map(Self::to_persisted).collect::<Vec<_>>();
        let active = self.active.read().await.as_ref().map(Self::to_persisted);
        let finished = self.finished.read().await.iter().map(Self::to_persisted).collect::<Vec<_>>();
        let revision = self.revision.load(Ordering::SeqCst);
        let payload = PersistedRecordingQueue {
            schema_version: RECORDING_QUEUE_SCHEMA_VERSION,
            queue,
            scheduled,
            active,
            finished,
            revision: QueueRevision(revision),
        };
        let content = serde_json::to_vec_pretty(&payload).map_err(std::io::Error::other)?;

        if let Some(parent) = state_file.parent() {
            fs::create_dir_all(parent).await?;
        }

        write_json_atomic(state_file, &content).await
    }

    /// Load the canonical recording state. A missing file is a fresh
    /// install. Malformed content or an unreadable entry is an error the
    /// caller must propagate: the file is never renamed, reset, or backed up.
    pub async fn load_from_disk(&self) -> std::io::Result<()> {
        let Some(state_file) = self.state_file.as_ref() else {
            return Ok(());
        };
        if !file_exists_async(state_file).await {
            return Ok(());
        }

        let content = fs::read_to_string(state_file).await?;
        let persisted: PersistedRecordingQueue =
            serde_json::from_str(&content).map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        if persisted.schema_version != RECORDING_QUEUE_SCHEMA_VERSION {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                PersistedError::UnsupportedSchema(persisted.schema_version),
            ));
        }

        let invalid = |err: PersistedError| std::io::Error::new(std::io::ErrorKind::InvalidData, err);
        let mut queue = VecDeque::with_capacity(persisted.queue.len());
        for task in persisted.queue {
            queue.push_back(Self::recover_loaded_task(Self::from_persisted(task).map_err(invalid)?));
        }
        let now_ts = Utc::now().timestamp();
        let mut scheduled = Vec::with_capacity(persisted.scheduled.len());
        let mut missed_scheduled = Vec::new();
        for task in persisted.scheduled {
            let task = Self::recover_loaded_task(Self::from_persisted(task).map_err(invalid)?);
            if Self::recording_start_missed_window(&task, now_ts) {
                missed_scheduled.push(task);
            } else {
                scheduled.push(task);
            }
        }
        let active = match persisted.active {
            Some(task) => Some(Self::recover_loaded_task(Self::from_persisted(task).map_err(invalid)?)),
            None => None,
        };
        let mut finished = Vec::with_capacity(persisted.finished.len());
        for task in persisted.finished {
            finished.push(Self::from_persisted(task).map_err(invalid)?);
        }
        finished.extend(missed_scheduled.into_iter().map(Self::finalize_missed_recording));

        *self.queue.lock().await = queue;
        *self.scheduled.write().await = scheduled;
        *self.finished.write().await = finished;
        self.revision.store(persisted.revision.0, Ordering::SeqCst);
        if let Some(active) = active {
            if active.paused || active.state == RecordingTaskState::Paused {
                *self.active.write().await = Some(active);
            } else if !active.finished && active.state != RecordingTaskState::Cancelled {
                self.queue.lock().await.push_front(active);
                *self.active.write().await = None;
            } else {
                self.finished.write().await.push(active);
                *self.active.write().await = None;
            }
        } else {
            *self.active.write().await = None;
        }
        *self.control_signal.write().await = RecordingControl::None;
        *self.worker_running.write().await = false;
        Ok(())
    }

    fn recover_loaded_task(mut task: RecordingTask) -> RecordingTask {
        if task.paused || task.state == RecordingTaskState::Paused {
            task.paused = true;
            task.finished = false;
            task.state = RecordingTaskState::Paused;
            return task;
        }
        if task.state == RecordingTaskState::Scheduled {
            task.paused = false;
            task.finished = false;
            return task;
        }
        if !task.finished {
            task.paused = false;
            task.state = RecordingTaskState::Queued;
            task.error = None;
            task.retry_attempts = 0;
            task.next_retry_at = None;
        }
        task
    }

    pub async fn find_duplicate(&self, candidate: &RecordingTask) -> Option<RecordingTask> {
        if let Some(active) = self.active.read().await.as_ref() {
            if active.matches_existing_task(candidate) {
                return Some(active.clone());
            }
        }

        if let Some(queued) = self.queue.lock().await.iter().find(|task| task.matches_existing_task(candidate)).cloned()
        {
            return Some(queued);
        }

        if let Some(scheduled) =
            self.scheduled.read().await.iter().find(|task| task.matches_existing_task(candidate)).cloned()
        {
            return Some(scheduled);
        }

        self.finished.read().await.iter().find(|task| task.matches_existing_task(candidate)).cloned()
    }

    /// Pause the active task. Persists the new state through the
    /// transactional boundary. The runtime-only control signal is published
    /// after the commit while the mutation guard still preserves ordering.
    /// Live captures are not pausable and are rejected here.
    pub async fn pause_active(&self, uuid: &str) -> Result<bool, QueueMutationError> {
        let _mutation = self.mutation_guard.lock().await;
        let changed = mutate_optional_locked(self, |candidate| {
            let Some(active) = candidate.active.as_mut().filter(|active| active.uuid == uuid) else {
                return Ok(None);
            };
            if !active.kind.is_resumable() {
                return Err(QueueMutationError::StateNotEditable);
            }
            active.paused = true;
            active.state = RecordingTaskState::Paused;
            active.next_retry_at = None;
            Ok(Some(true))
        })
        .await?
        .unwrap_or(false);
        if !changed {
            return Ok(false);
        }
        *self.control_signal.write().await = RecordingControl::Pause;
        self.control_notify.notify_waiters();
        Ok(true)
    }

    /// Resume the active task. Persists the new state through the
    /// transactional boundary.
    pub async fn resume_active(&self, uuid: &str) -> Result<bool, QueueMutationError> {
        let _mutation = self.mutation_guard.lock().await;
        let changed = mutate_optional_locked(self, |candidate| {
            let Some(active) = candidate.active.as_mut().filter(|active| active.uuid == uuid && active.paused) else {
                return Ok(None);
            };
            if !active.kind.is_resumable() {
                return Err(QueueMutationError::StateNotEditable);
            }
            active.paused = false;
            active.state = RecordingTaskState::Running;
            active.next_retry_at = None;
            Ok(Some(true))
        })
        .await?
        .unwrap_or(false);
        if !changed {
            return Ok(false);
        }
        *self.control_signal.write().await = RecordingControl::None;
        self.control_notify.notify_waiters();
        Ok(true)
    }

    /// Cancel the active task. Persists the new state through the
    /// transactional boundary.
    pub async fn cancel_active_matching(&self, uuid: &str) -> Result<bool, QueueMutationError> {
        let _mutation = self.mutation_guard.lock().await;
        let changed = mutate_optional_locked(self, |candidate| {
            let Some(active) = candidate.active.as_mut().filter(|active| active.uuid == uuid) else {
                return Ok(None);
            };
            active.state = RecordingTaskState::Cancelled;
            active.error = Some("Cancelled by user".to_string());
            active.next_retry_at = None;
            Ok(Some(true))
        })
        .await?
        .unwrap_or(false);
        if !changed {
            return Ok(false);
        }
        *self.control_signal.write().await = RecordingControl::Cancel;
        self.control_notify.notify_waiters();
        Ok(true)
    }

    pub async fn cancel_active(&self) -> Result<bool, QueueMutationError> {
        let Some(uuid) = self.active.read().await.as_ref().map(|active| active.uuid.clone()) else {
            return Ok(false);
        };
        self.cancel_active_matching(&uuid).await
    }

    pub async fn cancel_requested(&self, uuid: &str) -> Result<Option<bool>, QueueMutationError> {
        let _mutation = self.mutation_guard.lock().await;
        let was_paused = mutate_optional_locked(self, |candidate| {
            let Some(active) = candidate.active.as_ref().filter(|active| active.uuid == uuid) else {
                return Ok(None);
            };
            let was_paused = active.paused;
            if was_paused {
                let Some(mut cancelled) = candidate.active.take() else {
                    return Ok(None);
                };
                cancelled.finished = true;
                cancelled.paused = false;
                cancelled.next_retry_at = None;
                cancelled.error.get_or_insert_with(|| "Cancelled by user".to_string());
                cancelled.state = RecordingTaskState::Cancelled;
                candidate.finished.push(cancelled);
                if !candidate.queue.is_empty() {
                    candidate.active = Some(candidate.queue.remove(0));
                }
            } else if let Some(active) = candidate.active.as_mut() {
                active.state = RecordingTaskState::Cancelled;
                active.error = Some("Cancelled by user".to_string());
                active.next_retry_at = None;
            }
            Ok(Some(was_paused))
        })
        .await?;

        if let Some(was_paused) = was_paused {
            *self.control_signal.write().await =
                if was_paused { RecordingControl::None } else { RecordingControl::Cancel };
            self.control_notify.notify_waiters();
        }
        Ok(was_paused)
    }

    pub fn request_worker_restart(&self) {
        if let Ok(mut control) = self.control_signal.try_write() {
            *control = RecordingControl::Restart;
            self.control_notify.notify_waiters();
            return;
        }
        let control_signal = Arc::clone(&self.control_signal);
        let control_notify = Arc::clone(&self.control_notify);
        tokio::spawn(async move {
            *control_signal.write().await = RecordingControl::Restart;
            control_notify.notify_waiters();
        });
    }

    pub async fn remove_from_queue(&self, uuid: &str) -> Result<bool, QueueMutationError> {
        Ok(mutate_optional(self, |candidate| {
            let queue_len = candidate.queue.len();
            candidate.queue.retain(|task| task.uuid != uuid);
            if candidate.queue.len() != queue_len {
                return Ok(Some(true));
            }
            let scheduled_len = candidate.scheduled.len();
            candidate.scheduled.retain(|task| task.uuid != uuid);
            Ok((candidate.scheduled.len() != scheduled_len).then_some(true))
        })
        .await?
        .unwrap_or(false))
    }

    pub async fn remove_finished(&self, uuid: &str) -> Result<bool, QueueMutationError> {
        Ok(mutate_optional(self, |candidate| {
            let initial_len = candidate.finished.len();
            candidate.finished.retain(|task| task.uuid != uuid);
            Ok((candidate.finished.len() != initial_len).then_some(true))
        })
        .await?
        .unwrap_or(false))
    }

    pub async fn remove(&self, uuid: &str) -> Result<bool, QueueMutationError> {
        Ok(mutate_optional(self, |candidate| {
            let original_len = candidate.queue.len() + candidate.scheduled.len() + candidate.finished.len();
            candidate.queue.retain(|task| task.uuid != uuid);
            candidate.scheduled.retain(|task| task.uuid != uuid);
            candidate.finished.retain(|task| task.uuid != uuid);
            let current_len = candidate.queue.len() + candidate.scheduled.len() + candidate.finished.len();
            Ok((current_len != original_len).then_some(true))
        })
        .await?
        .unwrap_or(false))
    }

    /// Requeue a finished VOD/Series transfer. A Live capture cannot be
    /// retried: its programme window is gone.
    pub async fn retry_finished(&self, uuid: &str) -> Result<bool, QueueMutationError> {
        Ok(mutate_optional(self, |candidate| {
            if let Some(pos) = candidate.finished.iter().position(|task| task.uuid == uuid) {
                let mut task = candidate.finished.remove(pos);
                if !task.kind.is_resumable() {
                    candidate.finished.insert(pos, task);
                    return Ok(None);
                }
                task.finished = false;
                task.size = 0;
                task.paused = false;
                task.error = None;
                task.state = RecordingTaskState::Queued;
                task.retry_attempts = 0;
                task.next_retry_at = None;
                candidate.queue.push(task);
                Ok(Some(true))
            } else {
                Ok(None)
            }
        })
        .await?
        .unwrap_or(false))
    }

    pub async fn promote_due_scheduled(&self, now_ts: i64) -> usize {
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
            Ok(None) | Err(_) => 0,
        }
    }

    pub async fn promote_due_scheduled_now(&self) -> usize { self.promote_due_scheduled(Utc::now().timestamp()).await }
}
#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::recording::{RecordingOwner, RecordingSource, RecordingVisibility};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::time::{timeout, Duration};

    fn temp_state_file(name: &str) -> PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).expect("time").as_nanos();
        std::env::temp_dir().join(format!("tuliprox_{name}_{nanos}.json"))
    }

    fn live_meta(owner: &str, start: i64, duration: u64) -> RecordingMetadata {
        RecordingMetadata::new_live(
            RecordingOwner::User(UserId::from(owner)),
            RecordingVisibility::Private,
            RecordingSource::new("target", "v", "input-a"),
            start,
            start.saturating_add(duration.cast_signed()),
            0,
            0,
        )
    }

    fn media_meta(owner: &str) -> RecordingMetadata {
        RecordingMetadata::new_media(
            RecordingOwner::User(UserId::from(owner)),
            RecordingVisibility::Private,
            RecordingSource::new("target", "v", "input-a"),
            String::new(),
        )
    }

    fn task(uuid: &str, kind: RecordingKind, state: RecordingTaskState) -> RecordingTask {
        RecordingTask {
            uuid: uuid.to_string(),
            kind,
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from(format!("/tmp/{uuid}.ts")),
            filename: format!("{uuid}.ts"),
            url: reqwest::Url::parse(&format!("https://example.com/{uuid}")).expect("valid url"),
            finished: false,
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: match kind {
                RecordingKind::Live => live_meta("web:alice", 1_700_000_000, 3_600),
                _ => media_meta("web:alice"),
            },
        }
    }

    #[tokio::test]
    async fn pause_and_resume_keep_active_download_resumable() {
        let queue = RecordingQueue::new();
        let active = RecordingTask {
            size: 42,
            total_size: Some(100),
            state: RecordingTaskState::Running,
            ..task("id", RecordingKind::Vod, RecordingTaskState::Running)
        };

        *queue.active.write().await = Some(active);
        queue.pause_active("id").await.expect("pause active");

        let paused = queue.active.read().await.clone().expect("active download");
        assert_eq!(paused.state, RecordingTaskState::Paused);
        assert!(paused.paused);
        assert!(!paused.finished);

        queue.resume_active("id").await.expect("resume active");

        let resumed = queue.active.read().await.clone().expect("active download");
        assert_eq!(resumed.state, RecordingTaskState::Running);
        assert!(!resumed.paused);
        assert!(!resumed.finished);
    }

    #[tokio::test]
    async fn cancel_marks_active_download_cancelled_without_finishing_immediately() {
        let queue = RecordingQueue::new();
        let active = task("id", RecordingKind::Vod, RecordingTaskState::Running);

        *queue.active.write().await = Some(active);
        queue.cancel_active().await.expect("cancel active");

        let cancelled = queue.active.read().await.clone().expect("active download");
        assert_eq!(cancelled.state, RecordingTaskState::Cancelled);
        assert!(!cancelled.finished);
        assert_eq!(cancelled.error.as_deref(), Some("Cancelled by user"));
        assert!(queue.finished.read().await.is_empty());
    }

    #[tokio::test]
    async fn persisted_queue_round_trips_and_requeues_running_downloads() {
        let state_file = temp_state_file("download_state");
        let queue = RecordingQueue::new_with_state_file(Some(state_file.clone()));
        let queued = RecordingTask {
            size: 10,
            total_size: Some(100),
            ..task("queued", RecordingKind::Vod, RecordingTaskState::Queued)
        };
        let active = RecordingTask {
            size: 20,
            total_size: Some(200),
            ..task("active", RecordingKind::Vod, RecordingTaskState::Running)
        };
        let paused = RecordingTask {
            size: 30,
            total_size: Some(300),
            paused: true,
            state: RecordingTaskState::Paused,
            ..task("paused", RecordingKind::Vod, RecordingTaskState::Paused)
        };

        queue.queue.lock().await.push_back(queued);
        *queue.active.write().await = Some(active);
        queue.finished.write().await.push(paused.clone());
        queue.persist_to_disk().await.expect("persist state");

        let restored = RecordingQueue::new_with_state_file(Some(state_file.clone()));
        restored.load_from_disk().await.expect("load state");

        assert_eq!(restored.queue.lock().await.len(), 2);
        let restored_active = restored.active.read().await.clone();
        assert!(restored_active.is_none());
        let restored_finished = restored.finished.read().await.clone();
        assert_eq!(restored_finished.len(), 1);
        assert_eq!(restored_finished[0].uuid, paused.uuid);

        let queued_items = restored.queue.lock().await.iter().map(|d| d.uuid.clone()).collect::<Vec<_>>();
        assert!(queued_items.iter().any(|id| id == "queued"));
        assert!(queued_items.iter().any(|id| id == "active"));

        let _ = std::fs::remove_file(state_file);
    }

    #[tokio::test]
    async fn persisted_scheduled_recordings_round_trip_without_becoming_active() {
        let state_file = temp_state_file("record_state");
        let queue = RecordingQueue::new_with_state_file(Some(state_file.clone()));
        let future_start = Utc::now().timestamp().saturating_add(3_600);
        let scheduled = RecordingTask {
            url: reqwest::Url::parse("https://example.com/live/1").expect("valid url"),
            state: RecordingTaskState::Scheduled,
            recording: live_meta("web:alice", future_start, 5_400),
            ..task("recording", RecordingKind::Live, RecordingTaskState::Scheduled)
        };

        queue.scheduled.write().await.push(scheduled.clone());
        queue.persist_to_disk().await.expect("persist state");

        let restored = RecordingQueue::new_with_state_file(Some(state_file.clone()));
        restored.load_from_disk().await.expect("load state");

        assert!(restored.active.read().await.is_none());
        assert_eq!(restored.queue.lock().await.len(), 0);
        let restored_scheduled = restored.scheduled.read().await.clone();
        assert_eq!(restored_scheduled.len(), 1);
        assert_eq!(restored_scheduled[0].uuid, scheduled.uuid);
        assert_eq!(restored_scheduled[0].state, RecordingTaskState::Scheduled);
        assert_eq!(restored_scheduled[0].scheduled_start(), Some(future_start));
        assert_eq!(restored_scheduled[0].scheduled_end(), Some(future_start + 5_400));
        assert_eq!(restored_scheduled[0].scheduled_duration_secs(), Some(5_400));
        assert_eq!(restored_scheduled[0].kind, RecordingKind::Live);

        let _ = std::fs::remove_file(state_file);
    }

    #[test]
    fn recover_loaded_task_requeues_waiting_states() {
        let waiting_for_capacity = RecordingTask {
            size: 77,
            total_size: Some(99),
            error: Some("old error".to_string()),
            state: RecordingTaskState::WaitingForCapacity,
            ..task("capacity", RecordingKind::Vod, RecordingTaskState::WaitingForCapacity)
        };
        let retry_waiting = RecordingTask { state: RecordingTaskState::RetryWaiting, ..waiting_for_capacity.clone() };

        let restored_waiting_for_capacity = RecordingQueue::recover_loaded_task(waiting_for_capacity);
        let restored_retry_waiting = RecordingQueue::recover_loaded_task(retry_waiting);

        assert_eq!(restored_waiting_for_capacity.state, RecordingTaskState::Queued);
        assert!(!restored_waiting_for_capacity.paused);
        assert!(restored_waiting_for_capacity.error.is_none());

        assert_eq!(restored_retry_waiting.state, RecordingTaskState::Queued);
        assert!(!restored_retry_waiting.paused);
        assert!(restored_retry_waiting.error.is_none());
    }

    #[test]
    fn recover_loaded_task_clears_pending_retry_timestamp() {
        let retry_waiting = RecordingTask {
            size: 12,
            total_size: Some(20),
            error: Some("retrying".to_string()),
            state: RecordingTaskState::RetryWaiting,
            retry_attempts: 2,
            next_retry_at: Some(1_700_000_000),
            ..task("retry", RecordingKind::Vod, RecordingTaskState::RetryWaiting)
        };

        let restored = RecordingQueue::recover_loaded_task(retry_waiting);
        assert_eq!(restored.state, RecordingTaskState::Queued);
        assert_eq!(restored.retry_attempts, 0);
        assert!(restored.next_retry_at.is_none());
    }

    #[tokio::test]
    async fn retry_finished_clears_retry_metadata() {
        let queue = RecordingQueue::new();
        queue.finished.write().await.push(RecordingTask {
            finished: true,
            error: Some("Retry limit reached".to_string()),
            state: RecordingTaskState::Failed,
            retry_attempts: 5,
            next_retry_at: Some(1_700_000_000),
            ..task("done", RecordingKind::Vod, RecordingTaskState::Failed)
        });

        assert!(queue.retry_finished("done").await.expect("retry finished"));
        let queued = queue.queue.lock().await.front().cloned().expect("queued download");
        assert_eq!(queued.state, RecordingTaskState::Queued);
        assert_eq!(queued.retry_attempts, 0);
        assert!(queued.next_retry_at.is_none());
        assert!(queued.error.is_none());
    }

    #[tokio::test]
    async fn retry_finished_rejects_recordings() {
        let queue = RecordingQueue::new();
        queue.finished.write().await.push(RecordingTask {
            finished: true,
            error: Some("Cancelled by user".to_string()),
            state: RecordingTaskState::Cancelled,
            recording: live_meta("web:alice", 1_700_000_000, 300),
            ..task("recording", RecordingKind::Live, RecordingTaskState::Cancelled)
        });

        assert!(!queue.retry_finished("recording").await.expect("reject recording retry"));
        assert!(queue.queue.lock().await.is_empty());
        assert_eq!(queue.finished.read().await.len(), 1);
    }

    #[tokio::test]
    async fn promote_due_scheduled_moves_only_ready_recordings_to_queue() {
        let queue = RecordingQueue::new();
        let due = RecordingTask {
            size: 123,
            total_size: Some(999),
            error: Some("old error".to_string()),
            state: RecordingTaskState::Scheduled,
            recording: live_meta("web:alice", 100, 60),
            ..task("due", RecordingKind::Live, RecordingTaskState::Scheduled)
        };
        let future = RecordingTask {
            state: RecordingTaskState::Scheduled,
            recording: live_meta("web:alice", 200, 60),
            ..task("future", RecordingKind::Live, RecordingTaskState::Scheduled)
        };

        queue.scheduled.write().await.extend([due, future]);
        let revision = queue.revision.load(Ordering::SeqCst);

        let promoted = queue.promote_due_scheduled(150).await;

        assert_eq!(promoted, 1);
        assert_eq!(queue.revision.load(Ordering::SeqCst), revision + 1);
        let queued_items = queue.queue.lock().await.iter().cloned().collect::<Vec<_>>();
        assert_eq!(queued_items.len(), 1);
        assert_eq!(queued_items[0].uuid, "due");
        assert_eq!(queued_items[0].state, RecordingTaskState::Queued);
        assert_eq!(queued_items[0].size, 0);
        assert!(queued_items[0].error.is_none());
        let scheduled_items = queue.scheduled.read().await.clone();
        assert_eq!(scheduled_items.len(), 1);
        assert_eq!(scheduled_items[0].uuid, "future");
    }

    #[tokio::test]
    async fn promote_due_scheduled_marks_expired_recordings_failed() {
        let queue = RecordingQueue::new();
        let expired = RecordingTask {
            state: RecordingTaskState::Scheduled,
            recording: live_meta("web:alice", 100, 60),
            ..task("expired", RecordingKind::Live, RecordingTaskState::Scheduled)
        };

        queue.scheduled.write().await.push(expired);
        let promoted = queue.promote_due_scheduled(200).await;

        assert_eq!(promoted, 1);
        assert!(queue.queue.lock().await.is_empty());
        let finished = queue.finished.read().await.clone();
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].uuid, "expired");
        assert_eq!(finished[0].state, RecordingTaskState::Failed);
        assert!(finished[0].finished);
        assert_eq!(finished[0].error.as_deref(), Some("Recording window already expired"));
    }

    #[tokio::test]
    async fn load_from_disk_moves_expired_scheduled_recordings_to_finished() {
        let state_file = temp_state_file("expired_record_state");
        let queue = RecordingQueue::new_with_state_file(Some(state_file.clone()));
        let expired = RecordingTask {
            state: RecordingTaskState::Scheduled,
            recording: live_meta("web:alice", 100, 60),
            ..task("expired", RecordingKind::Live, RecordingTaskState::Scheduled)
        };

        queue.scheduled.write().await.push(expired);
        queue.persist_to_disk().await.expect("persist state");

        let restored = RecordingQueue::new_with_state_file(Some(state_file.clone()));
        restored.load_from_disk().await.expect("load state");

        assert!(restored.scheduled.read().await.is_empty());
        let finished = restored.finished.read().await.clone();
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].uuid, "expired");
        assert_eq!(finished[0].state, RecordingTaskState::Failed);
        assert_eq!(finished[0].error.as_deref(), Some("Recording window already expired"));

        let _ = std::fs::remove_file(state_file);
    }

    #[test]
    fn recording_uuid_differs_for_same_url_with_different_start_times() {
        let cfg = RecordingConfig::from(&shared::model::RecordingConfigDto {
            directory: Some("/tmp".to_string()),
            ..Default::default()
        });

        let first = RecordingTask::new(
            RecordingKind::Live,
            "https://example.com/live/1",
            "recording_1.ts",
            &cfg,
            None,
            0,
            live_meta("web:alice", 1_700_000_000, 5_400),
        )
        .expect("first recording");
        let second = RecordingTask::new(
            RecordingKind::Live,
            "https://example.com/live/1",
            "recording_2.ts",
            &cfg,
            None,
            0,
            live_meta("web:alice", 1_700_005_400, 5_400),
        )
        .expect("second recording");

        assert_ne!(first.uuid, second.uuid);
    }

    #[test]
    fn download_uuid_differs_for_same_url_with_different_filenames() {
        let cfg = RecordingConfig::from(&shared::model::RecordingConfigDto {
            directory: Some("/tmp".to_string()),
            ..Default::default()
        });

        let first = RecordingTask::new(
            RecordingKind::Vod,
            "https://example.com/video.mp4",
            "first.mp4",
            &cfg,
            None,
            0,
            media_meta("web:alice"),
        )
        .expect("first download");
        let second = RecordingTask::new(
            RecordingKind::Vod,
            "https://example.com/video.mp4",
            "second.mp4",
            &cfg,
            None,
            0,
            media_meta("web:alice"),
        )
        .expect("second download");

        assert_ne!(first.uuid, second.uuid);
    }

    #[test]
    fn download_new_omits_trailing_dot_when_filename_has_no_extension() {
        let cfg = RecordingConfig::from(&shared::model::RecordingConfigDto {
            directory: Some("/tmp".to_string()),
            ..Default::default()
        });

        let task = RecordingTask::new(
            RecordingKind::Vod,
            "https://example.com/live",
            "title with trailing dot.",
            &cfg,
            None,
            0,
            media_meta("web:alice"),
        )
        .expect("download");

        assert_eq!(task.filename, "title_with_trailing_dot");
        assert!(!task.filename.ends_with('.'));
    }

    #[tokio::test]
    async fn promote_due_scheduled_places_due_recordings_ahead_of_existing_queue_items() {
        let queue = RecordingQueue::new();
        queue.queue.lock().await.push_back(task("existing", RecordingKind::Vod, RecordingTaskState::Queued));
        queue.scheduled.write().await.extend([
            RecordingTask {
                state: RecordingTaskState::Scheduled,
                recording: live_meta("web:alice", 100, 60),
                ..task("due-first", RecordingKind::Live, RecordingTaskState::Scheduled)
            },
            RecordingTask {
                state: RecordingTaskState::Scheduled,
                recording: live_meta("web:alice", 110, 60),
                ..task("due-second", RecordingKind::Live, RecordingTaskState::Scheduled)
            },
        ]);

        let promoted = queue.promote_due_scheduled(150).await;

        assert_eq!(promoted, 2);
        let queued = queue.queue.lock().await.iter().map(|download| download.uuid.clone()).collect::<Vec<_>>();
        assert_eq!(queued, vec!["due-first", "due-second", "existing"]);
    }

    #[tokio::test]
    async fn download_slot_wait_queue_signals_matching_waiter_by_id() {
        let queue = Arc::new(RecordingSlotWaitQueue::new());
        let control_signal = Arc::new(RwLock::new(RecordingControl::None));
        let control_notify = Arc::new(Notify::new());

        let queue_for_a = Arc::clone(&queue);
        let control_signal_for_a = Arc::clone(&control_signal);
        let control_notify_for_a = Arc::clone(&control_notify);
        let waiter_a = tokio::spawn(async move {
            queue_for_a
                .wait(Some(Arc::from("input-a")), 1, control_signal_for_a.as_ref(), control_notify_for_a.as_ref())
                .await
        });

        let queue_for_b = Arc::clone(&queue);
        let control_signal_for_b = Arc::clone(&control_signal);
        let control_notify_for_b = Arc::clone(&control_notify);
        let waiter_b = tokio::spawn(async move {
            queue_for_b
                .wait(Some(Arc::from("input-b")), 0, control_signal_for_b.as_ref(), control_notify_for_b.as_ref())
                .await
        });

        let waiter_b_id = loop {
            let snapshots = queue.snapshots().await;
            if snapshots.len() == 2 {
                break snapshots
                    .into_iter()
                    .find(|waiter| waiter.input_name.as_deref() == Some("input-b"))
                    .map(|waiter| waiter.id)
                    .expect("waiter id for input-b");
            }
            tokio::task::yield_now().await;
        };

        assert!(queue.signal_waiter(waiter_b_id).await);
        assert_eq!(
            timeout(Duration::from_millis(100), waiter_b).await.expect("waiter_b finished").expect("join ok"),
            RecordingWaitOutcome::Signalled
        );

        *control_signal.write().await = RecordingControl::Cancel;
        control_notify.notify_waiters();
        assert_eq!(
            timeout(Duration::from_millis(100), waiter_a).await.expect("waiter_a finished").expect("join ok"),
            RecordingWaitOutcome::Cancelled
        );
    }

    #[tokio::test]
    async fn find_duplicate_matches_active_queue_scheduled_and_finished_downloads() {
        let queue = RecordingQueue::new();
        let candidate = task("candidate", RecordingKind::Vod, RecordingTaskState::Queued);

        *queue.active.write().await = Some(RecordingTask { uuid: "active".to_string(), ..candidate.clone() });
        assert_eq!(queue.find_duplicate(&candidate).await.map(|download| download.uuid), Some("active".to_string()));

        *queue.active.write().await = None;
        queue.queue.lock().await.push_back(RecordingTask { uuid: "queued".to_string(), ..candidate.clone() });
        assert_eq!(queue.find_duplicate(&candidate).await.map(|download| download.uuid), Some("queued".to_string()));

        queue.queue.lock().await.clear();
        let scheduled = RecordingTask {
            state: RecordingTaskState::Scheduled,
            recording: live_meta("web:alice", 100, 60),
            file_path: PathBuf::from("/tmp/recording.ts"),
            filename: "recording.ts".to_string(),
            ..task("scheduled", RecordingKind::Live, RecordingTaskState::Scheduled)
        };
        queue.scheduled.write().await.push(scheduled);
        let recording_candidate = RecordingTask {
            state: RecordingTaskState::Scheduled,
            recording: live_meta("web:alice", 100, 60),
            file_path: PathBuf::from("/tmp/recording.ts"),
            filename: "recording.ts".to_string(),
            ..task("recording-candidate", RecordingKind::Live, RecordingTaskState::Scheduled)
        };
        assert_eq!(
            queue.find_duplicate(&recording_candidate).await.map(|download| download.uuid),
            Some("scheduled".to_string())
        );

        queue.scheduled.write().await.clear();
        queue.finished.write().await.push(RecordingTask {
            uuid: "finished".to_string(),
            finished: true,
            state: RecordingTaskState::Completed,
            ..candidate.clone()
        });
        assert_eq!(queue.find_duplicate(&candidate).await.map(|download| download.uuid), Some("finished".to_string()));
    }

    #[tokio::test]
    async fn find_duplicate_allows_distinct_recording_windows() {
        let queue = RecordingQueue::new();
        queue.scheduled.write().await.push(RecordingTask {
            state: RecordingTaskState::Scheduled,
            recording: live_meta("web:alice", 100, 60),
            ..task("scheduled", RecordingKind::Live, RecordingTaskState::Scheduled)
        });

        let different_window = RecordingTask {
            state: RecordingTaskState::Scheduled,
            recording: live_meta("web:alice", 200, 60),
            ..task("candidate", RecordingKind::Live, RecordingTaskState::Scheduled)
        };

        assert!(queue.find_duplicate(&different_window).await.is_none());
    }

    #[tokio::test]
    async fn request_worker_restart_sets_restart_control_and_notifies_waiters() {
        let queue = RecordingQueue::new();
        let waiter_queue = Arc::clone(&queue.slot_waiters);
        let control_signal = Arc::clone(&queue.control_signal);
        let control_notify = Arc::clone(&queue.control_notify);

        let waiter =
            tokio::spawn(
                async move { waiter_queue.wait(None, 0, control_signal.as_ref(), control_notify.as_ref()).await },
            );

        tokio::task::yield_now().await;
        queue.request_worker_restart();

        assert_eq!(
            timeout(Duration::from_millis(100), waiter).await.expect("waiter finished").expect("join ok"),
            RecordingWaitOutcome::Restarted
        );
        assert_eq!(*queue.control_signal.read().await, RecordingControl::Restart);
    }

    #[tokio::test]
    async fn wait_observes_preexisting_control_before_selecting() {
        let queue = RecordingQueue::new();
        *queue.control_signal.write().await = RecordingControl::Pause;

        let outcome =
            queue.slot_waiters.wait(None, 0, queue.control_signal.as_ref(), queue.control_notify.as_ref()).await;

        assert_eq!(outcome, RecordingWaitOutcome::Paused);
        assert!(queue.slot_waiters.snapshots().await.is_empty());
    }

    #[test]
    fn from_persisted_rejects_invalid_url() {
        let mut p = RecordingQueue::to_persisted(&task("bad-url", RecordingKind::Vod, RecordingTaskState::Queued));
        p.url = "not a url at all".to_string();
        let result = RecordingQueue::from_persisted(p);
        assert!(result.is_err(), "invalid url must surface as an error");
        assert!(matches!(result.unwrap_err(), PersistedError::InvalidUrl(_)), "must surface the parse error");
    }

    // --- Transactional queue mutation boundary ---

    fn make_test_recording_task(uuid: &str, file_path: PathBuf) -> RecordingTask {
        make_test_task_of_kind(uuid, file_path, RecordingKind::Live)
    }

    /// Pause/resume are VOD/Series-only, so those tests need a resumable task.
    fn make_test_transfer_task(uuid: &str, file_path: PathBuf) -> RecordingTask {
        make_test_task_of_kind(uuid, file_path, RecordingKind::Vod)
    }

    fn make_test_task_of_kind(uuid: &str, file_path: PathBuf, kind: RecordingKind) -> RecordingTask {
        let mut task = task(uuid, kind, RecordingTaskState::Running);
        task.file_dir = file_path.parent().unwrap_or(Path::new("/")).to_path_buf();
        task.file_path = file_path;
        task.filename = format!("{uuid}.ts");
        task
    }

    #[tokio::test]
    async fn mutate_persists_and_increments_revision_on_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads_state.json");
        let queue = RecordingQueue::new_with_state_file(Some(state_file.clone()));
        assert_eq!(queue.revision.load(Ordering::SeqCst), 0);

        // Insert one recording so a candidate is non-empty.
        let task = make_test_recording_task("rec-1", dir.path().join("a.ts"));
        queue.queue.lock().await.push_back(task);

        let result: Result<(), QueueMutationError> = mutate(&queue, |_candidate| Ok(())).await;
        assert!(result.is_ok(), "mutate should succeed: {result:?}");
        // The first committed mutation publishes revision 1, both in
        // memory and in the persisted candidate.
        assert_eq!(queue.revision.load(Ordering::SeqCst), 1, "counter must store the new value");
        let content = std::fs::read(&state_file).expect("read state file");
        let restored: PersistedRecordingQueue = serde_json::from_slice(&content).expect("parse state file");
        assert_eq!(restored.revision, QueueRevision(1), "file carries the candidate's revision");
    }

    #[tokio::test]
    async fn mutate_keeps_state_when_closure_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads_state.json");
        let queue = RecordingQueue::new_with_state_file(Some(state_file.clone()));
        let original_revision = queue.revision.load(Ordering::SeqCst);
        let original_len = queue.queue.lock().await.len();

        let result: Result<(), QueueMutationError> =
            mutate(&queue, |_candidate| Err(QueueMutationError::new("validation failed"))).await;
        assert!(result.is_err(), "closure error should propagate");
        assert_eq!(queue.queue.lock().await.len(), original_len, "queue must stay unchanged");
        assert_eq!(queue.revision.load(Ordering::SeqCst), original_revision);
        assert!(!state_file.exists(), "no file should be written on closure error");
    }

    #[tokio::test]
    async fn mutate_keeps_state_when_persist_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Point the state file at a path that already exists as a directory
        // so the atomic write (open + write + rename) fails.
        let state_file = dir.path().join("blocking-dir");
        std::fs::create_dir_all(&state_file).expect("create blocking dir");
        let queue = RecordingQueue::new_with_state_file(Some(state_file));
        let original_len = queue.queue.lock().await.len();
        let original_revision = queue.revision.load(Ordering::SeqCst);

        let result: Result<(), QueueMutationError> = mutate(&queue, |_candidate| Ok(())).await;
        assert!(result.is_err(), "persist failure should propagate");
        assert!(result.unwrap_err().source_io().is_some(), "should carry io::Error");
        // State stays unchanged: the in-memory queue is intact.
        assert_eq!(queue.queue.lock().await.len(), original_len, "in-memory state must be unchanged");
        assert_eq!(queue.revision.load(Ordering::SeqCst), original_revision);
    }

    #[tokio::test]
    async fn mutate_invalid_candidate_keeps_existing_file_memory_and_revision() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads_state.json");
        let queue = RecordingQueue::new_with_state_file(Some(state_file.clone()));
        let initial = make_test_recording_task("rec-1", dir.path().join("a.ts"));
        let persisted = RecordingQueue::to_persisted(&initial);
        mutate(&queue, |candidate| {
            candidate.queue.push(persisted);
            Ok(())
        })
        .await
        .expect("initial commit");
        let original_file = std::fs::read(&state_file).expect("read initial state");

        let result: Result<(), QueueMutationError> = mutate(&queue, |candidate| {
            if let Some(download) = candidate.queue.first_mut() {
                download.url = "not a url".to_string();
            }
            Ok(())
        })
        .await;

        assert!(result.is_err());
        assert_eq!(queue.revision.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(&state_file).expect("read unchanged state"), original_file);
        assert_eq!(queue.queue.lock().await.front().map(|download| download.uuid.as_str()), Some("rec-1"));
    }

    #[tokio::test]
    async fn control_uuid_mismatch_is_noop_and_preserves_next_task() {
        let queue = RecordingQueue::new();
        *queue.active.write().await = Some(make_test_recording_task("active", PathBuf::from("/tmp/active.ts")));
        queue.queue.lock().await.push_back(make_test_recording_task("next", PathBuf::from("/tmp/next.ts")));

        assert!(!queue.pause_active("next").await.expect("uuid mismatch"));

        assert_eq!(queue.revision.load(Ordering::SeqCst), 0);
        assert_eq!(queue.active.read().await.as_ref().map(|download| download.uuid.as_str()), Some("active"));
        assert_eq!(queue.queue.lock().await.front().map(|download| download.uuid.as_str()), Some("next"));
        assert_eq!(*queue.control_signal.read().await, RecordingControl::None);
    }

    #[tokio::test]
    async fn retry_finished_after_failed_state_commits_one_revision() {
        let queue = RecordingQueue::new();
        let finished = RecordingTask {
            finished: true,
            state: RecordingTaskState::Failed,
            ..task("done", RecordingKind::Vod, RecordingTaskState::Failed)
        };
        queue.finished.write().await.push(finished);

        assert!(queue.retry_finished("done").await.expect("retry commit"));

        assert_eq!(queue.revision.load(Ordering::SeqCst), 1);
        assert!(queue.finished.read().await.is_empty());
        assert_eq!(queue.queue.lock().await.front().map(|download| download.uuid.as_str()), Some("done"));
    }

    #[tokio::test]
    async fn concurrent_writers_serialize_and_increment_each_revision() {
        let queue = Arc::new(RecordingQueue::new());
        queue.queue.lock().await.push_back(make_test_recording_task("remove", PathBuf::from("/tmp/remove.ts")));
        let finished = RecordingTask {
            finished: true,
            state: RecordingTaskState::Failed,
            ..task("retry", RecordingKind::Vod, RecordingTaskState::Failed)
        };
        queue.finished.write().await.push(finished);

        let remove_queue = Arc::clone(&queue);
        let retry_queue = Arc::clone(&queue);
        let (removed, retried) =
            tokio::join!(async move { remove_queue.remove_from_queue("remove").await }, async move {
                retry_queue.retry_finished("retry").await
            },);

        assert!(removed.expect("remove commit"));
        assert!(retried.expect("retry commit"));
        assert_eq!(queue.revision.load(Ordering::SeqCst), 2);
        assert_eq!(queue.queue.lock().await.front().map(|download| download.uuid.as_str()), Some("retry"));
        assert!(queue.finished.read().await.is_empty());
    }

    #[tokio::test]
    async fn mutate_serializes_concurrent_calls() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads_state.json");
        let queue = std::sync::Arc::new(RecordingQueue::new_with_state_file(Some(state_file)));
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(5));

        let mut handles = Vec::new();
        for _ in 0..5 {
            let q = std::sync::Arc::clone(&queue);
            let b = std::sync::Arc::clone(&barrier);
            handles.push(tokio::spawn(async move {
                b.wait().await;
                mutate(&q, |_candidate| Ok(())).await
            }));
        }
        for h in handles {
            assert!(h.await.expect("join").is_ok(), "all mutations should succeed");
        }
        let final_rev = queue.revision.load(Ordering::SeqCst);
        assert_eq!(final_rev, 5);
    }

    #[tokio::test]
    async fn committed_snapshot_waits_for_mutation_boundary() {
        let queue = std::sync::Arc::new(RecordingQueue::new());
        let task = make_test_recording_task("rec-1", PathBuf::from("/tmp/rec-1.ts"));
        queue.queue.lock().await.push_back(task);

        let mutation_guard = queue.mutation_guard.lock().await;
        let snapshot_queue = std::sync::Arc::clone(&queue);
        let snapshot = tokio::spawn(async move { snapshot_queue.committed_snapshot().await });

        tokio::task::yield_now().await;
        assert!(!snapshot.is_finished(), "snapshot must wait for the mutation boundary");

        drop(mutation_guard);
        let Ok((revision, tasks)) = snapshot.await else {
            unreachable!("snapshot task failed");
        };
        assert_eq!(revision, QueueRevision(0));
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks.first().map(|task| task.uuid.as_str()), Some("rec-1"));
    }

    #[tokio::test]
    async fn committed_partitioned_snapshot_waits_for_mutation_boundary() {
        let queue = Arc::new(RecordingQueue::new());
        let mutation_guard = queue.mutation_guard.lock().await;
        let snapshot_queue = Arc::clone(&queue);
        let snapshot = tokio::spawn(async move { snapshot_queue.committed_partitioned_snapshot().await });

        tokio::task::yield_now().await;
        assert!(!snapshot.is_finished());

        drop(mutation_guard);
        let Ok((queued, active, finished)) = snapshot.await else {
            unreachable!("snapshot task failed");
        };
        assert!(queued.is_empty());
        assert!(active.is_none());
        assert!(finished.is_empty());
    }

    #[tokio::test]
    async fn control_signal_is_ordered_inside_mutation_guard() {
        let queue = Arc::new(RecordingQueue::new());
        *queue.active.write().await = Some(make_test_transfer_task("active", PathBuf::from("/tmp/active.ts")));
        let control_lock = queue.control_signal.write().await;
        let pause_queue = Arc::clone(&queue);
        let pause = tokio::spawn(async move { pause_queue.pause_active("active").await });

        for _ in 0..100 {
            if queue.revision.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(queue.revision.load(Ordering::SeqCst), 1);
        let snapshot_queue = Arc::clone(&queue);
        let snapshot = tokio::spawn(async move { snapshot_queue.committed_snapshot().await });
        tokio::task::yield_now().await;
        assert!(!snapshot.is_finished(), "mutation guard must remain held until control publication");

        drop(control_lock);
        assert!(pause.await.is_ok_and(|result| result.is_ok()));
        assert!(snapshot.await.is_ok());
        assert_eq!(*queue.control_signal.read().await, RecordingControl::Pause);
    }

    #[tokio::test]
    async fn same_value_control_is_published_after_worker_commit_and_clear() {
        let queue = Arc::new(RecordingQueue::new());
        *queue.active.write().await = Some(make_test_recording_task("task-a", PathBuf::from("/tmp/task-a.ts")));
        queue.queue.lock().await.push_back(make_test_recording_task("task-b", PathBuf::from("/tmp/task-b.ts")));
        let mut control_lock = queue.control_signal.write().await;
        *control_lock = RecordingControl::Cancel;
        let worker_queue = Arc::clone(&queue);
        let worker_commit = tokio::spawn(async move {
            worker_queue
                .mutate_optional_and_clear_control(RecordingControl::Cancel, |candidate| {
                    let Some(mut active) = candidate.active.take() else {
                        return Ok(None);
                    };
                    active.finished = true;
                    candidate.finished.push(active);
                    if !candidate.queue.is_empty() {
                        candidate.active = Some(candidate.queue.remove(0));
                    }
                    Ok(Some(true))
                })
                .await
        });

        for _ in 0..100 {
            if queue.revision.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(queue.revision.load(Ordering::SeqCst), 1);
        let api_queue = Arc::clone(&queue);
        let newer_cancel = tokio::spawn(async move { api_queue.cancel_active_matching("task-b").await });
        tokio::task::yield_now().await;
        assert!(!newer_cancel.is_finished());

        drop(control_lock);

        assert!(worker_commit.await.expect("worker task").expect("worker commit").is_some());
        assert!(newer_cancel.await.expect("cancel task").expect("cancel commit"));
        assert_eq!(*queue.control_signal.read().await, RecordingControl::Cancel);
    }

    #[tokio::test]
    async fn mutate_swap_restores_in_memory_state_from_persisted_candidate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads_state.json");
        let queue = RecordingQueue::new_with_state_file(Some(state_file.clone()));

        let persisted = RecordingQueue::to_persisted(&make_test_recording_task("rec-1", dir.path().join("a.ts")));
        mutate(&queue, |candidate| {
            candidate.queue.push(persisted);
            Ok(())
        })
        .await
        .expect("first mutate");

        let len_after_commit = queue.queue.lock().await.len();
        assert_eq!(len_after_commit, 1, "committed task must be in memory");

        // Now mutate again to remove that task. The candidate should be
        // re-built from the just-committed state, not from the empty
        // pre-mutate in-memory state.
        mutate(&queue, |candidate| {
            candidate.queue.retain(|d| d.uuid != "rec-1");
            Ok(())
        })
        .await
        .expect("second mutate");

        assert!(queue.queue.lock().await.is_empty(), "remove must propagate to in-memory state");
        assert_eq!(queue.revision.load(Ordering::SeqCst), 2);
    }

    // --- load_from_disk rejection paths ---

    #[tokio::test]
    async fn load_from_disk_rejects_unknown_schema_version_and_leaves_state_unchanged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads_state.json");
        let payload = PersistedRecordingQueue {
            schema_version: RECORDING_QUEUE_SCHEMA_VERSION + 9,
            queue: vec![RecordingQueue::to_persisted(&task("rec-1", RecordingKind::Vod, RecordingTaskState::Queued))],
            scheduled: Vec::new(),
            active: None,
            finished: Vec::new(),
            revision: QueueRevision(7),
        };
        let serialized = serde_json::to_vec_pretty(&payload).expect("serialize");
        std::fs::write(&state_file, &serialized).expect("write state file");

        let queue = RecordingQueue::new_with_state_file(Some(state_file.clone()));
        let result = queue.load_from_disk().await;

        assert!(result.is_err(), "unknown schema_version must surface as an error");
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
        // In-memory state stays empty: rejection happens before assignment.
        assert!(queue.queue.lock().await.is_empty());
        assert!(queue.scheduled.read().await.is_empty());
        assert!(queue.finished.read().await.is_empty());
        assert!(queue.active.read().await.is_none());
        // Persisted file is not touched.
        assert_eq!(std::fs::read(&state_file).expect("read unchanged file"), serialized);
    }

    #[tokio::test]
    async fn load_from_disk_rejects_invalid_url_in_persisted_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads_state.json");
        let mut persisted =
            RecordingQueue::to_persisted(&task("rec-1", RecordingKind::Vod, RecordingTaskState::Queued));
        persisted.url = "not a url".to_string();
        let payload = PersistedRecordingQueue {
            schema_version: RECORDING_QUEUE_SCHEMA_VERSION,
            queue: vec![persisted],
            scheduled: Vec::new(),
            active: None,
            finished: Vec::new(),
            revision: QueueRevision::default(),
        };
        let serialized = serde_json::to_vec_pretty(&payload).expect("serialize");
        std::fs::write(&state_file, &serialized).expect("write state file");

        let queue = RecordingQueue::new_with_state_file(Some(state_file.clone()));
        let result = queue.load_from_disk().await;

        assert!(result.is_err(), "invalid url must surface as an error");
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
        assert!(queue.queue.lock().await.is_empty());
        assert!(queue.scheduled.read().await.is_empty());
        assert!(queue.finished.read().await.is_empty());
        assert!(queue.active.read().await.is_none());
        assert_eq!(std::fs::read(&state_file).expect("read unchanged file"), serialized);
    }

    // --- Filename rendering + collision reservation ---

    fn collect_existing_relative_paths(candidate: &PersistedRecordingQueue) -> Vec<String> {
        let mut out = Vec::new();
        for d in &candidate.queue {
            if let Some(p) = &d.recording.relative_path {
                out.push(p.clone());
            }
        }
        for d in &candidate.scheduled {
            if let Some(p) = &d.recording.relative_path {
                out.push(p.clone());
            }
        }
        if let Some(d) = &candidate.active {
            if let Some(p) = &d.recording.relative_path {
                out.push(p.clone());
            }
        }
        for d in &candidate.finished {
            if let Some(p) = &d.recording.relative_path {
                out.push(p.clone());
            }
        }
        out
    }

    /// Reserve a unique relative path for a new recording inside the
    /// queue mutation boundary. The candidate is the in-memory
    /// `PersistedRecordingQueue` the closure is building; the helper
    /// collects all already-reserved `relative_path` values, applies
    /// the supplied stem, and appends a numbered collision suffix
    /// (`_1`, `_2`, …) until the result is unique. The reserved path
    /// is also written to the recording metadata so a later collision
    /// created externally is detected by the worker at execute time.
    fn reserve_recording_relative_path(
        candidate: &mut PersistedRecordingQueue,
        stem: &str,
        recording_uuid: &str,
    ) -> String {
        // If this recording already has a reserved path (e.g., a retry
        // or an edit), keep it. Re-reserving on a re-entrant call must
        // not bump the suffix because of the caller's own previous
        // entry.
        let existing_self = find_recording_relative_path(candidate, recording_uuid);
        if let Some(prior) = existing_self {
            return prior;
        }
        let existing = collect_existing_relative_paths(candidate);
        let reserved = shared::utils::next_collision_suffix(stem, &existing);
        for d in &mut candidate.queue {
            if d.uuid == recording_uuid {
                d.recording.relative_path = Some(reserved.clone());
            }
        }
        for d in &mut candidate.scheduled {
            if d.uuid == recording_uuid {
                d.recording.relative_path = Some(reserved.clone());
            }
        }
        if let Some(d) = candidate.active.as_mut() {
            if d.uuid == recording_uuid {
                d.recording.relative_path = Some(reserved.clone());
            }
        }
        for d in &mut candidate.finished {
            if d.uuid == recording_uuid {
                d.recording.relative_path = Some(reserved.clone());
            }
        }
        reserved
    }

    fn find_recording_relative_path(candidate: &PersistedRecordingQueue, recording_uuid: &str) -> Option<String> {
        for d in &candidate.queue {
            if d.uuid == recording_uuid {
                return d.recording.relative_path.clone();
            }
        }
        for d in &candidate.scheduled {
            if d.uuid == recording_uuid {
                return d.recording.relative_path.clone();
            }
        }
        if let Some(d) = &candidate.active {
            if d.uuid == recording_uuid {
                return d.recording.relative_path.clone();
            }
        }
        for d in &candidate.finished {
            if d.uuid == recording_uuid {
                return d.recording.relative_path.clone();
            }
        }
        None
    }

    fn persisted_recording(uuid: &str, meta: RecordingMetadata) -> PersistedRecordingTask {
        PersistedRecordingTask {
            uuid: uuid.to_string(),
            kind: RecordingKind::Live,
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from(format!("/tmp/{uuid}.ts")),
            filename: format!("{uuid}.ts"),
            url: format!("https://example.com/{uuid}"),
            finished: false,
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state: RecordingTaskState::Scheduled,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: meta,
        }
    }

    #[test]
    fn reserve_recording_relative_path_returns_numbered_stem_when_no_collision() {
        let mut candidate = PersistedRecordingQueue::default();
        let reserved = reserve_recording_relative_path(&mut candidate, "pilot", "rec-1");
        assert_eq!(reserved, "pilot_1", "numbered suffix is the first candidate");
    }

    #[test]
    fn reserve_recording_relative_path_skips_existing_paths() {
        let mut candidate = PersistedRecordingQueue::default();
        let occupied = RecordingMetadata { relative_path: Some("pilot_1".to_string()), ..media_meta("web:alice") };
        candidate.queue.push(persisted_recording("other", occupied));
        let reserved = reserve_recording_relative_path(&mut candidate, "pilot", "rec-1");
        assert_eq!(reserved, "pilot_2");
    }

    #[test]
    fn reserve_recording_relative_path_does_not_double_bump_for_self() {
        let mut candidate = PersistedRecordingQueue::default();
        let existing = RecordingMetadata { relative_path: Some("pilot_3".to_string()), ..media_meta("web:alice") };
        candidate.queue.push(persisted_recording("rec-1", existing));
        let reserved = reserve_recording_relative_path(&mut candidate, "pilot", "rec-1");
        assert_eq!(reserved, "pilot_3", "must not bump because of self");
    }

    #[tokio::test]
    async fn pause_active_routes_through_mutate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads_state.json");
        let queue = RecordingQueue::new_with_state_file(Some(state_file));
        let task = make_test_transfer_task("rec-1", dir.path().join("a.ts"));
        {
            let mut active = queue.active.write().await;
            *active = Some(task);
        }
        let prior_revision = queue.revision.load(Ordering::SeqCst);
        queue.pause_active("rec-1").await.expect("pause_active");
        let active = queue.active.read().await.clone().expect("active");
        assert!(active.paused);
        assert_eq!(active.state, RecordingTaskState::Paused);
        assert!(active.next_retry_at.is_none());
        assert_eq!(queue.revision.load(Ordering::SeqCst), prior_revision + 1);
    }
}
