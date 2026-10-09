use super::{
    apply_persist_filter, assign_channel_no_playlist, catalog_membership, catalog_test_item,
    complete_catalog_evaluation, execute_pipe, execute_pipeline_on_groups, filter_skipped_clusters_from_source,
    retain_playlist_items, target, test_group, InputDownloadResult, InputJobState, PlaylistRunSignals, RenameOutcome,
};
use crate::fetched_playlist::FetchedPlaylist;
use shared::{
    foundation::{get_filter, ValueProvider},
    model::{
        ClusterFlags, ConfigRenameDto, ConfigTargetDto, ConfigTargetOptions, InputType, ItemField, PlaylistGroup,
        PlaylistItem, PlaylistItemHeader, PlaylistItemType, PlaylistUpdateState, XtreamCluster,
    },
    utils::Internable,
};
use std::collections::HashSet;
use tuliprox_core::model::{
    ClusterUpdateRejection, ConfigInput, ConfigRename, ConfigTarget, FilterOutcome, TransformStage,
};
use tuliprox_curation::{CurationMediaKind, CurationRunOutcome};
use tuliprox_repository::{MemoryPlaylistSource, PlaylistPublicationPlan};

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

#[test]
fn quality_rejection_keeps_a_usable_input_ready() {
    let mut result = InputDownloadResult {
        authoritative_library: false,
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
        accepted_empty_clusters: ClusterFlags::empty(),
        input_telemetry: None,
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

pub(in crate::processor::playlist::tests) fn make_test_item(name: &str, item_type: PlaylistItemType) -> PlaylistItem {
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

#[test]
fn persist_filter_can_select_a_base_group() {
    let mut target = ConfigTarget::from(&ConfigTargetDto::default());
    target.filter.persist = Some(get_filter(r#"Group = "Base""#, None).expect("persist filter"));
    let mut playlist = vec![
        PlaylistGroup {
            id: 1,
            title: "Base".intern(),
            channels: vec![PlaylistItem {
                header: PlaylistItemHeader { group: "Base".intern(), ..Default::default() },
            }],
            xtream_cluster: XtreamCluster::Video,
        },
        PlaylistGroup {
            id: 2,
            title: "Curated".intern(),
            channels: vec![PlaylistItem {
                header: PlaylistItemHeader { group: "Curated".intern(), ..Default::default() },
            }],
            xtream_cluster: XtreamCluster::Video,
        },
    ];

    apply_persist_filter(&target, &mut playlist);

    assert_eq!(playlist.len(), 1);
    assert_eq!(playlist[0].title.as_ref(), "Base");
}

#[test]
fn tmdb_complete_target_selection_preserves_live_and_keeps_xtream_aliases_out_of_normal_outputs() {
    for has_xtream in [false, true] {
        let mut value = serde_json::json!({"name": "discovery", "output": [{"type": "m3u"}],
            "curation": {"catalog_selection": "curated", "tmdb": {"trending": [{"kind": "movie", "time_window": "week", "limit": 100, "create_xtream_category": has_xtream, "category_name": "TMDB"}]}}});
        if has_xtream {
            value["output"].as_array_mut().unwrap().push(serde_json::json!({"type": "xtream"}));
        }
        let mut dto: ConfigTargetDto = serde_json::from_value(value).unwrap();
        dto.prepare(1, None, None).unwrap();
        let target = ConfigTarget::from(&dto);
        let id = shared::utils::hash_string("selected");
        let live_id = shared::utils::hash_string("live");
        let playlist = vec![
            PlaylistGroup {
                id: 1,
                title: "Live".intern(),
                xtream_cluster: XtreamCluster::Live,
                channels: vec![catalog_test_item("Live", live_id, PlaylistItemType::Live, XtreamCluster::Live, None)],
            },
            PlaylistGroup {
                id: 2,
                title: "Movies".intern(),
                xtream_cluster: XtreamCluster::Video,
                channels: vec![
                    catalog_test_item("Selected", id, PlaylistItemType::Video, XtreamCluster::Video, None),
                    catalog_test_item(
                        "Other",
                        shared::utils::hash_string("other"),
                        PlaylistItemType::Video,
                        XtreamCluster::Video,
                        None,
                    ),
                ],
            },
        ];
        let config = target.effective_curation().unwrap();
        let outcome = CurationRunOutcome::Complete(complete_catalog_evaluation(vec![catalog_membership(
            id,
            CurationMediaKind::Movie,
            0,
        )]));
        let views = target::curation_playlist_views(&target, playlist.clone(), &config, outcome);
        assert_eq!(
            views.base.iter().flat_map(|g| &g.channels).map(|i| i.header.uuid).collect::<Vec<_>>(),
            [live_id, id]
        );
        if has_xtream {
            let groups = views.xtream.unwrap();
            let projected = groups.iter().find(|g| g.title.as_ref() == "TMDB").unwrap();
            assert_ne!(projected.channels[0].header.uuid, id);
        } else {
            assert!(views.xtream.is_none());
        }
        let empty = CurationRunOutcome::Complete(complete_catalog_evaluation(Vec::new()));
        let empty_views = target::curation_playlist_views(&target, playlist, &config, empty);
        assert_eq!(empty_views.base.len(), 1);
        assert_eq!(empty_views.base[0].xtream_cluster, XtreamCluster::Live);
        assert_ne!(empty_views.publication_plan, PlaylistPublicationPlan::Ordinary);
    }
}
