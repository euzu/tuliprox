use crate::{model::VideoDownloadConfig, utils::{file_exists_async, write_json_atomic}};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use shared::model::{
    FileDownloadDto, QueueRevision, RecordingMetadata, RecordingTaskDto, TaskKindDto, TaskPriorityDto, TransferStatusDto,
};
#[cfg(test)]
use shared::model::UserId;
use shared::utils::{deunicode_string, CONSTANTS, FILENAME_TRIM_PATTERNS};
use std::{
    collections::VecDeque,
    ffi::OsStr,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
    sync::{atomic::{AtomicU64, Ordering}, Arc},
};
use tokio::{fs, sync::{Mutex, Notify, RwLock}};

const RECORDING_WINDOW_EXPIRED_ERR: &str = "Recording window already expired";
static DOWNLOAD_TASK_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Reason a persisted entry cannot be converted back to its in-memory
/// form during the commit step. Surfaced to the caller so a corrupt
/// persisted file fails closed instead of silently dropping entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistedError {
    /// The persisted URL could not be parsed.
    InvalidUrl(String),
    /// A plain download claimed a recording metadata block, or a
    /// recording was missing its metadata in a way that the legacy
    /// normalizer cannot repair.
    KindMetadataInvariant { uuid: String },
}

impl std::fmt::Display for PersistedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl(s) => write!(f, "persisted url is invalid: {s}"),
            Self::KindMetadataInvariant { uuid } => write!(
                f,
                "persisted task {uuid} violates the kind/metadata invariant"
            ),
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
    /// `pub(crate)` because the only callers are download worker
    /// actions (`download_api.rs`) that wrap an inner error or carry
    /// an action label.
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }

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
/// owned `PersistedDownloadQueue` candidate cloned from the current state
/// and returns either a value or a [`QueueMutationError`]. On success the
/// candidate is persisted atomically, then swapped into the in-memory state,
/// then the `QueueRevision` is incremented. On any failure — closure error
/// or persist error — the in-memory state, the persisted file, and the
/// revision are all unchanged.
pub async fn mutate<F, R>(this: &DownloadQueue, op: F) -> Result<R, QueueMutationError>
where
    F: FnOnce(&mut PersistedDownloadQueue) -> Result<R, QueueMutationError>,
{
    match mutate_optional(this, |candidate| op(candidate).map(Some)).await? {
        Some(result) => Ok(result),
        None => Err(QueueMutationError::MutationSkipped),
    }
}

pub(in crate::api) async fn mutate_optional<F, R>(
    this: &DownloadQueue,
    op: F,
) -> Result<Option<R>, QueueMutationError>
where
    F: FnOnce(&mut PersistedDownloadQueue) -> Result<Option<R>, QueueMutationError>,
{
    let _mutation = this.mutation_guard.lock().await;
    mutate_optional_locked(this, op).await
}

async fn mutate_optional_locked<F, R>(
    this: &DownloadQueue,
    op: F,
) -> Result<Option<R>, QueueMutationError>
where
    F: FnOnce(&mut PersistedDownloadQueue) -> Result<Option<R>, QueueMutationError>,
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

    let PersistedDownloadQueue {
        queue: candidate_queue,
        scheduled: candidate_scheduled,
        active: candidate_active,
        finished: candidate_finished,
        revision: _,
    } = candidate;

    // 4. Validate every persisted entry into its in-memory form before
    // swapping. A single corrupt entry (invalid URL, kind/metadata
    // invariant violation) must abort the commit so the persisted file
    // and the in-memory state stay identical. Without this, a bad URL
    // would be silently dropped from memory while the file still
    // listed it, desyncing the two.
    let mut queue: VecDeque<FileDownload> = VecDeque::with_capacity(candidate_queue.len());
    for p in candidate_queue {
        queue.push_back(
            DownloadQueue::from_persisted(p)
                .map_err(|e| QueueMutationError::new(format!("persisted queue entry invalid: {e}")))?,
        );
    }
    let mut scheduled: Vec<FileDownload> = Vec::with_capacity(candidate_scheduled.len());
    for p in candidate_scheduled {
        scheduled.push(
            DownloadQueue::from_persisted(p)
                .map_err(|e| QueueMutationError::new(format!("persisted scheduled entry invalid: {e}")))?,
        );
    }
    let active = match candidate_active {
        Some(p) => Some(
            DownloadQueue::from_persisted(p)
                .map_err(|e| QueueMutationError::new(format!("persisted active entry invalid: {e}")))?,
        ),
        None => None,
    };
    let mut finished: Vec<FileDownload> = Vec::with_capacity(candidate_finished.len());
    for p in candidate_finished {
        finished.push(
            DownloadQueue::from_persisted(p)
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

/// Normalize a pre-DVR recording (kind == Recording, metadata == None) to
/// `LegacyAdmin` ownership, private visibility, zero padding, and a scheduled
/// interval derived from the legacy `start_at` + `duration_secs`. Derives
/// `completed_at` from a safe file mtime when the task is in a terminal
/// state and the file exists; otherwise falls back to the scheduled end.
/// Initializes `measured_bytes` from the file size when safe to do so.
/// Only sets `relative_path` when the legacy canonical path is safely
/// contained by `recording_root` or `legacy_root`.
fn normalize_legacy_recording(
    task: &mut FileDownload,
    recording_root: Option<&Path>,
    legacy_root: Option<&Path>,
) {
    let start_at = task.start_at.unwrap_or(0);
    let duration_secs = task.duration_secs.unwrap_or(0);
    let mut meta = RecordingMetadata::for_legacy_admin(start_at, duration_secs);

    if let Some(relative) = derive_legacy_relative_path(&task.file_path, recording_root, legacy_root) {
        meta.relative_path = Some(relative);
    }

    meta.completed_at = derive_legacy_completed_at(&task.state, &task.file_path, meta.scheduled_end);
    meta.measured_bytes = safe_regular_file_size(&task.file_path).unwrap_or(0);

    task.recording = Some(meta);
}

/// Derive a relative path from the legacy canonical `file_path` if it is
/// safely contained by either the new recording root or the configured
/// legacy download root. The check is a legacy string prefix comparison;
/// new recording paths use stricter containment helpers.
fn derive_legacy_relative_path(
    file_path: &Path,
    recording_root: Option<&Path>,
    legacy_root: Option<&Path>,
) -> Option<String> {
    if let Some(root) = recording_root {
        if path_is_contained(file_path, root) {
            return strip_prefix(file_path, root);
        }
    }
    if let Some(root) = legacy_root {
        if path_is_contained(file_path, root) {
            return strip_prefix(file_path, root);
        }
    }
    None
}

fn path_is_contained(path: &Path, root: &Path) -> bool {
    if root.as_os_str().is_empty() {
        return false;
    }
    // `Path::starts_with` compares component-by-component, so
    // `/data/rec` is correctly rejected as a prefix of `/data/records`.
    path.starts_with(root)
}

fn strip_prefix(path: &Path, root: &Path) -> Option<String> {
    if root.as_os_str().is_empty() {
        return None;
    }
    // `Path::strip_prefix` enforces the component-level boundary, but
    // `..` is still a valid relative-path component: a legacy path
    // like `<root>/../etc/passwd` strips cleanly to `../etc/passwd`,
    // and a downstream `join(root)` would resolve back outside the
    // root. Reject anything other than `Component::Normal` so a
    // traversal in the stored path cannot escape the recording root.
    let stripped = path.strip_prefix(root).ok()?;
    if !stripped
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        return None;
    }
    Some(path_to_unix_string(stripped))
}

/// Render a path with `/` as the separator regardless of platform. The
/// callers of `derive_legacy_relative_path` store the result as a
/// portable relative-path string.
fn path_to_unix_string(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn derive_legacy_completed_at(
    state: &DownloadState,
    file_path: &Path,
    fallback_scheduled_end: Option<i64>,
) -> Option<i64> {
    let safe_mtime = safe_regular_file_mtime(file_path);
    let terminal = matches!(state, DownloadState::Completed | DownloadState::Failed | DownloadState::Cancelled);
    if terminal {
        if let Some(mtime) = safe_mtime {
            return Some(mtime);
        }
    }
    fallback_scheduled_end
}

fn safe_regular_file_mtime(path: &Path) -> Option<i64> {
    let Ok(meta) = std::fs::metadata(path) else {
        return None;
    };
    if !meta.is_file() {
        return None;
    }
    meta.modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn safe_regular_file_size(path: &Path) -> Option<u64> {
    let Ok(meta) = std::fs::metadata(path) else {
        return None;
    };
    if !meta.is_file() {
        return None;
    }
    Some(meta.len())
}

/// Pre-scan a persisted queue state file for `RecordingOwner::User` IDs
/// without requiring the identity registry to be loaded. Returns the
/// unique set of user IDs found across all `recording` blocks.
#[cfg(test)]
pub fn pre_scan_recording_user_ids(path: &Path) -> Vec<UserId> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(queue) = serde_json::from_slice::<PersistedDownloadQueue>(&bytes) else {
        return Vec::new();
    };
    let mut found = std::collections::HashSet::new();
    for task in queue
        .queue
        .iter()
        .chain(queue.scheduled.iter())
        .chain(queue.active.iter())
        .chain(queue.finished.iter())
    {
        if let Some(meta) = &task.recording {
            if let shared::model::RecordingOwner::User(uid) = &meta.owner {
                found.insert(uid.clone());
            }
        }
    }
    let mut sorted: Vec<UserId> = found.into_iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    sorted
}

/// File-Download information.
#[derive(Clone, Debug)]
pub struct FileDownload {
    /// uuid of the download for identification.
    pub uuid: String,
    /// `file_dir` is the directory where the download should be placed.
    pub file_dir: PathBuf,
    /// `file_path` is the complete path including the filename.
    pub file_path: PathBuf,
    /// filename is the filename.
    pub filename: String,
    /// url is the download url.
    pub url: reqwest::Url,
    /// finished is true, if download is finished, otherweise false
    pub finished: bool,
    /// the filesize.
    pub size: u64,
    /// total size in bytes (from Content-Length header)
    pub total_size: Option<u64>,
    /// paused state
    pub paused: bool,
    /// Optional error if something goes wrong during downloading.
    pub error: Option<String>,
    /// Download state
    pub state: DownloadState,
    /// Scheduled recording start timestamp.
    pub start_at: Option<i64>,
    /// Scheduled recording duration in seconds.
    pub duration_secs: Option<u64>,
    /// Distinguishes plain downloads from scheduled recordings.
    pub kind: DownloadKind,
    /// The input source name used to acquire a provider connection.
    pub input_name: Option<Arc<str>>,
    /// Priority for provider connection preemption (lower = higher priority).
    pub priority: i8,
    /// Consecutive retry attempts for transient failures.
    pub retry_attempts: u8,
    /// Unix timestamp of the next retry attempt while waiting.
    pub next_retry_at: Option<i64>,
    /// DVR recording metadata. `Some` iff the task is a recording after
    /// legacy normalization; `None` for plain downloads. The kind/metadata
    /// invariant is enforced in `FileDownload::new` and `new_recording`.
    pub recording: Option<RecordingMetadata>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum DownloadKind {
    #[default]
    Download,
    Recording,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistedFileDownload {
    pub uuid: String,
    pub file_dir: PathBuf,
    pub file_path: PathBuf,
    pub filename: String,
    pub url: String,
    pub finished: bool,
    pub size: u64,
    pub total_size: Option<u64>,
    pub paused: bool,
    pub error: Option<String>,
    pub state: DownloadState,
    pub start_at: Option<i64>,
    pub duration_secs: Option<u64>,
    pub kind: DownloadKind,
    #[serde(default)]
    pub input_name: Option<String>,
    #[serde(default)]
    pub priority: i8,
    #[serde(default)]
    pub retry_attempts: u8,
    #[serde(default)]
    pub next_retry_at: Option<i64>,
    /// DVR recording metadata. `Some` iff the task is a recording. Missing
    /// older payloads deserialize to `None` and are normalized on load.
    #[serde(default)]
    pub recording: Option<RecordingMetadata>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PersistedDownloadQueue {
    pub queue: Vec<PersistedFileDownload>,
    pub scheduled: Vec<PersistedFileDownload>,
    pub active: Option<PersistedFileDownload>,
    pub finished: Vec<PersistedFileDownload>,
    /// Monotonic revision. Increments once per committed queue mutation.
    /// Defaults to 0 on first read; the in-memory `DownloadQueue` mirrors
    /// this counter via an `AtomicU64`.
    #[serde(default)]
    pub revision: QueueRevision,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum DownloadState {
    #[default]
    Queued,
    Scheduled,
    WaitingForCapacity,
    RetryWaiting,
    Downloading,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DownloadControl {
    #[default]
    None,
    Pause,
    Cancel,
    Restart,
}

/// Returns the directory for th file download.
/// if option `organize_into_directories` is set, the root directory is determined.
/// - For series, the episode pattern is used to determine the sub directory for the series.
/// - For vod files, the title is used to determine the sub directory.
///
/// # Arguments
/// * `download_cfg` the download configuration
/// * `filestem` the prepared filestem to use as sub directory
///
fn get_download_directory(download_cfg: &VideoDownloadConfig, filestem: &str) -> PathBuf {
    if download_cfg.organize_into_directories {
        let mut stem = filestem;
        if let Some(re) = &download_cfg.episode_pattern {
            if let Some(captures) = re.captures(stem) {
                if let Some(episode) = captures.name("episode") {
                    if !episode.as_str().is_empty() {
                        stem = &stem[..episode.start()];
                    }
                }
            }
        }
        let dir_name = CONSTANTS.re_remove_filename_ending.replace(stem, "");
        let file_dir: PathBuf = [download_cfg.directory.as_str(), dir_name.as_ref()].iter().collect();
        file_dir
    } else {
        PathBuf::from(download_cfg.directory.as_str())
    }
}

fn generate_download_task_id() -> String {
    let now_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let counter = DOWNLOAD_TASK_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{now_nanos:032x}{counter:016x}")
}

impl FileDownload {
    // TODO read header size info  and restart support
    // "content-type" => ".../..."
    // "content-length" => "1975828544"
    // "accept-ranges" => "0-1975828544"
    // "content-range" => "bytes 0-1975828543/1975828544"

    pub fn new(req_url: &str, req_filename: &str, download_cfg: &VideoDownloadConfig, input_name: Option<Arc<str>>, priority: i8) -> Option<Self> {
        match reqwest::Url::parse(req_url) {
            Ok(url) => {
                let tmp_filename = CONSTANTS
                    .re_filename
                    .replace_all(&deunicode_string(req_filename).replace(' ', "_"), "")
                    .replace("__", "_")
                    .replace("_-_", "-");
                let filename_path = Path::new(&tmp_filename);
                let file_stem = filename_path
                    .file_stem()
                    .and_then(OsStr::to_str)
                    .unwrap_or("")
                    .trim_matches(FILENAME_TRIM_PATTERNS);
                let file_ext = filename_path.extension().and_then(OsStr::to_str).unwrap_or("");

                let mut filename = if file_ext.is_empty() {
                    file_stem.to_string()
                } else {
                    format!("{file_stem}.{file_ext}")
                };
                let file_dir = get_download_directory(download_cfg, file_stem);
                let mut file_path: PathBuf = file_dir.clone();
                file_path.push(&filename);
                let mut x: usize = 1;
                while file_path.is_file() {
                    filename = if file_ext.is_empty() {
                        format!("{file_stem}_{x}")
                    } else {
                        format!("{file_stem}_{x}.{file_ext}")
                    };
                    file_path.clone_from(&file_dir);
                    file_path.push(&filename);
                    x += 1;
                }

                file_path.to_str()?;

                Some(Self {
                    uuid: generate_download_task_id(),
                    file_dir,
                    file_path,
                    filename,
                    url,
                    finished: false,
                    size: 0,
                    total_size: None,
                    paused: false,
                    error: None,
                    state: DownloadState::Queued,
                    start_at: None,
                    duration_secs: None,
                    kind: DownloadKind::Download,
                    input_name,
                    priority,
                    retry_attempts: 0,
                    next_retry_at: None,
                    recording: None,
                })
            }
            Err(_) => None,
        }
    }

    pub fn new_recording(
        req_url: &str,
        req_filename: &str,
        download_cfg: &VideoDownloadConfig,
        start_at: i64,
        duration_secs: u64,
        input_name: Option<Arc<str>>,
        priority: i8,
    ) -> Option<Self> {
        let mut recording = Self::new(req_url, req_filename, download_cfg, input_name, priority)?;
        recording.state = DownloadState::Scheduled;
        recording.start_at = Some(start_at);
        recording.duration_secs = Some(duration_secs);
        recording.kind = DownloadKind::Recording;
        // Legacy records do not carry a server-owned source identifier. The
        // kind/metadata invariant is upheld because every new recording
        // receives `RecordingMetadata`.
        recording.recording = Some(RecordingMetadata::for_legacy_admin(start_at, duration_secs));
        Some(recording)
    }

    /// Invariant: `kind == Recording` iff `recording.is_some()`. Plain
    /// downloads never carry metadata. The mirrored persisted form enforces
    /// the same check in `from_persisted`.
    pub fn kind_metadata_invariant_ok(&self) -> bool {
        match self.kind {
            DownloadKind::Download => self.recording.is_none(),
            DownloadKind::Recording => self.recording.is_some(),
        }
    }
}

impl FileDownload {
    fn matches_existing_task(&self, other: &Self) -> bool {
        if self.kind != other.kind {
            return false;
        }

        match self.kind {
            DownloadKind::Download => self.url == other.url || self.file_path == other.file_path,
            DownloadKind::Recording => {
                (self.url == other.url && self.start_at == other.start_at && self.duration_secs == other.duration_secs)
                    || self.file_path == other.file_path
            }
        }
    }
}

impl From<&FileDownload> for FileDownloadDto {
    fn from(value: &FileDownload) -> Self {
        Self {
            id: value.uuid.clone(),
            title: value.filename.clone(),
            kind: match value.kind {
                DownloadKind::Download => TaskKindDto::Download,
                DownloadKind::Recording => TaskKindDto::Recording,
            },
            priority: match value.priority.cmp(&0) {
                std::cmp::Ordering::Less => TaskPriorityDto::High,
                std::cmp::Ordering::Equal => TaskPriorityDto::Normal,
                std::cmp::Ordering::Greater => TaskPriorityDto::Background,
            },
            status: match value.state {
                DownloadState::Queued => TransferStatusDto::Queued,
                DownloadState::Scheduled => TransferStatusDto::Scheduled,
                DownloadState::WaitingForCapacity => TransferStatusDto::WaitingForCapacity,
                DownloadState::RetryWaiting => TransferStatusDto::RetryWaiting,
                DownloadState::Downloading => TransferStatusDto::Running,
                DownloadState::Paused => TransferStatusDto::Paused,
                DownloadState::Completed => TransferStatusDto::Completed,
                DownloadState::Failed => TransferStatusDto::Failed,
                DownloadState::Cancelled => TransferStatusDto::Cancelled,
            },
            downloaded_bytes: value.size,
            retry_attempts: value.retry_attempts,
            total_bytes: value.total_size,
            next_retry_at: value.next_retry_at,
            scheduled_start_at: value.start_at,
            duration_secs: value.duration_secs,
            error: value.error.clone(),
            recording: value.recording.as_ref().map(RecordingTaskDto::from_metadata),
        }
    }
}

impl From<FileDownload> for FileDownloadDto {
    fn from(value: FileDownload) -> Self { Self::from(&value) }
}

/// Priority-aware wait queue for download connection slots.
/// When the provider is at capacity, download tasks register here and are
/// woken one-at-a-time in descending priority order (lowest i8 = highest priority).
struct DownloadWaiter {
    id: u64,
    input_name: Option<Arc<str>>,
    priority: i8,
    notify: Arc<Notify>,
}

type DownloadWaiters = Arc<Mutex<Vec<DownloadWaiter>>>;

#[derive(Clone)]
pub struct DownloadWaiterSnapshot {
    pub id: u64,
    pub input_name: Option<Arc<str>>,
    pub priority: i8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadWaitOutcome {
    Signalled,
    Paused,
    Cancelled,
    Restarted,
}

pub struct DownloadSlotWaitQueue {
    waiters: DownloadWaiters,
    next_waiter_id: AtomicU64,
}

impl DownloadSlotWaitQueue {
    pub fn new() -> Self {
        Self {
            waiters: Arc::new(Mutex::new(Vec::new())),
            next_waiter_id: AtomicU64::new(1),
        }
    }

    async fn remove_waiter(&self, waiter_id: u64) {
        self.waiters.lock().await.retain(|waiter| waiter.id != waiter_id);
    }

    /// Register and block until this task is signalled or control flow requests pause/cancel.
    pub async fn wait(
        &self,
        input_name: Option<Arc<str>>,
        priority: i8,
        control_signal: &RwLock<DownloadControl>,
        control_notify: &Notify,
    ) -> DownloadWaitOutcome {
        let waiter_id = self.next_waiter_id.fetch_add(1, Ordering::Relaxed);
        let notify = Arc::new(Notify::new());
        self.waiters.lock().await.push(DownloadWaiter {
            id: waiter_id,
            input_name,
            priority,
            notify: Arc::clone(&notify),
        });

        match *control_signal.read().await {
            DownloadControl::Pause => {
                self.remove_waiter(waiter_id).await;
                return DownloadWaitOutcome::Paused;
            }
            DownloadControl::Cancel => {
                self.remove_waiter(waiter_id).await;
                return DownloadWaitOutcome::Cancelled;
            }
            DownloadControl::Restart => {
                self.remove_waiter(waiter_id).await;
                return DownloadWaitOutcome::Restarted;
            }
            DownloadControl::None => {}
        }

        loop {
            tokio::select! {
                () = notify.notified() => return DownloadWaitOutcome::Signalled,
                () = control_notify.notified() => {
                    match *control_signal.read().await {
                        DownloadControl::Pause => {
                            self.remove_waiter(waiter_id).await;
                            return DownloadWaitOutcome::Paused;
                        }
                        DownloadControl::Cancel => {
                            self.remove_waiter(waiter_id).await;
                            return DownloadWaitOutcome::Cancelled;
                        }
                        DownloadControl::Restart => {
                            self.remove_waiter(waiter_id).await;
                            return DownloadWaitOutcome::Restarted;
                        }
                        DownloadControl::None => {}
                    }
                }
            }
        }
    }

    pub async fn snapshots(&self) -> Vec<DownloadWaiterSnapshot> {
        self.waiters
            .lock()
            .await
            .iter()
            .map(|waiter| DownloadWaiterSnapshot {
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

pub struct DownloadQueue {
    pub queue: Arc<Mutex<VecDeque<FileDownload>>>,
    pub scheduled: Arc<RwLock<Vec<FileDownload>>>,
    pub active: Arc<RwLock<Option<FileDownload>>>,
    pub finished: Arc<RwLock<Vec<FileDownload>>>,
    pub control_signal: Arc<RwLock<DownloadControl>>,
    pub control_notify: Arc<Notify>,
    pub worker_running: Arc<RwLock<bool>>,
    pub state_file: Option<PathBuf>,
    /// Priority-aware waiter queue for provider connection slots.
    pub slot_waiters: Arc<DownloadSlotWaitQueue>,
    /// In-memory mirror of the persisted queue revision. Incremented
    /// once per committed mutation.
    pub revision: Arc<AtomicU64>,
    mutation_guard: Arc<Mutex<()>>,
}

impl Default for DownloadQueue {
    fn default() -> Self { Self::new() }
}

impl DownloadQueue {
    pub(in crate::api) async fn mutate_optional_and_clear_control<F, R>(
        &self,
        expected: DownloadControl,
        op: F,
    ) -> Result<Option<R>, QueueMutationError>
    where
        F: FnOnce(&mut PersistedDownloadQueue) -> Result<Option<R>, QueueMutationError>,
    {
        let _mutation = self.mutation_guard.lock().await;
        let result = mutate_optional_locked(self, op).await?;
        if result.is_some() {
            let mut control = self.control_signal.write().await;
            if *control == expected {
                *control = DownloadControl::None;
            }
        }
        Ok(result)
    }

    async fn snapshot_current(&self, revision: QueueRevision) -> PersistedDownloadQueue {
        let queue = self.queue.lock().await;
        let scheduled = self.scheduled.read().await;
        let active = self.active.read().await;
        let finished = self.finished.read().await;

        PersistedDownloadQueue {
            queue: queue.iter().map(Self::to_persisted).collect(),
            scheduled: scheduled.iter().map(Self::to_persisted).collect(),
            active: active.as_ref().map(Self::to_persisted),
            finished: finished.iter().map(Self::to_persisted).collect(),
            revision,
        }
    }

    pub async fn committed_snapshot(&self) -> (QueueRevision, Vec<FileDownload>) {
        let _mutation = self.mutation_guard.lock().await;
        let revision = QueueRevision(self.revision.load(Ordering::SeqCst));
        let queue = self.queue.lock().await;
        let scheduled = self.scheduled.read().await;
        let active = self.active.read().await;
        let finished = self.finished.read().await;
        let mut tasks = Vec::with_capacity(
            queue.len() + scheduled.len() + finished.len() + usize::from(active.is_some()),
        );
        tasks.extend(queue.iter().cloned());
        tasks.extend(scheduled.iter().cloned());
        tasks.extend(active.iter().cloned());
        tasks.extend(finished.iter().cloned());
        (revision, tasks)
    }

    pub async fn committed_download_snapshot(
        &self,
    ) -> (Vec<FileDownload>, Option<FileDownload>, Vec<FileDownload>) {
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

    fn finalize_missed_recording(mut download: FileDownload) -> FileDownload {
        download.finished = true;
        download.paused = false;
        download.state = DownloadState::Failed;
        download.error = Some(RECORDING_WINDOW_EXPIRED_ERR.to_string());
        download
    }

    fn recording_start_missed_window(download: &FileDownload, now_ts: i64) -> bool {
        download.kind == DownloadKind::Recording
            && download
                .start_at
                .zip(download.duration_secs)
                .is_some_and(|(start_at, duration_secs)| {
                    crate::api::model::recording::recording_math::window_elapsed(
                        start_at,
                        duration_secs,
                        now_ts,
                    )
                })
    }

    pub fn new() -> Self {
        Self::new_with_state_file(None)
    }

    pub fn new_with_state_file(state_file: Option<PathBuf>) -> Self {
        Self {
            queue: Arc::from(Mutex::new(VecDeque::new())),
            scheduled: Arc::from(RwLock::new(Vec::new())),
            active: Arc::from(RwLock::new(None)),
            finished: Arc::from(RwLock::new(Vec::new())),
            control_signal: Arc::from(RwLock::new(DownloadControl::None)),
            control_notify: Arc::new(Notify::new()),
            worker_running: Arc::from(RwLock::new(false)),
            state_file,
            slot_waiters: Arc::new(DownloadSlotWaitQueue::new()),
            revision: Arc::new(AtomicU64::new(0)),
            mutation_guard: Arc::new(Mutex::new(())),
        }
    }

    pub fn to_persisted(download: &FileDownload) -> PersistedFileDownload {
        PersistedFileDownload {
            uuid: download.uuid.clone(),
            file_dir: download.file_dir.clone(),
            file_path: download.file_path.clone(),
            filename: download.filename.clone(),
            url: download.url.to_string(),
            finished: download.finished,
            size: download.size,
            total_size: download.total_size,
            paused: download.paused,
            error: download.error.clone(),
            state: download.state.clone(),
            start_at: download.start_at,
            duration_secs: download.duration_secs,
            kind: download.kind.clone(),
            input_name: download.input_name.as_ref().map(std::string::ToString::to_string),
            priority: download.priority,
            retry_attempts: download.retry_attempts,
            next_retry_at: download.next_retry_at,
            recording: download.recording.clone(),
        }
    }

    pub fn from_persisted(download: PersistedFileDownload) -> Result<FileDownload, PersistedError> {
        Self::from_persisted_with(download, None, None)
    }

    /// Reconstruct a `FileDownload` from its persisted form. When the task
    /// is a recording without nested metadata (the pre-DVR shape), the
    /// legacy pre-scan normalizes it: `LegacyAdmin` owner, private visibility,
    /// zero padding, scheduled interval from `start_at` + `duration_secs`,
    /// `completed_at` from a safe file mtime or the scheduled end, and a
    /// `relative_path` only if the legacy file path is safely contained
    /// under `recording_root` or `legacy_root`.
    pub fn from_persisted_with(
        download: PersistedFileDownload,
        recording_root: Option<&Path>,
        legacy_root: Option<&Path>,
    ) -> Result<FileDownload, PersistedError> {
        let url = reqwest::Url::parse(&download.url).map_err(|e| PersistedError::InvalidUrl(e.to_string()))?;
        let recording = download.recording.clone();
        let kind = download.kind.clone();
        // Kind/metadata invariant check. New recordings always carry
        // `recording`; plain downloads never do. Legacy records (Recording
        // without metadata) are normalized below.
        if matches!(kind, DownloadKind::Download) && recording.is_some() {
            return Err(PersistedError::KindMetadataInvariant {
                uuid: download.uuid.clone(),
            });
        }
        let mut task = FileDownload {
            uuid: download.uuid,
            file_dir: download.file_dir,
            file_path: download.file_path,
            filename: download.filename,
            url,
            finished: download.finished,
            size: download.size,
            total_size: download.total_size,
            paused: download.paused,
            error: download.error,
            state: download.state,
            start_at: download.start_at,
            duration_secs: download.duration_secs,
            kind,
            input_name: download.input_name.map(|s| Arc::from(s.as_str())),
            priority: download.priority,
            retry_attempts: download.retry_attempts,
            next_retry_at: download.next_retry_at,
            recording,
        };
        if matches!(task.kind, DownloadKind::Recording) && task.recording.is_none() {
            normalize_legacy_recording(&mut task, recording_root, legacy_root);
        }
        Ok(task)
    }

    pub async fn persist_to_disk(&self) -> std::io::Result<()> {
        let Some(state_file) = self.state_file.as_ref() else {
            return Ok(());
        };

        let queue = self.queue.lock().await.iter().map(Self::to_persisted).collect::<Vec<_>>();
        let scheduled = self.scheduled.read().await.iter().map(Self::to_persisted).collect::<Vec<_>>();
        let active = self.active.read().await.as_ref().map(Self::to_persisted);
        let finished = self.finished.read().await.iter().map(Self::to_persisted).collect::<Vec<_>>();
        let revision = self.revision.load(Ordering::SeqCst);
        let payload = PersistedDownloadQueue {
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

    pub async fn load_from_disk(&self) -> std::io::Result<()> {
        let Some(state_file) = self.state_file.as_ref() else {
            return Ok(());
        };
        if !file_exists_async(state_file).await {
            return Ok(());
        }

        let content = fs::read_to_string(state_file).await?;
        let persisted: PersistedDownloadQueue =
            serde_json::from_str(&content).map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;

        let queue = persisted
            .queue
            .into_iter()
            .filter_map(|p| Self::from_persisted(p).ok())
            .map(Self::recover_loaded_download)
            .collect::<VecDeque<_>>();
        let now_ts = Utc::now().timestamp();
        let scheduled_loaded = persisted
            .scheduled
            .into_iter()
            .filter_map(|p| Self::from_persisted(p).ok())
            .map(Self::recover_loaded_download)
            .collect::<Vec<_>>();
        let (scheduled, missed_scheduled): (Vec<_>, Vec<_>) = scheduled_loaded
            .into_iter()
            .partition(|download| !Self::recording_start_missed_window(download, now_ts));
        let active = persisted
            .active
            .and_then(|p| Self::from_persisted(p).ok())
            .map(Self::recover_loaded_download);
        let mut finished =
            persisted.finished.into_iter().filter_map(|p| Self::from_persisted(p).ok()).collect::<Vec<_>>();
        finished.extend(missed_scheduled.into_iter().map(Self::finalize_missed_recording));

        *self.queue.lock().await = queue;
        *self.scheduled.write().await = scheduled;
        *self.finished.write().await = finished;
        self.revision.store(persisted.revision.0, Ordering::SeqCst);
        if let Some(active) = active {
            if active.paused || active.state == DownloadState::Paused {
                *self.active.write().await = Some(active);
            } else if !active.finished && active.state != DownloadState::Cancelled {
                self.queue.lock().await.push_front(active);
                *self.active.write().await = None;
            } else {
                self.finished.write().await.push(active);
                *self.active.write().await = None;
            }
        } else {
            *self.active.write().await = None;
        }
        *self.control_signal.write().await = DownloadControl::None;
        *self.worker_running.write().await = false;
        Ok(())
    }

    fn recover_loaded_download(mut download: FileDownload) -> FileDownload {
        if download.paused || download.state == DownloadState::Paused {
            download.paused = true;
            download.finished = false;
            download.state = DownloadState::Paused;
            return download;
        }
        if download.state == DownloadState::Scheduled {
            download.paused = false;
            download.finished = false;
            return download;
        }
        if !download.finished {
            download.paused = false;
            download.state = DownloadState::Queued;
            download.error = None;
            download.retry_attempts = 0;
            download.next_retry_at = None;
        }
        download
    }

    pub async fn find_duplicate(&self, candidate: &FileDownload) -> Option<FileDownload> {
        if let Some(active) = self.active.read().await.as_ref() {
            if active.matches_existing_task(candidate) {
                return Some(active.clone());
            }
        }

        if let Some(queued) = self
            .queue
            .lock()
            .await
            .iter()
            .find(|download| download.matches_existing_task(candidate))
            .cloned()
        {
            return Some(queued);
        }

        if let Some(scheduled) = self
            .scheduled
            .read()
            .await
            .iter()
            .find(|download| download.matches_existing_task(candidate))
            .cloned()
        {
            return Some(scheduled);
        }

        self.finished
            .read()
            .await
            .iter()
            .find(|download| download.matches_existing_task(candidate))
            .cloned()
    }

    /// Pause the active download. Persists the new state through the
    /// transactional boundary. The runtime-only control signal is published
    /// after the commit while the mutation guard still preserves ordering.
    pub async fn pause_active(&self, uuid: &str) -> Result<bool, QueueMutationError> {
        let _mutation = self.mutation_guard.lock().await;
        let changed = mutate_optional_locked(self, |candidate| {
            let Some(active) = candidate.active.as_mut().filter(|active| active.uuid == uuid) else {
                return Ok(None);
            };
            active.paused = true;
            active.state = DownloadState::Paused;
            active.next_retry_at = None;
            Ok(Some(true))
        })
        .await?
        .unwrap_or(false);
        if !changed {
            return Ok(false);
        }
        *self.control_signal.write().await = DownloadControl::Pause;
        self.control_notify.notify_waiters();
        Ok(true)
    }

    /// Resume the active download. Persists the new state through the
    /// transactional boundary.
    pub async fn resume_active(&self, uuid: &str) -> Result<bool, QueueMutationError> {
        let _mutation = self.mutation_guard.lock().await;
        let changed = mutate_optional_locked(self, |candidate| {
            let Some(active) = candidate
                .active
                .as_mut()
                .filter(|active| active.uuid == uuid && active.paused)
            else {
                return Ok(None);
            };
            active.paused = false;
            active.state = DownloadState::Downloading;
            active.next_retry_at = None;
            Ok(Some(true))
        })
        .await?
        .unwrap_or(false);
        if !changed {
            return Ok(false);
        }
        *self.control_signal.write().await = DownloadControl::None;
        self.control_notify.notify_waiters();
        Ok(true)
    }

    /// Cancel the active download. Persists the new state through the
    /// transactional boundary.
    pub async fn cancel_active_matching(&self, uuid: &str) -> Result<bool, QueueMutationError> {
        let _mutation = self.mutation_guard.lock().await;
        let changed = mutate_optional_locked(self, |candidate| {
            let Some(active) = candidate.active.as_mut().filter(|active| active.uuid == uuid) else {
                return Ok(None);
            };
            active.state = DownloadState::Cancelled;
            active.error = Some("Cancelled by user".to_string());
            active.next_retry_at = None;
            Ok(Some(true))
        })
        .await?
        .unwrap_or(false);
        if !changed {
            return Ok(false);
        }
        *self.control_signal.write().await = DownloadControl::Cancel;
        self.control_notify.notify_waiters();
        Ok(true)
    }

    pub async fn cancel_active(&self) -> Result<bool, QueueMutationError> {
        let Some(uuid) = self.active.read().await.as_ref().map(|active| active.uuid.clone()) else {
            return Ok(false);
        };
        self.cancel_active_matching(&uuid).await
    }

    pub(in crate::api) async fn cancel_requested(&self, uuid: &str) -> Result<Option<bool>, QueueMutationError> {
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
                cancelled.state = DownloadState::Cancelled;
                candidate.finished.push(cancelled);
                if !candidate.queue.is_empty() {
                    candidate.active = Some(candidate.queue.remove(0));
                }
            } else if let Some(active) = candidate.active.as_mut() {
                active.state = DownloadState::Cancelled;
                active.error = Some("Cancelled by user".to_string());
                active.next_retry_at = None;
            }
            Ok(Some(was_paused))
        })
        .await?;

        if let Some(was_paused) = was_paused {
            *self.control_signal.write().await = if was_paused {
                DownloadControl::None
            } else {
                DownloadControl::Cancel
            };
            self.control_notify.notify_waiters();
        }
        Ok(was_paused)
    }

    pub fn request_worker_restart(&self) {
        if let Ok(mut control) = self.control_signal.try_write() {
            *control = DownloadControl::Restart;
            self.control_notify.notify_waiters();
            return;
        }
        let control_signal = Arc::clone(&self.control_signal);
        let control_notify = Arc::clone(&self.control_notify);
        tokio::spawn(async move {
            *control_signal.write().await = DownloadControl::Restart;
            control_notify.notify_waiters();
        });
    }

    pub async fn remove_from_queue(&self, uuid: &str) -> Result<bool, QueueMutationError> {
        Ok(mutate_optional(self, |candidate| {
            let queue_len = candidate.queue.len();
            candidate.queue.retain(|download| download.uuid != uuid);
            if candidate.queue.len() != queue_len {
                return Ok(Some(true));
            }
            let scheduled_len = candidate.scheduled.len();
            candidate.scheduled.retain(|download| download.uuid != uuid);
            Ok((candidate.scheduled.len() != scheduled_len).then_some(true))
        })
        .await?
        .unwrap_or(false))
    }

    pub async fn remove_finished(&self, uuid: &str) -> Result<bool, QueueMutationError> {
        Ok(mutate_optional(self, |candidate| {
            let initial_len = candidate.finished.len();
            candidate.finished.retain(|download| download.uuid != uuid);
            Ok((candidate.finished.len() != initial_len).then_some(true))
        })
        .await?
        .unwrap_or(false))
    }

    pub async fn remove(&self, uuid: &str) -> Result<bool, QueueMutationError> {
        Ok(mutate_optional(self, |candidate| {
            let original_len = candidate.queue.len() + candidate.scheduled.len() + candidate.finished.len();
            candidate.queue.retain(|download| download.uuid != uuid);
            candidate.scheduled.retain(|download| download.uuid != uuid);
            candidate.finished.retain(|download| download.uuid != uuid);
            let current_len = candidate.queue.len() + candidate.scheduled.len() + candidate.finished.len();
            Ok((current_len != original_len).then_some(true))
        })
        .await?
        .unwrap_or(false))
    }

    pub async fn retry_finished(&self, uuid: &str) -> Result<bool, QueueMutationError> {
        Ok(mutate_optional(self, |candidate| {
            if let Some(pos) = candidate.finished.iter().position(|download| download.uuid == uuid) {
                let mut download = candidate.finished.remove(pos);
            if download.kind == DownloadKind::Recording {
                    candidate.finished.insert(pos, download);
                    return Ok(None);
            }
            download.finished = false;
            download.size = 0;
            download.paused = false;
            download.error = None;
            download.state = DownloadState::Queued;
            download.retry_attempts = 0;
            download.next_retry_at = None;
                candidate.queue.push(download);
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
            let mut due_downloads = Vec::new();
            let mut missed_recordings = Vec::new();
            candidate.scheduled.retain(|download| {
                let is_missed = download.kind == DownloadKind::Recording
                    && download
                        .start_at
                        .zip(download.duration_secs)
                        .is_some_and(|(start_at, duration_secs)| {
                            now_ts
                                >= start_at.saturating_add(
                                    i64::try_from(duration_secs).unwrap_or(i64::MAX),
                                )
                        });
                if is_missed {
                    let mut missed = download.clone();
                    missed.finished = true;
                    missed.paused = false;
                    missed.state = DownloadState::Failed;
                    missed.error = Some(RECORDING_WINDOW_EXPIRED_ERR.to_string());
                    missed_recordings.push(missed);
                    return false;
                }
                let is_due = download.start_at.is_some_and(|start_at| start_at <= now_ts);
                if is_due {
                    let mut queued = download.clone();
                    queued.state = DownloadState::Queued;
                    queued.paused = false;
                    queued.finished = false;
                    queued.error = None;
                    queued.size = 0;
                    queued.total_size = None;
                    queued.retry_attempts = 0;
                    queued.next_retry_at = None;
                    due_downloads.push(queued);
                }
                !is_due
            });

            if due_downloads.is_empty() && missed_recordings.is_empty() {
                return Ok(None);
            }

            let due_count = due_downloads.len();
            let missed_count = missed_recordings.len();
            candidate.finished.extend(missed_recordings);
            candidate.queue.splice(0..0, due_downloads);
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

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct FileDownloadRequest {
    pub url: String,
    pub filename: String,
    #[serde(default)]
    pub input_name: Option<String>,
    #[serde(default)]
    pub priority: Option<i8>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct FileRecordingRequest {
    pub url: String,
    pub filename: String,
    pub start_at: i64,
    pub duration_secs: u64,
    #[serde(default)]
    pub input_name: Option<String>,
    #[serde(default)]
    pub priority: Option<i8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::time::{timeout, Duration};

    fn temp_state_file(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        std::env::temp_dir().join(format!("tuliprox_{name}_{nanos}.json"))
    }

    #[tokio::test]
    async fn pause_and_resume_keep_active_download_resumable() {
        let queue = DownloadQueue::new();
        let active = FileDownload {
            uuid: "id".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/file.mp4"),
            filename: "file.mp4".to_string(),
            url: reqwest::Url::parse("https://example.com/file.mp4").expect("valid url"),
            finished: false,
            size: 42,
            total_size: Some(100),
            paused: false,
            error: None,
            state: DownloadState::Downloading,
            start_at: None,
            duration_secs: None,
            kind: DownloadKind::Download,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: None,
        };

        *queue.active.write().await = Some(active);
        queue.pause_active("id").await.expect("pause active");

        let paused = queue.active.read().await.clone().expect("active download");
        assert_eq!(paused.state, DownloadState::Paused);
        assert!(paused.paused);
        assert!(!paused.finished);

        queue.resume_active("id").await.expect("resume active");

        let resumed = queue.active.read().await.clone().expect("active download");
        assert_eq!(resumed.state, DownloadState::Downloading);
        assert!(!resumed.paused);
        assert!(!resumed.finished);
    }

    #[tokio::test]
    async fn cancel_marks_active_download_cancelled_without_finishing_immediately() {
        let queue = DownloadQueue::new();
        let active = FileDownload {
            uuid: "id".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/file.mp4"),
            filename: "file.mp4".to_string(),
            url: reqwest::Url::parse("https://example.com/file.mp4").expect("valid url"),
            finished: false,
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state: DownloadState::Downloading,
            start_at: None,
            duration_secs: None,
            kind: DownloadKind::Download,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: None,
        };

        *queue.active.write().await = Some(active);
        queue.cancel_active().await.expect("cancel active");

        let cancelled = queue.active.read().await.clone().expect("active download");
        assert_eq!(cancelled.state, DownloadState::Cancelled);
        assert!(!cancelled.finished);
        assert_eq!(cancelled.error.as_deref(), Some("Cancelled by user"));
        assert!(queue.finished.read().await.is_empty());
    }

    #[tokio::test]
    async fn persisted_queue_round_trips_and_requeues_running_downloads() {
        let state_file = temp_state_file("download_state");
        let queue = DownloadQueue::new_with_state_file(Some(state_file.clone()));
        let queued = FileDownload {
            uuid: "queued".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/queued.mp4"),
            filename: "queued.mp4".to_string(),
            url: reqwest::Url::parse("https://example.com/queued.mp4").expect("valid url"),
            finished: false,
            size: 10,
            total_size: Some(100),
            paused: false,
            error: None,
            state: DownloadState::Queued,
            start_at: None,
            duration_secs: None,
            kind: DownloadKind::Download,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: None,
        };
        let active = FileDownload {
            uuid: "active".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/active.mp4"),
            filename: "active.mp4".to_string(),
            url: reqwest::Url::parse("https://example.com/active.mp4").expect("valid url"),
            finished: false,
            size: 20,
            total_size: Some(200),
            paused: false,
            error: None,
            state: DownloadState::Downloading,
            start_at: None,
            duration_secs: None,
            kind: DownloadKind::Download,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: None,
        };
        let paused = FileDownload {
            uuid: "paused".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/paused.mp4"),
            filename: "paused.mp4".to_string(),
            url: reqwest::Url::parse("https://example.com/paused.mp4").expect("valid url"),
            finished: false,
            size: 30,
            total_size: Some(300),
            paused: true,
            error: None,
            state: DownloadState::Paused,
            start_at: None,
            duration_secs: None,
            kind: DownloadKind::Download,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: None,
        };

        queue.queue.lock().await.push_back(queued);
        *queue.active.write().await = Some(active);
        queue.finished.write().await.push(paused.clone());
        queue.persist_to_disk().await.expect("persist state");

        let restored = DownloadQueue::new_with_state_file(Some(state_file.clone()));
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
        let queue = DownloadQueue::new_with_state_file(Some(state_file.clone()));
        let future_start = Utc::now().timestamp().saturating_add(3_600);
        let scheduled = FileDownload {
            uuid: "recording".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/recording.ts"),
            filename: "recording.ts".to_string(),
            url: reqwest::Url::parse("https://example.com/live/1").expect("valid url"),
            finished: false,
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state: DownloadState::Scheduled,
            start_at: Some(future_start),
            duration_secs: Some(5400),
            kind: DownloadKind::Recording,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: None,
        };

        queue.scheduled.write().await.push(scheduled.clone());
        queue.persist_to_disk().await.expect("persist state");

        let restored = DownloadQueue::new_with_state_file(Some(state_file.clone()));
        restored.load_from_disk().await.expect("load state");

        assert!(restored.active.read().await.is_none());
        assert_eq!(restored.queue.lock().await.len(), 0);
        let restored_scheduled = restored.scheduled.read().await.clone();
        assert_eq!(restored_scheduled.len(), 1);
        assert_eq!(restored_scheduled[0].uuid, scheduled.uuid);
        assert_eq!(restored_scheduled[0].state, DownloadState::Scheduled);
        assert_eq!(restored_scheduled[0].start_at, Some(future_start));
        assert_eq!(restored_scheduled[0].duration_secs, Some(5400));
        assert_eq!(restored_scheduled[0].kind, DownloadKind::Recording);

        let _ = std::fs::remove_file(state_file);
    }

    #[test]
    fn recover_loaded_download_requeues_waiting_states() {
        let waiting_for_capacity = FileDownload {
            uuid: "capacity".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/capacity.ts"),
            filename: "capacity.ts".to_string(),
            url: reqwest::Url::parse("https://example.com/capacity.ts").expect("valid url"),
            finished: false,
            size: 77,
            total_size: Some(99),
            paused: false,
            error: Some("old error".to_string()),
            state: DownloadState::WaitingForCapacity,
            start_at: None,
            duration_secs: None,
            kind: DownloadKind::Download,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: None,
        };
        let retry_waiting = FileDownload {
            state: DownloadState::RetryWaiting,
            ..waiting_for_capacity.clone()
        };

        let restored_waiting_for_capacity = DownloadQueue::recover_loaded_download(waiting_for_capacity);
        let restored_retry_waiting = DownloadQueue::recover_loaded_download(retry_waiting);

        assert_eq!(restored_waiting_for_capacity.state, DownloadState::Queued);
        assert!(!restored_waiting_for_capacity.paused);
        assert!(restored_waiting_for_capacity.error.is_none());

        assert_eq!(restored_retry_waiting.state, DownloadState::Queued);
        assert!(!restored_retry_waiting.paused);
        assert!(restored_retry_waiting.error.is_none());
    }

    #[test]
    fn recover_loaded_download_clears_pending_retry_timestamp() {
        let retry_waiting = FileDownload {
            uuid: "retry".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/retry.ts"),
            filename: "retry.ts".to_string(),
            url: reqwest::Url::parse("https://example.com/retry.ts").expect("valid url"),
            finished: false,
            size: 12,
            total_size: Some(20),
            paused: false,
            error: Some("retrying".to_string()),
            state: DownloadState::RetryWaiting,
            start_at: None,
            duration_secs: None,
            kind: DownloadKind::Download,
            input_name: None,
            priority: 0,
            retry_attempts: 2,
            next_retry_at: Some(1_700_000_000),
            recording: None,
        };

        let restored = DownloadQueue::recover_loaded_download(retry_waiting);
        assert_eq!(restored.state, DownloadState::Queued);
        assert_eq!(restored.retry_attempts, 0);
        assert!(restored.next_retry_at.is_none());
    }

    #[tokio::test]
    async fn retry_finished_clears_retry_metadata() {
        let queue = DownloadQueue::new();
        queue.finished.write().await.push(FileDownload {
            uuid: "done".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/done.ts"),
            filename: "done.ts".to_string(),
            url: reqwest::Url::parse("https://example.com/done.ts").expect("valid url"),
            finished: true,
            size: 0,
            total_size: None,
            paused: false,
            error: Some("Retry limit reached".to_string()),
            state: DownloadState::Failed,
            start_at: None,
            duration_secs: None,
            kind: DownloadKind::Download,
            input_name: None,
            priority: 0,
            retry_attempts: 5,
            next_retry_at: Some(1_700_000_000),
            recording: None,
        });

        assert!(queue.retry_finished("done").await.expect("retry finished"));
        let queued = queue.queue.lock().await.front().cloned().expect("queued download");
        assert_eq!(queued.state, DownloadState::Queued);
        assert_eq!(queued.retry_attempts, 0);
        assert!(queued.next_retry_at.is_none());
        assert!(queued.error.is_none());
    }

    #[tokio::test]
    async fn retry_finished_rejects_recordings() {
        let queue = DownloadQueue::new();
        queue.finished.write().await.push(FileDownload {
            uuid: "recording".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/recording.ts"),
            filename: "recording.ts".to_string(),
            url: reqwest::Url::parse("https://example.com/live/recording.ts").expect("valid url"),
            finished: true,
            size: 0,
            total_size: None,
            paused: false,
            error: Some("Cancelled by user".to_string()),
            state: DownloadState::Cancelled,
            start_at: Some(1_700_000_000),
            duration_secs: Some(300),
            kind: DownloadKind::Recording,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: None,
        });

        assert!(!queue.retry_finished("recording").await.expect("reject recording retry"));
        assert!(queue.queue.lock().await.is_empty());
        assert_eq!(queue.finished.read().await.len(), 1);
    }

    #[tokio::test]
    async fn promote_due_scheduled_moves_only_ready_recordings_to_queue() {
        let queue = DownloadQueue::new();
        let due = FileDownload {
            uuid: "due".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/due.ts"),
            filename: "due.ts".to_string(),
            url: reqwest::Url::parse("https://example.com/live/due").expect("valid url"),
            finished: false,
            size: 123,
            total_size: Some(999),
            paused: false,
            error: Some("old error".to_string()),
            state: DownloadState::Scheduled,
            start_at: Some(100),
            duration_secs: Some(60),
            kind: DownloadKind::Recording,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: None,
        };
        let future = FileDownload {
            uuid: "future".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/future.ts"),
            filename: "future.ts".to_string(),
            url: reqwest::Url::parse("https://example.com/live/future").expect("valid url"),
            finished: false,
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state: DownloadState::Scheduled,
            start_at: Some(200),
            duration_secs: Some(60),
            kind: DownloadKind::Recording,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: None,
        };

        queue.scheduled.write().await.extend([due, future]);
        let revision = queue.revision.load(Ordering::SeqCst);

        let promoted = queue.promote_due_scheduled(150).await;

        assert_eq!(promoted, 1);
        assert_eq!(queue.revision.load(Ordering::SeqCst), revision + 1);
        let queued_items = queue.queue.lock().await.iter().cloned().collect::<Vec<_>>();
        assert_eq!(queued_items.len(), 1);
        assert_eq!(queued_items[0].uuid, "due");
        assert_eq!(queued_items[0].state, DownloadState::Queued);
        assert_eq!(queued_items[0].size, 0);
        assert!(queued_items[0].error.is_none());
        let scheduled_items = queue.scheduled.read().await.clone();
        assert_eq!(scheduled_items.len(), 1);
        assert_eq!(scheduled_items[0].uuid, "future");
    }

    #[tokio::test]
    async fn promote_due_scheduled_marks_expired_recordings_failed() {
        let queue = DownloadQueue::new();
        let expired = FileDownload {
            uuid: "expired".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/expired.ts"),
            filename: "expired.ts".to_string(),
            url: reqwest::Url::parse("https://example.com/live/expired").expect("valid url"),
            finished: false,
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state: DownloadState::Scheduled,
            start_at: Some(100),
            duration_secs: Some(60),
            kind: DownloadKind::Recording,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: None,
        };

        queue.scheduled.write().await.push(expired);
        let promoted = queue.promote_due_scheduled(200).await;

        assert_eq!(promoted, 1);
        assert!(queue.queue.lock().await.is_empty());
        let finished = queue.finished.read().await.clone();
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].uuid, "expired");
        assert_eq!(finished[0].state, DownloadState::Failed);
        assert!(finished[0].finished);
        assert_eq!(finished[0].error.as_deref(), Some("Recording window already expired"));
    }

    #[tokio::test]
    async fn load_from_disk_moves_expired_scheduled_recordings_to_finished() {
        let state_file = temp_state_file("expired_record_state");
        let queue = DownloadQueue::new_with_state_file(Some(state_file.clone()));
        let expired = FileDownload {
            uuid: "expired".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/expired.ts"),
            filename: "expired.ts".to_string(),
            url: reqwest::Url::parse("https://example.com/live/expired").expect("valid url"),
            finished: false,
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state: DownloadState::Scheduled,
            start_at: Some(100),
            duration_secs: Some(60),
            kind: DownloadKind::Recording,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: None,
        };

        queue.scheduled.write().await.push(expired);
        queue.persist_to_disk().await.expect("persist state");

        let restored = DownloadQueue::new_with_state_file(Some(state_file.clone()));
        restored.load_from_disk().await.expect("load state");

        assert!(restored.scheduled.read().await.is_empty());
        let finished = restored.finished.read().await.clone();
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].uuid, "expired");
        assert_eq!(finished[0].state, DownloadState::Failed);
        assert_eq!(finished[0].error.as_deref(), Some("Recording window already expired"));

        let _ = std::fs::remove_file(state_file);
    }

    #[test]
    fn recording_uuid_differs_for_same_url_with_different_start_times() {
        let download_cfg = VideoDownloadConfig {
            directory: "/tmp".to_string(),
            organize_into_directories: false,
            episode_pattern: None,
            headers: std::collections::HashMap::new(),
            download_priority: 0,
            recording_priority: 0,
            reserve_slots_for_users: 0,
            max_background_per_provider: 0,
            retry_backoff_initial_secs: 3,
            retry_backoff_multiplier: 3.0,
            retry_backoff_max_secs: 30,
            retry_backoff_jitter_percent: 0,
            retry_max_attempts: 5,
            recording: None,
        };

        let first = FileDownload::new_recording(
            "https://example.com/live/1",
            "recording_1.ts",
            &download_cfg,
            1_700_000_000,
            5400,
            None,
            0,
        )
        .expect("first recording");
        let second = FileDownload::new_recording(
            "https://example.com/live/1",
            "recording_2.ts",
            &download_cfg,
            1_700_005_400,
            5400,
            None,
            0,
        )
        .expect("second recording");

        assert_ne!(first.uuid, second.uuid);
    }

    #[test]
    fn download_uuid_differs_for_same_url_with_different_filenames() {
        let download_cfg = VideoDownloadConfig {
            directory: "/tmp".to_string(),
            organize_into_directories: false,
            episode_pattern: None,
            headers: std::collections::HashMap::new(),
            download_priority: 0,
            recording_priority: 0,
            reserve_slots_for_users: 0,
            max_background_per_provider: 0,
            retry_backoff_initial_secs: 3,
            retry_backoff_multiplier: 3.0,
            retry_backoff_max_secs: 30,
            retry_backoff_jitter_percent: 0,
            retry_max_attempts: 5,
            recording: None,
        };

        let first = FileDownload::new("https://example.com/video.mp4", "first.mp4", &download_cfg, None, 0)
            .expect("first download");
        let second = FileDownload::new("https://example.com/video.mp4", "second.mp4", &download_cfg, None, 0)
            .expect("second download");

        assert_ne!(first.uuid, second.uuid);
    }

    #[test]
    fn download_new_omits_trailing_dot_when_filename_has_no_extension() {
        let download_cfg = VideoDownloadConfig {
            directory: "/tmp".to_string(),
            organize_into_directories: false,
            episode_pattern: None,
            headers: std::collections::HashMap::new(),
            download_priority: 0,
            recording_priority: 0,
            reserve_slots_for_users: 0,
            max_background_per_provider: 0,
            retry_backoff_initial_secs: 3,
            retry_backoff_multiplier: 3.0,
            retry_backoff_max_secs: 30,
            retry_backoff_jitter_percent: 0,
            retry_max_attempts: 5,
            recording: None,
        };

        let task = FileDownload::new("https://example.com/live", "title with trailing dot.", &download_cfg, None, 0)
            .expect("download");

        assert_eq!(task.filename, "title_with_trailing_dot");
        assert!(!task.filename.ends_with('.'));
    }

    #[tokio::test]
    async fn promote_due_scheduled_places_due_recordings_ahead_of_existing_queue_items() {
        let queue = DownloadQueue::new();
        queue.queue.lock().await.push_back(FileDownload {
            uuid: "existing".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/existing.ts"),
            filename: "existing.ts".to_string(),
            url: reqwest::Url::parse("https://example.com/live/existing").expect("valid url"),
            finished: false,
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state: DownloadState::Queued,
            start_at: None,
            duration_secs: None,
            kind: DownloadKind::Download,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: None,
        });
        queue.scheduled.write().await.extend([
            FileDownload {
                uuid: "due-first".to_string(),
                file_dir: PathBuf::from("/tmp"),
                file_path: PathBuf::from("/tmp/due-first.ts"),
                filename: "due-first.ts".to_string(),
                url: reqwest::Url::parse("https://example.com/live/due-first").expect("valid url"),
                finished: false,
                size: 0,
                total_size: None,
                paused: false,
                error: None,
                state: DownloadState::Scheduled,
                start_at: Some(100),
                duration_secs: Some(60),
                kind: DownloadKind::Recording,
                input_name: None,
                priority: 0,
                retry_attempts: 0,
                next_retry_at: None,
            recording: None,
            },
            FileDownload {
                uuid: "due-second".to_string(),
                file_dir: PathBuf::from("/tmp"),
                file_path: PathBuf::from("/tmp/due-second.ts"),
                filename: "due-second.ts".to_string(),
                url: reqwest::Url::parse("https://example.com/live/due-second").expect("valid url"),
                finished: false,
                size: 0,
                total_size: None,
                paused: false,
                error: None,
                state: DownloadState::Scheduled,
                start_at: Some(110),
                duration_secs: Some(60),
                kind: DownloadKind::Recording,
                input_name: None,
                priority: 0,
                retry_attempts: 0,
                next_retry_at: None,
            recording: None,
            },
        ]);

        let promoted = queue.promote_due_scheduled(150).await;

        assert_eq!(promoted, 2);
        let queued = queue.queue.lock().await.iter().map(|download| download.uuid.clone()).collect::<Vec<_>>();
        assert_eq!(queued, vec!["due-first", "due-second", "existing"]);
    }

    #[tokio::test]
    async fn download_slot_wait_queue_signals_matching_waiter_by_id() {
        let queue = Arc::new(DownloadSlotWaitQueue::new());
        let control_signal = Arc::new(RwLock::new(DownloadControl::None));
        let control_notify = Arc::new(Notify::new());

        let queue_for_a = Arc::clone(&queue);
        let control_signal_for_a = Arc::clone(&control_signal);
        let control_notify_for_a = Arc::clone(&control_notify);
        let waiter_a = tokio::spawn(async move {
            queue_for_a
                .wait(
                    Some(Arc::from("input-a")),
                    1,
                    control_signal_for_a.as_ref(),
                    control_notify_for_a.as_ref(),
                )
                .await
        });

        let queue_for_b = Arc::clone(&queue);
        let control_signal_for_b = Arc::clone(&control_signal);
        let control_notify_for_b = Arc::clone(&control_notify);
        let waiter_b = tokio::spawn(async move {
            queue_for_b
                .wait(
                    Some(Arc::from("input-b")),
                    0,
                    control_signal_for_b.as_ref(),
                    control_notify_for_b.as_ref(),
                )
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
            DownloadWaitOutcome::Signalled
        );

        *control_signal.write().await = DownloadControl::Cancel;
        control_notify.notify_waiters();
        assert_eq!(
            timeout(Duration::from_millis(100), waiter_a).await.expect("waiter_a finished").expect("join ok"),
            DownloadWaitOutcome::Cancelled
        );
    }

    #[tokio::test]
    async fn find_duplicate_matches_active_queue_scheduled_and_finished_downloads() {
        let queue = DownloadQueue::new();
        let candidate = FileDownload {
            uuid: "candidate".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/movie.mp4"),
            filename: "movie.mp4".to_string(),
            url: reqwest::Url::parse("https://example.com/movie.mp4").expect("valid url"),
            finished: false,
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state: DownloadState::Queued,
            start_at: None,
            duration_secs: None,
            kind: DownloadKind::Download,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: None,
        };

        *queue.active.write().await = Some(FileDownload {
            uuid: "active".to_string(),
            ..candidate.clone()
        });
        assert_eq!(queue.find_duplicate(&candidate).await.map(|download| download.uuid), Some("active".to_string()));

        *queue.active.write().await = None;
        queue.queue.lock().await.push_back(FileDownload {
            uuid: "queued".to_string(),
            ..candidate.clone()
        });
        assert_eq!(queue.find_duplicate(&candidate).await.map(|download| download.uuid), Some("queued".to_string()));

        queue.queue.lock().await.clear();
        queue.scheduled.write().await.push(FileDownload {
            uuid: "scheduled".to_string(),
            state: DownloadState::Scheduled,
            kind: DownloadKind::Recording,
            start_at: Some(100),
            duration_secs: Some(60),
            url: reqwest::Url::parse("https://example.com/live/1").expect("valid url"),
            file_path: PathBuf::from("/tmp/recording.ts"),
            filename: "recording.ts".to_string(),
            ..candidate.clone()
        });
        let recording_candidate = FileDownload {
            uuid: "recording-candidate".to_string(),
            state: DownloadState::Scheduled,
            kind: DownloadKind::Recording,
            start_at: Some(100),
            duration_secs: Some(60),
            url: reqwest::Url::parse("https://example.com/live/1").expect("valid url"),
            file_path: PathBuf::from("/tmp/recording.ts"),
            filename: "recording.ts".to_string(),
            ..candidate.clone()
        };
        assert_eq!(
            queue.find_duplicate(&recording_candidate).await.map(|download| download.uuid),
            Some("scheduled".to_string())
        );

        queue.scheduled.write().await.clear();
        queue.finished.write().await.push(FileDownload {
            uuid: "finished".to_string(),
            finished: true,
            state: DownloadState::Completed,
            ..candidate.clone()
        });
        assert_eq!(
            queue.find_duplicate(&candidate).await.map(|download| download.uuid),
            Some("finished".to_string())
        );
    }

    #[tokio::test]
    async fn find_duplicate_allows_distinct_recording_windows() {
        let queue = DownloadQueue::new();
        queue.scheduled.write().await.push(FileDownload {
            uuid: "scheduled".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/recording_1.ts"),
            filename: "recording_1.ts".to_string(),
            url: reqwest::Url::parse("https://example.com/live/1").expect("valid url"),
            finished: false,
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state: DownloadState::Scheduled,
            start_at: Some(100),
            duration_secs: Some(60),
            kind: DownloadKind::Recording,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: None,
        });

        let different_window = FileDownload {
            uuid: "candidate".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/recording_2.ts"),
            filename: "recording_2.ts".to_string(),
            url: reqwest::Url::parse("https://example.com/live/1").expect("valid url"),
            finished: false,
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state: DownloadState::Scheduled,
            start_at: Some(200),
            duration_secs: Some(60),
            kind: DownloadKind::Recording,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: None,
        };

        assert!(queue.find_duplicate(&different_window).await.is_none());
    }

    #[tokio::test]
    async fn request_worker_restart_sets_restart_control_and_notifies_waiters() {
        let queue = DownloadQueue::new();
        let waiter_queue = Arc::clone(&queue.slot_waiters);
        let control_signal = Arc::clone(&queue.control_signal);
        let control_notify = Arc::clone(&queue.control_notify);

        let waiter = tokio::spawn(async move {
            waiter_queue
                .wait(None, 0, control_signal.as_ref(), control_notify.as_ref())
                .await
        });

        tokio::task::yield_now().await;
        queue.request_worker_restart();

        assert_eq!(
            timeout(Duration::from_millis(100), waiter).await.expect("waiter finished").expect("join ok"),
            DownloadWaitOutcome::Restarted
        );
        assert_eq!(*queue.control_signal.read().await, DownloadControl::Restart);
    }

    #[tokio::test]
    async fn wait_observes_preexisting_control_before_selecting() {
        let queue = DownloadQueue::new();
        *queue.control_signal.write().await = DownloadControl::Pause;

        let outcome = queue
            .slot_waiters
            .wait(None, 0, queue.control_signal.as_ref(), queue.control_notify.as_ref())
            .await;

        assert_eq!(outcome, DownloadWaitOutcome::Paused);
        assert!(queue.slot_waiters.snapshots().await.is_empty());
    }

    // --- Legacy pre-scan + normalization ---

    fn make_persisted_recording_legacy(file_path: PathBuf) -> PersistedFileDownload {
        PersistedFileDownload {
            uuid: "rec-legacy".to_string(),
            file_dir: file_path.parent().unwrap_or(Path::new("/")).to_path_buf(),
            file_path,
            filename: "rec.ts".to_string(),
            url: "https://example.com/live/rec".to_string(),
            finished: true,
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state: DownloadState::Completed,
            start_at: Some(1_700_000_000),
            duration_secs: Some(3_600),
            kind: DownloadKind::Recording,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: None,
        }
    }

    fn make_persisted_recording_with_user(file_path: PathBuf) -> PersistedFileDownload {
        let mut p = make_persisted_recording_legacy(file_path);
        p.recording = Some(RecordingMetadata {
            owner: shared::model::RecordingOwner::User(UserId::from("web:abc")),
            visibility: shared::model::RecordingVisibility::default(),
            source: None,
            program_start: Some(1_700_000_000),
            program_end: Some(1_700_003_600),
            scheduled_start: Some(1_700_000_000),
            scheduled_end: Some(1_700_003_600),
            pre_roll_secs: 0,
            post_roll_secs: 0,
            channel_id: None,
            channel_name: None,
            program_title: None,
            epg: None,
            provenance: shared::model::recording::RecordingProvenance::default(),
            relative_path: None,
            partial_relative_path: None,
            reserved_bytes: 0,
            measured_bytes: 0,
            completed_at: None,
            notification_markers: Vec::new(),
            deleting_previous_state: None,
        });
        p
    }

    #[test]
    fn legacy_recording_normalizes_to_legacy_admin_with_zero_padding() {
        let persisted = make_persisted_recording_legacy(PathBuf::from("/tmp/recordings/rec.ts"));
        let task = DownloadQueue::from_persisted_with(persisted, None, None).expect("restore");
        let meta = task.recording.as_ref().expect("recording metadata");
        assert!(meta.owner.is_legacy_admin());
        assert_eq!(meta.visibility, shared::model::RecordingVisibility::Private);
        assert_eq!(meta.pre_roll_secs, 0);
        assert_eq!(meta.post_roll_secs, 0);
        assert_eq!(meta.scheduled_start, Some(1_700_000_000));
        assert_eq!(meta.scheduled_end, Some(1_700_003_600));
    }

    #[test]
    fn legacy_recording_derives_relative_path_when_contained() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recording_root = dir.path().join("recordings");
        std::fs::create_dir_all(&recording_root).expect("mkdir");
        let file = recording_root.join("2025/pilot.ts");
        let persisted = make_persisted_recording_legacy(file);
        let task =
            DownloadQueue::from_persisted_with(persisted, Some(&recording_root), None).expect("restore");
        let meta = task.recording.as_ref().expect("metadata");
        assert_eq!(meta.relative_path.as_deref(), Some("2025/pilot.ts"));
    }

    #[test]
    fn legacy_recording_omits_relative_path_when_outside_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recording_root = dir.path().join("recordings");
        std::fs::create_dir_all(&recording_root).expect("mkdir");
        let file = dir.path().join("downloads/old.ts");
        let persisted = make_persisted_recording_legacy(file);
        let task =
            DownloadQueue::from_persisted_with(persisted, Some(&recording_root), None).expect("restore");
        let meta = task.recording.as_ref().expect("metadata");
        assert!(meta.relative_path.is_none());
    }

    #[test]
    fn legacy_recording_uses_legacy_root_when_recording_root_misses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let legacy_root = dir.path().join("downloads");
        std::fs::create_dir_all(&legacy_root).expect("mkdir");
        let file = legacy_root.join("rec.ts");
        let persisted = make_persisted_recording_legacy(file.clone());
        let task = DownloadQueue::from_persisted_with(persisted, None, Some(&legacy_root)).expect("restore");
        let meta = task.recording.as_ref().expect("metadata");
        assert_eq!(meta.relative_path.as_deref(), Some("rec.ts"));
    }

    #[test]
    fn legacy_recording_falls_back_completed_at_to_scheduled_end_when_mtime_unavailable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("missing-rec.ts");
        let persisted = make_persisted_recording_legacy(file);
        let task = DownloadQueue::from_persisted_with(persisted, None, None).expect("restore");
        let meta = task.recording.as_ref().expect("metadata");
        assert_eq!(meta.completed_at, meta.scheduled_end);
        assert_eq!(meta.measured_bytes, 0);
    }

    #[test]
    fn legacy_recording_uses_real_file_mtime_and_size_when_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("rec.ts");
        std::fs::write(&file, b"hello").expect("write");
        let mtime_secs = std::fs::metadata(&file)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        let persisted = make_persisted_recording_legacy(file);
        let task = DownloadQueue::from_persisted_with(persisted, None, None).expect("restore");
        let meta = task.recording.as_ref().expect("metadata");
        assert_eq!(meta.completed_at, Some(mtime_secs));
        assert_eq!(meta.measured_bytes, 5);
    }

    #[test]
    fn legacy_recording_rejects_unsafe_dir_symlink_substitution() {
        // The legacy check uses string-prefix matching. A symlinked file
        // whose canonical path resolves outside the recording root still looks
        // contained by the prefix check.
        let dir = tempfile::tempdir().expect("tempdir");
        let recording_root = dir.path().join("recordings");
        let other = dir.path().join("elsewhere");
        std::fs::create_dir_all(&other).expect("mkdir");
        std::fs::create_dir_all(&recording_root).expect("mkdir");
        let real = other.join("real.ts");
        std::fs::write(&real, b"x").expect("write");
        let link_path = recording_root.join("alias.ts");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link_path).expect("symlink");
        let persisted = make_persisted_recording_legacy(link_path.clone());
        let task =
            DownloadQueue::from_persisted_with(persisted, Some(&recording_root), None).expect("restore");
        let meta = task.recording.as_ref().expect("metadata");
        // The string-prefix check accepts this legacy edge case.
        assert_eq!(meta.relative_path.as_deref(), Some("alias.ts"));
    }

    #[test]
    fn download_with_recording_metadata_is_rejected_by_persisted_load() {
        // Plain download (kind=Download) must never carry recording metadata.
        let mut p = make_persisted_recording_legacy(PathBuf::from("/tmp/file.ts"));
        p.kind = DownloadKind::Download;
        p.recording = Some(RecordingMetadata::for_legacy_admin(0, 0));
        let result = DownloadQueue::from_persisted_with(p, None, None);
        assert!(result.is_err(), "Download + recording metadata must be rejected");
        assert!(
            matches!(result.unwrap_err(), PersistedError::KindMetadataInvariant { .. }),
            "must surface the invariant violation, not a parse error"
        );
    }

    #[test]
    fn from_persisted_rejects_invalid_url() {
        let mut p = make_persisted_recording_legacy(PathBuf::from("/tmp/file.ts"));
        p.url = "not a url at all".to_string();
        let result = DownloadQueue::from_persisted_with(p, None, None);
        assert!(result.is_err(), "invalid url must surface as an error");
        assert!(
            matches!(result.unwrap_err(), PersistedError::InvalidUrl(_)),
            "must surface the parse error, not an invariant violation"
        );
    }

    #[test]
    fn normalized_recording_preserves_existing_metadata() {
        // Already-normalized tasks (User owner) must not be re-normalized.
        let persisted = make_persisted_recording_with_user(PathBuf::from("/tmp/rec.ts"));
        let task = DownloadQueue::from_persisted_with(persisted, None, None).expect("restore");
        let meta = task.recording.as_ref().expect("metadata");
        assert!(!meta.owner.is_legacy_admin(), "user owner must be preserved");
    }

    #[test]
    fn pre_scan_recording_user_ids_returns_empty_for_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist.json");
        let ids = pre_scan_recording_user_ids(&missing);
        assert!(ids.is_empty());
    }

    fn persisted_recording_with_owner(
        uuid: &str,
        owner: shared::model::RecordingOwner,
        visibility: shared::model::RecordingVisibility,
    ) -> PersistedFileDownload {
        let meta = RecordingMetadata::new(
            owner,
            visibility,
            shared::model::recording::RecordingSource::new("1", "1", "input-a"),
            1_700_000_000,
            1_700_003_600,
            0,
            0,
        );
        PersistedFileDownload {
            uuid: uuid.to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from(format!("/tmp/{uuid}.ts")),
            filename: format!("{uuid}.ts"),
            url: format!("https://example.com/{uuid}"),
            finished: false,
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state: DownloadState::Completed,
            start_at: None,
            duration_secs: None,
            kind: DownloadKind::Recording,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: Some(meta),
        }
    }

    #[test]
    fn pre_scan_recording_user_ids_finds_user_ids_in_nested_recording_blocks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("downloads_state.json");
        let queue = PersistedDownloadQueue {
            queue: vec![
                persisted_recording_with_owner(
                    "a",
                    shared::model::RecordingOwner::User(UserId::from("web:abc")),
                    shared::model::RecordingVisibility::Private,
                ),
                persisted_recording_with_owner(
                    "b",
                    shared::model::RecordingOwner::User(UserId::from("api:def")),
                    shared::model::RecordingVisibility::Shared,
                ),
            ],
            scheduled: vec![],
            active: None,
            finished: vec![],
            revision: QueueRevision::default(),
        };
        std::fs::write(&path, serde_json::to_vec_pretty(&queue).unwrap()).expect("write");
        let ids = pre_scan_recording_user_ids(&path);
        assert_eq!(ids.len(), 2);
        assert!(ids.iter().any(|u| u.0 == "web:abc"));
        assert!(ids.iter().any(|u| u.0 == "api:def"));
    }

    #[test]
    fn pre_scan_recording_user_ids_ignores_legacy_admin_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("downloads_state.json");
        let queue = PersistedDownloadQueue {
            queue: vec![persisted_recording_with_owner(
                "a",
                shared::model::RecordingOwner::LegacyAdmin,
                shared::model::RecordingVisibility::Private,
            )],
            scheduled: vec![],
            active: None,
            finished: vec![],
            revision: QueueRevision::default(),
        };
        std::fs::write(&path, serde_json::to_vec_pretty(&queue).unwrap()).expect("write");
        let ids = pre_scan_recording_user_ids(&path);
        assert!(ids.is_empty(), "legacy_admin entries must not surface as user IDs");
    }

    #[test]
    fn pre_scan_recording_user_ids_returns_empty_for_invalid_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("downloads_state.json");
        std::fs::write(&path, b"not json").expect("write");
        let ids = pre_scan_recording_user_ids(&path);
        assert!(ids.is_empty());
    }

    // --- Transactional queue mutation boundary ---

    fn make_test_recording_task(uuid: &str, file_path: PathBuf) -> FileDownload {
        let task = FileDownload {
            uuid: uuid.to_string(),
            file_dir: file_path.parent().unwrap_or(Path::new("/")).to_path_buf(),
            file_path,
            filename: format!("{uuid}.ts"),
            url: reqwest::Url::parse(&format!("https://example.com/{uuid}")).expect("valid url"),
            finished: false,
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state: DownloadState::Downloading,
            start_at: None,
            duration_secs: None,
            kind: DownloadKind::Recording,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: Some(RecordingMetadata::for_legacy_admin(0, 60)),
        };
        assert!(task.kind_metadata_invariant_ok());
        task
    }

    #[tokio::test]
    async fn mutate_persists_and_increments_revision_on_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads_state.json");
        let queue = DownloadQueue::new_with_state_file(Some(state_file.clone()));
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
        let restored: PersistedDownloadQueue =
            serde_json::from_slice(&content).expect("parse state file");
        assert_eq!(restored.revision, QueueRevision(1), "file carries the candidate's revision");
    }

    #[tokio::test]
    async fn mutate_keeps_state_when_closure_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads_state.json");
        let queue = DownloadQueue::new_with_state_file(Some(state_file.clone()));
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
        let queue = DownloadQueue::new_with_state_file(Some(state_file));
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
        let queue = DownloadQueue::new_with_state_file(Some(state_file.clone()));
        let task = DownloadQueue::to_persisted(&make_test_recording_task("rec-1", dir.path().join("a.ts")));
        mutate(&queue, |candidate| {
            candidate.queue.push(task);
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
        let queue = DownloadQueue::new();
        *queue.active.write().await = Some(make_test_recording_task("active", PathBuf::from("/tmp/active.ts")));
        queue
            .queue
            .lock()
            .await
            .push_back(make_test_recording_task("next", PathBuf::from("/tmp/next.ts")));

        assert!(!queue.pause_active("next").await.expect("uuid mismatch"));

        assert_eq!(queue.revision.load(Ordering::SeqCst), 0);
        assert_eq!(queue.active.read().await.as_ref().map(|download| download.uuid.as_str()), Some("active"));
        assert_eq!(queue.queue.lock().await.front().map(|download| download.uuid.as_str()), Some("next"));
        assert_eq!(*queue.control_signal.read().await, DownloadControl::None);
    }

    #[tokio::test]
    async fn retry_finished_legacy_writer_commits_one_revision() {
        let queue = DownloadQueue::new();
        let mut finished = make_test_recording_task("done", PathBuf::from("/tmp/done.ts"));
        finished.kind = DownloadKind::Download;
        finished.recording = None;
        finished.finished = true;
        finished.state = DownloadState::Failed;
        queue.finished.write().await.push(finished);

        assert!(queue.retry_finished("done").await.expect("retry commit"));

        assert_eq!(queue.revision.load(Ordering::SeqCst), 1);
        assert!(queue.finished.read().await.is_empty());
        assert_eq!(queue.queue.lock().await.front().map(|download| download.uuid.as_str()), Some("done"));
    }

    #[tokio::test]
    async fn concurrent_legacy_writers_serialize_and_increment_each_revision() {
        let queue = Arc::new(DownloadQueue::new());
        queue
            .queue
            .lock()
            .await
            .push_back(make_test_recording_task("remove", PathBuf::from("/tmp/remove.ts")));
        let mut finished = make_test_recording_task("retry", PathBuf::from("/tmp/retry.ts"));
        finished.kind = DownloadKind::Download;
        finished.recording = None;
        finished.finished = true;
        finished.state = DownloadState::Failed;
        queue.finished.write().await.push(finished);

        let remove_queue = Arc::clone(&queue);
        let retry_queue = Arc::clone(&queue);
        let (removed, retried) = tokio::join!(
            async move { remove_queue.remove_from_queue("remove").await },
            async move { retry_queue.retry_finished("retry").await },
        );

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
        let queue = std::sync::Arc::new(DownloadQueue::new_with_state_file(Some(state_file)));
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
        let queue = std::sync::Arc::new(DownloadQueue::new());
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
    async fn committed_download_snapshot_waits_for_mutation_boundary() {
        let queue = Arc::new(DownloadQueue::new());
        let mutation_guard = queue.mutation_guard.lock().await;
        let snapshot_queue = Arc::clone(&queue);
        let snapshot = tokio::spawn(async move { snapshot_queue.committed_download_snapshot().await });

        tokio::task::yield_now().await;
        assert!(!snapshot.is_finished());

        drop(mutation_guard);
        assert!(snapshot.await.is_ok());
    }

    #[tokio::test]
    async fn control_signal_is_ordered_inside_mutation_guard() {
        let queue = Arc::new(DownloadQueue::new());
        *queue.active.write().await = Some(make_test_recording_task("active", PathBuf::from("/tmp/active.ts")));
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
        assert_eq!(*queue.control_signal.read().await, DownloadControl::Pause);
    }

    #[tokio::test]
    async fn same_value_control_is_published_after_worker_commit_and_clear() {
        let queue = Arc::new(DownloadQueue::new());
        *queue.active.write().await = Some(make_test_recording_task("task-a", PathBuf::from("/tmp/task-a.ts")));
        queue
            .queue
            .lock()
            .await
            .push_back(make_test_recording_task("task-b", PathBuf::from("/tmp/task-b.ts")));
        let mut control_lock = queue.control_signal.write().await;
        *control_lock = DownloadControl::Cancel;
        let worker_queue = Arc::clone(&queue);
        let worker_commit = tokio::spawn(async move {
            worker_queue
                .mutate_optional_and_clear_control(DownloadControl::Cancel, |candidate| {
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
        assert_eq!(*queue.control_signal.read().await, DownloadControl::Cancel);
    }

    #[tokio::test]
    async fn mutate_swap_restores_in_memory_state_from_persisted_candidate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads_state.json");
        let queue = DownloadQueue::new_with_state_file(Some(state_file.clone()));

        let persisted = DownloadQueue::to_persisted(&make_test_recording_task("rec-1", dir.path().join("a.ts")));
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

    // --- Filename rendering + collision reservation ---

    fn collect_existing_relative_paths(candidate: &PersistedDownloadQueue) -> Vec<String> {
        let mut out = Vec::new();
        for d in &candidate.queue {
            if let Some(meta) = &d.recording {
                if let Some(p) = &meta.relative_path {
                    out.push(p.clone());
                }
            }
        }
        for d in &candidate.scheduled {
            if let Some(meta) = &d.recording {
                if let Some(p) = &meta.relative_path {
                    out.push(p.clone());
                }
            }
        }
        if let Some(d) = &candidate.active {
            if let Some(meta) = &d.recording {
                if let Some(p) = &meta.relative_path {
                    out.push(p.clone());
                }
            }
        }
        for d in &candidate.finished {
            if let Some(meta) = &d.recording {
                if let Some(p) = &meta.relative_path {
                    out.push(p.clone());
                }
            }
        }
        out
    }

    /// Reserve a unique relative path for a new recording inside the
    /// queue mutation boundary. The candidate is the in-memory
    /// `PersistedDownloadQueue` the closure is building; the helper
    /// collects all already-reserved `relative_path` values, applies
    /// the supplied stem, and appends a numbered collision suffix
    /// (`_1`, `_2`, …) until the result is unique. The reserved path
    /// is also written to the recording metadata so a later collision
    /// created externally is detected by the worker at execute time.
    fn reserve_recording_relative_path(
        candidate: &mut PersistedDownloadQueue,
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
                attach_relative_path(&mut d.recording, &reserved);
            }
        }
        for d in &mut candidate.scheduled {
            if d.uuid == recording_uuid {
                attach_relative_path(&mut d.recording, &reserved);
            }
        }
        if let Some(d) = candidate.active.as_mut() {
            if d.uuid == recording_uuid {
                attach_relative_path(&mut d.recording, &reserved);
            }
        }
        for d in &mut candidate.finished {
            if d.uuid == recording_uuid {
                attach_relative_path(&mut d.recording, &reserved);
            }
        }
        reserved
    }

    fn find_recording_relative_path(
        candidate: &PersistedDownloadQueue,
        recording_uuid: &str,
    ) -> Option<String> {
        for d in &candidate.queue {
            if d.uuid == recording_uuid {
                if let Some(meta) = &d.recording {
                    if let Some(p) = &meta.relative_path {
                        return Some(p.clone());
                    }
                }
            }
        }
        for d in &candidate.scheduled {
            if d.uuid == recording_uuid {
                if let Some(meta) = &d.recording {
                    if let Some(p) = &meta.relative_path {
                        return Some(p.clone());
                    }
                }
            }
        }
        if let Some(d) = &candidate.active {
            if d.uuid == recording_uuid {
                if let Some(meta) = &d.recording {
                    if let Some(p) = &meta.relative_path {
                        return Some(p.clone());
                    }
                }
            }
        }
        for d in &candidate.finished {
            if d.uuid == recording_uuid {
                if let Some(meta) = &d.recording {
                    if let Some(p) = &meta.relative_path {
                        return Some(p.clone());
                    }
                }
            }
        }
        None
    }

    fn attach_relative_path(meta: &mut Option<RecordingMetadata>, path: &str) {
        if let Some(m) = meta.as_mut() {
            m.relative_path = Some(path.to_string());
        }
    }

    fn persisted_recording(uuid: &str, meta: RecordingMetadata) -> PersistedFileDownload {
        PersistedFileDownload {
            uuid: uuid.to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from(format!("/tmp/{uuid}.ts")),
            filename: format!("{uuid}.ts"),
            url: format!("https://example.com/{uuid}"),
            finished: false,
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state: DownloadState::Scheduled,
            start_at: Some(0),
            duration_secs: Some(60),
            kind: DownloadKind::Recording,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: Some(meta),
        }
    }

    #[test]
    fn reserve_recording_relative_path_returns_numbered_stem_when_no_collision() {
        let mut candidate = PersistedDownloadQueue::default();
        let reserved = reserve_recording_relative_path(&mut candidate, "pilot", "rec-1");
        assert_eq!(reserved, "pilot_1", "numbered suffix is the first candidate");
    }

    #[test]
    fn reserve_recording_relative_path_skips_existing_paths() {
        let mut candidate = PersistedDownloadQueue::default();
        let mut occupied = RecordingMetadata::for_legacy_admin(0, 60);
        occupied.relative_path = Some("pilot_1".to_string());
        candidate.queue.push(persisted_recording("other", occupied));
        let reserved = reserve_recording_relative_path(&mut candidate, "pilot", "rec-1");
        assert_eq!(reserved, "pilot_2");
    }

    #[test]
    fn reserve_recording_relative_path_does_not_double_bump_for_self() {
        let mut candidate = PersistedDownloadQueue::default();
        let mut existing = RecordingMetadata::for_legacy_admin(0, 60);
        existing.relative_path = Some("pilot_3".to_string());
        let mut d = persisted_recording("rec-1", existing);
        d.recording.as_mut().expect("recording").relative_path = Some("pilot_3".to_string());
        candidate.queue.push(d);
        let reserved = reserve_recording_relative_path(&mut candidate, "pilot", "rec-1");
        assert_eq!(reserved, "pilot_3", "must not bump because of self");
    }

    #[tokio::test]
    async fn pause_active_routes_through_mutate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads_state.json");
        let queue = DownloadQueue::new_with_state_file(Some(state_file));
        let task = make_test_recording_task("rec-1", dir.path().join("a.ts"));
        {
            let mut active = queue.active.write().await;
            *active = Some(task);
        }
        let prior_revision = queue.revision.load(Ordering::SeqCst);
        queue.pause_active("rec-1").await.expect("pause_active");
        let active = queue.active.read().await.clone().expect("active");
        assert!(active.paused);
        assert_eq!(active.state, DownloadState::Paused);
        assert!(active.next_retry_at.is_none());
        assert_eq!(queue.revision.load(Ordering::SeqCst), prior_revision + 1);
    }

    #[test]
    fn derive_legacy_relative_path_rejects_parent_dir_traversal() {
        // `<root>/../downloads/old.ts` strips cleanly under
        // `Path::strip_prefix`, but the remaining `../downloads/old.ts`
        // would let a downstream `join(root)` resolve back outside the
        // recording root. The fix rejects anything other than
        // `Component::Normal` after the strip.
        let root = Path::new("/data/recordings");
        let traversal = Path::new("/data/recordings/../downloads/old.ts");
        assert!(derive_legacy_relative_path(traversal, Some(root), None).is_none());

        // Sanity: a path that genuinely lives under the root still
        // produces its relative form.
        let inside = Path::new("/data/recordings/2026-08/rec.ts");
        assert_eq!(
            derive_legacy_relative_path(inside, Some(root), None).as_deref(),
            Some("2026-08/rec.ts"),
        );
    }
}
