use crate::library::MetadataCacheEntry;
use log::error;
use std::path::{Path, PathBuf};

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
    pub async fn new(storage_dir: &Path) -> Self {
        let paths = collect_paths(storage_dir).await;
        Self { paths, index: 0 }
    }

    pub async fn next(&mut self) -> Option<MetadataCacheEntry> {
        while self.index < self.paths.len() {
            let path = &self.paths[self.index];
            self.index += 1;

            match tokio::fs::read_to_string(path).await {
                Ok(content) => match serde_json::from_str::<MetadataCacheEntry>(&content) {
                    Ok(entry) => return Some(entry),
                    Err(e) => {
                        error!("Failed to parse library metadata {}: {}", path.display(), e);
                    }
                },
                Err(e) => {
                    error!("Failed to read library metadata {}: {}", path.display(), e);
                }
            }
        }
        None
    }
}
