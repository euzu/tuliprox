use std::collections::VecDeque;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use tokio::sync::{RwLock, Mutex};
use std::sync::Arc;
use deunicode::deunicode;
use serde::{Deserialize, Serialize};

use crate::model::{VideoDownloadConfig};
use shared::utils::{CONSTANTS, FILENAME_TRIM_PATTERNS, hash_string_as_hex};

/// File-Download information.
#[derive(Clone)]
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
    /// Optional error if something goes wrong during downloading.
    pub error: Option<String>,
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
                let tmp_filename = CONSTANTS.re_filename.replace_all(&deunicode(req_filename)
                    .replace(' ', "_"), "")
                    .replace("__", "_")
                    .replace("_-_", "-");
                let filename_path = Path::new(&tmp_filename);
                let file_stem = filename_path.file_stem().and_then(OsStr::to_str).unwrap_or("").trim_matches(FILENAME_TRIM_PATTERNS);
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
                    error: None,
                })
            }
            Err(_) => None
        }
    }
}

pub struct DownloadQueue {
    pub queue: Arc<Mutex<VecDeque<FileDownload>>>,
    pub active: Arc<RwLock<Option<FileDownload>>>,
    pub finished: Arc<RwLock<Vec<FileDownload>>>,
}

impl Default for DownloadQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl DownloadQueue {
    pub fn new() -> Self {
        Self {
            queue: Arc::from(Mutex::new(VecDeque::new())),
            active: Arc::from(RwLock::new(None)),
            finished: Arc::from(RwLock::new(Vec::new())),
        }
    }
}


#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct FileDownloadRequest {
    pub url: String,
    pub filename: String,
}
