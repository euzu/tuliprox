use super::{
    ConfigTargetDto, ConfigTargetFilterDto, ConfigTargetOptions, ConfigTargetShareLiveStreams, EpgOutputOptions,
    M3uTargetOutputDto, StrmTargetOutputDto, TargetOutputDto, XtreamTargetOutputDto,
};

fn target_with_outputs(output: Vec<TargetOutputDto>) -> ConfigTargetDto {
    ConfigTargetDto {
        name: "target".to_string(),
        filter: "Group ~ \".*\"".into(),
        output,
        ..ConfigTargetDto::default()
    }
}

fn strm_with_username() -> TargetOutputDto {
    TargetOutputDto::Strm(StrmTargetOutputDto {
        directory: "/tmp/strm".to_string(),
        username: Some("alice".to_string()),
        ..StrmTargetOutputDto::default()
    })
}

fn strm_without_username() -> TargetOutputDto {
    TargetOutputDto::Strm(StrmTargetOutputDto { directory: "/tmp/strm".to_string(), ..StrmTargetOutputDto::default() })
}

fn xtream_output() -> TargetOutputDto { TargetOutputDto::Xtream(XtreamTargetOutputDto::default()) }

#[test]
fn shipped_tmdb_example_is_safe_by_default_and_prepares_when_explicitly_enabled() {
    let sources: crate::model::SourcesConfigDto =
        serde_saphyr::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../config/source.yml"))).unwrap();
    let mut target = sources
        .sources
        .into_iter()
        .flat_map(|source| source.targets)
        .find(|target| target.name == "tmdb-discovery-example")
        .expect("shipped TMDB example");
    assert!(!target.enabled);
    assert!(!target.curation.as_ref().unwrap().enabled);
    assert!(target.curation.as_ref().unwrap().tmdb.as_ref().unwrap().api.access_token.is_empty());
    target.prepare(1, None, None).unwrap();
    target.enabled = true;
    target.curation.as_mut().unwrap().enabled = true;
    target.prepare(1, None, None).unwrap();
    let yaml = serde_saphyr::to_string(&target).unwrap();
    let restored: ConfigTargetDto = serde_saphyr::from_str(&yaml).unwrap();
    assert_eq!(restored.curation, target.curation);
}

#[test]
fn target_curation_prepares_and_round_trips_tmdb_only_and_mixed_sources() {
    for with_trakt in [false, true] {
        let mut value = serde_json::json!({"name": "discovery", "output": [{"type": "xtream"}, {"type": "m3u"}],
                "curation": {"catalog_selection": "curated", "tmdb": {"trending": [{"kind": "movie", "time_window": "week", "limit": 37, "category_name": "TMDB"}]}}});
        if with_trakt {
            value["curation"]["trakt"] =
                serde_json::json!({"charts": [{"kind": "movies", "chart": "popular", "category_name": "Trakt"}]});
        }
        let mut target: ConfigTargetDto = serde_json::from_value(value).unwrap();
        target.prepare(1, None, None).unwrap();
        let restored: ConfigTargetDto = serde_saphyr::from_str(&serde_saphyr::to_string(&target).unwrap()).unwrap();
        assert_eq!(restored.curation, target.curation);
        assert!(matches!(&restored.output[0], TargetOutputDto::Xtream(output) if output.trakt.is_none()));
    }
}

#[test]
fn target_curation_rejects_dual_declarations_before_preparing_legacy_values() {
    let mut target: ConfigTargetDto = serde_json::from_value(serde_json::json!({"name": "discovery",
            "curation": {"enabled": false}, "output": [{"type": "xtream", "trakt": {"catalog_selection": "curated"}}]}))
    .unwrap();
    assert!(target.prepare(1, None, None).unwrap_err().to_string().contains("output[].trakt"));
}

#[test]
fn target_curation_m3u_only_requires_selection_only_entries() {
    let mut target: ConfigTargetDto = serde_json::from_value(serde_json::json!({"name": "discovery", "output": [{"type": "m3u"}],
            "curation": {"catalog_selection": "curated", "tmdb": {"trending": [{"kind": "tv", "time_window": "day", "limit": 500, "category_name": "Shows"}]}}})).unwrap();
    assert!(target.prepare(1, None, None).is_err());
    target.curation.as_mut().unwrap().tmdb.as_mut().unwrap().trending[0].create_xtream_category = false;
    target.prepare(1, None, None).unwrap();
}

#[test]
fn target_curation_absence_and_null_keep_the_legacy_surface() {
    for value in [
        serde_json::json!({"name": "discovery", "output": [{"type": "xtream", "trakt": {}}]}),
        serde_json::json!({"name": "discovery", "curation": null, "output": [{"type": "xtream", "trakt": {}}]}),
    ] {
        let mut target: ConfigTargetDto = serde_json::from_value(value).unwrap();
        target.prepare(1, None, None).unwrap();
        let serialized = serde_json::to_value(target).unwrap();
        assert!(serialized.get("curation").is_none());
        assert!(serialized["output"][0].get("trakt").is_some());
    }
}

#[test]
fn target_filter_string_roundtrips_as_string() {
    let dto = serde_saphyr::from_str::<ConfigTargetDto>(
        r#"
name: target
filter: 'Group ~ ".*"'
output:
  - type: m3u
"#,
    )
    .expect("legacy target filter should deserialize");

    assert_eq!(dto.filter.processing.as_deref(), Some(r#"Group ~ ".*""#));
    assert_eq!(dto.filter.persist, None);
    let serialized = serde_saphyr::to_string(&dto).expect("target should serialize");
    assert!(serialized.contains("filter: Group ~"), "expected scalar filter, got: {serialized}");
    assert!(!serialized.contains("processing:"), "processing-only filter must stay scalar: {serialized}");
}

#[test]
fn target_without_filter_defaults_to_no_stages_and_omits_filter() {
    let dto = serde_saphyr::from_str::<ConfigTargetDto>(
        r"
name: target
output:
  - type: m3u
",
    )
    .expect("target without filter should deserialize");

    assert!(dto.filter.is_empty());
    let serialized = serde_saphyr::to_string(&dto).expect("target should serialize");
    assert!(!serialized.contains("filter:"), "empty filter should be omitted: {serialized}");
}

#[test]
fn target_filter_stages_roundtrip_as_mapping() {
    let dto = serde_saphyr::from_str::<ConfigTargetDto>(
        r#"
name: target
filter:
  processing: 'Group ~ ".*"'
  persist: 'EpgId ~ ".+"'
output:
  - type: m3u
"#,
    )
    .expect("staged target filter should deserialize");

    assert_eq!(dto.filter.processing.as_deref(), Some(r#"Group ~ ".*""#));
    assert_eq!(dto.filter.persist.as_deref(), Some(r#"EpgId ~ ".+""#));
    let serialized = serde_saphyr::to_string(&dto).expect("target should serialize");
    assert!(serialized.contains("processing:"), "expected staged filter mapping: {serialized}");
    assert!(serialized.contains("persist:"), "expected persist filter: {serialized}");
}

#[test]
fn staged_target_filter_rejects_empty_mapping() {
    let result = serde_saphyr::from_str::<ConfigTargetFilterDto>("{}\n");
    assert!(result.is_err());
}

#[test]
fn staged_target_filter_accepts_missing_processing_stage() {
    let filter = serde_saphyr::from_str::<ConfigTargetFilterDto>(
        r#"persist: 'EpgId ~ ".+"'
"#,
    )
    .expect("persist-only filter should deserialize");

    assert_eq!(filter.processing, None);
    assert_eq!(filter.persist.as_deref(), Some(r#"EpgId ~ ".+""#));
}

#[test]
fn target_filter_treats_empty_and_whitespace_stages_as_match_all() {
    for value in ["", " \t\r\n "] {
        let mut filter = ConfigTargetFilterDto {
            processing: Some(value.to_string()),
            persist: Some(value.to_string()),
            ..ConfigTargetFilterDto::default()
        };

        filter.prepare(None).expect("blank filter stages should prepare");

        assert!(filter.t_processing.is_none());
        assert!(filter.t_persist.is_none());
    }
}

#[test]
fn processing_only_mapping_serializes_as_scalar() {
    let filter = serde_saphyr::from_str::<ConfigTargetFilterDto>(
        r#"processing: 'Group ~ ".*"'
"#,
    )
    .expect("processing-only mapping should deserialize");

    let serialized = serde_saphyr::to_string(&filter).expect("processing filter should serialize");
    assert!(!serialized.contains("processing:"), "processing-only filter must become scalar: {serialized}");
    assert!(serialized.contains("Group ~"));
}

#[test]
fn target_filter_persist_roundtrips_as_mapping() {
    let filter = ConfigTargetFilterDto {
        processing: None,
        persist: Some(r#"EpgId ~ ".+""#.to_string()),
        ..ConfigTargetFilterDto::default()
    };

    let serialized = serde_saphyr::to_string(&filter).expect("target filter should serialize");
    assert!(!serialized.contains("processing:"));
    assert!(serialized.contains("persist:"));
    let reparsed =
        serde_saphyr::from_str::<ConfigTargetFilterDto>(&serialized).expect("target filter should deserialize");
    assert_eq!(reparsed, filter);
}

#[test]
fn strm_with_username_is_allowed_with_m3u_output() {
    let mut target =
        target_with_outputs(vec![TargetOutputDto::M3u(M3uTargetOutputDto::default()), strm_with_username()]);

    assert!(target.prepare(1, None, None).is_ok());
}

#[test]
fn strm_with_username_is_allowed_with_xtream_output() {
    let mut target = target_with_outputs(vec![xtream_output(), strm_with_username()]);

    assert!(target.prepare(1, None, None).is_ok());
}

#[test]
fn strm_without_username_is_allowed_with_m3u_output() {
    let mut target =
        target_with_outputs(vec![TargetOutputDto::M3u(M3uTargetOutputDto::default()), strm_without_username()]);

    assert!(target.prepare(1, None, None).is_ok());
}

#[test]
fn strm_with_username_requires_m3u_or_xtream_output() {
    let mut target = target_with_outputs(vec![strm_with_username()]);

    let err = target.prepare(1, None, None).expect_err("STRM username without stream output should fail");

    assert!(err.to_string().contains("xtream or m3u output"));
}

#[test]
fn strm_without_username_requires_m3u_or_xtream_output() {
    let mut target = target_with_outputs(vec![strm_without_username()]);

    let err = target.prepare(1, None, None).expect_err("STRM without stream output should fail");

    assert!(err.to_string().contains("xtream or m3u output"));
}

#[test]
fn target_options_deserialize_structured_share_live_streams() {
    let yaml = r"
share_live_streams:
  hls: true
  mpeg_ts: true
";

    let options: ConfigTargetOptions =
        serde_saphyr::from_str(yaml).expect("structured share_live_streams should deserialize");

    assert!(options.share_live_hls_enabled());
    assert!(options.share_live_mpeg_ts_enabled());
    assert!(options.share_live_any_enabled());
}

#[test]
fn target_options_maps_legacy_true_share_live_streams_to_both_modes() {
    let yaml = r"
share_live_streams: true
";

    let options = serde_saphyr::from_str::<ConfigTargetOptions>(yaml);

    assert!(options.is_ok(), "legacy boolean should deserialize: {options:?}");
    if let Ok(options) = options {
        assert_eq!(options.share_live_streams, ConfigTargetShareLiveStreams { hls: false, mpeg_ts: true });
    }
}

#[test]
fn target_options_maps_legacy_false_share_live_streams_to_both_modes() {
    let yaml = r"
share_live_streams: false
";

    let options: ConfigTargetOptions =
        serde_saphyr::from_str(yaml).expect("legacy false share_live_streams should deserialize");

    assert_eq!(options.share_live_streams, ConfigTargetShareLiveStreams { hls: false, mpeg_ts: false });
}

#[test]
fn target_options_omit_default_share_live_streams() {
    let options = ConfigTargetOptions::default();

    assert!(options.is_empty());
    assert!(!options.clear_invalid_epg_ids());

    let serialized = serde_saphyr::to_string(&options).expect("default options should serialize");
    assert!(
        !serialized.contains("share_live_streams"),
        "default share_live_streams should be omitted, got: {serialized}"
    );
}

#[test]
fn target_options_round_trips_partial_share_live_streams() {
    let options = ConfigTargetOptions {
        share_live_streams: ConfigTargetShareLiveStreams { hls: true, mpeg_ts: false },
        ..ConfigTargetOptions::default()
    };

    let serialized = serde_saphyr::to_string(&options).expect("partial share_live_streams should serialize");
    let reparsed: ConfigTargetOptions =
        serde_saphyr::from_str(&serialized).expect("partial share_live_streams should deserialize");

    assert_eq!(reparsed.share_live_streams, options.share_live_streams);
}

#[test]
fn target_options_clear_invalid_epg_ids_roundtrips_and_accepts_legacy_alias() {
    let options = serde_saphyr::from_str::<ConfigTargetOptions>("required_epg: true\n")
        .expect("legacy required_epg should deserialize");

    assert!(options.clear_invalid_epg_ids());
    assert!(!options.is_empty());

    let serialized = serde_saphyr::to_string(&options).expect("clear_invalid_epg_ids should serialize");
    assert!(serialized.contains("clear_invalid_epg_ids: true"));
    assert!(!serialized.contains("required_epg:"));
}

#[test]
fn target_options_default_epg_output_is_disabled_and_omitted() {
    let options = serde_saphyr::from_str::<ConfigTargetOptions>("{}")
        .expect("target options without epg_output should deserialize");

    assert!(!options.lowercase_epg_ids());
    assert!(!options.lowercase_xmltv_display_names());
    assert!(options.epg_output.is_empty());
    assert!(options.is_empty());

    let serialized = serde_saphyr::to_string(&options).expect("default target options should serialize");
    assert!(!serialized.contains("epg_output"), "default epg_output should be omitted, got: {serialized}");
}

#[test]
fn target_options_epg_output_roundtrips() {
    let yaml = r"
epg_output:
  lowercase_ids: true
  lowercase_xmltv_display_names: true
";

    let options =
        serde_saphyr::from_str::<ConfigTargetOptions>(yaml).expect("configured epg_output should deserialize");

    assert!(options.lowercase_epg_ids());
    assert!(options.lowercase_xmltv_display_names());
    assert!(!options.is_empty());

    let serialized = serde_saphyr::to_string(&options).expect("configured epg_output should serialize");
    let roundtripped =
        serde_saphyr::from_str::<ConfigTargetOptions>(&serialized).expect("serialized epg_output should deserialize");
    assert_eq!(roundtripped, options);
}

#[test]
fn target_options_epg_output_makes_options_nonempty() {
    let lowercase_ids = ConfigTargetOptions {
        epg_output: EpgOutputOptions { lowercase_ids: true, ..EpgOutputOptions::default() },
        ..ConfigTargetOptions::default()
    };
    let lowercase_display_names = ConfigTargetOptions {
        epg_output: EpgOutputOptions { lowercase_xmltv_display_names: true, ..EpgOutputOptions::default() },
        ..ConfigTargetOptions::default()
    };

    assert!(!lowercase_ids.is_empty());
    assert!(!lowercase_display_names.is_empty());
}

#[test]
fn target_options_reject_unknown_epg_output_fields() {
    let yaml = r"
epg_output:
  lowercase_id: true
";

    let result = serde_saphyr::from_str::<ConfigTargetOptions>(yaml);

    assert!(result.is_err(), "unknown epg_output fields must be rejected");
}

#[test]
fn target_options_mpeg_ts_helper_keeps_existing_stream_share_semantics() {
    let hls_only = ConfigTargetOptions {
        share_live_streams: ConfigTargetShareLiveStreams { hls: true, mpeg_ts: false },
        ..Default::default()
    };
    let mpeg_ts = ConfigTargetOptions {
        share_live_streams: ConfigTargetShareLiveStreams { hls: false, mpeg_ts: true },
        ..Default::default()
    };

    assert!(hls_only.share_live_hls_enabled());
    assert!(!hls_only.share_live_mpeg_ts_enabled());
    assert!(mpeg_ts.share_live_mpeg_ts_enabled());
}
