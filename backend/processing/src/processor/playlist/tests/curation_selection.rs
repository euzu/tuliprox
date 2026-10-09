use super::{
    apply_persist_filter, build_curated_playlist_views, catalog_membership, catalog_test_item,
    complete_catalog_evaluation, prepare_eligible_catalog, prepare_target_playlist_views, select_target_catalog,
    FinalizationStage, FINALIZATION_ORDER,
};
use shared::{
    foundation::get_filter,
    model::{
        ConfigTargetDto, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType, TargetOutputDto,
        TraktApiConfigDto, TraktCatalogSelection, TraktConfigDto, TraktContentType, TraktListConfigDto, UUIDType,
        XtreamCluster, XtreamTargetOutputDto,
    },
    utils::Internable,
};
use tuliprox_core::{
    model::{ConfigTarget, CurationConfig, TraktConfig},
    utils::StepMeasure,
};
use tuliprox_curation::{
    CurationEvaluation, CurationMediaKind, CurationMembership, CurationSelectorKey, CurationSelectorSummary,
};
use tuliprox_repository::PlaylistPublicationPlan;

#[test]
fn current_target_finalization_order_merges_before_dedup_and_presentation() {
    assert_eq!(
        FINALIZATION_ORDER,
        [
            FinalizationStage::Merge,
            FinalizationStage::Deduplicate,
            FinalizationStage::Sort,
            FinalizationStage::AssignChannelNumbers,
            FinalizationStage::AssignCounters,
        ]
    );
}

#[test]
fn curation_eligible_catalog_is_merged_and_deduplicated_before_matching() {
    let mut target = ConfigTarget::from(&ConfigTargetDto::default());
    target.execution_plan.post_merge_content_dedup = Some(shared::model::DeduplicateConfig::default());
    let losing_uuid = UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000051");
    let winning_uuid = UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000052");
    let playlist = vec![
        PlaylistGroup {
            id: 1,
            title: "Movies".intern(),
            channels: vec![catalog_test_item(
                "Movie HD",
                losing_uuid,
                PlaylistItemType::Video,
                XtreamCluster::Video,
                None,
            )],
            xtream_cluster: XtreamCluster::Video,
        },
        PlaylistGroup {
            id: 2,
            title: "movies".intern(),
            channels: vec![catalog_test_item(
                "Movie 4K",
                winning_uuid,
                PlaylistItemType::Video,
                XtreamCluster::Video,
                None,
            )],
            xtream_cluster: XtreamCluster::Video,
        },
    ];
    let mut step = StepMeasure::new("test", |_, _| {});

    let eligible = prepare_eligible_catalog(&target, playlist, &mut step);

    assert_eq!(eligible.len(), 1);
    assert_eq!(eligible[0].channels.len(), 1);
    assert_eq!(eligible[0].channels[0].header.uuid, winning_uuid);
}

#[test]
fn persist_filter_can_select_a_generated_curation_group() {
    let mut target = ConfigTarget::from(&ConfigTargetDto::default());
    target.filter.persist = Some(get_filter(r#"Group = "Trending""#, None).expect("persist filter"));
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
            title: "Trending".intern(),
            channels: vec![PlaylistItem {
                header: PlaylistItemHeader { group: "Trending".intern(), ..Default::default() },
            }],
            xtream_cluster: XtreamCluster::Video,
        },
    ];

    apply_persist_filter(&target, &mut playlist);

    assert_eq!(playlist.len(), 1);
    assert_eq!(playlist[0].title.as_ref(), "Trending");
}

#[tokio::test]
async fn trakt_target_curation_is_a_noop_without_xtream_configuration() {
    let target = ConfigTarget::from(&ConfigTargetDto::default());

    let views = prepare_target_playlist_views(&reqwest::Client::new(), None, &target, Vec::new()).await;

    assert!(views.base.is_empty());
    assert!(views.xtream.is_none());
    assert_eq!(views.publication_plan, PlaylistPublicationPlan::Ordinary);
}

#[tokio::test]
async fn unavailable_required_selector_continues_with_the_base_catalog() {
    let target = ConfigTarget::from(&ConfigTargetDto {
        name: "curation-failure".to_string(),
        output: vec![TargetOutputDto::Xtream(XtreamTargetOutputDto {
            trakt: Some(TraktConfigDto {
                lists: vec![TraktListConfigDto {
                    user: "alice".to_string(),
                    list_slug: "watchlist".to_string(),
                    category_name: Some("Watchlist".to_string()),
                    create_xtream_category: true,
                    content_type: TraktContentType::Vod,
                    tmdb_only: true,
                    fuzzy_match_threshold: 100,
                }],
                ..TraktConfigDto::default()
            }),
            ..XtreamTargetOutputDto::default()
        })],
        ..ConfigTargetDto::default()
    });
    let base = vec![PlaylistGroup {
        id: 1,
        title: "Movies".intern(),
        channels: vec![catalog_test_item(
            "Base movie",
            UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000061"),
            PlaylistItemType::Video,
            XtreamCluster::Video,
            None,
        )],
        xtream_cluster: XtreamCluster::Video,
    }];

    let views = prepare_target_playlist_views(&reqwest::Client::new(), None, &target, base).await;
    assert_eq!(views.base.len(), 1);
    assert_eq!(views.base[0].channels[0].header.title.as_ref(), "Base movie");
    assert!(views.xtream.is_none());
    assert_eq!(views.publication_plan, PlaylistPublicationPlan::Ordinary);
}

#[test]
fn catalog_selection_preserves_live_and_selected_series_children() {
    let live_uuid = UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000001");
    let selected_movie_uuid = UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000002");
    let rejected_movie_uuid = UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000003");
    let series_uuid = UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000004");
    let episode_uuid = UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000005");
    let rejected_series_uuid = UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000006");
    let rejected_episode_uuid = UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000007");
    let playlist = vec![
        PlaylistGroup {
            id: 1,
            title: "Live".intern(),
            channels: vec![catalog_test_item(
                "Live channel",
                live_uuid,
                PlaylistItemType::Live,
                XtreamCluster::Live,
                None,
            )],
            xtream_cluster: XtreamCluster::Live,
        },
        PlaylistGroup {
            id: 2,
            title: "Movies".intern(),
            channels: vec![
                catalog_test_item(
                    "Selected movie",
                    selected_movie_uuid,
                    PlaylistItemType::Video,
                    XtreamCluster::Video,
                    None,
                ),
                catalog_test_item(
                    "Rejected movie",
                    rejected_movie_uuid,
                    PlaylistItemType::Video,
                    XtreamCluster::Video,
                    None,
                ),
            ],
            xtream_cluster: XtreamCluster::Video,
        },
        PlaylistGroup {
            id: 3,
            title: "Series".intern(),
            channels: vec![
                catalog_test_item(
                    "Selected series",
                    series_uuid,
                    PlaylistItemType::SeriesInfo,
                    XtreamCluster::Series,
                    None,
                ),
                catalog_test_item(
                    "Selected episode",
                    episode_uuid,
                    PlaylistItemType::Series,
                    XtreamCluster::Series,
                    Some(&series_uuid.to_string()),
                ),
                catalog_test_item(
                    "Rejected series",
                    rejected_series_uuid,
                    PlaylistItemType::SeriesInfo,
                    XtreamCluster::Series,
                    None,
                ),
                catalog_test_item(
                    "Rejected episode",
                    rejected_episode_uuid,
                    PlaylistItemType::Series,
                    XtreamCluster::Series,
                    Some(&rejected_series_uuid.to_string()),
                ),
            ],
            xtream_cluster: XtreamCluster::Series,
        },
    ];
    let evaluation = complete_catalog_evaluation(vec![
        catalog_membership(selected_movie_uuid, CurationMediaKind::Movie, 0),
        catalog_membership(series_uuid, CurationMediaKind::Series, 1),
    ]);

    let selected = select_target_catalog(playlist.clone(), &evaluation, true);
    let titles =
        selected.iter().flat_map(|group| &group.channels).map(|item| item.header.title.as_ref()).collect::<Vec<_>>();

    assert_eq!(titles, ["Live channel", "Selected movie", "Selected series", "Selected episode"]);
    assert_eq!(
        select_target_catalog(playlist.clone(), &evaluation, false).iter().flat_map(|group| &group.channels).count(),
        7
    );
    assert_eq!(selected[0].channels[0].header.uuid, live_uuid, "target-wide selection must not rewrite Live");

    let remote_empty = select_target_catalog(playlist, &complete_catalog_evaluation(Vec::new()), true);
    assert_eq!(remote_empty.len(), 1);
    assert_eq!(remote_empty[0].xtream_cluster, XtreamCluster::Live);
    assert_eq!(remote_empty[0].channels[0].header.uuid, live_uuid);
}

#[test]
fn curation_policy_truth_table_covers_a_through_h() {
    let selected_uuid = UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000041");
    let rejected_uuid = UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000042");
    let playlist = vec![PlaylistGroup {
        id: 1,
        title: "Movies".intern(),
        channels: vec![
            catalog_test_item("Selected", selected_uuid, PlaylistItemType::Video, XtreamCluster::Video, None),
            catalog_test_item("Rejected", rejected_uuid, PlaylistItemType::Video, XtreamCluster::Video, None),
        ],
        xtream_cluster: XtreamCluster::Video,
    }];
    let evaluation = complete_catalog_evaluation(vec![catalog_membership(selected_uuid, CurationMediaKind::Movie, 0)]);

    let cases = [
        ("A", TraktCatalogSelection::Full, true, true),
        ("B", TraktCatalogSelection::Curated, true, true),
        ("C", TraktCatalogSelection::Curated, true, false),
        ("D", TraktCatalogSelection::Curated, false, true),
        ("E", TraktCatalogSelection::Full, true, false),
        ("F", TraktCatalogSelection::Full, false, true),
        ("G", TraktCatalogSelection::Full, false, false),
        ("H", TraktCatalogSelection::Curated, false, false),
    ];

    for (case, catalog_selection, include_base, create_category) in cases {
        let config = TraktConfig::from(&TraktConfigDto {
            enabled: true,
            catalog_selection,
            include_xtream_base_categories: include_base,
            api: TraktApiConfigDto::default(),
            lists: vec![TraktListConfigDto {
                user: "alice".to_string(),
                list_slug: "watchlist".to_string(),
                category_name: Some("Curated".to_string()),
                create_xtream_category: create_category,
                content_type: TraktContentType::Vod,
                tmdb_only: true,
                fuzzy_match_threshold: 100,
            }],
            charts: Vec::new(),
        });

        let views =
            build_curated_playlist_views(playlist.clone(), &evaluation, &CurationConfig::from(&config), false, true);
        let base_items = views.base.iter().flat_map(|group| &group.channels).collect::<Vec<_>>();
        let expected_base_count = if catalog_selection == TraktCatalogSelection::Full { 2 } else { 1 };
        assert_eq!(base_items.len(), expected_base_count, "case {case} selected catalog");
        assert!(base_items.iter().any(|item| item.header.uuid == selected_uuid), "case {case} selected subject");

        let xtream = views.xtream.expect("complete curation Xtream view");
        let base_groups = xtream.iter().filter(|group| group.title.as_ref() == "Movies").count();
        let category_groups = xtream.iter().filter(|group| group.title.as_ref() == "Curated").count();
        assert_eq!(base_groups, usize::from(include_base), "case {case} base appearance");
        assert_eq!(category_groups, usize::from(create_category), "case {case} category appearance");
        let expected_unfiltered_items = usize::from(include_base) * expected_base_count + usize::from(create_category);
        assert_eq!(
            xtream.iter().map(|group| group.channels.len()).sum::<usize>(),
            expected_unfiltered_items,
            "case {case} unfiltered Xtream catalog"
        );
        if let Some(alias) =
            xtream.iter().find(|group| group.title.as_ref() == "Curated").and_then(|group| group.channels.first())
        {
            assert_ne!(alias.header.uuid, selected_uuid, "case {case} alias identity");
        }
    }
}

#[test]
fn large_catalog_projection_smoke_keeps_two_explicit_views_bounded() {
    const CATALOG_SIZE: usize = 10_000;
    const SELECTOR_COUNT: usize = 4;
    const MEMBERSHIP_STRIDE: usize = 10;

    let uuid_for = |index: usize| {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&u64::try_from(index + 1).expect("test index").to_be_bytes());
        UUIDType(bytes)
    };
    let playlist = vec![PlaylistGroup {
        id: 1,
        title: "Movies".intern(),
        channels: (0..CATALOG_SIZE)
            .map(|index| {
                catalog_test_item(
                    &format!("Movie {index}"),
                    uuid_for(index),
                    PlaylistItemType::Video,
                    XtreamCluster::Video,
                    None,
                )
            })
            .collect(),
        xtream_cluster: XtreamCluster::Video,
    }];
    let mut memberships = Vec::with_capacity(CATALOG_SIZE / MEMBERSHIP_STRIDE * SELECTOR_COUNT);
    let mut selectors = Vec::with_capacity(SELECTOR_COUNT);
    for selector in 0..SELECTOR_COUNT {
        let key = CurationSelectorKey(selector);
        let start = memberships.len();
        for index in (selector..CATALOG_SIZE).step_by(MEMBERSHIP_STRIDE) {
            memberships.push(CurationMembership {
                selector_key: key,
                subject_uuid: uuid_for(index),
                media_kind: CurationMediaKind::Movie,
                rank: Some(u32::try_from(index).expect("test rank")),
                title_tiebreak: format!("movie-{index}"),
                candidate_order: index,
            });
        }
        selectors.push(CurationSelectorSummary {
            key,
            reference_count: memberships.len() - start,
            membership_count: memberships.len() - start,
        });
    }
    let evaluation = CurationEvaluation { selectors, memberships };
    let config = TraktConfig::from(&TraktConfigDto {
        lists: (0..SELECTOR_COUNT)
            .map(|selector| TraktListConfigDto {
                user: "alice".to_string(),
                list_slug: format!("list-{selector}"),
                category_name: Some(format!("Curated {selector}")),
                create_xtream_category: true,
                content_type: TraktContentType::Vod,
                tmdb_only: true,
                fuzzy_match_threshold: 100,
            })
            .collect(),
        ..TraktConfigDto::default()
    });

    let views = build_curated_playlist_views(playlist, &evaluation, &CurationConfig::from(&config), false, true);

    assert_eq!(views.base[0].channels.len(), CATALOG_SIZE);
    let xtream = views.xtream.expect("Xtream view");
    assert_eq!(xtream.len(), SELECTOR_COUNT + 1);
    assert_eq!(xtream.iter().map(|group| group.channels.len()).sum::<usize>(), CATALOG_SIZE + 4_000);
}
