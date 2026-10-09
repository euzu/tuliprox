use super::*;
use crate::defaults::{
    MAX_ICS_DAYS_FUTURE, MAX_ICS_DAYS_PAST, MAX_ICS_DECOMPRESSED_BYTES_HARD_LIMIT, MAX_ICS_DESCRIPTION_LENGTH,
    MAX_ICS_DOWNLOAD_BYTES_HARD_LIMIT, MAX_ICS_EVENTS_HARD_LIMIT, MAX_ICS_SUMMARY_LENGTH,
};

fn xmltv_source(url: &str) -> EpgSourceDto { EpgSourceDto { url: url.to_owned(), ..EpgSourceDto::default() } }

fn ics_source(url: &str, channel_id: Option<&str>) -> EpgSourceDto {
    EpgSourceDto {
        source_type: EpgSourceTypeDto::Ics,
        url: url.to_owned(),
        channel_id: channel_id.map(ToOwned::to_owned),
        ..EpgSourceDto::default()
    }
}

#[test]
fn xmltv_source_without_type_remains_valid() {
    let mut cfg =
        EpgConfigDto { sources: Some(vec![xmltv_source("http://example.com/xmltv.php")]), ..Default::default() };
    cfg.prepare(|| Err("no auto".to_owned()), true).expect("prepare failed");
    assert_eq!(cfg.t_sources.len(), 1);
    assert_eq!(cfg.t_sources[0].source_type, EpgSourceTypeDto::Xmltv);
    assert_eq!(cfg.t_sources[0].url, "http://example.com/xmltv.php");
}

#[test]
fn provider_scheme_kept_unresolved() {
    let mut source = xmltv_source("provider://myprovider/xmltv.php?username=u&password=p");
    source.priority = 1;
    source.logo_override = true;
    let mut cfg = EpgConfigDto { sources: Some(vec![source]), ..Default::default() };
    cfg.prepare(|| Err("no auto".to_owned()), true).expect("prepare failed");
    assert_eq!(cfg.t_sources.len(), 1);
    assert_eq!(cfg.t_sources[0].url, "provider://myprovider/xmltv.php?username=u&password=p");
    assert_eq!(cfg.t_sources[0].priority, 1);
    assert!(cfg.t_sources[0].logo_override);
}

#[test]
fn xmltv_auto_url_used() {
    let mut cfg = EpgConfigDto { sources: Some(vec![xmltv_source(AUTO_URL)]), ..Default::default() };
    cfg.prepare(|| Ok("http://auto.example.com/xmltv.php?username=u&password=p".to_owned()), true)
        .expect("prepare failed");
    assert_eq!(cfg.t_sources.len(), 1);
    assert!(cfg.t_sources[0].url.starts_with("http://auto.example.com/"));
}

#[test]
fn include_computed_false_skips_resolution() {
    let mut cfg =
        EpgConfigDto { sources: Some(vec![xmltv_source("provider://myprovider/xmltv.php")]), ..Default::default() };
    cfg.prepare(|| Err("no auto".to_owned()), false).expect("prepare with include_computed=false should succeed");
    assert!(cfg.t_sources.is_empty());
}

#[test]
fn ics_auto_url_is_rejected() {
    let mut cfg = EpgConfigDto { sources: Some(vec![ics_source(AUTO_URL, Some("f1.calendar"))]), ..Default::default() };
    let err = cfg.prepare(|| Ok("http://auto.example.com/xmltv.php".to_owned()), true).unwrap_err();
    assert!(err.to_string().contains("only supported for XMLTV"));
}

#[test]
fn ics_requires_channel_id() {
    let mut cfg =
        EpgConfigDto { sources: Some(vec![ics_source("https://example.com/f1.ics", None)]), ..Default::default() };
    let err = cfg.prepare(|| Err("no auto".to_owned()), true).unwrap_err();
    assert!(err.to_string().contains("channel_id is required"));
}

#[test]
fn ics_webcal_url_is_normalized_to_https() {
    for url in ["webcal://example.com/f1.ics", "WEBcal://example.com/f1.ics"] {
        let mut cfg = EpgConfigDto { sources: Some(vec![ics_source(url, Some("f1.calendar"))]), ..Default::default() };
        cfg.prepare(|| Err("no auto".to_owned()), true).expect("prepare failed");
        assert_eq!(cfg.t_sources[0].url, "https://example.com/f1.ics");
    }
}

#[test]
fn ics_unicode_local_path_is_accepted_without_panicking() {
    let mut source = ics_source("äääää/calendar.ics", Some("calendar"));
    source.prepare().expect("unicode local path should be valid");
}

#[test]
fn ics_absolute_windows_paths_are_local_sources() {
    for path in [r"C:\calendar.ics", "D:/calendar.ics", r"\\server\share\calendar.ics"] {
        let mut source = ics_source(path, Some("calendar"));
        source.prepare().expect("absolute Windows path should be valid");
    }
}

#[test]
fn xmltv_rejects_ics_only_fields() {
    let mut with_ics_block = xmltv_source("https://example.com/epg.xml");
    with_ics_block.ics = Some(IcsEpgSourceConfigDto::default());
    let err = with_ics_block.prepare().unwrap_err();
    assert!(err.to_string().contains("only supported for ICS"));

    let mut with_channel_id = xmltv_source("https://example.com/epg.xml");
    with_channel_id.channel_id = Some("not-an-xmltv-option".to_string());
    let err = with_channel_id.prepare().unwrap_err();
    assert!(err.to_string().contains("only supported for ICS"));
}

#[test]
fn ics_unsupported_scheme_is_rejected() {
    let mut cfg = EpgConfigDto {
        sources: Some(vec![ics_source("ftp://example.com/f1.ics", Some("f1.calendar"))]),
        ..Default::default()
    };
    let err = cfg.prepare(|| Err("no auto".to_owned()), true).unwrap_err();
    assert!(err.to_string().contains("Unsupported ICS url scheme"));
}

#[test]
fn ics_match_names_are_trimmed_and_empty_values_removed() {
    let mut source = ics_source("https://example.com/f1.ics", Some(" f1.calendar "));
    source.channel_title = Some(" Formula 1 ".to_owned());
    source.match_names = vec![" F1 ".to_owned(), String::new(), "  ".to_owned(), "Formel 1".to_owned()];
    let mut cfg = EpgConfigDto { sources: Some(vec![source]), ..Default::default() };
    cfg.prepare(|| Err("no auto".to_owned()), true).expect("prepare failed");
    assert_eq!(cfg.t_sources[0].channel_id.as_deref(), Some("f1.calendar"));
    assert_eq!(cfg.t_sources[0].channel_title.as_deref(), Some("Formula 1"));
    assert_eq!(cfg.t_sources[0].match_names, vec!["F1", "Formel 1"]);
}

#[test]
fn aliases_field_is_rejected() {
    let yaml = r"
type: ics
url: https://example.com/f1.ics
channel_id: f1.calendar
aliases:
  - F1
";
    let err = serde_saphyr::from_str::<EpgSourceDto>(yaml).unwrap_err();
    assert!(err.to_string().contains("aliases") || err.to_string().contains("unknown field"));
}

#[test]
fn ics_block_hours_must_divide_day_evenly() {
    let mut valid = ics_source("https://example.com/f1.ics", Some("f1.calendar"));
    valid.ics = Some(IcsEpgSourceConfigDto {
        dummy: IcsDummyConfigDto { block_hours: 4, ..IcsDummyConfigDto::default() },
        ..IcsEpgSourceConfigDto::default()
    });
    valid.prepare().expect("valid block size");

    let mut invalid = ics_source("https://example.com/f1.ics", Some("f1.calendar"));
    invalid.ics = Some(IcsEpgSourceConfigDto {
        dummy: IcsDummyConfigDto { block_hours: 5, ..IcsDummyConfigDto::default() },
        ..IcsEpgSourceConfigDto::default()
    });
    let err = invalid.prepare().unwrap_err();
    assert!(err.to_string().contains("must divide 24 evenly"));
}

#[test]
fn invalid_ics_timezone_is_rejected() {
    let mut source = ics_source("https://example.com/f1.ics", Some("f1.calendar"));
    source.ics =
        Some(IcsEpgSourceConfigDto { timezone: "Mars/Olympus".to_string(), ..IcsEpgSourceConfigDto::default() });

    let err = source.prepare().unwrap_err();

    assert!(err.to_string().contains("ics.timezone"));
}

#[test]
fn ics_max_download_bytes_above_hard_cap_is_rejected() {
    let mut source = ics_source("https://example.com/f1.ics", Some("f1.calendar"));
    source.ics = Some(IcsEpgSourceConfigDto {
        max_download_bytes: MAX_ICS_DOWNLOAD_BYTES_HARD_LIMIT + 1,
        ..IcsEpgSourceConfigDto::default()
    });

    let err = source.prepare().unwrap_err();

    assert!(err.to_string().contains("ics.max_download_bytes"));
}

#[test]
fn ics_max_decompressed_bytes_above_hard_cap_is_rejected() {
    let mut source = ics_source("https://example.com/f1.ics", Some("f1.calendar"));
    source.ics = Some(IcsEpgSourceConfigDto {
        max_decompressed_bytes: MAX_ICS_DECOMPRESSED_BYTES_HARD_LIMIT + 1,
        ..IcsEpgSourceConfigDto::default()
    });

    let err = source.prepare().unwrap_err();

    assert!(err.to_string().contains("ics.max_decompressed_bytes"));
}

#[test]
fn ics_max_events_above_hard_cap_is_rejected() {
    let mut source = ics_source("https://example.com/f1.ics", Some("f1.calendar"));
    source.ics =
        Some(IcsEpgSourceConfigDto { max_events: MAX_ICS_EVENTS_HARD_LIMIT + 1, ..IcsEpgSourceConfigDto::default() });

    let err = source.prepare().unwrap_err();

    assert!(err.to_string().contains("ics.max_events"));
}

#[test]
fn ics_dummy_days_above_hard_caps_are_rejected() {
    let mut too_much_past = ics_source("https://example.com/f1.ics", Some("f1.calendar"));
    too_much_past.ics = Some(IcsEpgSourceConfigDto {
        dummy: IcsDummyConfigDto { days_past: MAX_ICS_DAYS_PAST + 1, ..IcsDummyConfigDto::default() },
        ..IcsEpgSourceConfigDto::default()
    });
    let err = too_much_past.prepare().unwrap_err();
    assert!(err.to_string().contains("ics.dummy.days_past"));

    let mut too_much_future = ics_source("https://example.com/f1.ics", Some("f1.calendar"));
    too_much_future.ics = Some(IcsEpgSourceConfigDto {
        dummy: IcsDummyConfigDto { days_future: MAX_ICS_DAYS_FUTURE + 1, ..IcsDummyConfigDto::default() },
        ..IcsEpgSourceConfigDto::default()
    });
    let err = too_much_future.prepare().unwrap_err();
    assert!(err.to_string().contains("ics.dummy.days_future"));
}

#[test]
fn ics_extreme_template_and_dummy_texts_are_rejected() {
    let mut source = ics_source("https://example.com/f1.ics", Some("f1.calendar"));
    source.ics = Some(IcsEpgSourceConfigDto {
        event: IcsEventMappingDto { title: "x".repeat(MAX_ICS_SUMMARY_LENGTH + 1), ..IcsEventMappingDto::default() },
        ..IcsEpgSourceConfigDto::default()
    });
    let err = source.prepare().unwrap_err();
    assert!(err.to_string().contains("ics.event.title"));

    let mut source = ics_source("https://example.com/f1.ics", Some("f1.calendar"));
    source.ics = Some(IcsEpgSourceConfigDto {
        dummy: IcsDummyConfigDto {
            description: "x".repeat(MAX_ICS_DESCRIPTION_LENGTH + 1),
            ..IcsDummyConfigDto::default()
        },
        ..IcsEpgSourceConfigDto::default()
    });
    let err = source.prepare().unwrap_err();
    assert!(err.to_string().contains("ics.dummy.description"));
}
