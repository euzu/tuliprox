use crate::model::ThumbnailConfig;
use log::debug;
use std::path::Path;
use tokio::process::Command;

/// Computes a stable BLAKE3 hash for a file path (or URL).
pub fn file_hash(path: &str) -> String {
    blake3::hash(path.as_bytes()).to_hex().to_string()
}

pub struct ThumbnailExtractor {
    config: ThumbnailConfig,
}

impl ThumbnailExtractor {
    pub fn new(config: ThumbnailConfig) -> Self { Self { config } }

    /// Checks if the system `ffmpeg` binary is available.
    pub async fn check_ffmpeg_availability() -> bool {
        match Command::new("ffmpeg").arg("-version").output().await {
            Ok(output) => output.status.success(),
            Err(_) => false,
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

        self.run_ffmpeg(file_path).await
    }

    // TODO: Future feature: support thumbnail extraction from remote HTTP(S)
    // inputs via ranged reads without tying this logic into the local library
    // scan path yet.

    fn build_scale_filter(&self) -> String {
        format!(
            "scale={}:{}:force_original_aspect_ratio=increase,crop={}:{}",
            self.config.width, self.config.height, self.config.width, self.config.height
        )
    }

    /// Runs ffmpeg to extract a single frame at ~10s into the video.
    /// Falls back to position 0 if the video is shorter than 10s.
    async fn run_ffmpeg(&self, input_path: &str) -> Result<Vec<u8>, String> {
        let temp_dir = tempfile::tempdir()
            .map_err(|e| format!("Failed to create temp dir: {e}"))?;
        let output_path = temp_dir.path().join("thumb.jpg");
        let scale_filter = self.build_scale_filter();
        let quality = self.config.quality.to_string();

        let output = Command::new("ffmpeg")
            .args([
                "-ss", "10",
                "-i", input_path,
                "-frames:v", "1",
                "-vf", &scale_filter,
                "-q:v", &quality,
                "-y",
                &output_path.to_string_lossy(),
            ])
            .output()
            .await
            .map_err(|e| format!("Failed to run ffmpeg: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Retry at position 0 if seeking past end of short video
            if stderr.contains("Output file is empty") || stderr.contains("nothing was encoded") {
                debug!("Video shorter than 10s, retrying at position 0: {input_path}");
                let output = Command::new("ffmpeg")
                    .args([
                        "-ss", "0",
                        "-i", input_path,
                        "-frames:v", "1",
                        "-vf", &scale_filter,
                        "-q:v", &quality,
                        "-y",
                        &output_path.to_string_lossy(),
                    ])
                    .output()
                    .await
                    .map_err(|e| format!("Failed to run ffmpeg retry: {e}"))?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(format!("ffmpeg failed at position 0: {stderr}"));
                }
            } else {
                return Err(format!("ffmpeg failed: {stderr}"));
            }
        }

        tokio::fs::read(&output_path)
            .await
            .map_err(|e| format!("Failed to read thumbnail: {e}"))
    }
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
