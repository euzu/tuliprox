use std::time::Duration;

const HLS_CLIENT_BODY_SEND_TIMEOUT_SECS: u64 = 90;

pub fn hls_client_body_send_deadline() -> Duration { Duration::from_secs(HLS_CLIENT_BODY_SEND_TIMEOUT_SECS) }

pub fn hls_object_body_deadline(timeout_ms: u64) -> Duration { Duration::from_millis(timeout_ms.max(1)) }

#[cfg(test)]
mod tests {
    use super::{hls_client_body_send_deadline, hls_object_body_deadline};
    use std::time::Duration;

    #[test]
    fn object_body_deadline_uses_configured_segment_timeout() {
        assert_eq!(hls_object_body_deadline(10_000), Duration::from_secs(10));
    }

    #[test]
    fn client_body_send_deadline_is_fixed_to_ninety_seconds() {
        assert_eq!(hls_client_body_send_deadline(), Duration::from_secs(90));
    }
}
