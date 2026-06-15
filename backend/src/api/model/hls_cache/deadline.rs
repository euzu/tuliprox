use std::time::Duration;

pub fn hls_object_body_deadline(timeout_ms: u64) -> Duration { Duration::from_millis(timeout_ms.max(1)) }

#[cfg(test)]
mod tests {
    use super::hls_object_body_deadline;
    use std::time::Duration;

    #[test]
    fn object_body_deadline_uses_configured_segment_timeout() {
        assert_eq!(hls_object_body_deadline(10_000), Duration::from_secs(10));
    }
}
