use super::{hls_session_idle_protection_retry_at, HLS_SESSION_IDLE_PROTECTION_RETRY_MS};

#[test]
fn protected_idle_session_retry_cannot_form_a_millisecond_busy_loop() {
    let now_ms = 50_000_u64;
    assert_eq!(
        hls_session_idle_protection_retry_at(now_ms.saturating_sub(1), now_ms),
        now_ms.saturating_add(HLS_SESSION_IDLE_PROTECTION_RETRY_MS)
    );
    assert_eq!(hls_session_idle_protection_retry_at(60_000, now_ms), 60_000);
}
