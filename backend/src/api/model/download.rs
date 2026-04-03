use crate::model::VideoDownloadConfig;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use shared::model::{FileDownloadDto, TaskKindDto, TaskPriorityDto, TransferStatusDto};
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
}

#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum DownloadKind {
    #[default]
    Download,
    Recording,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedFileDownload {
    uuid: String,
    file_dir: PathBuf,
    file_path: PathBuf,
    filename: String,
    url: String,
    finished: bool,
    size: u64,
    total_size: Option<u64>,
    paused: bool,
    error: Option<String>,
    state: DownloadState,
    start_at: Option<i64>,
    duration_secs: Option<u64>,
    kind: DownloadKind,
    #[serde(default)]
    input_name: Option<String>,
    #[serde(default)]
    priority: i8,
    #[serde(default)]
    retry_attempts: u8,
    #[serde(default)]
    next_retry_at: Option<i64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedDownloadQueue {
    queue: Vec<PersistedFileDownload>,
    scheduled: Vec<PersistedFileDownload>,
    active: Option<PersistedFileDownload>,
    finished: Vec<PersistedFileDownload>,
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

                let mut filename = format!("{file_stem}.{file_ext}");
                let file_dir = get_download_directory(download_cfg, file_stem);
                let mut file_path: PathBuf = file_dir.clone();
                file_path.push(&filename);
                let mut x: usize = 1;
                while file_path.is_file() {
                    filename = format!("{file_stem}_{x}.{file_ext}");
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
        Some(recording)
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
}

impl Default for DownloadQueue {
    fn default() -> Self { Self::new() }
}

impl DownloadQueue {
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
                .is_some_and(|(start_at, duration_secs)| now_ts >= start_at.saturating_add(i64::try_from(duration_secs).unwrap_or(i64::MAX)))
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
        }
    }

    fn to_persisted(download: &FileDownload) -> PersistedFileDownload {
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
        }
    }

    fn from_persisted(download: PersistedFileDownload) -> Option<FileDownload> {
        Some(FileDownload {
            uuid: download.uuid,
            file_dir: download.file_dir,
            file_path: download.file_path,
            filename: download.filename,
            url: reqwest::Url::parse(&download.url).ok()?,
            finished: download.finished,
            size: download.size,
            total_size: download.total_size,
            paused: download.paused,
            error: download.error,
            state: download.state,
            start_at: download.start_at,
            duration_secs: download.duration_secs,
            kind: download.kind,
            input_name: download.input_name.map(|s| Arc::from(s.as_str())),
            priority: download.priority,
            retry_attempts: download.retry_attempts,
            next_retry_at: download.next_retry_at,
        })
    }

    pub async fn persist_to_disk(&self) -> std::io::Result<()> {
        let Some(state_file) = self.state_file.as_ref() else {
            return Ok(());
        };

        let queue = self.queue.lock().await.iter().map(Self::to_persisted).collect::<Vec<_>>();
        let scheduled = self.scheduled.read().await.iter().map(Self::to_persisted).collect::<Vec<_>>();
        let active = self.active.read().await.as_ref().map(Self::to_persisted);
        let finished = self.finished.read().await.iter().map(Self::to_persisted).collect::<Vec<_>>();
        let payload = PersistedDownloadQueue { queue, scheduled, active, finished };
        let content = serde_json::to_vec_pretty(&payload).map_err(std::io::Error::other)?;

        if let Some(parent) = state_file.parent() {
            fs::create_dir_all(parent).await?;
        }

        let tmp_file = state_file.with_extension("json.tmp");
        fs::write(&tmp_file, content).await?;
        fs::rename(&tmp_file, state_file).await
    }

    pub async fn load_from_disk(&self) -> std::io::Result<()> {
        let Some(state_file) = self.state_file.as_ref() else {
            return Ok(());
        };
        if !state_file.exists() {
            return Ok(());
        }

        let content = fs::read_to_string(state_file).await?;
        let persisted: PersistedDownloadQueue =
            serde_json::from_str(&content).map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;

        let queue = persisted
            .queue
            .into_iter()
            .filter_map(Self::from_persisted)
            .map(Self::recover_loaded_download)
            .collect::<VecDeque<_>>();
        let now_ts = Utc::now().timestamp();
        let scheduled_loaded = persisted
            .scheduled
            .into_iter()
            .filter_map(Self::from_persisted)
            .map(Self::recover_loaded_download)
            .collect::<Vec<_>>();
        let (scheduled, missed_scheduled): (Vec<_>, Vec<_>) = scheduled_loaded
            .into_iter()
            .partition(|download| !Self::recording_start_missed_window(download, now_ts));
        let active = persisted.active.and_then(Self::from_persisted).map(Self::recover_loaded_download);
        let mut finished =
            persisted.finished.into_iter().filter_map(Self::from_persisted).collect::<Vec<_>>();
        finished.extend(missed_scheduled.into_iter().map(Self::finalize_missed_recording));

        *self.queue.lock().await = queue;
        *self.scheduled.write().await = scheduled;
        *self.finished.write().await = finished;
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

    pub async fn pause_active(&self) {
        *self.control_signal.write().await = DownloadControl::Pause;
        self.control_notify.notify_waiters();
        if let Some(download) = self.active.write().await.as_mut() {
            download.paused = true;
            download.state = DownloadState::Paused;
            download.next_retry_at = None;
        }
        let _ = self.persist_to_disk().await;
    }

    pub async fn resume_active(&self) {
        *self.control_signal.write().await = DownloadControl::None;
        self.control_notify.notify_waiters();
        if let Some(download) = self.active.write().await.as_mut() {
            download.paused = false;
            download.state = DownloadState::Downloading;
            download.next_retry_at = None;
        }
        let _ = self.persist_to_disk().await;
    }

    pub async fn cancel_active(&self) {
        *self.control_signal.write().await = DownloadControl::Cancel;
        self.control_notify.notify_waiters();
        if let Some(download) = self.active.write().await.as_mut() {
            download.state = DownloadState::Cancelled;
            download.error = Some("Cancelled by user".to_string());
            download.next_retry_at = None;
        }
        let _ = self.persist_to_disk().await;
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

    pub async fn remove_from_queue(&self, uuid: &str) -> bool {
        let mut queue = self.queue.lock().await;
        let initial_len = queue.len();
        queue.retain(|d| d.uuid != uuid);
        let removed = queue.len() < initial_len;
        drop(queue);
        if !removed {
            let mut scheduled = self.scheduled.write().await;
            let initial_len = scheduled.len();
            scheduled.retain(|d| d.uuid != uuid);
            let scheduled_removed = scheduled.len() < initial_len;
            drop(scheduled);
            if scheduled_removed {
                let _ = self.persist_to_disk().await;
                return true;
            }
        }
        if removed {
            let _ = self.persist_to_disk().await;
        }
        removed
    }

    pub async fn remove_finished(&self, uuid: &str) -> bool {
        let mut finished = self.finished.write().await;
        let initial_len = finished.len();
        finished.retain(|d| d.uuid != uuid);
        let removed = finished.len() < initial_len;
        drop(finished);
        if removed {
            let _ = self.persist_to_disk().await;
        }
        removed
    }

    pub async fn retry_finished(&self, uuid: &str) -> bool {
        let mut finished = self.finished.write().await;
        if let Some(pos) = finished.iter().position(|d| d.uuid == uuid) {
            let mut download = finished.remove(pos);
            download.finished = false;
            download.size = 0;
            download.paused = false;
            download.error = None;
            download.state = DownloadState::Queued;
            download.retry_attempts = 0;
            download.next_retry_at = None;
            drop(finished);
            self.queue.lock().await.push_back(download);
            let _ = self.persist_to_disk().await;
            true
        } else {
            false
        }
    }

    pub async fn promote_due_scheduled(&self, now_ts: i64) -> usize {
        let mut scheduled = self.scheduled.write().await;
        if scheduled.is_empty() {
            return 0;
        }

        let mut due_downloads = Vec::new();
        let mut missed_recordings = Vec::new();
        scheduled.retain(|download| {
            let is_missed = Self::recording_start_missed_window(download, now_ts);
            if is_missed {
                missed_recordings.push(Self::finalize_missed_recording(download.clone()));
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
        drop(scheduled);

        let had_missed_recordings = !missed_recordings.is_empty();
        let missed_count = missed_recordings.len();
        if had_missed_recordings {
            self.finished.write().await.extend(missed_recordings);
        }

        if due_downloads.is_empty() {
            if had_missed_recordings {
                let _ = self.persist_to_disk().await;
                return missed_count;
            }
            return 0;
        }

        let due_count = due_downloads.len();
        let mut queue = self.queue.lock().await;
        for download in due_downloads.into_iter().rev() {
            queue.push_front(download);
        }
        drop(queue);

        let _ = self.persist_to_disk().await;
        due_count
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
        };

        *queue.active.write().await = Some(active);
        queue.pause_active().await;

        let paused = queue.active.read().await.clone().expect("active download");
        assert_eq!(paused.state, DownloadState::Paused);
        assert!(paused.paused);
        assert!(!paused.finished);

        queue.resume_active().await;

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
        };

        *queue.active.write().await = Some(active);
        queue.cancel_active().await;

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
        });

        assert!(queue.retry_finished("done").await);
        let queued = queue.queue.lock().await.front().cloned().expect("queued download");
        assert_eq!(queued.state, DownloadState::Queued);
        assert_eq!(queued.retry_attempts, 0);
        assert!(queued.next_retry_at.is_none());
        assert!(queued.error.is_none());
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
        };

        queue.scheduled.write().await.extend([due, future]);

        let promoted = queue.promote_due_scheduled(150).await;

        assert_eq!(promoted, 1);
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
        };

        let first = FileDownload::new("https://example.com/video.mp4", "first.mp4", &download_cfg, None, 0)
            .expect("first download");
        let second = FileDownload::new("https://example.com/video.mp4", "second.mp4", &download_cfg, None, 0)
            .expect("second download");

        assert_ne!(first.uuid, second.uuid);
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
}
