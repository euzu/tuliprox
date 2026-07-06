use std::{pin::Pin, time::Duration};
use tokio::time::Sleep;

const HLS_CLIENT_BODY_SEND_TIMEOUT_SECS: u64 = 90;

pub fn hls_client_body_send_deadline() -> Duration { Duration::from_secs(HLS_CLIENT_BODY_SEND_TIMEOUT_SECS) }

pub fn refresh_hls_client_body_send_deadline(mut deadline: Pin<&mut Sleep>) {
    deadline.as_mut().reset(tokio::time::Instant::now() + hls_client_body_send_deadline());
}

pub fn hls_object_body_deadline(timeout_ms: u64) -> Duration { Duration::from_millis(timeout_ms.max(1)) }

#[cfg(test)]
mod tests {
    use super::{hls_client_body_send_deadline, hls_object_body_deadline, refresh_hls_client_body_send_deadline};
    use std::time::Duration;

    #[test]
    fn object_body_deadline_uses_configured_segment_timeout() {
        assert_eq!(hls_object_body_deadline(10_000), Duration::from_secs(10));
    }

    #[test]
    fn client_body_send_deadline_is_fixed_to_ninety_seconds() {
        assert_eq!(hls_client_body_send_deadline(), Duration::from_secs(90));
    }

    #[tokio::test]
    async fn client_body_send_deadline_refreshes_after_progress() {
        let mut deadline = Box::pin(tokio::time::sleep(Duration::ZERO));
        let expired_at = deadline.deadline();

        refresh_hls_client_body_send_deadline(deadline.as_mut());

        assert!(deadline.deadline() > expired_at);
    }
}
