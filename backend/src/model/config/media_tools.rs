use crate::utils::ffmpeg::FfmpegExecutor;
use shared::create_bitset;
use tokio::sync::OnceCell;

create_bitset!(u8, MediaToolCapability, Ffmpeg, Ffprobe);

#[derive(Debug, Default)]
pub struct MediaToolCapabilities {
    available: OnceCell<MediaToolCapabilitySet>,
}

impl MediaToolCapabilities {
    #[must_use]
    pub fn new() -> Self {
        Self {
            available: OnceCell::new(),
        }
    }

    pub async fn is_ffmpeg_available(&self) -> bool {
        self.available().await.contains(MediaToolCapability::Ffmpeg)
    }

    pub async fn is_ffprobe_available(&self) -> bool {
        self.available().await.contains(MediaToolCapability::Ffprobe)
    }

    async fn available(&self) -> MediaToolCapabilitySet {
        *self.available.get_or_init(Self::detect_available_tools).await
    }

    async fn detect_available_tools() -> MediaToolCapabilitySet {
        let executor = FfmpegExecutor::new();
        let ffmpeg = executor.check_ffmpeg_availability();
        let ffprobe = executor.check_ffprobe_availability();
        let (ffmpeg_available, ffprobe_available) = tokio::join!(ffmpeg, ffprobe);

        let mut available = MediaToolCapabilitySet::new();
        if ffmpeg_available {
            available.set(MediaToolCapability::Ffmpeg);
        }
        if ffprobe_available {
            available.set(MediaToolCapability::Ffprobe);
        }
        available
    }
}

