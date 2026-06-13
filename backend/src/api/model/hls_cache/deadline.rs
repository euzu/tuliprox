use super::HlsSessionHandle;
use std::time::Duration;

pub const HLS_OBJECT_BODY_FALLBACK_TIMEOUT_MS: u64 = 10_000;

pub fn hls_object_body_deadline(target_duration_secs: Option<u32>, fallback_timeout_ms: u64) -> Duration {
    let deadline_ms =
        target_duration_secs.map_or(fallback_timeout_ms.max(1), |duration| u64::from(duration).saturating_mul(1_000));
    Duration::from_millis(deadline_ms.max(1))
}

pub async fn hls_session_object_body_deadline(
    session: &HlsSessionHandle,
    fallback_timeout_ms: u64,
) -> Duration {
    hls_object_body_deadline(session.read().await.target_duration, fallback_timeout_ms)
}

#[cfg(test)]
mod tests {
    use super::hls_object_body_deadline;
    use std::time::Duration;

    #[test]
    fn target_duration_wins_over_fallback_for_object_body_deadline() {
        assert_eq!(hls_object_body_deadline(Some(12), 10_000), Duration::from_secs(12));
    }

    #[test]
    fn missing_target_duration_uses_segment_timeout_fallback() {
        assert_eq!(hls_object_body_deadline(None, 10_000), Duration::from_secs(10));
    }
}
