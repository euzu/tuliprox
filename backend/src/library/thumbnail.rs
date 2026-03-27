use crate::model::ThumbnailConfig;
use crate::utils::ffmpeg::FfmpegExecutor;
use std::path::Path;

/// Computes a stable BLAKE3 hash for a file path (or URL).
pub fn file_hash(path: &str) -> String {
    blake3::hash(path.as_bytes()).to_hex().to_string()
}

pub struct ThumbnailExtractor {
    config: ThumbnailConfig,
    ffmpeg: FfmpegExecutor,
}

impl ThumbnailExtractor {
    pub fn new(config: ThumbnailConfig) -> Self {
        Self {
            config,
            ffmpeg: FfmpegExecutor::new(),
        }
    }

    /// Extracts a thumbnail from a local file. Returns JPEG bytes on success.
    ///
    /// The library scan already runs in a detached background task, so thumbnail
    /// extraction here must stay simple and must not introduce its own parallel
    /// ffmpeg scheduling inside the scan worker.
    pub async fn extract_from_file(&self, file_path: &str) -> Result<Vec<u8>, String> {
        let path = Path::new(file_path);
        if !path.exists() {
            return Err(format!("File not found: {file_path}"));
        }

        self.ffmpeg
            .create_thumbnail(file_path, self.config.width, self.config.height)
            .await
    }

    // TODO: Future feature: support thumbnail extraction from remote HTTP(S)
    // inputs via ranged reads without tying this logic into the local library
    // scan path yet.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_hash_deterministic() {
        let h1 = file_hash("/some/path/video.mkv");
        let h2 = file_hash("/some/path/video.mkv");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_file_hash_different_paths() {
        let h1 = file_hash("/a/video.mkv");
        let h2 = file_hash("/b/video.mkv");
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_thumbnail_url_format() {
        let url = format!("/api/v1/library/thumbnail/{}", "test-uuid-123");
        assert_eq!(url, "/api/v1/library/thumbnail/test-uuid-123");
    }
}
