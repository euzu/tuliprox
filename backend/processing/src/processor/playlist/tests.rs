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
use tuliprox_core::model::{CompiledMappingRule, CompiledTargetMappings, Config};

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
    let filter = tuliprox_core::model::TargetBouquetFilter::from_dto(bouquet_dto).unwrap();

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
    let filter = tuliprox_core::model::TargetBouquetFilter::from_dto(bouquet_dto).unwrap();

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
        }
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
