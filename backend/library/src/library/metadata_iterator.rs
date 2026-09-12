use crate::library::MetadataCacheEntry;
use log::error;
use std::{
    io,
    path::{Path, PathBuf},
};

pub struct MetadataAsyncIter {
    paths: Vec<PathBuf>,
    index: usize,
}

async fn collect_paths(storage_dir: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(mut read_dir) = tokio::fs::read_dir(storage_dir).await {
        while let Ok(Some(entry)) = read_dir.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                paths.push(path);
            }
        }
    }
    paths
}

impl MetadataAsyncIter {
    /// Opens the canonical catalog without treating an unreadable directory as empty.
    pub async fn try_new(storage_dir: &Path) -> io::Result<Self> {
        let mut paths = Vec::new();
        let mut read_dir = tokio::fs::read_dir(storage_dir).await?;
        while let Some(entry) = read_dir.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                paths.push(path);
            }
        }
        Ok(Self { paths, index: 0 })
    }

    pub async fn new(storage_dir: &Path) -> Self {
        let paths = collect_paths(storage_dir).await;
        Self { paths, index: 0 }
    }

    pub async fn next(&mut self) -> Option<MetadataCacheEntry> {
        while self.index < self.paths.len() {
            match self.try_next().await {
                Ok(entry) => return entry,
                Err(e) => error!("Failed to read library metadata: {e}"),
            }
        }
        None
    }

    /// Reads a stored entry without hiding corruption or read failures from publishers.
    pub async fn try_next(&mut self) -> io::Result<Option<MetadataCacheEntry>> {
        let Some(path) = self.paths.get(self.index) else { return Ok(None) };
        self.index += 1;
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|err| io::Error::new(err.kind(), format!("{}: {err}", path.display())))?;
        serde_json::from_str(&content)
            .map(Some)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {err}", path.display())))
    }
}
