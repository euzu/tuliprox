pub fn format_hls_duration_ms(duration_ms: u64) -> String {
    format!("{}.{:03}", duration_ms / 1_000, duration_ms % 1_000)
}

pub const fn hls_target_duration_secs(duration_ms: u64) -> u64 { duration_ms.saturating_add(999) / 1_000 }

#[cfg(test)]
mod tests {
    use super::{format_hls_duration_ms, hls_target_duration_secs};

    #[test]
    fn hls_duration_format_and_target_rounding_share_one_policy() {
        assert_eq!(format_hls_duration_ms(9_750), "9.750");
        assert_eq!(hls_target_duration_secs(9_750), 10);
        assert_eq!(format_hls_duration_ms(10_000), "10.000");
        assert_eq!(hls_target_duration_secs(10_000), 10);
    }
}
