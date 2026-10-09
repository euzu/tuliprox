use super::*;

#[test]
fn startup_defaults_and_modes_roundtrip() -> Result<(), Box<dyn std::error::Error>> {
    let mut legacy: HlsCacheConfigDto = serde_json::from_str("{}")?;
    legacy.prepare()?;
    assert_eq!(legacy.startup.mode, HlsStartupMode::Conservative);
    assert!(serde_json::to_value(&legacy)?.get("startup").is_none());
    for mode in [HlsStartupMode::Conservative, HlsStartupMode::FirstReady, HlsStartupMode::Progressive] {
        let startup = HlsStartupConfigDto { mode, max_progressive_segments: 7, ..Default::default() };
        startup.prepare()?;
        let serialized = serde_json::to_string(&startup)?;
        assert_eq!(serde_json::from_str::<HlsStartupConfigDto>(&serialized)?, startup);
    }
    assert!(serde_json::from_str::<HlsStartupConfigDto>(r#"{"mode":"unknown"}"#).is_err());
    assert!(serde_json::from_str::<HlsStartupConfigDto>(r#"{"typo":true}"#).is_err());
    Ok(())
}

#[test]
fn invalid_startup_limits_are_rejected() {
    for config in [
        HlsStartupConfigDto { max_progressive_segments: 0, ..Default::default() },
        HlsStartupConfigDto { max_progressive_bytes_per_segment: ByteSize::new("bad"), ..Default::default() },
        HlsStartupConfigDto { max_progressive_bytes_per_segment: ByteSize::new("0MB"), ..Default::default() },
        HlsStartupConfigDto { max_progressive_bytes_total: ByteSize::new("1MB"), ..Default::default() },
        HlsStartupConfigDto { max_progressive_reader_lifetime_secs: Secs::new(0), ..Default::default() },
        HlsStartupConfigDto { max_deferred_repairs: 0, ..Default::default() },
    ] {
        assert!(config.prepare().is_err());
    }
}
