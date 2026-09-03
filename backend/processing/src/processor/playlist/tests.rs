#![allow(clippy::wildcard_imports)]
use super::*;
use shared::{
    foundation::{get_filter, MapperScript, ValueProvider},
    model::{
        ClusterFlags, ConfigInputDto, ConfigRenameDto, ConfigTargetDto, ConfigTargetOptions, FieldSetAccessor,
        ItemField, M3uPlaylistItem, MappingStage, PlaylistEntry, PlaylistItem, PlaylistItemHeader, PlaylistItemType,
        XtreamCluster, XtreamPlaylistItem,
    },
    utils::Internable,
};
use tuliprox_core::model::{CompiledMappingRule, CompiledTargetMappings, Config, ConfigInputAlias};

fn serialize_without_trailing_fields<T: serde::Serialize>(value: &T, trailing_fields: &[u8]) -> Vec<u8> {
    let mut encoded = rmp_serde::to_vec(value).expect("playlist item should serialize");
    for expected in trailing_fields {
        assert_eq!(encoded.pop(), Some(*expected), "unexpected trailing MessagePack field");
    }
    let removed = trailing_fields.len();
    match encoded[0] {
        marker @ 0x92..=0x9f => {
            let len = usize::from(marker - 0x90);
            assert!(len >= removed, "trailing field count exceeds MessagePack sequence length");
            encoded[0] = 0x90 + u8::try_from(len - removed).unwrap_or_default();
        }
        0xdc => {
            let len = u16::from_be_bytes([encoded[1], encoded[2]]);
            let removed = u16::try_from(removed).unwrap_or(u16::MAX);
            assert!(len >= removed, "trailing field count exceeds MessagePack sequence length");
            encoded[1..3].copy_from_slice(&(len - removed).to_be_bytes());
        }
        0xdd => {
            let len = u32::from_be_bytes([encoded[1], encoded[2], encoded[3], encoded[4]]);
            let removed = u32::try_from(removed).unwrap_or(u32::MAX);
            assert!(len >= removed, "trailing field count exceeds MessagePack sequence length");
            encoded[1..5].copy_from_slice(&(len - removed).to_be_bytes());
        }
        marker => panic!("unexpected MessagePack sequence marker {marker:#x}"),
    }
    encoded
}

fn item_with_props(props: StreamProperties) -> PlaylistItem {
    let header = shared::model::PlaylistItemHeader { additional_properties: Some(props), ..Default::default() };
    PlaylistItem { header }
}

fn live_item_with_probe_timestamp_and_bitrate(last_probed_timestamp: i64, bitrate: u32) -> PlaylistItem {
    item_with_props(StreamProperties::Live(Box::new(shared::model::LiveStreamProperties {
        video: Some("{\"codec_name\":\"h264\"}".intern()),
        audio: Some("{\"codec_name\":\"aac\"}".intern()),
        bitrate,
        last_probed_timestamp: Some(last_probed_timestamp),
        ..Default::default()
    })))
}

#[test]
fn rename_preserves_input_stream_id_captured_at_target_boundary() {
    let mut item = PlaylistItem {
        header: PlaylistItemHeader {
            id: "origin-alpha".intern(),
            url: "http://provider.example/channel.m3u8".intern(),
            ..Default::default()
        },
    };
    item.header.freeze_input_stream_id();
    let rename = ConfigRename::from(&ConfigRenameDto {
        field: ItemField::Url,
        pattern: "provider".to_string(),
        new_name: "target".to_string(),
        t_pattern: None,
    });

    exec_rename(&mut item, Some(&vec![rename]));

    assert_eq!(item.header.url.as_ref(), "http://target.example/channel.m3u8");
    assert_eq!(item.header.input_stream_id.as_ref(), "origin-alpha");
}

#[test]
fn mapper_changes_id_without_changing_frozen_input_stream_id() {
    let mut item = PlaylistItem {
        header: PlaylistItemHeader { id: "origin-alpha".intern(), name: "Channel".intern(), ..Default::default() },
    };
    item.header.freeze_input_stream_id();
    let mapping = CompiledMapping {
        rules: vec![CompiledMappingRule {
            name: None,
            filter: get_filter(r#"name ~ ".*""#, None).expect("filter should parse"),
            program: MappingProgram::Script(
                MapperScript::parse(r#"@id = "target-id""#, None).expect("mapper should parse"),
            ),
        }],
        ..Default::default()
    };

    let outcome = map_channel(item, &mapping);

    assert_eq!(outcome.matched_rules, 1);
    assert_eq!(outcome.channel.header.id.as_ref(), "target-id");
    assert_eq!(outcome.channel.header.input_stream_id.as_ref(), "origin-alpha");
}

#[test]
fn mapper_cannot_resurrect_missing_legacy_input_stream_id_from_target_id() {
    let mut source = PlaylistItem {
        header: PlaylistItemHeader {
            id: "80510".intern(),
            url: "http://provider.example/live/user/pass/80510.ts".intern(),
            input_name: "input".intern(),
            item_type: PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            ..Default::default()
        },
    };
    source.header.freeze_input_stream_id();
    let mut legacy_xtream = XtreamPlaylistItem::from(&source);
    legacy_xtream.provider_id = 0;
    legacy_xtream.input_stream_id = "".intern();
    legacy_xtream.url = "http://provider.example/live/channel.m3u8".intern();
    let mut legacy_item = PlaylistItem::from(&legacy_xtream);
    legacy_item.header.freeze_input_stream_id();
    let mapping = CompiledMapping {
        rules: vec![CompiledMappingRule {
            name: None,
            filter: get_filter(r#"name ~ ".*""#, None).expect("filter should parse"),
            program: MappingProgram::Script(
                MapperScript::parse(r#"@id = "target-id""#, None).expect("mapper should parse"),
            ),
        }],
        ..Default::default()
    };

    let outcome = map_channel(legacy_item, &mapping);
    let materialized_m3u = M3uPlaylistItem::from(&outcome.channel);
    let materialized_xtream = XtreamPlaylistItem::from(&outcome.channel);

    assert_eq!(outcome.matched_rules, 1);
    assert_eq!(outcome.channel.header.id.as_ref(), "target-id");
    assert_eq!(outcome.channel.get_input_stream_id(), None);
    assert!(materialized_m3u.provider_id.is_empty());
    assert_eq!(materialized_m3u.get_input_stream_id(), None);
    assert_eq!(materialized_xtream.provider_id, 0);
    assert_eq!(materialized_xtream.get_input_stream_id(), None);
}

#[test]
fn execute_pipe_freezes_input_stream_id_without_rename_or_mapper() {
    let input = ConfigInput::default();
    let item = PlaylistItem { header: PlaylistItemHeader { id: "origin-alpha".intern(), ..Default::default() } };
    let source = MemoryPlaylistSource::new(vec![PlaylistGroup {
        id: 1,
        title: "Group".intern(),
        channels: vec![item],
        xtream_cluster: XtreamCluster::Live,
    }])
    .into_source();
    let mut fetched = FetchedPlaylist { input: &input, source, epg: None };
    let mut duplicates = HashSet::new();
    let target = ConfigTarget::from(&ConfigTargetDto::default());

    let (mut processed, _outcome) = execute_pipe(&target, &vec![], &mut fetched, &mut duplicates, false, None)
        .expect("target processing should succeed");
    let mut groups = processed.source.take_groups();

    assert_eq!(groups[0].channels[0].header.input_stream_id.as_ref(), "origin-alpha");
    assert!(groups[0].channels[0].header.set_field("id", "late-target-id"));
    assert_eq!(groups[0].channels[0].header.input_stream_id.as_ref(), "origin-alpha");
}

#[test]
fn execute_pipe_applies_target_bouquet_prefilter() {
    let input = ConfigInput::default();
    let item1 = PlaylistItem {
        header: PlaylistItemHeader {
            id: "ch-1".intern(),
            group: "Kids".intern(),
            xtream_cluster: XtreamCluster::Live,
            ..Default::default()
        },
    };
    let item2 = PlaylistItem {
        header: PlaylistItemHeader {
            id: "ch-2".intern(),
            group: "Adults".intern(),
            xtream_cluster: XtreamCluster::Live,
            ..Default::default()
        },
    };
    let source = MemoryPlaylistSource::new(vec![
        PlaylistGroup { id: 1, title: "Kids".intern(), channels: vec![item1], xtream_cluster: XtreamCluster::Live },
        PlaylistGroup { id: 2, title: "Adults".intern(), channels: vec![item2], xtream_cluster: XtreamCluster::Live },
    ])
    .into_source();
    let mut fetched = FetchedPlaylist { input: &input, source, epg: None };
    let mut duplicates = HashSet::new();
    let target = ConfigTarget::from(&ConfigTargetDto::default());

    let bouquet_dto =
        shared::model::PlaylistClusterBouquetDto { live: Some(vec!["Kids".to_string()]), vod: None, series: None };
    let filter =
        tuliprox_core::model::TargetBouquetFilter::from_dto(shared::model::TargetBouquetDto::whitelist(bouquet_dto))
            .unwrap();

    let (mut processed, _outcome) = execute_pipe(&target, &vec![], &mut fetched, &mut duplicates, false, Some(&filter))
        .expect("target processing should succeed");
    let groups = processed.source.take_groups();

    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].title.as_ref(), "Kids");
    assert_eq!(groups[0].channels.len(), 1);
    assert_eq!(groups[0].channels[0].header.id.as_ref(), "ch-1");
}

#[test]
fn execute_pipe_prefilter_does_not_suppress_allowed_duplicate() {
    let input = ConfigInput::default();
    // Two items with the same URL (same UUID): one in disallowed group, one in allowed group.
    let item_disallowed = PlaylistItem {
        header: PlaylistItemHeader {
            id: "ch-1".intern(),
            url: "http://provider.example/stream.ts".intern(),
            group: "Adults".intern(),
            xtream_cluster: XtreamCluster::Live,
            ..Default::default()
        },
    };
    let item_allowed = PlaylistItem {
        header: PlaylistItemHeader {
            id: "ch-2".intern(),
            url: "http://provider.example/stream.ts".intern(),
            group: "Kids".intern(),
            xtream_cluster: XtreamCluster::Live,
            ..Default::default()
        },
    };
    let source = MemoryPlaylistSource::new(vec![
        PlaylistGroup {
            id: 1,
            title: "Adults".intern(),
            channels: vec![item_disallowed],
            xtream_cluster: XtreamCluster::Live,
        },
        PlaylistGroup {
            id: 2,
            title: "Kids".intern(),
            channels: vec![item_allowed],
            xtream_cluster: XtreamCluster::Live,
        },
    ])
    .into_source();
    let mut fetched = FetchedPlaylist { input: &input, source, epg: None };
    let mut duplicates = HashSet::new();
    let target = ConfigTarget::from(&ConfigTargetDto {
        options: Some(ConfigTargetOptions { remove_duplicates: true, ..Default::default() }),
        ..Default::default()
    });

    let bouquet_dto =
        shared::model::PlaylistClusterBouquetDto { live: Some(vec!["Kids".to_string()]), vod: None, series: None };
    let filter =
        tuliprox_core::model::TargetBouquetFilter::from_dto(shared::model::TargetBouquetDto::whitelist(bouquet_dto))
            .unwrap();

    let (mut processed, _outcome) = execute_pipe(&target, &vec![], &mut fetched, &mut duplicates, false, Some(&filter))
        .expect("target processing should succeed");
    let groups = processed.source.take_groups();

    // The allowed item must be retained because the rejected item did not consume the duplicate UUID slot.
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].title.as_ref(), "Kids");
    assert_eq!(groups[0].channels.len(), 1);
    assert_eq!(groups[0].channels[0].header.id.as_ref(), "ch-2");
}

#[test]
fn legacy_messagepack_playlist_items_default_missing_input_stream_id() {
    let mut source = PlaylistItem {
        header: PlaylistItemHeader {
            id: "origin-alpha".intern(),
            url: "http://provider.example/live/user/pass/80510.ts".intern(),
            input_name: "input".intern(),
            item_type: PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            ..Default::default()
        },
    };

    let header_bytes = serialize_without_trailing_fields(&source.header, &[0xc0, 0xa0]);
    let decoded_header: PlaylistItemHeader =
        rmp_serde::from_slice(&header_bytes).expect("legacy header should deserialize");
    assert!(decoded_header.input_stream_id.is_empty());
    assert_eq!(decoded_header.get_input_stream_id(), None);
    assert_eq!(decoded_header.upstream_user_agent, None);
    let mut decoded_header = decoded_header;
    decoded_header.freeze_input_stream_id();
    assert_eq!(decoded_header.get_input_stream_id().as_deref(), Some("origin-alpha"));

    source.header.freeze_input_stream_id();
    let mut m3u_item = M3uPlaylistItem::from(&source);
    m3u_item.input_stream_id = "".intern();
    let m3u_bytes = serialize_without_trailing_fields(&m3u_item, &[0xc0, 0xa0]);
    let decoded_m3u: M3uPlaylistItem = rmp_serde::from_slice(&m3u_bytes).expect("legacy M3U item should deserialize");
    assert!(decoded_m3u.input_stream_id.is_empty());
    assert_eq!(decoded_m3u.get_input_stream_id().as_deref(), Some("origin-alpha"));
    assert_eq!(decoded_m3u.upstream_user_agent, None);

    let mut xtream_item = XtreamPlaylistItem::from(&source);
    xtream_item.input_stream_id = "".intern();
    let xtream_bytes = serialize_without_trailing_fields(&xtream_item, &[0xc0, 0xa0]);
    let decoded_xtream: XtreamPlaylistItem =
        rmp_serde::from_slice(&xtream_bytes).expect("legacy Xtream item should deserialize");
    assert!(decoded_xtream.input_stream_id.is_empty());
    assert_eq!(decoded_xtream.get_input_stream_id().as_deref(), Some("80510"));
    assert_eq!(decoded_xtream.upstream_user_agent, None);
}

#[test]
fn previous_messagepack_playlist_items_default_missing_upstream_user_agent() {
    let source = PlaylistItem {
        header: PlaylistItemHeader {
            id: "80510".intern(),
            input_stream_id: "origin-alpha".intern(),
            ..Default::default()
        },
    };

    let header: PlaylistItemHeader = rmp_serde::from_slice(&serialize_without_trailing_fields(&source.header, &[0xc0]))
        .expect("previous header should deserialize");
    let m3u: M3uPlaylistItem =
        rmp_serde::from_slice(&serialize_without_trailing_fields(&M3uPlaylistItem::from(&source), &[0xc0]))
            .expect("previous M3U item should deserialize");
    let xtream: XtreamPlaylistItem =
        rmp_serde::from_slice(&serialize_without_trailing_fields(&XtreamPlaylistItem::from(&source), &[0xc0]))
            .expect("previous Xtream item should deserialize");

    assert_eq!(header.input_stream_id.as_ref(), "origin-alpha");
    assert_eq!(m3u.input_stream_id.as_ref(), "origin-alpha");
    assert_eq!(xtream.input_stream_id.as_ref(), "origin-alpha");
    assert_eq!(header.upstream_user_agent, None);
    assert_eq!(m3u.upstream_user_agent, None);
    assert_eq!(xtream.upstream_user_agent, None);
}

#[test]
fn messagepack_playlist_items_preserve_upstream_user_agent() -> Result<(), Box<dyn std::error::Error>> {
    let source = PlaylistItem {
        header: PlaylistItemHeader { upstream_user_agent: Some("Provider-UA".intern()), ..Default::default() },
    };

    let header: PlaylistItemHeader = rmp_serde::from_slice(&rmp_serde::to_vec(&source.header)?)?;
    let m3u: M3uPlaylistItem = rmp_serde::from_slice(&rmp_serde::to_vec(&M3uPlaylistItem::from(&source))?)?;
    let xtream: XtreamPlaylistItem = rmp_serde::from_slice(&rmp_serde::to_vec(&XtreamPlaylistItem::from(&source))?)?;

    assert_eq!(header.upstream_user_agent.as_deref(), Some("Provider-UA"));
    assert_eq!(m3u.upstream_user_agent.as_deref(), Some("Provider-UA"));
    assert_eq!(xtream.upstream_user_agent.as_deref(), Some("Provider-UA"));
    Ok(())
}

#[test]
fn has_probe_details_requires_video_and_audio_for_video() {
    let video = shared::model::VideoStreamProperties {
        details: Some(shared::model::VideoStreamDetailProperties {
            video: Some("{\"codec_name\":\"h264\"}".intern()),
            audio: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let item_missing_audio = item_with_props(StreamProperties::Video(Box::new(video)));
    assert!(!has_probe_details(&item_missing_audio));

    let video_complete = shared::model::VideoStreamProperties {
        details: Some(shared::model::VideoStreamDetailProperties {
            video: Some("{\"codec_name\":\"h264\"}".intern()),
            audio: Some("{\"codec_name\":\"aac\"}".intern()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let item_complete = item_with_props(StreamProperties::Video(Box::new(video_complete)));
    assert!(has_probe_details(&item_complete));
}

#[test]
fn has_probe_details_requires_video_audio_and_bitrate_for_live() {
    let live_missing_audio = shared::model::LiveStreamProperties {
        video: Some("{\"codec_name\":\"h264\"}".intern()),
        audio: None,
        ..Default::default()
    };
    let item_missing_audio = item_with_props(StreamProperties::Live(Box::new(live_missing_audio)));
    assert!(!has_probe_details(&item_missing_audio));

    let live_missing_bitrate = shared::model::LiveStreamProperties {
        video: Some("{\"codec_name\":\"h264\"}".intern()),
        audio: Some("{\"codec_name\":\"aac\"}".intern()),
        ..Default::default()
    };
    let item_missing_bitrate = item_with_props(StreamProperties::Live(Box::new(live_missing_bitrate)));
    assert!(!has_probe_details(&item_missing_bitrate));

    let live_complete = shared::model::LiveStreamProperties {
        video: Some("{\"codec_name\":\"h264\"}".intern()),
        audio: Some("{\"codec_name\":\"aac\"}".intern()),
        bitrate: 2_500_000,
        ..Default::default()
    };
    let item_complete = item_with_props(StreamProperties::Live(Box::new(live_complete)));
    assert!(has_probe_details(&item_complete));
}

#[test]
fn needs_live_probe_when_fresh_probe_has_no_bitrate() {
    let item = live_item_with_probe_timestamp_and_bitrate(101, 0);

    assert!(needs_live_probe(&item, 100));
}

#[test]
fn does_not_need_live_probe_when_fresh_probe_has_positive_bitrate() {
    let item = live_item_with_probe_timestamp_and_bitrate(101, 2_500_000);

    assert!(!needs_live_probe(&item, 100));
}

#[test]
fn needs_live_probe_when_positive_bitrate_probe_is_older_than_cutoff() {
    let item = live_item_with_probe_timestamp_and_bitrate(99, 2_500_000);

    assert!(needs_live_probe(&item, 100));
}

#[test]
fn has_probe_details_is_false_for_series() {
    let series = shared::model::SeriesStreamProperties::default();
    let item = item_with_props(StreamProperties::Series(Box::new(series)));
    assert!(!has_probe_details(&item));
}

#[test]
fn collect_effective_skip_clusters_uses_input_skip_flags() {
    use tuliprox_core::model::{ConfigInputFlags, ConfigInputOptions};
    let input = ConfigInput {
        name: "skip_live".intern(),
        input_type: InputType::Xtream,
        options: Some(ConfigInputOptions {
            flags: ConfigInputFlags::SkipLive.into(),
            ..ConfigInputOptions::defaults().clone()
        }),
        ..ConfigInput::default()
    };
    let skip = collect_effective_skip_clusters(&input);
    assert!(skip.contains(&XtreamCluster::Live));
    assert!(!skip.contains(&XtreamCluster::Video));
    assert!(!skip.contains(&XtreamCluster::Series));
}

#[test]
fn filter_skipped_clusters_removes_cached_groups() {
    use tuliprox_core::model::{ConfigInputFlags, ConfigInputOptions};
    let live_item = PlaylistItem {
        header: shared::model::PlaylistItemHeader { xtream_cluster: XtreamCluster::Live, ..Default::default() },
    };
    let vod_item = PlaylistItem {
        header: shared::model::PlaylistItemHeader { xtream_cluster: XtreamCluster::Video, ..Default::default() },
    };

    let groups = vec![
        PlaylistGroup { id: 1, title: "Live".intern(), channels: vec![live_item], xtream_cluster: XtreamCluster::Live },
        PlaylistGroup { id: 2, title: "Vod".intern(), channels: vec![vod_item], xtream_cluster: XtreamCluster::Video },
    ];

    let source = MemoryPlaylistSource::new(groups).into_source();
    let input = ConfigInput {
        name: "skip_live".intern(),
        input_type: InputType::Xtream,
        options: Some(ConfigInputOptions {
            flags: ConfigInputFlags::SkipLive.into(),
            ..ConfigInputOptions::defaults().clone()
        }),
        ..ConfigInput::default()
    };

    let mut filtered = filter_skipped_clusters_from_source(source, &input);
    let filtered_groups = filtered.take_groups();
    assert_eq!(filtered_groups.len(), 1);
    assert_eq!(filtered_groups[0].xtream_cluster, XtreamCluster::Video);
}

fn test_group(cluster: XtreamCluster, item_name: &str, input_name: &str) -> PlaylistGroup {
    PlaylistGroup {
        id: 1,
        title: item_name.intern(),
        xtream_cluster: cluster,
        channels: vec![PlaylistItem {
            header: PlaylistItemHeader {
                name: item_name.intern(),
                input_name: input_name.intern(),
                xtream_cluster: cluster,
                item_type: match cluster {
                    XtreamCluster::Live => PlaylistItemType::Live,
                    XtreamCluster::Video => PlaylistItemType::Video,
                    XtreamCluster::Series => PlaylistItemType::Series,
                },
                ..Default::default()
            },
        }],
    }
}

#[test]
fn staged_overlay_replaces_selected_clusters_and_rewrites_input_name() {
    let provider_name = "provider".intern();
    let provider_groups = vec![
        test_group(XtreamCluster::Live, "provider-live", "provider"),
        test_group(XtreamCluster::Video, "provider-vod", "provider"),
    ];
    let staged_groups = vec![
        test_group(XtreamCluster::Live, "staged-live", "staged"),
        test_group(XtreamCluster::Series, "staged-series", "staged"),
    ];

    let groups = apply_staged_overlay_groups(&provider_name, ClusterFlags::Live, provider_groups, staged_groups);

    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].title.as_ref(), "provider-vod");
    assert_eq!(groups[0].channels[0].header.input_name.as_ref(), "provider");
    assert_eq!(groups[1].title.as_ref(), "staged-live");
    assert_eq!(groups[1].channels[0].header.input_name.as_ref(), "provider");
}

#[test]
fn staged_overlay_is_skipped_when_provider_playlist_is_cached() {
    let result = PlaylistDownloadResult::new(vec![], vec![], true, false);

    assert!(!should_apply_staged_overlay(&result));
}

#[test]
fn quality_rejection_keeps_a_usable_input_ready() {
    let mut result = InputDownloadResult {
        errors: Vec::new(),
        source: MemoryPlaylistSource::new(vec![test_group(XtreamCluster::Video, "retained-vod", "provider-a")])
            .into_source(),
        storage_error: None,
        partial: false,
        quality_rejections: vec![ClusterUpdateRejection {
            cluster: XtreamCluster::Video,
            current_count: 12_543,
            candidate_count: 217,
            threshold: 90,
            quality: 1,
        }],
    };

    assert_eq!(result.job_state(), InputJobState::Ready);
    assert!(result.errors.is_empty());
    assert!(!result.partial);
    assert_eq!(result.quality_rejections.len(), 1);
}

#[test]
fn quality_rejection_marks_the_run_partial_without_reusing_stalker_partial() {
    let quality_rejection = PlaylistRunSignals { has_quality_rejections: true, ..PlaylistRunSignals::default() };
    assert!(!quality_rejection.has_pending_stalker_refresh);
    assert_eq!(quality_rejection.state(), PlaylistUpdateState::Partial);

    let stalker_partial = PlaylistRunSignals { has_pending_stalker_refresh: true, ..PlaylistRunSignals::default() };
    assert!(!stalker_partial.has_quality_rejections);
    assert_eq!(stalker_partial.state(), PlaylistUpdateState::Partial);

    let technical_failure = PlaylistRunSignals { has_error: true, ..quality_rejection };
    assert_eq!(technical_failure.state(), PlaylistUpdateState::Failure);
}

fn make_test_item(name: &str, item_type: PlaylistItemType) -> PlaylistItem {
    let header =
        PlaylistItemHeader { name: name.into(), group: "Test Group".intern(), item_type, ..Default::default() };
    PlaylistItem { header }
}

#[test]
fn test_filter_evalutes_correctly() {
    let filter = get_filter(r#"name ~ "Allowed""#, None).unwrap();

    let allowed_item = make_test_item("Allowed Channel", PlaylistItemType::Live);
    let denied_item = make_test_item("Denied Channel", PlaylistItemType::Live);

    let allowed_provider = ValueProvider { pli: &allowed_item, match_as_ascii: false };
    let denied_provider = ValueProvider { pli: &denied_item, match_as_ascii: false };

    assert!(filter.filter(&allowed_provider));
    assert!(!filter.filter(&denied_provider));
}

#[test]
fn test_filter_with_type_comparison() {
    let filter = get_filter("type = vod", None).unwrap();

    let vod_item = make_test_item("Test Movie", PlaylistItemType::Video);
    let live_item = make_test_item("Test Channel", PlaylistItemType::Live);

    let vod_provider = ValueProvider { pli: &vod_item, match_as_ascii: false };
    let live_provider = ValueProvider { pli: &live_item, match_as_ascii: false };

    assert!(filter.filter(&vod_provider));
    assert!(!filter.filter(&live_provider));
}

#[test]
fn playlist_retention_reports_filter_counts() {
    let groups = vec![PlaylistGroup {
        id: 1,
        title: "Test Group".intern(),
        channels: vec![
            make_test_item("Allowed", PlaylistItemType::Live),
            make_test_item("Denied", PlaylistItemType::Live),
        ],
        xtream_cluster: XtreamCluster::Live,
    }];
    let mut source = MemoryPlaylistSource::new(groups).into_source();

    let (filtered, outcome) = retain_playlist_items(&mut source, |item| item.header.name.as_ref() == "Allowed");

    assert_eq!(outcome, FilterOutcome { inspected: 2, retained: 1, removed: 1 });
    assert_eq!(filtered.expect("one item should remain")[0].channels[0].header.name.as_ref(), "Allowed");
}

#[test]
fn filter_stage_can_remove_every_item() {
    let groups = vec![PlaylistGroup {
        id: 1,
        title: "Test Group".intern(),
        channels: vec![make_test_item("Denied", PlaylistItemType::Live)],
        xtream_cluster: XtreamCluster::Live,
    }];
    let mut target = ConfigTarget::from(&ConfigTargetDto::default());
    target.filter = get_filter(r#"name ~ "Allowed""#, None).expect("filter should parse").into();

    let (groups, outcome) = execute_pipeline_on_groups(groups, &target, &[TransformStage::Filter]);

    assert!(groups.is_empty());
    assert_eq!(outcome.filter, Some(FilterOutcome { inspected: 1, retained: 0, removed: 1 }));
}

#[test]
fn missing_processing_filter_skips_filter_stage() {
    let groups = vec![PlaylistGroup {
        id: 1,
        title: "Test Group".intern(),
        channels: vec![make_test_item("Allowed", PlaylistItemType::Live)],
        xtream_cluster: XtreamCluster::Live,
    }];
    let target = ConfigTarget::from(&ConfigTargetDto::default());

    let (groups, outcome) = execute_pipeline_on_groups(groups, &target, &[TransformStage::Filter]);

    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].channels.len(), 1);
    assert!(outcome.filter.is_none());
}

#[test]
fn missing_processing_filter_preserves_filter_stage_group_normalization() {
    let mut first = make_test_item("One", PlaylistItemType::Live);
    first.header.group = "News".intern();
    let mut second = make_test_item("Two", PlaylistItemType::Live);
    second.header.group = "news".intern();
    let groups = vec![
        PlaylistGroup { id: 1, title: "News".intern(), channels: vec![first], xtream_cluster: XtreamCluster::Live },
        PlaylistGroup { id: 2, title: "news".intern(), channels: vec![second], xtream_cluster: XtreamCluster::Live },
    ];
    let target = ConfigTarget::from(&ConfigTargetDto::default());

    let (groups, outcome) = execute_pipeline_on_groups(groups, &target, &[TransformStage::Filter]);

    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].channels.len(), 2);
    assert!(outcome.filter.is_none());
}

#[test]
fn pipeline_reports_filter_and_rename_outcomes() {
    let groups = vec![PlaylistGroup {
        id: 1,
        title: "Test Group".intern(),
        channels: vec![
            make_test_item("Allowed", PlaylistItemType::Live),
            make_test_item("Denied", PlaylistItemType::Live),
        ],
        xtream_cluster: XtreamCluster::Live,
    }];
    let mut target = ConfigTarget::from(&ConfigTargetDto::default());
    target.filter = get_filter(r#"name ~ "Allowed""#, None).expect("filter should parse").into();
    target.rename = Some(vec![ConfigRename::from(&ConfigRenameDto {
        field: ItemField::Name,
        pattern: "Allowed".to_string(),
        new_name: "Renamed".to_string(),
        t_pattern: None,
    })]);

    let (groups, outcome) =
        execute_pipeline_on_groups(groups, &target, &[TransformStage::Filter, TransformStage::Rename]);

    assert_eq!(groups[0].channels[0].header.name.as_ref(), "Renamed");
    assert_eq!(outcome.filter, Some(FilterOutcome { inspected: 2, retained: 1, removed: 1 }));
    assert_eq!(outcome.rename, Some(RenameOutcome { inspected: 1, changed_items: 1, changed_fields: 1 }));
}

#[test]
fn assign_channel_no_playlist_preserves_non_zero_chno() {
    let mut groups = vec![
        PlaylistGroup {
            id: 1,
            title: "Group A".intern(),
            channels: vec![
                PlaylistItem { header: PlaylistItemHeader { name: "A".intern(), chno: 10, ..Default::default() } },
                PlaylistItem { header: PlaylistItemHeader { name: "B".intern(), chno: 0, ..Default::default() } },
            ],
            xtream_cluster: XtreamCluster::Live,
        },
        PlaylistGroup {
            id: 2,
            title: "Group C".intern(),
            channels: vec![
                PlaylistItem { header: PlaylistItemHeader { name: "C".intern(), chno: 1, ..Default::default() } },
                PlaylistItem { header: PlaylistItemHeader { name: "D".intern(), chno: 0, ..Default::default() } },
            ],
            xtream_cluster: XtreamCluster::Live,
        },
    ];

    assign_channel_no_playlist(&mut groups);

    // Non-zero chno values must be preserved
    assert_eq!(groups[0].channels[0].header.chno, 10);
    assert_eq!(groups[1].channels[0].header.chno, 1);
}

#[test]
fn assign_channel_no_playlist_assigns_zero_chno_only() {
    let mut groups = vec![PlaylistGroup {
        id: 1,
        title: "Group A".intern(),
        channels: vec![
            PlaylistItem { header: PlaylistItemHeader { name: "A".intern(), chno: 0, ..Default::default() } },
            PlaylistItem { header: PlaylistItemHeader { name: "B".intern(), chno: 0, ..Default::default() } },
            PlaylistItem { header: PlaylistItemHeader { name: "C".intern(), chno: 0, ..Default::default() } },
        ],
        xtream_cluster: XtreamCluster::Live,
    }];

    assign_channel_no_playlist(&mut groups);

    // All zero-chno channels should get assigned numbers starting at 1
    assert_eq!(groups[0].channels[0].header.chno, 1);
    assert_eq!(groups[0].channels[1].header.chno, 2);
    assert_eq!(groups[0].channels[2].header.chno, 3);
}

#[test]
fn assign_channel_no_playlist_skips_existing_nonzero_numbers() {
    let mut groups = vec![PlaylistGroup {
        id: 1,
        title: "Group A".intern(),
        channels: vec![
            PlaylistItem { header: PlaylistItemHeader { name: "A".intern(), chno: 5, ..Default::default() } },
            PlaylistItem { header: PlaylistItemHeader { name: "B".intern(), chno: 0, ..Default::default() } },
            PlaylistItem { header: PlaylistItemHeader { name: "C".intern(), chno: 2, ..Default::default() } },
            PlaylistItem { header: PlaylistItemHeader { name: "D".intern(), chno: 0, ..Default::default() } },
        ],
        xtream_cluster: XtreamCluster::Live,
    }];

    assign_channel_no_playlist(&mut groups);

    // Existing non-zero numbers (2, 5) must be skipped when assigning new numbers
    assert_eq!(groups[0].channels[0].header.chno, 5); // preserved
    assert_eq!(groups[0].channels[2].header.chno, 2); // preserved
                                                      // B gets 1 (smallest available), D gets 3 (next available after 1 and existing 2)
    assert_eq!(groups[0].channels[1].header.chno, 1);
    assert_eq!(groups[0].channels[3].header.chno, 3);
}

#[test]
fn assign_channel_no_playlist_assigns_following_group_order() {
    let mut groups = vec![
        PlaylistGroup {
            id: 1,
            title: "Group 1".intern(),
            channels: vec![
                PlaylistItem { header: PlaylistItemHeader { name: "A".intern(), chno: 0, ..Default::default() } },
                PlaylistItem { header: PlaylistItemHeader { name: "B".intern(), chno: 0, ..Default::default() } },
            ],
            xtream_cluster: XtreamCluster::Live,
        },
        PlaylistGroup {
            id: 2,
            title: "Group 2".intern(),
            channels: vec![PlaylistItem {
                header: PlaylistItemHeader { name: "C".intern(), chno: 0, ..Default::default() },
            }],
            xtream_cluster: XtreamCluster::Live,
        },
    ];

    assign_channel_no_playlist(&mut groups);

    // Numbers should follow iteration order across groups: A=1, B=2, C=3
    assert_eq!(groups[0].channels[0].header.chno, 1);
    assert_eq!(groups[0].channels[1].header.chno, 2);
    assert_eq!(groups[1].channels[0].header.chno, 3);
}

#[tokio::test]
async fn parallel_input_scheduler_serializes_equal_groups_and_overlaps_distinct_groups() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn observe(active: &AtomicUsize, maximum: &AtomicUsize) {
        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
        maximum.fetch_max(current, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(20)).await;
        active.fetch_sub(1, Ordering::SeqCst);
    }

    let locks = tuliprox_core::utils::FileLockManager::default();
    let active = AtomicUsize::new(0);
    let maximum = AtomicUsize::new(0);
    tokio::join!(
        with_sequential_group(&locks, Some(7), true, observe(&active, &maximum)),
        with_sequential_group(&locks, Some(7), true, observe(&active, &maximum)),
    );
    assert_eq!(maximum.load(Ordering::SeqCst), 1);

    maximum.store(0, Ordering::SeqCst);
    tokio::join!(
        with_sequential_group(&locks, Some(7), true, observe(&active, &maximum)),
        with_sequential_group(&locks, Some(8), true, observe(&active, &maximum)),
    );
    assert_eq!(maximum.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn parallel_input_scheduler_releases_group_after_abort() {
    let locks = Arc::new(tuliprox_core::utils::FileLockManager::default());
    let task_locks = Arc::clone(&locks);
    let task = tokio::spawn(async move {
        with_sequential_group(&task_locks, Some(7), true, std::future::pending::<()>()).await;
    });
    tokio::task::yield_now().await;
    task.abort();
    let _ = task.await;

    tokio::time::timeout(Duration::from_secs(1), with_sequential_group(&locks, Some(7), true, std::future::ready(())))
        .await
        .expect("aborting an input job must release its sequential group");
}

#[test]
fn input_progress_message_contains_each_target_and_blocking_input() {
    let targets = ["target-a", "target-b"];
    let inputs = ["input-a", "input-b"];
    let messages: Vec<_> = targets
        .iter()
        .flat_map(|target| inputs.iter().map(move |input| target_waiting_message(target, input)))
        .collect();

    assert_eq!(messages.len(), 4);
    for target in targets {
        for input in inputs {
            assert!(messages.contains(&format!("Target '{target}' is waiting for input '{input}'")));
        }
    }
    assert!(stalker_checkpoint_message("portal-a").contains("portal-a"));
}

#[tokio::test]
async fn parallel_target_pipeline_bounds_active_finalizers() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let mut tasks = JoinSet::new();
    let mut results = Vec::new();
    let mut errors = Vec::new();

    for index in 0..6 {
        wait_for_target_finalizer_slot(&mut tasks, &mut results, &mut errors).await;
        let active = Arc::clone(&active);
        let maximum = Arc::clone(&maximum);
        tasks.spawn(async move {
            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
            maximum.fetch_max(current, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(10)).await;
            active.fetch_sub(1, Ordering::SeqCst);
            TargetJobResult {
                index,
                name: format!("target-{index}"),
                result: Ok(()),
                errors: Vec::new(),
                processing: PipelineStats::default(),
            }
        });
    }
    while let Some(result) = tasks.join_next().await {
        collect_target_task_result(result, &mut results, &mut errors);
    }

    assert!(maximum.load(Ordering::SeqCst) <= MAX_CONCURRENT_TARGET_FINALIZERS);
    assert_eq!(results.len(), 6);
    assert!(errors.is_empty());
}

#[test]
fn parallel_target_pipeline_normalizes_conflicting_output_resources() {
    let config = Config { storage_dir: "/tmp/tuliprox-target-resources".to_string(), ..Config::default() };

    let mut spaced = ConfigTarget::from(&ConfigTargetDto::default());
    spaced.name = "A B".to_string();
    let mut underscored = ConfigTarget::from(&ConfigTargetDto::default());
    underscored.name = "A_B".to_string();
    assert!(!target_mutated_resources(&config, &spaced).is_disjoint(&target_mutated_resources(&config, &underscored)));

    spaced.name = "one".to_string();
    spaced.output = vec![tuliprox_core::model::TargetOutput::M3u(tuliprox_core::model::M3uTargetOutput {
        filename: Some("out/../x.m3u".to_string()),
        include_type_in_url: false,
        mask_redirect_url: false,
        filter: None,
    })];
    underscored.name = "two".to_string();
    underscored.output = vec![tuliprox_core::model::TargetOutput::M3u(tuliprox_core::model::M3uTargetOutput {
        filename: Some("x.m3u".to_string()),
        include_type_in_url: false,
        mask_redirect_url: false,
        filter: None,
    })];
    assert!(!target_mutated_resources(&config, &spaced).is_disjoint(&target_mutated_resources(&config, &underscored)));
}

mod mapping_stage {
    use super::*;
    use arc_swap::{ArcSwap, ArcSwapOption};
    use shared::model::{ConfigPaths, EpgSmartMatchConfigDto};
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::runtime::Runtime;
    use tuliprox_core::{
        model::{
            EpgConfig, EpgSmartMatchConfig, IcsEpgSourceConfig, MediaToolCapabilities, PersistedEpgSource,
            PersistedEpgSourceKind, SourcesConfig,
        },
        utils::FileLockManager,
    };

    fn build_mapping(id: &str, stage: MappingStage, script: &str) -> CompiledMapping {
        CompiledMapping {
            id: id.to_string(),
            match_as_ascii: false,
            stage,
            rules: vec![CompiledMappingRule {
                name: None,
                filter: get_filter(r#"name ~ ".*""#, None).expect("filter parses"),
                program: MappingProgram::Script(MapperScript::parse(script, None).expect("script parses")),
            }],
            counters: vec![],
            templates: None,
        }
    }

    fn build_target(mappings: Vec<CompiledMapping>, remove_duplicates: bool) -> ConfigTarget {
        let dto = ConfigTargetDto {
            options: if remove_duplicates {
                Some(ConfigTargetOptions { remove_duplicates, ..Default::default() })
            } else {
                None
            },
            ..Default::default()
        };
        let mut target = ConfigTarget::from(&dto);
        target.mapping = Arc::new(ArcSwapOption::from(Some(Arc::new(CompiledTargetMappings::new(
            mappings.into_iter().map(Arc::new).collect(),
        )))));
        target
    }

    /// Pinned to `NoopSink` rather than staying generic: these tests
    /// exercise the pipeline, not the bus, and an inferred sink type
    /// would just make every call site name one.
    fn processing_context() -> PlaylistProcessingContext<shared::model::NoopSink> {
        let paths = ConfigPaths {
            home_path: String::new(),
            config_path: String::new(),
            storage_path: String::new(),
            config_file_path: String::new(),
            sources_file_path: String::new(),
            mapping_file_path: None,
            mapping_files_used: None,
            template_file_path: None,
            template_files_used: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        };
        let config = AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            api_proxy: Arc::new(ArcSwapOption::default()),
            file_locks: Arc::new(FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(paths)),
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(MediaToolCapabilities::new()),
        };
        PlaylistProcessingContext {
            client: reqwest::Client::new(),
            config: Arc::new(config),
            user_targets: Arc::new(ProcessTargets {
                enabled: false,
                inputs: Vec::new(),
                targets: Vec::new(),
                target_names: Vec::new(),
            }),
            events: shared::model::NoopSink,
            playlist_state: None,
            disabled_headers: None,
            processed_inputs: Arc::new(Mutex::new(HashSet::new())),
            input_locks: Arc::new(Mutex::new(HashMap::new())),
            provider_manager: None,
            metadata_manager: None,
            pre_processed_inputs: None,
            stalker_refresh_mode: StalkerRefreshMode::Complete,
            partial_refresh: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            had_quality_rejections: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    #[tokio::test]
    async fn m3u_alias_playlist_is_downloaded_and_indexed_separately() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let primary_playlist_path = temp.path().join("primary.m3u");
        let alias_playlist_path = temp.path().join("backup.m3u");
        tokio::fs::write(
            &primary_playlist_path,
            "#EXTM3U\n#EXTINF:-1 tvg-id=\"323\",Channel\nhttp://stream.example:4000/323/mono.m3u8?token=primary-stream-token\n",
        )
        .await
        .expect("primary fixture should be written");
        tokio::fs::write(
            &alias_playlist_path,
            "#EXTM3U\n#EXTINF:-1 tvg-id=\"323\",Channel\nhttp://stream.example:4000/323/mono.m3u8?token=backup-stream-token\n",
        )
        .await
        .expect("alias fixture should be written");

        let ctx = processing_context();
        let config =
            Config { storage_dir: temp.path().join("storage").to_string_lossy().into_owned(), ..Config::default() };
        ctx.config.config.store(Arc::new(config));
        let input = Arc::new(ConfigInput {
            id: 1,
            name: "primary-account".intern(),
            input_type: InputType::M3u,
            url: primary_playlist_path.to_string_lossy().into_owned(),
            enabled: true,
            aliases: Some(vec![ConfigInputAlias {
                id: 2,
                name: "backup-account".intern(),
                url: alias_playlist_path.to_string_lossy().into_owned(),
                username: None,
                password: None,
                priority: 1,
                max_connections: 1,
                exp_date: None,
                enabled: true,
                stalker: None,
            }]),
            ..ConfigInput::default()
        });

        let InputDownloadResult { errors, source: mut primary_playlist, storage_error, partial, quality_rejections } =
            download_input(&ctx, &input, false).await;

        assert!(errors.is_empty(), "unexpected download errors: {errors:?}");
        assert!(storage_error.is_none(), "unexpected primary storage error: {storage_error:?}");
        assert!(!partial);
        assert!(quality_rejections.is_empty());
        assert!(!primary_playlist.is_empty());
        let alias_url = tuliprox_repository::load_input_m3u_stream_url(
            &ctx.config,
            &"backup-account".intern(),
            "http://stream.example:4000/323/mono.m3u8?token=primary-stream-token",
        )
        .await
        .expect("alias URL lookup should succeed");
        assert_eq!(alias_url.as_deref(), Some("http://stream.example:4000/323/mono.m3u8?token=backup-stream-token"));
    }

    #[tokio::test]
    async fn failed_m3u_alias_is_retried_after_primary_input_is_processed() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let primary_playlist_path = temp.path().join("main.m3u");
        let alias_playlist_path = temp.path().join("retry.m3u");
        tokio::fs::write(
            &primary_playlist_path,
            "#EXTM3U\n#EXTINF:-1 tvg-id=\"323\",Channel\nhttp://stream.example:4000/323/mono.m3u8?token=main-stream-token\n",
        )
        .await
        .expect("primary fixture should be written");

        let ctx = processing_context();
        let config =
            Config { storage_dir: temp.path().join("storage").to_string_lossy().into_owned(), ..Config::default() };
        ctx.config.config.store(Arc::new(config));
        let input = Arc::new(ConfigInput {
            id: 1,
            name: "main-account".intern(),
            input_type: InputType::M3u,
            url: primary_playlist_path.to_string_lossy().into_owned(),
            enabled: true,
            aliases: Some(vec![ConfigInputAlias {
                id: 2,
                name: "retry-account".intern(),
                url: alias_playlist_path.to_string_lossy().into_owned(),
                username: None,
                password: None,
                priority: 1,
                max_connections: 1,
                exp_date: None,
                enabled: true,
                stalker: None,
            }]),
            ..ConfigInput::default()
        });

        let InputDownloadResult {
            errors: first_errors,
            source: mut first_playlist,
            storage_error: first_storage_error,
            partial: first_partial,
            quality_rejections: first_quality_rejections,
        } = download_input(&ctx, &input, false).await;

        assert!(!first_errors.is_empty(), "missing alias should report an error");
        assert!(first_storage_error.is_none(), "primary storage should succeed: {first_storage_error:?}");
        assert!(!first_partial);
        assert!(first_quality_rejections.is_empty());
        assert!(!first_playlist.is_empty());
        assert!(ctx.is_input_downloaded("main-account").await);
        assert!(!ctx.is_input_downloaded("retry-account").await);

        tokio::fs::write(
            &alias_playlist_path,
            "#EXTM3U\n#EXTINF:-1 tvg-id=\"323\",Channel\nhttp://stream.example:4000/323/mono.m3u8?token=retry-stream-token\n",
        )
        .await
        .expect("alias fixture should be written");

        let InputDownloadResult {
            errors: second_errors,
            source: mut second_playlist,
            storage_error: second_storage_error,
            partial: second_partial,
            quality_rejections: second_quality_rejections,
        } = download_input(&ctx, &input, false).await;

        assert!(second_errors.is_empty(), "unexpected retry errors: {second_errors:?}");
        assert!(second_storage_error.is_none(), "primary storage should remain readable: {second_storage_error:?}");
        assert!(!second_partial);
        assert!(second_quality_rejections.is_empty());
        assert!(!second_playlist.is_empty());
        assert!(ctx.is_input_downloaded("retry-account").await);
        let alias_url = tuliprox_repository::load_input_m3u_stream_url(
            &ctx.config,
            &"retry-account".intern(),
            "http://stream.example:4000/323/mono.m3u8?token=main-stream-token",
        )
        .await
        .expect("retried alias URL lookup should succeed");
        assert_eq!(alias_url.as_deref(), Some("http://stream.example:4000/323/mono.m3u8?token=retry-stream-token"));
    }

    #[test]
    fn persist_filter_runs_after_after_epg_mapping() {
        let runtime = Runtime::new().expect("runtime");
        runtime.block_on(async {
            let mut input = ConfigInput::from(ConfigInputDto::default());
            input.name = "input".intern();
            let groups = vec![PlaylistGroup {
                id: 1,
                title: "Live".intern(),
                channels: vec![PlaylistItem {
                    header: PlaylistItemHeader {
                        name: "Before".intern(),
                        group: "Live".intern(),
                        xtream_cluster: XtreamCluster::Live,
                        item_type: PlaylistItemType::Live,
                        ..Default::default()
                    },
                }],
                xtream_cluster: XtreamCluster::Live,
            }];
            let mut playlist =
                FetchedPlaylist { input: &input, source: MemoryPlaylistSource::new(groups).into_source(), epg: None };
            let rename = build_mapping("rename", MappingStage::AfterEpg, r#"@Name = "After""#);
            let mut target = build_target(vec![rename], false);
            target.filter.persist = Some(get_filter(r#"Name = "After""#, None).expect("filter parses"));
            let mut stats = HashMap::from([(
                Arc::clone(&input.name),
                create_input_stat(1, 1, 0, input.input_type, &input.name, 0),
            )]);
            let mut errors = Vec::new();

            let mut prepared = prepare_playlist_for_target(
                &processing_context(),
                std::slice::from_mut(&mut playlist),
                &target,
                &mut stats,
                &mut errors,
                false,
            )
            .await
            .expect("target preparation");

            assert!(errors.is_empty());
            apply_persist_filter(&target, &mut prepared.playlist);
            let item = &prepared.playlist[0].channels[0];
            assert_eq!(item.header.name.as_ref(), "After");
        });
    }

    fn make_channel(name: &str) -> PlaylistItem {
        let mut item = PlaylistItem {
            header: PlaylistItemHeader {
                name: name.intern(),
                group: "Originals".intern(),
                xtream_cluster: XtreamCluster::Live,
                item_type: PlaylistItemType::Live,
                ..Default::default()
            },
        };
        item.header.freeze_input_stream_id();
        item
    }

    fn memory_source(channels: Vec<PlaylistItem>) -> PlaylistSource {
        MemoryPlaylistSource::new(vec![PlaylistGroup {
            id: 1,
            title: "Live".intern(),
            channels,
            xtream_cluster: XtreamCluster::Live,
        }])
        .into_source()
    }

    fn channel_count(source: &mut PlaylistSource) -> usize {
        source.take_groups().iter().map(|g| g.channels.len()).sum()
    }

    #[test]
    fn map_playlist_applies_only_the_requested_stage() {
        let processing = build_mapping("processing", MappingStage::Processing, r#"@name = concat(@Name, "-P")"#);
        let after_epg = build_mapping("after_epg", MappingStage::AfterEpg, r#"@name = concat(@Name, "-E")"#);
        let target = build_target(vec![processing, after_epg], false);

        let mut source = memory_source(vec![make_channel("Alpha")]);
        let (groups, _) = execute_pipeline_on_groups(source.take_groups(), &target, &[TransformStage::Map]);
        assert_eq!(groups[0].channels[0].header.name.as_ref(), "Alpha-P");

        let mut source = MemoryPlaylistSource::new(groups).into_source();
        let groups = map_playlist_at_stage(&mut source, &target, MappingStage::AfterEpg, None)
            .expect("after_epg mapping should run");
        assert_eq!(groups[0].channels[0].header.name.as_ref(), "Alpha-P-E");
    }

    #[test]
    fn map_playlist_at_stage_returns_none_without_consuming_source_when_no_match() {
        let target = build_target(Vec::new(), false);
        let mut source = memory_source(vec![make_channel("Alpha")]);

        let result = map_playlist_at_stage(&mut source, &target, MappingStage::AfterEpg, None);
        assert!(result.is_none(), "no matching stage must return None");
        assert_eq!(channel_count(&mut source), 1, "source must remain intact");
    }

    #[test]
    fn prepare_target_applies_after_epg_mapping_before_sampling_stats() {
        let runtime = Runtime::new().expect("runtime");
        runtime.block_on(async {
                let dir = tempdir().expect("tempdir");
                let ics_path = dir.path().join("bbc.ics");
                std::fs::write(
                    &ics_path,
                    "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:News\nDTSTART:20260306T120000Z\nDTEND:20260306T130000Z\nEND:VEVENT\nEND:VCALENDAR",
                )
                .expect("write ics");

                let mut smart_dto = EpgSmartMatchConfigDto {
                    enabled: true,
                    fuzzy_matching: false,
                    ..EpgSmartMatchConfigDto::default()
                };
                smart_dto.prepare().expect("smart config");
                let mut input = ConfigInput::from(ConfigInputDto::default());
                input.name = "input".intern();
                input.epg = Some(EpgConfig {
                    sources: vec![],
                    smart_match: Some(EpgSmartMatchConfig::from(smart_dto)),
                });

                let channels = vec![live_item_for_epg("BBC One")];
                let groups = vec![PlaylistGroup {
                    id: 1,
                    title: "Live".intern(),
                    channels,
                    xtream_cluster: XtreamCluster::Live,
                }];
                let tv_guide = TVGuide::new(vec![PersistedEpgSource {
                    file_path: ics_path,
                    priority: 0,
                    logo_override: false,
                    kind: PersistedEpgSourceKind::Ics {
                        channel_id: "bbc.one".intern(),
                        channel_title: Some("BBC One".intern()),
                        match_names: vec!["BBC One".intern()],
                        config: Box::new(IcsEpgSourceConfig::default()),
                    },
                }]);

                let mut playlist = FetchedPlaylist {
                    input: &input,
                    source: MemoryPlaylistSource::new(groups).into_source(),
                    epg: Some(tv_guide),
                };

                let rename_from_epg = build_mapping(
                    "rename",
                    MappingStage::AfterEpg,
                    r#"epg = @epg_channel_id ~ "(.+)"
match {
  epg => @Name = epg.1
}"#,
                );
                let add_virtual = build_mapping("virtual", MappingStage::AfterEpg, r#"add_favourite("Echo")"#);
                let target = build_target(vec![rename_from_epg, add_virtual], false);
                let mut stats = HashMap::from([(
                    Arc::clone(&input.name),
                    create_input_stat(1, 1, 0, input.input_type, &input.name, 0),
                )]);
                let mut errors = Vec::new();
                let prepared = prepare_playlist_for_target(
                    &processing_context(),
                    std::slice::from_mut(&mut playlist),
                    &target,
                    &mut stats,
                    &mut errors,
                    false,
                )
                .await
                .expect("target preparation");

                assert!(errors.is_empty());
                assert_eq!(prepared.playlist.iter().map(|group| group.channels.len()).sum::<usize>(), 2);
                let channel = prepared
                    .playlist
                    .iter()
                    .flat_map(|group| &group.channels)
                    .find(|channel| channel.header.group.as_ref() != "Echo")
                    .expect("original channel");
                assert_eq!(channel.header.epg_channel_id.as_deref(), Some("bbc.one"));
                assert_eq!(
                    channel.header.name.as_ref(),
                    "bbc.one",
                    "after_epg mapper must consume the EPG-enriched field"
                );
                let processed_stats = &stats[&input.name].processed_stats;
                assert_eq!(processed_stats.group_count, 2);
                assert_eq!(processed_stats.channel_count, 2);
            });
    }

    #[test]
    fn clear_invalid_epg_ids_clears_ids_invalidated_by_after_epg_mapping() {
        let runtime = Runtime::new().expect("runtime");
        runtime.block_on(async {
                let dir = tempdir().expect("tempdir");
                let ics_path = dir.path().join("bbc.ics");
                std::fs::write(
                    &ics_path,
                    "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:News\nDTSTART:20260306T120000Z\nDTEND:20260306T130000Z\nEND:VEVENT\nEND:VCALENDAR",
                )
                .expect("write ics");

                let mut input = ConfigInput::from(ConfigInputDto::default());
                input.name = "input".intern();
                input.epg = Some(EpgConfig { sources: vec![], smart_match: None });
                let groups = vec![PlaylistGroup {
                    id: 1,
                    title: "Live".intern(),
                    channels: vec![PlaylistItem {
                        header: PlaylistItemHeader {
                            name: "BBC One".intern(),
                            epg_channel_id: Some("bbc.one".intern()),
                            group: "Live".intern(),
                            xtream_cluster: XtreamCluster::Live,
                            item_type: PlaylistItemType::Live,
                            ..Default::default()
                        },
                    }],
                    xtream_cluster: XtreamCluster::Live,
                }];
                let tv_guide = TVGuide::new(vec![PersistedEpgSource {
                    file_path: ics_path,
                    priority: 0,
                    logo_override: false,
                    kind: PersistedEpgSourceKind::Ics {
                        channel_id: "bbc.one".intern(),
                        channel_title: Some("BBC One".intern()),
                        match_names: vec![],
                        config: Box::new(IcsEpgSourceConfig::default()),
                    },
                }]);
                let mut playlist = FetchedPlaylist {
                    input: &input,
                    source: MemoryPlaylistSource::new(groups).into_source(),
                    epg: Some(tv_guide),
                };

                let rewrite_epg =
                    build_mapping("rewrite", MappingStage::AfterEpg, r#"@epg_channel_id = "missing.epg""#);
                let add_virtual = build_mapping("virtual", MappingStage::AfterEpg, r#"add_favourite("Echo")"#);
                let mut target = build_target(vec![rewrite_epg, add_virtual], false);
                target.options = Some(ConfigTargetOptions { clear_invalid_epg_ids: true, ..Default::default() });
                let mut stats = HashMap::from([(
                    Arc::clone(&input.name),
                    create_input_stat(1, 1, 0, input.input_type, &input.name, 0),
                )]);
                let mut errors = Vec::new();

                let prepared = prepare_playlist_for_target(
                    &processing_context(),
                    std::slice::from_mut(&mut playlist),
                    &target,
                    &mut stats,
                    &mut errors,
                    false,
                )
                .await
                .expect("target preparation");

                assert!(errors.is_empty());
                assert!(!prepared.playlist.is_empty());
                assert!(prepared
                    .playlist
                    .iter()
                    .flat_map(|group| &group.channels)
                    .all(|channel| channel.header.epg_channel_id.is_none()));
                assert_eq!(stats[&input.name].processed_stats.channel_count, 2);
            });
    }

    fn live_item_for_epg(name: &str) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                name: name.intern(),
                group: "Live".intern(),
                xtream_cluster: XtreamCluster::Live,
                item_type: PlaylistItemType::Live,
                ..Default::default()
            },
        }
    }

    #[test]
    fn after_epg_hook_runs_on_source_already_deduplicated_by_processing_pipe() {
        let processing = build_mapping("processing", MappingStage::Processing, r#"@group = "PROCESSED""#);
        let after_epg = build_mapping("after_epg", MappingStage::AfterEpg, r#"add_favourite("Echo")"#);
        let target = build_target(vec![processing, after_epg], true);

        let input = ConfigInput::default();
        let channel = make_channel("Alpha");
        let mut fetched =
            FetchedPlaylist { input: &input, source: memory_source(vec![channel.clone(), channel]), epg: None };
        let mut duplicates = HashSet::new();
        let (mut processed, _outcome) =
            execute_pipe(&target, &get_processing_pipe(&target), &mut fetched, &mut duplicates, false, None)
                .expect("processing pipe must run");
        assert_eq!(processed.get_channel_count(), 1, "processing pipe must remove the duplicate");

        let groups = map_playlist_at_stage(&mut processed.source, &target, MappingStage::AfterEpg, None)
            .expect("after_epg hook must run");

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].title.as_ref(), "PROCESSED");
        assert_eq!(groups[0].channels.len(), 1);
        assert_eq!(groups[1].title.as_ref(), "Echo");
        assert_eq!(groups[1].channels.len(), 1);
    }

    #[test]
    fn prepare_target_deduplicates_virtual_items_created_by_after_epg_mappings() {
        let runtime = Runtime::new().expect("runtime");
        runtime.block_on(async {
            let first = build_mapping("first", MappingStage::AfterEpg, r#"add_favourite("Echo")"#);
            let second = build_mapping("second", MappingStage::AfterEpg, r#"add_favourite("Echo")"#);
            let target = build_target(vec![first, second], true);
            let input = ConfigInput { name: "input".intern(), ..Default::default() };
            let mut playlist =
                FetchedPlaylist { input: &input, source: memory_source(vec![make_channel("Alpha")]), epg: None };
            let mut stats = HashMap::from([(
                Arc::clone(&input.name),
                create_input_stat(1, 1, 0, input.input_type, &input.name, 0),
            )]);
            let mut errors = Vec::new();

            let prepared = prepare_playlist_for_target(
                &processing_context(),
                std::slice::from_mut(&mut playlist),
                &target,
                &mut stats,
                &mut errors,
                false,
            )
            .await
            .expect("target preparation");

            assert!(errors.is_empty());
            assert_eq!(prepared.playlist.iter().map(|group| group.channels.len()).sum::<usize>(), 3);
            assert_eq!(stats[&input.name].processed_stats.channel_count, 3);
        });
    }
}

#[cfg(test)]
mod quality_rejection_fallback {
    use super::*;
    use arc_swap::{ArcSwap, ArcSwapOption};
    use shared::model::{
        xtream_const::XTREAM_CLUSTER, ConfigInputOptionsDto, ConfigInputUpdateQualityDto, ConfigPaths, EventMessage,
        EventSink, PlaylistUpdateState, SeriesStreamDetailEpisodeProperties, SeriesStreamDetailProperties,
        SeriesStreamProperties,
    };
    use std::{
        io::{ErrorKind, Read, Write},
        net::{TcpListener, TcpStream},
        path::Path,
        sync::{
            atomic::{AtomicBool, Ordering},
            Mutex as StdMutex,
        },
        thread::{self, JoinHandle},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };
    use tuliprox_core::{
        model::{MediaToolCapabilities, SourcesConfig},
        utils::FileLockManager,
    };
    use tuliprox_repository::{count_input_xtream_cluster, get_input_storage_path};

    struct TestXtreamServer {
        base_url: String,
        stop: Arc<AtomicBool>,
        requests: Arc<StdMutex<Vec<String>>>,
        errors: Arc<StdMutex<Vec<String>>>,
        worker: Option<JoinHandle<()>>,
    }

    impl TestXtreamServer {
        fn start(candidate_counts: [usize; 3]) -> Self {
            Self::start_with_responses(fixture_responses(candidate_counts))
        }

        fn start_with_responses(responses: HashMap<String, String>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind Xtream fixture server");
            listener.set_nonblocking(true).expect("configure Xtream fixture listener");
            let address = listener.local_addr().expect("Xtream fixture address");
            let stop = Arc::new(AtomicBool::new(false));
            let requests = Arc::new(StdMutex::new(Vec::new()));
            let errors = Arc::new(StdMutex::new(Vec::new()));
            let worker_stop = Arc::clone(&stop);
            let worker_requests = Arc::clone(&requests);
            let worker_errors = Arc::clone(&errors);
            let worker = thread::spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => match serve_fixture_request(stream, &responses) {
                            Ok(action) => worker_requests.lock().expect("request log lock").push(action),
                            Err(err) => worker_errors.lock().expect("server error lock").push(err.to_string()),
                        },
                        Err(err) if err.kind() == ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(1)),
                        Err(err) => {
                            worker_errors.lock().expect("server error lock").push(err.to_string());
                            break;
                        }
                    }
                }
            });

            Self { base_url: format!("http://{address}"), stop, requests, errors, worker: Some(worker) }
        }

        fn finish(mut self) -> Vec<String> {
            self.stop_worker();
            let errors = self.errors.lock().expect("server error lock").clone();
            assert!(errors.is_empty(), "Xtream fixture server errors: {errors:?}");
            self.requests.lock().expect("request log lock").clone()
        }

        fn stop_worker(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(worker) = self.worker.take() {
                worker.join().expect("Xtream fixture server should stop cleanly");
            }
        }
    }

    impl Drop for TestXtreamServer {
        fn drop(&mut self) { self.stop_worker(); }
    }

    #[derive(Clone, Default)]
    struct CollectSink(Arc<StdMutex<Vec<EventMessage>>>);

    impl EventSink for CollectSink {
        fn emit(&self, event: EventMessage) { self.0.lock().expect("event sink lock").push(event); }
    }

    fn fixture_responses(candidate_counts: [usize; 3]) -> HashMap<String, String> {
        let mut responses =
            HashMap::from([("login".to_string(), serde_json::json!({"user_info": {"status": "Active"}}).to_string())]);
        for (cluster, count) in XTREAM_CLUSTER.into_iter().zip(candidate_counts) {
            let (category_action, stream_action, category_id, id_field, id_base) = match cluster {
                XtreamCluster::Live => ("get_live_categories", "get_live_streams", 1_u32, "stream_id", 1_000_u32),
                XtreamCluster::Video => ("get_vod_categories", "get_vod_streams", 2, "stream_id", 2_000),
                XtreamCluster::Series => ("get_series_categories", "get_series", 3, "series_id", 3_000),
            };
            responses.insert(
                category_action.to_string(),
                serde_json::json!([{"category_id": category_id, "category_name": format!("candidate-{cluster}")}])
                    .to_string(),
            );
            let streams: Vec<_> = (0..count)
                .map(|offset| {
                    serde_json::json!({
                        "name": format!("candidate-{cluster}-{offset}"),
                        id_field: id_base + u32::try_from(offset).expect("fixture count fits u32"),
                        "category_id": category_id,
                    })
                })
                .collect();
            responses.insert(stream_action.to_string(), serde_json::Value::Array(streams).to_string());
        }
        responses
    }

    fn live_fixture_responses(
        categories: &serde_json::Value,
        streams: Vec<serde_json::Value>,
    ) -> HashMap<String, String> {
        HashMap::from([
            ("login".to_string(), serde_json::json!({"user_info": {"status": "Active"}}).to_string()),
            ("get_live_categories".to_string(), categories.to_string()),
            ("get_live_streams".to_string(), serde_json::Value::Array(streams).to_string()),
        ])
    }

    fn live_fixture_stream(provider_id: u32, category_id: u32, name: &str) -> serde_json::Value {
        serde_json::json!({
            "name": name,
            "stream_id": provider_id,
            "category_id": category_id,
        })
    }

    fn serve_fixture_request(mut stream: TcpStream, responses: &HashMap<String, String>) -> std::io::Result<String> {
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;
        let mut request = Vec::with_capacity(1_024);
        let mut buffer = [0_u8; 1_024];
        loop {
            let read = stream.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8_lossy(&request);
        let action = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|target| target.split_once("action=").map(|(_, value)| value))
            .map_or_else(|| "login".to_string(), |value| value.split('&').next().unwrap_or(value).to_string());
        let body = responses
            .get(&action)
            .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidInput, format!("unexpected action {action}")))?;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes())?;
        Ok(action)
    }

    fn processing_context(storage_dir: &Path) -> PlaylistProcessingContext<shared::model::NoopSink> {
        processing_context_with_events(storage_dir, shared::model::NoopSink)
    }

    fn processing_context_with_events<E: EventSink>(storage_dir: &Path, events: E) -> PlaylistProcessingContext<E> {
        let paths = ConfigPaths {
            home_path: String::new(),
            config_path: String::new(),
            storage_path: String::new(),
            config_file_path: String::new(),
            sources_file_path: String::new(),
            mapping_file_path: None,
            mapping_files_used: None,
            template_file_path: None,
            template_files_used: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        };
        let config = AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config {
                storage_dir: storage_dir.to_string_lossy().into_owned(),
                disk_based_processing: false,
                ..Config::default()
            })),
            sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            api_proxy: Arc::new(ArcSwapOption::default()),
            file_locks: Arc::new(FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(paths)),
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(MediaToolCapabilities::new()),
        };
        PlaylistProcessingContext {
            client: reqwest::Client::new(),
            config: Arc::new(config),
            user_targets: Arc::new(ProcessTargets {
                enabled: false,
                inputs: Vec::new(),
                targets: Vec::new(),
                target_names: Vec::new(),
            }),
            events,
            playlist_state: None,
            disabled_headers: None,
            processed_inputs: Arc::new(Mutex::new(HashSet::new())),
            input_locks: Arc::new(Mutex::new(HashMap::new())),
            provider_manager: None,
            metadata_manager: None,
            pre_processed_inputs: None,
            stalker_refresh_mode: StalkerRefreshMode::Complete,
            partial_refresh: Arc::new(AtomicBool::new(false)),
            had_quality_rejections: Arc::new(AtomicBool::new(false)),
        }
    }

    fn test_input(
        base_url: &str,
        update_quality: ConfigInputUpdateQualityDto,
        cache_duration_seconds: u64,
        skipped_clusters: &[XtreamCluster],
    ) -> Arc<ConfigInput> {
        let options = ConfigInputOptionsDto {
            skip_live: skipped_clusters.contains(&XtreamCluster::Live),
            skip_vod: skipped_clusters.contains(&XtreamCluster::Video),
            skip_series: skipped_clusters.contains(&XtreamCluster::Series),
            update_quality,
            ..ConfigInputOptionsDto::default()
        };
        Arc::new(ConfigInput {
            id: 1,
            name: "quality-provider".intern(),
            input_type: InputType::Xtream,
            url: base_url.to_string(),
            username: Some("user".to_string()),
            password: Some("password".to_string()),
            enabled: true,
            options: Some(ConfigInputOptions::from(&options)),
            cache_duration_seconds,
            ..ConfigInput::default()
        })
    }

    fn baseline_group(cluster: XtreamCluster, category_id: u32, title: &str, first_item_id: u32) -> PlaylistGroup {
        baseline_group_with_count(cluster, category_id, title, first_item_id, 2)
    }

    fn baseline_group_with_count(
        cluster: XtreamCluster,
        category_id: u32,
        title: &str,
        first_item_id: u32,
        count: usize,
    ) -> PlaylistGroup {
        let title = title.intern();
        let item_type = PlaylistItemType::from(cluster);
        let channels = (0..count)
            .map(|offset| {
                let offset = u32::try_from(offset).expect("baseline fixture count fits u32");
                let item_id = (first_item_id + offset).to_string().intern();
                PlaylistItem {
                    header: PlaylistItemHeader {
                        id: Arc::clone(&item_id),
                        input_stream_id: item_id,
                        name: format!("{title}-{offset}").intern(),
                        title: format!("{title}-{offset}").intern(),
                        group: Arc::clone(&title),
                        url: format!("http://old.example/{cluster}/{first_item_id}/{offset}").intern(),
                        input_name: "quality-provider".intern(),
                        item_type,
                        xtream_cluster: cluster,
                        category_id,
                        ..PlaylistItemHeader::default()
                    },
                }
            })
            .collect();
        PlaylistGroup { id: category_id, title, channels, xtream_cluster: cluster }
    }

    async fn seed_live_baseline<E: EventSink>(
        ctx: &PlaylistProcessingContext<E>,
        input: &ConfigInput,
        first_item_id: u32,
    ) {
        let baseline = baseline_group_with_count(XtreamCluster::Live, 1, "old-live", first_item_id, 100);
        let (persisted, error) = persist_input_playlist(&ctx.config, input, vec![baseline]).await;
        assert!(error.is_none(), "Live baseline persistence failed: {error:?}");
        assert_eq!(persisted.iter().map(|group| group.channels.len()).sum::<usize>(), 100);
    }

    fn baseline_groups() -> Vec<PlaylistGroup> {
        vec![
            baseline_group(XtreamCluster::Live, 1, "old-live", 100),
            baseline_group(XtreamCluster::Video, 2, "old-vod", 200),
            baseline_group(XtreamCluster::Series, 3, "old-series", 300),
        ]
    }

    async fn seed_baseline<E: EventSink>(ctx: &PlaylistProcessingContext<E>, input: &ConfigInput) {
        let (persisted, error) = persist_input_playlist(&ctx.config, input, baseline_groups()).await;
        assert!(error.is_none(), "baseline persistence failed: {error:?}");
        assert_baseline(&persisted);
    }

    fn assert_baseline(groups: &[PlaylistGroup]) {
        for (cluster, category_id, title, item_ids) in [
            (XtreamCluster::Live, 1, "old-live", ["100", "101"]),
            (XtreamCluster::Video, 2, "old-vod", ["200", "201"]),
            (XtreamCluster::Series, 3, "old-series", ["300", "301"]),
        ] {
            let group = groups
                .iter()
                .find(|group| group.xtream_cluster == cluster && group.id == category_id)
                .unwrap_or_else(|| panic!("missing persisted {cluster} group"));
            assert_eq!(group.title.as_ref(), title);
            assert_eq!(group.channels.len(), 2);
            assert!(group.channels.iter().all(|item| item.header.xtream_cluster == cluster));
            assert_eq!(group.channels[0].header.id.as_ref(), item_ids[0]);
            assert_eq!(group.channels[1].header.id.as_ref(), item_ids[1]);
        }
    }

    async fn input_storage_path<E: EventSink>(ctx: &PlaylistProcessingContext<E>, input: &ConfigInput) -> PathBuf {
        let storage_dir = ctx.config.config.load().storage_dir.clone();
        get_input_storage_path(&input.name, &storage_dir).await.expect("input storage path")
    }

    fn assert_requested_actions(mut actual: Vec<String>, expected: &[&str]) {
        actual.sort();
        let mut expected: Vec<_> = expected.iter().map(ToString::to_string).collect();
        expected.sort();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn mixed_in_memory_update_keeps_rejected_vod_ready_and_marks_the_run_partial() {
        let temp = tempfile::tempdir().expect("temporary storage");
        let server = TestXtreamServer::start([2, 1, 2]);
        let events = CollectSink::default();
        let ctx = processing_context_with_events(temp.path(), events.clone());
        let input =
            test_input(&server.base_url, ConfigInputUpdateQualityDto { live: 100, vod: 100, series: 100 }, 0, &[]);
        seed_baseline(&ctx, &input).await;

        let mut result = process_input_job_inner(0, &ctx, &input).await;
        let requests = server.finish();

        assert_eq!(result.state, InputJobState::Ready);
        assert!(result.errors.is_empty(), "quality rejection must not become an error: {:?}", result.errors);
        assert!(ctx.had_quality_rejections.load(Ordering::Acquire));
        assert_eq!(
            PlaylistRunSignals { has_quality_rejections: true, ..PlaylistRunSignals::default() }.state(),
            PlaylistUpdateState::Partial
        );

        let groups = result.source.take().expect("ready input source").take_groups();
        let live =
            groups.iter().find(|group| group.xtream_cluster == XtreamCluster::Live).expect("accepted Live group");
        let vod = groups.iter().find(|group| group.xtream_cluster == XtreamCluster::Video).expect("retained VOD group");
        let series =
            groups.iter().find(|group| group.xtream_cluster == XtreamCluster::Series).expect("accepted Series group");
        assert_eq!(live.title.as_ref(), "candidate-live");
        assert_eq!(live.channels.len(), 2);
        assert_eq!(vod.title.as_ref(), "old-vod");
        assert_eq!(vod.channels.len(), 2);
        assert_eq!(series.title.as_ref(), "candidate-series");
        assert_eq!(series.channels.len(), 2);

        let storage_path = input_storage_path(&ctx, &input).await;
        let status = input_cache::load_input_status(&storage_path);
        assert_eq!(
            status.clusters.get(XtreamCluster::Live.as_ref()).map(|entry| &entry.status),
            Some(&input_cache::ClusterState::Ok)
        );
        assert_eq!(
            status.clusters.get(XtreamCluster::Video.as_ref()).map(|entry| &entry.status),
            Some(&input_cache::ClusterState::Failed)
        );
        assert_eq!(
            status.clusters.get(XtreamCluster::Series.as_ref()).map(|entry| &entry.status),
            Some(&input_cache::ClusterState::Ok)
        );

        let emitted = events.0.lock().expect("event sink lock");
        let rejection_events = emitted
            .iter()
            .filter_map(|event| match event {
                EventMessage::PlaylistUpdateProgress(progress)
                    if progress.message.contains("cluster 'vod' rejected") =>
                {
                    Some(progress)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(rejection_events.len(), 1);
        assert_eq!(rejection_events[0].target, "quality-provider");
        assert_eq!(
            rejection_events[0].message,
            "Input 'quality-provider' cluster 'vod' rejected: current=2 candidate=1 threshold=100 quality=50; retaining previous cluster"
        );
        assert_requested_actions(
            requests,
            &[
                "login",
                "get_live_categories",
                "get_live_streams",
                "get_vod_categories",
                "get_vod_streams",
                "get_series_categories",
                "get_series",
            ],
        );
    }

    #[tokio::test]
    async fn in_memory_series_quality_compares_catalog_rows_with_an_enriched_baseline() {
        let temp = tempfile::tempdir().expect("temporary storage");
        let server = TestXtreamServer::start([0, 0, 1]);
        let ctx = processing_context(temp.path());
        let input = test_input(
            &server.base_url,
            ConfigInputUpdateQualityDto { series: 100, ..ConfigInputUpdateQualityDto::default() },
            0,
            &[XtreamCluster::Live, XtreamCluster::Video],
        );
        let mut baseline = baseline_group(XtreamCluster::Series, 3, "old-series", 3_000);
        baseline.channels.truncate(1);
        let episodes = (0..200_u32)
            .map(|id| SeriesStreamDetailEpisodeProperties { id, ..SeriesStreamDetailEpisodeProperties::default() })
            .collect();
        baseline.channels[0].header.additional_properties =
            Some(StreamProperties::Series(Box::new(SeriesStreamProperties {
                series_id: 3_000,
                details: Some(SeriesStreamDetailProperties::new(None, Vec::new(), Some(episodes))),
                ..SeriesStreamProperties::default()
            })));
        let (persisted, error) = persist_input_playlist(&ctx.config, &input, vec![baseline]).await;
        assert!(error.is_none(), "enriched baseline persistence failed: {error:?}");
        assert_eq!(persisted.iter().map(|group| group.channels.len()).sum::<usize>(), 1);
        assert_eq!(
            count_input_xtream_cluster(&ctx.config, &input, XtreamCluster::Series)
                .await
                .expect("enriched baseline should be countable"),
            Some(1)
        );

        let mut result = download_input(&ctx, &input, false).await;
        let requests = server.finish();

        assert!(result.errors.is_empty(), "Series update failed: {:?}", result.errors);
        assert!(result.quality_rejections.is_empty());
        let groups = result.source.take_groups();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].title.as_ref(), "candidate-series");
        assert_eq!(groups[0].channels.len(), 1);
        let episode_count = groups[0].channels[0]
            .header
            .additional_properties
            .as_ref()
            .and_then(|properties| match properties {
                StreamProperties::Series(series) => series.details.as_ref(),
                _ => None,
            })
            .and_then(|details| details.episodes.as_ref())
            .map(Vec::len);
        assert_eq!(episode_count, Some(200));
        assert_requested_actions(requests, &["login", "get_series_categories", "get_series"]);
    }

    #[tokio::test]
    async fn in_memory_quality_rejection_counts_duplicate_provider_ids_once_and_retains_the_baseline() {
        let temp = tempfile::tempdir().expect("temporary storage");
        let first_provider_id = 1_000_u32;
        let streams = (0..50_u32)
            .flat_map(|offset| {
                let provider_id = first_provider_id + offset;
                [
                    live_fixture_stream(provider_id, 1, &format!("candidate-{provider_id}")),
                    live_fixture_stream(provider_id, 1, &format!("duplicate-{provider_id}")),
                ]
            })
            .collect();
        let responses = live_fixture_responses(
            &serde_json::json!([{"category_id": 1, "category_name": "candidate-live"}]),
            streams,
        );
        let server = TestXtreamServer::start_with_responses(responses);
        let ctx = processing_context(temp.path());
        let input = test_input(
            &server.base_url,
            ConfigInputUpdateQualityDto { live: 100, ..ConfigInputUpdateQualityDto::default() },
            0,
            &[XtreamCluster::Video, XtreamCluster::Series],
        );
        seed_live_baseline(&ctx, &input, first_provider_id).await;

        let mut result = download_input(&ctx, &input, false).await;
        let requests = server.finish();

        assert!(result.errors.is_empty(), "duplicate rejection must not become an error: {:?}", result.errors);
        assert!(result.storage_error.is_none());
        assert_eq!(
            result.quality_rejections,
            vec![ClusterUpdateRejection {
                cluster: XtreamCluster::Live,
                current_count: 100,
                candidate_count: 50,
                threshold: 100,
                quality: 50,
            }]
        );
        let groups = result.source.take_groups();
        assert_eq!(groups.iter().map(|group| group.channels.len()).sum::<usize>(), 100);
        assert_eq!(groups[0].title.as_ref(), "old-live");

        let mut reloaded = load_input_playlist(&ctx.config, &input, None).await.expect("reload retained Live baseline");
        let reloaded_groups = reloaded.take_groups();
        assert_eq!(reloaded_groups.iter().map(|group| group.channels.len()).sum::<usize>(), 100);
        assert_eq!(reloaded_groups[0].title.as_ref(), "old-live");
        assert_eq!(
            count_input_xtream_cluster(&ctx.config, &input, XtreamCluster::Live)
                .await
                .expect("retained baseline should be countable"),
            Some(100)
        );
        assert_requested_actions(requests, &["login", "get_live_categories", "get_live_streams"]);
    }

    #[tokio::test]
    async fn in_memory_quality_accepts_unique_population_and_persists_the_last_duplicate_row() {
        let temp = tempfile::tempdir().expect("temporary storage");
        let first_provider_id = 1_000_u32;
        let winning_provider_id = first_provider_id + 42;
        let mut streams = (0..100_u32)
            .map(|offset| {
                let provider_id = first_provider_id + offset;
                live_fixture_stream(provider_id, 1, &format!("candidate-{provider_id}"))
            })
            .collect::<Vec<_>>();
        streams.push(live_fixture_stream(winning_provider_id, 2, "winning-duplicate"));
        let responses = live_fixture_responses(
            &serde_json::json!([
                {"category_id": 1, "category_name": "candidate-live"},
                {"category_id": 2, "category_name": "winning-category"}
            ]),
            streams,
        );
        let server = TestXtreamServer::start_with_responses(responses);
        let ctx = processing_context(temp.path());
        let input = test_input(
            &server.base_url,
            ConfigInputUpdateQualityDto { live: 100, ..ConfigInputUpdateQualityDto::default() },
            0,
            &[XtreamCluster::Video, XtreamCluster::Series],
        );
        seed_live_baseline(&ctx, &input, first_provider_id).await;

        let mut result = download_input(&ctx, &input, false).await;
        let requests = server.finish();

        assert!(result.errors.is_empty(), "deduplicated acceptance failed: {:?}", result.errors);
        assert!(result.storage_error.is_none());
        assert!(result.quality_rejections.is_empty());
        let groups = result.source.take_groups();
        assert_eq!(groups.iter().map(|group| group.channels.len()).sum::<usize>(), 100);
        let winner_group = groups.iter().find(|group| group.id == 2).expect("winning category");
        assert_eq!(winner_group.title.as_ref(), "winning-category");
        assert_eq!(winner_group.channels.len(), 1);
        assert_eq!(winner_group.channels[0].header.id.as_ref(), winning_provider_id.to_string());
        assert_eq!(winner_group.channels[0].header.name.as_ref(), "winning-duplicate");
        assert_eq!(winner_group.channels[0].header.category_id, 2);

        let mut reloaded =
            load_input_playlist(&ctx.config, &input, None).await.expect("reload accepted Live candidate");
        let reloaded_groups = reloaded.take_groups();
        assert_eq!(reloaded_groups.iter().map(|group| group.channels.len()).sum::<usize>(), 100);
        let reloaded_winner = reloaded_groups.iter().find(|group| group.id == 2).expect("persisted winning category");
        assert_eq!(reloaded_winner.channels.len(), 1);
        assert_eq!(reloaded_winner.channels[0].header.id.as_ref(), winning_provider_id.to_string());
        assert_eq!(reloaded_winner.channels[0].header.name.as_ref(), "winning-duplicate");
        assert_eq!(reloaded_winner.channels[0].header.category_id, 2);
        assert_eq!(
            count_input_xtream_cluster(&ctx.config, &input, XtreamCluster::Live)
                .await
                .expect("accepted candidate should be countable"),
            Some(100)
        );
        assert_requested_actions(requests, &["login", "get_live_categories", "get_live_streams"]);
    }

    #[tokio::test]
    async fn m3u_updates_ignore_cluster_quality_thresholds() {
        let temp = tempfile::tempdir().expect("temporary storage");
        let playlist_path = temp.path().join("input.m3u");
        tokio::fs::write(
            &playlist_path,
            "#EXTM3U\n#EXTINF:-1,Old One\nhttp://old.example/1\n#EXTINF:-1,Old Two\nhttp://old.example/2\n",
        )
        .await
        .expect("initial M3U fixture");
        let options = ConfigInputOptionsDto {
            update_quality: ConfigInputUpdateQualityDto { live: 100, vod: 100, series: 100 },
            ..ConfigInputOptionsDto::default()
        };
        let input = Arc::new(ConfigInput {
            id: 2,
            name: "m3u-outside-quality-guard".intern(),
            input_type: InputType::M3u,
            url: playlist_path.to_string_lossy().into_owned(),
            enabled: true,
            options: Some(ConfigInputOptions::from(&options)),
            ..ConfigInput::default()
        });
        let initial_ctx = processing_context(temp.path());

        let mut initial = download_input(&initial_ctx, &input, false).await;

        assert!(initial.errors.is_empty(), "initial M3U download failed: {:?}", initial.errors);
        assert!(initial.quality_rejections.is_empty());
        assert_eq!(initial.source.get_channel_count(), 2);

        tokio::fs::write(&playlist_path, "#EXTM3U\n#EXTINF:-1,New\nhttp://new.example/1\n")
            .await
            .expect("replacement M3U fixture");
        let refreshed_ctx = processing_context(temp.path());
        let mut refreshed = download_input(&refreshed_ctx, &input, false).await;

        assert!(refreshed.errors.is_empty(), "replacement M3U download failed: {:?}", refreshed.errors);
        assert!(refreshed.storage_error.is_none());
        assert!(refreshed.quality_rejections.is_empty());
        assert_eq!(refreshed.source.get_channel_count(), 1);
        let mut reloaded = load_input_playlist(&refreshed_ctx.config, &input, None)
            .await
            .expect("replacement M3U should be persisted");
        assert_eq!(reloaded.get_channel_count(), 1);
    }

    #[tokio::test]
    async fn quality_rejection_for_only_requested_vod_loads_complete_persisted_input() {
        let temp = tempfile::tempdir().expect("temporary storage");
        let server = TestXtreamServer::start([0, 1, 0]);
        let ctx = processing_context(temp.path());
        let input = test_input(
            &server.base_url,
            ConfigInputUpdateQualityDto { vod: 100, ..ConfigInputUpdateQualityDto::default() },
            3_600,
            &[],
        );
        seed_baseline(&ctx, &input).await;
        let storage_path = input_storage_path(&ctx, &input).await;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("current time").as_secs();
        let mut status = input_cache::InputStatus::default();
        status.clusters.insert(
            XtreamCluster::Live.as_ref().to_string(),
            input_cache::ClusterStatus { status: input_cache::ClusterState::Ok, timestamp: now },
        );
        status.clusters.insert(
            XtreamCluster::Video.as_ref().to_string(),
            input_cache::ClusterStatus { status: input_cache::ClusterState::Ok, timestamp: now.saturating_sub(3_601) },
        );
        status.clusters.insert(
            XtreamCluster::Series.as_ref().to_string(),
            input_cache::ClusterStatus { status: input_cache::ClusterState::Ok, timestamp: now },
        );
        input_cache::save_input_status(&storage_path, &status);

        let mut result = download_input(&ctx, &input, false).await;
        let requests = server.finish();

        assert!(result.errors.is_empty(), "quality rejection must not become an error: {:?}", result.errors);
        assert!(result.storage_error.is_none());
        assert!(!result.partial);
        assert_eq!(result.job_state(), InputJobState::Ready);
        assert_eq!(result.quality_rejections.len(), 1);
        assert_eq!(
            result.quality_rejections[0],
            ClusterUpdateRejection {
                cluster: XtreamCluster::Video,
                current_count: 2,
                candidate_count: 1,
                threshold: 100,
                quality: 50,
            }
        );
        assert_eq!(
            PlaylistRunSignals {
                has_quality_rejections: !result.quality_rejections.is_empty(),
                ..PlaylistRunSignals::default()
            }
            .state(),
            PlaylistUpdateState::Partial
        );
        assert_baseline(&result.source.take_groups());

        let status = input_cache::load_input_status(&storage_path);
        let live = status.clusters.get(XtreamCluster::Live.as_ref()).expect("Live cache status");
        let vod = status.clusters.get(XtreamCluster::Video.as_ref()).expect("VOD cache status");
        let series = status.clusters.get(XtreamCluster::Series.as_ref()).expect("Series cache status");
        assert_eq!(live.status, input_cache::ClusterState::Ok);
        assert_eq!(live.timestamp, now);
        assert_eq!(vod.status, input_cache::ClusterState::Failed);
        assert_eq!(series.status, input_cache::ClusterState::Ok);
        assert_eq!(series.timestamp, now);
        assert_requested_actions(requests, &["login", "get_vod_categories", "get_vod_streams"]);
    }

    #[tokio::test]
    async fn all_requested_clusters_rejected_load_the_complete_unchanged_baseline() {
        let temp = tempfile::tempdir().expect("temporary storage");
        let server = TestXtreamServer::start([1, 1, 1]);
        let ctx = processing_context(temp.path());
        let input =
            test_input(&server.base_url, ConfigInputUpdateQualityDto { live: 100, vod: 100, series: 100 }, 0, &[]);
        seed_baseline(&ctx, &input).await;

        let mut result = download_input(&ctx, &input, false).await;
        let requests = server.finish();

        assert!(result.errors.is_empty(), "quality rejections must not become errors: {:?}", result.errors);
        assert!(result.storage_error.is_none());
        assert_eq!(result.job_state(), InputJobState::Ready);
        assert_eq!(
            result.quality_rejections.iter().map(|rejection| rejection.cluster).collect::<Vec<_>>(),
            XTREAM_CLUSTER
        );
        assert!(result.quality_rejections.iter().all(|rejection| {
            rejection.current_count == 2
                && rejection.candidate_count == 1
                && rejection.threshold == 100
                && rejection.quality == 50
        }));
        assert_baseline(&result.source.take_groups());

        let mut reloaded = load_input_playlist(&ctx.config, &input, None).await.expect("reload persisted baseline");
        assert_baseline(&reloaded.take_groups());
        let storage_path = input_storage_path(&ctx, &input).await;
        let status = input_cache::load_input_status(&storage_path);
        assert!(XTREAM_CLUSTER.iter().all(|cluster| {
            status.clusters.get(cluster.as_ref()).map(|entry| &entry.status) == Some(&input_cache::ClusterState::Failed)
        }));
        assert_requested_actions(
            requests,
            &[
                "login",
                "get_live_categories",
                "get_live_streams",
                "get_vod_categories",
                "get_vod_streams",
                "get_series_categories",
                "get_series",
            ],
        );
    }

    #[tokio::test]
    async fn quality_rejection_for_empty_bootstrap_does_not_invent_a_baseline() {
        let temp = tempfile::tempdir().expect("temporary storage");
        let server = TestXtreamServer::start([0, 0, 0]);
        let ctx = processing_context(temp.path());
        let input = test_input(
            &server.base_url,
            ConfigInputUpdateQualityDto { vod: 100, ..ConfigInputUpdateQualityDto::default() },
            0,
            &[XtreamCluster::Live, XtreamCluster::Series],
        );

        let mut result = download_input(&ctx, &input, false).await;
        let requests = server.finish();

        assert!(result.errors.is_empty(), "unexpected bootstrap errors: {:?}", result.errors);
        assert!(result.storage_error.is_none());
        assert_eq!(result.quality_rejections.len(), 1);
        assert_eq!(
            result.quality_rejections[0],
            ClusterUpdateRejection {
                cluster: XtreamCluster::Video,
                current_count: 0,
                candidate_count: 0,
                threshold: 100,
                quality: 0,
            }
        );
        assert!(result.source.is_empty());
        assert_eq!(result.job_state(), InputJobState::Failed);
        let mut reloaded = load_input_playlist(&ctx.config, &input, None).await.expect("empty bootstrap reload");
        assert!(reloaded.take_groups().is_empty());
        assert_requested_actions(requests, &["login", "get_vod_categories", "get_vod_streams"]);
    }
}

#[cfg(test)]
mod disk_epg_wireup_tests {
    use super::spill_epg_to_disk;
    use shared::model::EpgChannel;
    use std::sync::Arc;
    use tuliprox_core::model::Epg;

    /// Build an `Epg` with `channel_count` channels whose ids follow the
    /// `id_base` prefix. Two sources built with the same `id_base` and
    /// `channel_count` will share all channel ids, which is what we need to
    /// exercise the priority-override `Occupied` branch in
    /// `EpgMergeAccumulator::upsert_channel`.
    fn build_epg(id_base: &str, priority: i16, channel_count: usize) -> Epg {
        Epg {
            priority,
            logo_override: false,
            attributes: None,
            children: (0..channel_count)
                .map(|i| {
                    let id: Arc<str> = format!("{id_base}-ch-{i:04}").into();
                    Arc::new(EpgChannel {
                        id: Arc::clone(&id),
                        title: Some(format!("title-{priority}-{i}").into()),
                        icon: None,
                        programmes: vec![shared::model::EpgProgramme::new(
                            i64::try_from(i).expect("test index fits in i64"),
                            i64::try_from(i + 1).expect("test index fits in i64"),
                            id,
                        )],
                    })
                })
                .collect(),
        }
    }

    /// Wire-up regression guard: `spill_epg_to_disk` is the function called
    /// by `finalize_prepared_target` when `disk_based_processing = true`. It
    /// must (a) preserve per-source priority on shared channels, (b) clean up
    /// its temp files, and (c) merge into a single `Epg` of the right size.
    ///
    /// Both sources share channel ids (`shared-ch-NNNN`), forcing the merge
    /// to take the `Occupied` branch in `EpgMergeAccumulator::upsert_channel`.
    /// The lower-priority source (priority 3) must win, the higher-priority
    /// (priority 7) must be discarded for shared ids. Without this assertion
    /// the test would pass even if priority resolution were broken — the
    /// earlier version used unique ids and therefore never hit the merge path.
    #[test]
    fn spill_epg_to_disk_merges_shared_channels_by_priority() {
        let epg_low = build_epg("shared", 3, 50); // wins on every shared channel
        let epg_high = build_epg("shared", 7, 50); // discarded on every shared channel

        let merged = spill_epg_to_disk(vec![epg_low, epg_high])
            .expect("disk merge returned an error")
            .expect("merged Epg is unexpectedly None for two non-empty sources");

        // 50 distinct channels, not 100 — the merge must have collapsed the
        // shared ids.
        assert_eq!(merged.children.len(), 50, "shared channel ids must collapse to one entry, not be duplicated");

        // Every channel title comes from the lower-priority source. If the
        // merge logic is wrong, some titles will carry the "-7-" marker.
        for ch in &merged.children {
            let title = ch.title.as_deref().expect("title preserved through merge");
            assert!(
                title.starts_with("title-3-"),
                "channel {:?} kept title {title:?} from higher-priority source; \
                 priority override is broken",
                ch.id,
            );
            // `add_channel_with_programmes` on the disk-merge path must
            // preserve the lower-priority source's single programme per
            // channel — `upsert_channel` would silently drop them.
            assert_eq!(ch.programmes.len(), 1, "channel {:?} lost programmes through the disk-merge path", ch.id);
            let prog = &ch.programmes[0];
            assert!(prog.title.is_none() || prog.title.as_deref() != Some("title-7"));
        }
    }

    /// The non-shared case: sources with disjoint channel ids. Both
    /// sources' channels appear in the result with no priority loss (no
    /// `Occupied` branch is taken).
    #[test]
    fn spill_epg_to_disk_keeps_disjoint_sources_intact() {
        let epg_low = build_epg("src-a", 3, 50);
        let epg_high = build_epg("src-b", 7, 50);

        let merged = spill_epg_to_disk(vec![epg_low, epg_high])
            .expect("disk merge returned an error")
            .expect("merged Epg is unexpectedly None for two non-empty sources");

        assert_eq!(merged.children.len(), 100, "disjoint ids must not collapse");
        assert!(merged.children.iter().any(|ch| ch.title.as_deref() == Some("title-3-0")));
        assert!(merged.children.iter().any(|ch| ch.title.as_deref() == Some("title-7-0")));
    }

    #[test]
    fn spill_epg_to_disk_returns_none_for_empty_input() {
        let merged = spill_epg_to_disk(vec![]).expect("disk merge returned an error");
        assert!(merged.is_none());
    }
}
