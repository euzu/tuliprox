use crate::model::VideoDownloadConfig;
use serde::{Deserialize, Serialize};
use shared::utils::{deunicode_string, hash_string_as_hex, CONSTANTS, FILENAME_TRIM_PATTERNS};
use std::{
    collections::VecDeque,
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{fs, sync::{Mutex, RwLock}};

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

impl FileDownload {
    // TODO read header size info  and restart support
    // "content-type" => ".../..."
    // "content-length" => "1975828544"
    // "accept-ranges" => "0-1975828544"
    // "content-range" => "bytes 0-1975828543/1975828544"

    pub fn new(req_url: &str, req_filename: &str, download_cfg: &VideoDownloadConfig) -> Option<Self> {
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
                    uuid: hash_string_as_hex(req_url),
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
    ) -> Option<Self> {
        let mut recording = Self::new(req_url, req_filename, download_cfg)?;
        let identity = format!(
            "{}:{}:{}:{}:{}",
            "recording",
            req_url,
            recording.filename,
            start_at,
            duration_secs
        );
        recording.uuid = hash_string_as_hex(&identity);
        recording.state = DownloadState::Scheduled;
        recording.start_at = Some(start_at);
        recording.duration_secs = Some(duration_secs);
        recording.kind = DownloadKind::Recording;
        Some(recording)
    }
}

pub struct DownloadQueue {
    pub queue: Arc<Mutex<VecDeque<FileDownload>>>,
    pub scheduled: Arc<RwLock<Vec<FileDownload>>>,
    pub active: Arc<RwLock<Option<FileDownload>>>,
    pub finished: Arc<RwLock<Vec<FileDownload>>>,
    pub control_signal: Arc<RwLock<DownloadControl>>,
    pub worker_running: Arc<RwLock<bool>>,
    pub state_file: Option<PathBuf>,
}

impl Default for DownloadQueue {
    fn default() -> Self { Self::new() }
}

impl DownloadQueue {
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
            worker_running: Arc::from(RwLock::new(false)),
            state_file,
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
        let persisted: PersistedDownloadQueue = serde_json::from_str(&content).map_err(std::io::Error::other)?;

        let queue = persisted
            .queue
            .into_iter()
            .filter_map(Self::from_persisted)
            .map(Self::recover_loaded_download)
            .collect::<VecDeque<_>>();
        let scheduled = persisted
            .scheduled
            .into_iter()
            .filter_map(Self::from_persisted)
            .map(Self::recover_loaded_download)
            .collect::<Vec<_>>();
        let active = persisted.active.and_then(Self::from_persisted).map(Self::recover_loaded_download);
        let finished =
            persisted.finished.into_iter().filter_map(Self::from_persisted).collect::<Vec<_>>();

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
        }
        download
    }

    pub async fn pause_active(&self) {
        *self.control_signal.write().await = DownloadControl::Pause;
        if let Some(download) = self.active.write().await.as_mut() {
            download.paused = true;
            download.state = DownloadState::Paused;
        }
        let _ = self.persist_to_disk().await;
    }

    pub async fn resume_active(&self) {
        *self.control_signal.write().await = DownloadControl::None;
        if let Some(download) = self.active.write().await.as_mut() {
            download.paused = false;
            download.state = DownloadState::Downloading;
        }
        let _ = self.persist_to_disk().await;
    }

    pub async fn cancel_active(&self) {
        *self.control_signal.write().await = DownloadControl::Cancel;
        if let Some(download) = self.active.write().await.as_mut() {
            download.state = DownloadState::Cancelled;
            download.error = Some("Cancelled by user".to_string());
        }
        let _ = self.persist_to_disk().await;
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
            drop(finished);
            self.queue.lock().await.push_back(download);
            let _ = self.persist_to_disk().await;
            true
        } else {
            false
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct FileDownloadRequest {
    pub url: String,
    pub filename: String,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct FileRecordingRequest {
    pub url: String,
    pub filename: String,
    pub start_at: i64,
    pub duration_secs: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

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
            start_at: Some(1_700_000_000),
            duration_secs: Some(5400),
            kind: DownloadKind::Recording,
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
        assert_eq!(restored_scheduled[0].duration_secs, Some(5400));
        assert_eq!(restored_scheduled[0].kind, DownloadKind::Recording);

        let _ = std::fs::remove_file(state_file);
    }

    #[test]
    fn recording_uuid_differs_for_same_url_with_different_start_times() {
        let download_cfg = VideoDownloadConfig {
            directory: "/tmp".to_string(),
            organize_into_directories: false,
            episode_pattern: None,
            headers: std::collections::HashMap::new(),
        };

        let first = FileDownload::new_recording(
            "https://example.com/live/1",
            "recording_1.ts",
            &download_cfg,
            1_700_000_000,
            5400,
        )
        .expect("first recording");
        let second = FileDownload::new_recording(
            "https://example.com/live/1",
            "recording_2.ts",
            &download_cfg,
            1_700_005_400,
            5400,
        )
        .expect("second recording");

        assert_ne!(first.uuid, second.uuid);
    }
}
