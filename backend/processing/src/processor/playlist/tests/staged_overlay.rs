use super::{
    apply_staged_overlay_groups, build_curated_playlist_views, catalog_membership, catalog_test_item,
    complete_catalog_evaluation, should_apply_staged_overlay, target_watch_view, test_group, PlaylistDownloadResult,
};
use shared::{
    model::{
        ClusterFlags, InputType, PlaylistGroup, PlaylistItemType, StagedInputType, StreamProperties, TraktApiConfigDto,
        TraktCatalogSelection, TraktConfigDto, TraktContentType, TraktListConfigDto, UUIDType, XtreamCluster,
    },
    utils::Internable,
};
use tuliprox_core::model::{ConfigInput, ConfigInputFlags, ConfigInputOptions, CurationConfig, TraktConfig};
use tuliprox_curation::CurationMediaKind;

#[test]
fn staged_xtream_vod_url_uses_provider_credentials_and_one_extension() {
    let provider = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("real-user".to_string()),
        password: Some("real-pass".to_string()),
        ..Default::default()
    };
    let mut staged_vod = test_group(XtreamCluster::Video, "staged-vod", "staged");
    staged_vod.channels[0].header.id = "310".intern();
    staged_vod.channels[0].header.url = "http://iptvhost.example/movie/fake-user/fake-pass/310.mkv".intern();

    let groups = apply_staged_overlay_groups(&provider, ClusterFlags::Vod, Vec::new(), vec![staged_vod]);

    assert_eq!(groups[0].channels[0].header.url.as_ref(), "http://provider.example/movie/real-user/real-pass/310.mkv");
}

#[test]
fn staged_xtream_overlay_keeps_known_provider_stream_urls() {
    let provider = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("real-user".to_string()),
        password: Some("real-pass".to_string()),
        ..Default::default()
    };
    let mut provider_vod = test_group(XtreamCluster::Video, "Movies", "provider");
    provider_vod.channels[0].header.id = "310".intern();
    provider_vod.channels[0].header.url = "https://cdn.example/video?id=310".intern();
    let mut second_provider = provider_vod.channels[0].clone();
    second_provider.header.id = "311".intern();
    second_provider.header.url = "http://provider.example/movie/real-user/real-pass/311.mp4".intern();
    provider_vod.channels.push(second_provider);

    let mut staged_vod = test_group(XtreamCluster::Video, "Edited Movies", "staged");
    staged_vod.channels[0].header.id = "310".intern();
    staged_vod.channels[0].header.url = "http://editor.example/movie/310.mkv".intern();
    let mut second_staged = staged_vod.channels[0].clone();
    second_staged.header.id = "311".intern();
    second_staged.header.url = "http://editor.example/movie/311.mkv".intern();
    staged_vod.channels.push(second_staged);

    let groups = apply_staged_overlay_groups(&provider, ClusterFlags::Vod, vec![provider_vod], vec![staged_vod]);

    assert_eq!(groups[0].title.as_ref(), "Edited Movies");
    assert_eq!(groups[0].channels[0].header.url.as_ref(), "https://cdn.example/video?id=310");
    assert_eq!(groups[0].channels[1].header.url.as_ref(), "http://provider.example/movie/real-user/real-pass/311.mp4");
}

#[test]
fn staged_xtream_new_vod_stream_uses_metadata_extension() {
    let provider = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("real-user".to_string()),
        password: Some("real-pass".to_string()),
        ..Default::default()
    };
    let mut staged_vod = test_group(XtreamCluster::Video, "New Movies", "staged");
    staged_vod.channels[0].header.id = "310".intern();
    staged_vod.channels[0].header.url = "http://editor.example/movie/310".intern();
    staged_vod.channels[0].header.additional_properties =
        Some(StreamProperties::Video(Box::new(shared::model::VideoStreamProperties {
            container_extension: "mkv".intern(),
            ..Default::default()
        })));

    let groups = apply_staged_overlay_groups(&provider, ClusterFlags::Vod, Vec::new(), vec![staged_vod]);

    assert_eq!(groups[0].channels[0].header.url.as_ref(), "http://provider.example/movie/real-user/real-pass/310.mkv");
}

#[test]
fn staged_xtream_live_url_respects_prefix_and_without_extension_flags() {
    let provider_prefix = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("real-user".to_string()),
        password: Some("real-pass".to_string()),
        options: Some(ConfigInputOptions {
            flags: ConfigInputFlags::XtreamLiveStreamUsePrefix.into(),
            ..ConfigInputOptions::defaults().clone()
        }),
        ..Default::default()
    };
    let mut staged_live_prefix = test_group(XtreamCluster::Live, "staged-live", "staged");
    staged_live_prefix.channels[0].header.id = "11203".intern();
    staged_live_prefix.channels[0].header.url = "http://iptvhost.example/fake-user/fake-pass/11203.ts".intern();

    let groups_prefix =
        apply_staged_overlay_groups(&provider_prefix, ClusterFlags::Live, Vec::new(), vec![staged_live_prefix]);

    assert_eq!(
        groups_prefix[0].channels[0].header.url.as_ref(),
        "http://provider.example/live/real-user/real-pass/11203.ts"
    );

    let provider_no_ext = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("real-user".to_string()),
        password: Some("real-pass".to_string()),
        options: Some(ConfigInputOptions {
            flags: ConfigInputFlags::XtreamLiveStreamWithoutExtension.into(),
            ..ConfigInputOptions::defaults().clone()
        }),
        ..Default::default()
    };
    let mut staged_live_no_ext = test_group(XtreamCluster::Live, "staged-live", "staged");
    staged_live_no_ext.channels[0].header.id = "11203".intern();
    staged_live_no_ext.channels[0].header.url = "http://iptvhost.example/fake-user/fake-pass/11203.ts".intern();

    let groups_no_ext =
        apply_staged_overlay_groups(&provider_no_ext, ClusterFlags::Live, Vec::new(), vec![staged_live_no_ext]);

    assert_eq!(groups_no_ext[0].channels[0].header.url.as_ref(), "http://provider.example/real-user/real-pass/11203");
}

#[test]
fn staged_xtream_overlay_full_cluster_staging_replaces_all_matching_groups() {
    let provider = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("real-user".to_string()),
        password: Some("real-pass".to_string()),
        ..Default::default()
    };
    let mut provider_action = test_group(XtreamCluster::Video, "Action", "provider");
    provider_action.channels[0].header.id = "10".intern();
    let mut provider_comedy = test_group(XtreamCluster::Video, "Comedy", "provider");
    provider_comedy.channels[0].header.id = "20".intern();

    let mut staged_action = test_group(XtreamCluster::Video, "Action", "staged");
    staged_action.channels[0].header.id = "11".intern();
    let mut staged_comedy = test_group(XtreamCluster::Video, "Comedy", "staged");
    staged_comedy.channels[0].header.id = "21".intern();

    let groups = apply_staged_overlay_groups(
        &provider,
        ClusterFlags::Vod,
        vec![provider_action, provider_comedy],
        vec![staged_action, staged_comedy],
    );

    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].title.as_ref(), "Action");
    assert_eq!(groups[0].channels[0].header.id.as_ref(), "11");
    assert_eq!(groups[0].channels[0].header.input_name.as_ref(), "provider");
    assert_eq!(groups[1].title.as_ref(), "Comedy");
    assert_eq!(groups[1].channels[0].header.id.as_ref(), "21");
    assert_eq!(groups[1].channels[0].header.input_name.as_ref(), "provider");
}

#[test]
fn staged_xtream_overlay_partial_cluster_staging_preserves_unstaged_provider_groups() {
    let provider = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("real-user".to_string()),
        password: Some("real-pass".to_string()),
        ..Default::default()
    };
    let mut provider_action = test_group(XtreamCluster::Video, "Action", "provider");
    provider_action.channels[0].header.id = "10".intern();
    let mut provider_comedy = test_group(XtreamCluster::Video, "Comedy", "provider");
    provider_comedy.channels[0].header.id = "20".intern();
    let mut provider_drama = test_group(XtreamCluster::Video, "Drama", "provider");
    provider_drama.channels[0].header.id = "30".intern();

    let mut staged_action = test_group(XtreamCluster::Video, "Action", "staged");
    staged_action.channels[0].header.id = "11".intern();

    let groups = apply_staged_overlay_groups(
        &provider,
        ClusterFlags::Vod,
        vec![provider_action, provider_comedy, provider_drama],
        vec![staged_action],
    );

    assert_eq!(groups.len(), 3);
    assert_eq!(groups[0].title.as_ref(), "Action");
    assert_eq!(groups[0].channels[0].header.id.as_ref(), "11");
    assert_eq!(groups[1].title.as_ref(), "Comedy");
    assert_eq!(groups[1].channels[0].header.id.as_ref(), "20");
    assert_eq!(groups[2].title.as_ref(), "Drama");
    assert_eq!(groups[2].channels[0].header.id.as_ref(), "30");
}

#[test]
fn staged_xtream_overlay_no_staged_groups_for_selected_cluster_preserves_all_provider_groups() {
    let provider = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("real-user".to_string()),
        password: Some("real-pass".to_string()),
        ..Default::default()
    };
    let provider_news = test_group(XtreamCluster::Live, "News", "provider");
    let provider_sports = test_group(XtreamCluster::Live, "Sports", "provider");
    let staged_series = test_group(XtreamCluster::Series, "Shows", "staged");

    let groups = apply_staged_overlay_groups(
        &provider,
        ClusterFlags::Live,
        vec![provider_news, provider_sports],
        vec![staged_series],
    );

    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].title.as_ref(), "News");
    assert_eq!(groups[1].title.as_ref(), "Sports");
}

#[test]
fn staged_xtream_overlay_all_invalid_ids_falls_back_to_provider_group() {
    let provider = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("real-user".to_string()),
        password: Some("real-pass".to_string()),
        ..Default::default()
    };
    let mut provider_sports = test_group(XtreamCluster::Live, "Sports", "provider");
    provider_sports.channels[0].header.id = "501".intern();

    let mut staged_sports = test_group(XtreamCluster::Live, "Sports", "staged");
    staged_sports.channels[0].header.id = "non-numeric-a".intern();
    let mut second_invalid = staged_sports.channels[0].clone();
    second_invalid.header.id = "non-numeric-b".intern();
    staged_sports.channels.push(second_invalid);

    let groups = apply_staged_overlay_groups(&provider, ClusterFlags::Live, vec![provider_sports], vec![staged_sports]);

    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].title.as_ref(), "Sports");
    assert_eq!(groups[0].channels.len(), 1);
    assert_eq!(groups[0].channels[0].header.id.as_ref(), "501");
}

#[test]
fn staged_xtream_overlay_partially_valid_ids_keeps_valid_and_drops_invalid() {
    let provider = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("real-user".to_string()),
        password: Some("real-pass".to_string()),
        ..Default::default()
    };
    let mut provider_sports = test_group(XtreamCluster::Live, "Sports", "provider");
    provider_sports.channels[0].header.id = "501".intern();

    let mut staged_sports = test_group(XtreamCluster::Live, "Sports", "staged");
    staged_sports.channels[0].header.id = "502".intern();
    let mut invalid = staged_sports.channels[0].clone();
    invalid.header.id = "invalid-stream".intern();
    staged_sports.channels.push(invalid);

    let groups = apply_staged_overlay_groups(&provider, ClusterFlags::Live, vec![provider_sports], vec![staged_sports]);

    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].title.as_ref(), "Sports");
    assert_eq!(groups[0].channels.len(), 1);
    assert_eq!(groups[0].channels[0].header.id.as_ref(), "502");
}

#[test]
fn staged_xtream_overlay_missing_credentials_preserves_all_provider_groups() {
    let provider = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: None,
        password: None,
        ..Default::default()
    };
    let provider_news = test_group(XtreamCluster::Live, "News", "provider");
    let provider_sports = test_group(XtreamCluster::Live, "Sports", "provider");
    let mut staged_sports = test_group(XtreamCluster::Live, "Sports", "staged");
    staged_sports.channels[0].header.id = "501".intern();

    let groups = apply_staged_overlay_groups(
        &provider,
        ClusterFlags::Live,
        vec![provider_news, provider_sports],
        vec![staged_sports],
    );

    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].title.as_ref(), "News");
    assert_eq!(groups[1].title.as_ref(), "Sports");
}

#[test]
fn staged_xtream_overlay_multiple_groups_with_different_clusters() {
    let provider = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("real-user".to_string()),
        password: Some("real-pass".to_string()),
        ..Default::default()
    };
    let mut provider_live = test_group(XtreamCluster::Live, "News", "provider");
    provider_live.channels[0].header.id = "100".intern();
    let mut provider_action = test_group(XtreamCluster::Video, "Action", "provider");
    provider_action.channels[0].header.id = "200".intern();
    let mut provider_comedy = test_group(XtreamCluster::Video, "Comedy", "provider");
    provider_comedy.channels[0].header.id = "300".intern();
    let mut provider_series = test_group(XtreamCluster::Series, "Shows", "provider");
    provider_series.channels[0].header.id = "400".intern();

    let mut staged_live = test_group(XtreamCluster::Live, "News", "staged");
    staged_live.channels[0].header.id = "101".intern();
    let mut staged_action = test_group(XtreamCluster::Video, "Action", "staged");
    staged_action.channels[0].header.id = "201".intern();

    let groups = apply_staged_overlay_groups(
        &provider,
        ClusterFlags::Live | ClusterFlags::Vod,
        vec![provider_live, provider_action, provider_comedy, provider_series],
        vec![staged_live, staged_action],
    );

    assert_eq!(groups.len(), 4);
    assert_eq!(groups[0].title.as_ref(), "News");
    assert_eq!(groups[0].channels[0].header.id.as_ref(), "101");
    assert_eq!(groups[1].title.as_ref(), "Action");
    assert_eq!(groups[1].channels[0].header.id.as_ref(), "201");
    assert_eq!(groups[2].title.as_ref(), "Comedy");
    assert_eq!(groups[2].channels[0].header.id.as_ref(), "300");
    assert_eq!(groups[3].title.as_ref(), "Shows");
    assert_eq!(groups[3].channels[0].header.id.as_ref(), "400");
}

#[test]
fn staged_xtream_overlay_preserves_original_group_order() {
    let provider = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("real-user".to_string()),
        password: Some("real-pass".to_string()),
        ..Default::default()
    };
    let mut live_1 = test_group(XtreamCluster::Live, "Live1", "provider");
    live_1.channels[0].header.id = "1".intern();
    let mut vod_1 = test_group(XtreamCluster::Video, "Vod1", "provider");
    vod_1.channels[0].header.id = "2".intern();
    let mut live_2 = test_group(XtreamCluster::Live, "Live2", "provider");
    live_2.channels[0].header.id = "3".intern();
    let mut vod_2 = test_group(XtreamCluster::Video, "Vod2", "provider");
    vod_2.channels[0].header.id = "4".intern();

    let mut staged_live_1 = test_group(XtreamCluster::Live, "Live1", "staged");
    staged_live_1.channels[0].header.id = "11".intern();
    let mut staged_live_2 = test_group(XtreamCluster::Live, "Live2", "staged");
    staged_live_2.channels[0].header.id = "33".intern();

    let groups = apply_staged_overlay_groups(
        &provider,
        ClusterFlags::Live,
        vec![live_1, vod_1, live_2, vod_2],
        vec![staged_live_1, staged_live_2],
    );

    assert_eq!(groups.len(), 4);
    assert_eq!(groups[0].title.as_ref(), "Live1");
    assert_eq!(groups[0].channels[0].header.id.as_ref(), "11");
    assert_eq!(groups[1].title.as_ref(), "Vod1");
    assert_eq!(groups[1].channels[0].header.id.as_ref(), "2");
    assert_eq!(groups[2].title.as_ref(), "Live2");
    assert_eq!(groups[2].channels[0].header.id.as_ref(), "33");
    assert_eq!(groups[3].title.as_ref(), "Vod2");
    assert_eq!(groups[3].channels[0].header.id.as_ref(), "4");
}

/// The merged playlist reaches persistence keyed by `(cluster, id)`, which collapses groups that
/// share an id within one cluster.
pub(in crate::processor::playlist::tests) fn assert_unique_category_ids(groups: &[PlaylistGroup]) {
    let mut seen = std::collections::HashSet::new();
    for group in groups {
        assert!(
            seen.insert((group.xtream_cluster, group.id)),
            "category id {} is used by more than one {} group",
            group.id,
            group.xtream_cluster
        );
    }
}

#[test]
fn staged_overlay_syncs_channel_category_ids_with_the_final_group_id() {
    let provider = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("real-user".to_string()),
        password: Some("real-pass".to_string()),
        ..Default::default()
    };
    let mut provider_news = test_group(XtreamCluster::Live, "News", "provider");
    provider_news.id = 5;
    provider_news.channels[0].header.id = "100".intern();

    // Matched by stream id, so the group takes over the provider category id.
    let mut staged_rewritten = test_group(XtreamCluster::Live, "Rewritten", "staged");
    staged_rewritten.id = 7;
    staged_rewritten.channels[0].header.id = "100".intern();
    staged_rewritten.channels[0].header.category_id = 7;
    // No provider counterpart: the group becomes a new category with an allocated id.
    let mut staged_kids = test_group(XtreamCluster::Live, "Kids", "staged");
    staged_kids.id = 0;
    staged_kids.channels[0].header.id = "300".intern();

    let groups = apply_staged_overlay_groups(
        &provider,
        ClusterFlags::Live,
        vec![provider_news],
        vec![staged_rewritten, staged_kids],
    );

    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].id, 5);
    assert_eq!(groups[0].channels[0].header.category_id, 5);
    assert_eq!(groups[1].id, 6);
    assert_eq!(groups[1].channels[0].header.category_id, 6);
    assert_unique_category_ids(&groups);
}

#[test]
fn staged_overlay_on_non_xtream_provider_replaces_cluster_and_rewrites_input_name() {
    let provider = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::M3u,
        url: "http://provider.example/list.m3u".to_string(),
        ..Default::default()
    };
    let provider_live = test_group(XtreamCluster::Live, "provider-live", "provider");
    let provider_vod = test_group(XtreamCluster::Video, "provider-vod", "provider");
    let mut staged_live = test_group(XtreamCluster::Live, "staged-live", "staged");
    staged_live.channels[0].header.url = "http://editor.example/live/1.ts".intern();

    let groups = apply_staged_overlay_groups(
        &provider,
        ClusterFlags::Live,
        vec![provider_live, provider_vod],
        vec![staged_live],
    );

    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].title.as_ref(), "provider-vod");
    assert_eq!(groups[0].channels[0].header.input_name.as_ref(), "provider");
    assert_eq!(groups[1].title.as_ref(), "staged-live");
    // A non-Xtream provider cannot rebuild Xtream stream URLs, so the staged URL and its group id stay.
    assert_eq!(groups[1].channels[0].header.input_name.as_ref(), "provider");
    assert_eq!(groups[1].channels[0].header.url.as_ref(), "http://editor.example/live/1.ts");
    assert_eq!(groups[1].id, 1);
}

#[test]
fn staged_xtream_overlay_matches_by_category_id_not_by_title() {
    let provider = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("real-user".to_string()),
        password: Some("real-pass".to_string()),
        ..Default::default()
    };
    // The staged tool renamed the provider category 7 and moved its streams; the title now collides
    // with the unrelated provider category 5.
    let mut provider_kids = test_group(XtreamCluster::Live, "Kids", "provider");
    provider_kids.id = 5;
    provider_kids.channels[0].header.id = "105".intern();
    let mut provider_news = test_group(XtreamCluster::Live, "News", "provider");
    provider_news.id = 7;
    provider_news.channels[0].header.id = "107".intern();

    let mut staged_renamed = test_group(XtreamCluster::Live, "Kids", "staged");
    staged_renamed.id = 7;
    staged_renamed.channels[0].header.id = "207".intern();

    let groups = apply_staged_overlay_groups(
        &provider,
        ClusterFlags::Live,
        vec![provider_kids, provider_news],
        vec![staged_renamed],
    );

    assert_eq!(groups.len(), 2);
    // Category 5 keeps its content: the staged group belongs to category 7.
    assert_eq!(groups[0].title.as_ref(), "Kids");
    assert_eq!(groups[0].id, 5);
    assert_eq!(groups[0].channels[0].header.id.as_ref(), "105");
    // Category 7 carries the staged rename and the staged stream, under its own id.
    assert_eq!(groups[1].title.as_ref(), "Kids");
    assert_eq!(groups[1].id, 7);
    assert_eq!(groups[1].channels[0].header.id.as_ref(), "207");
    assert_unique_category_ids(&groups);
}

#[test]
fn staged_xtream_overlay_matches_by_stream_id_over_group_id() {
    let provider = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("real-user".to_string()),
        password: Some("real-pass".to_string()),
        ..Default::default()
    };
    let mut provider_news = test_group(XtreamCluster::Live, "News", "provider");
    provider_news.id = 5;
    provider_news.channels[0].header.id = "100".intern();
    let mut provider_sports = test_group(XtreamCluster::Live, "Sports", "provider");
    provider_sports.id = 7;
    provider_sports.channels[0].header.id = "200".intern();

    // A m3u staged playlist numbers its groups on its own, so the group id points at another
    // provider category. The stream id of the overlaid channel proves which category it replaces.
    let mut staged_sports = test_group(XtreamCluster::Live, "Sports Rewritten", "staged");
    staged_sports.id = 5;
    staged_sports.channels[0].header.id = "200".intern();

    let groups = apply_staged_overlay_groups(
        &provider,
        ClusterFlags::Live,
        vec![provider_news, provider_sports],
        vec![staged_sports],
    );

    assert_eq!(groups.len(), 2);
    // Category 5 keeps its content: no staged channel belongs to it.
    assert_eq!(groups[0].title.as_ref(), "News");
    assert_eq!(groups[0].id, 5);
    assert_eq!(groups[0].channels[0].header.id.as_ref(), "100");
    // Category 7 carries the rewritten staged group, under its own id.
    assert_eq!(groups[1].title.as_ref(), "Sports Rewritten");
    assert_eq!(groups[1].id, 7);
    assert_eq!(groups[1].channels[0].header.id.as_ref(), "200");
    assert_unique_category_ids(&groups);
}

#[test]
fn staged_xtream_split_group_does_not_replace_an_unrelated_category() {
    let provider = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("real-user".to_string()),
        password: Some("real-pass".to_string()),
        ..Default::default()
    };
    let mut provider_sports = test_group(XtreamCluster::Live, "Sports", "provider");
    provider_sports.id = 5;
    provider_sports.channels[0].header.id = "100".intern();
    let mut second_sport = provider_sports.channels[0].clone();
    second_sport.header.id = "101".intern();
    provider_sports.channels.push(second_sport);
    let mut provider_news = test_group(XtreamCluster::Live, "News", "provider");
    provider_news.id = 7;
    provider_news.channels[0].header.id = "200".intern();

    let mut staged_first = test_group(XtreamCluster::Live, "Sports A", "staged");
    staged_first.id = 5;
    staged_first.channels[0].header.id = "100".intern();
    let mut staged_second = test_group(XtreamCluster::Live, "News", "staged");
    staged_second.id = 7;
    staged_second.channels[0].header.id = "101".intern();

    let groups = apply_staged_overlay_groups(
        &provider,
        ClusterFlags::Live,
        vec![provider_sports, provider_news],
        vec![staged_first, staged_second],
    );

    assert_eq!(groups.len(), 3);
    assert_eq!(groups[0].id, 5);
    assert_eq!(groups[0].title.as_ref(), "Sports A");
    assert_eq!(groups[1].id, 7);
    assert_eq!(groups[1].channels[0].header.id.as_ref(), "200");
    assert_eq!(groups[2].title.as_ref(), "News");
    assert_eq!(groups[2].channels[0].header.id.as_ref(), "101");
    assert_unique_category_ids(&groups);
}

#[test]
fn staged_m3u_unknown_streams_do_not_match_positional_category_id() {
    let provider = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("real-user".to_string()),
        password: Some("real-pass".to_string()),
        ..Default::default()
    };
    let mut provider_sports = test_group(XtreamCluster::Live, "Sports", "provider");
    provider_sports.id = 5;
    provider_sports.channels[0].header.id = "100".intern();
    let mut staged_new = test_group(XtreamCluster::Live, "New", "staged");
    staged_new.id = 5;
    staged_new.channels[0].header.id = "300".intern();

    let groups = super::super::apply_staged_overlay_groups(
        &provider,
        StagedInputType::M3u,
        ClusterFlags::Live,
        vec![provider_sports],
        vec![staged_new],
    );

    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].title.as_ref(), "New");
    assert_eq!(groups[0].channels[0].header.id.as_ref(), "300");
    assert_unique_category_ids(&groups);
}

#[test]
fn staged_m3u_replaces_selected_xtream_groups_and_keeps_other_clusters() {
    let provider = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("real-user".to_string()),
        password: Some("real-pass".to_string()),
        ..Default::default()
    };
    let mut provider_news = test_group(XtreamCluster::Live, "Provider News", "provider");
    provider_news.id = 5;
    provider_news.channels[0].header.id = "100".intern();
    let mut provider_sports = test_group(XtreamCluster::Live, "Provider Sports", "provider");
    provider_sports.id = 7;
    provider_sports.channels[0].header.id = "200".intern();
    provider_sports.channels[0].header.url = "http://provider.example/direct/200.ts".intern();
    let mut provider_kids = test_group(XtreamCluster::Live, "Provider Kids", "provider");
    provider_kids.id = 9;
    provider_kids.channels[0].header.id = "300".intern();
    let provider_vod = test_group(XtreamCluster::Video, "Provider VOD", "provider");
    let mut staged_custom = test_group(XtreamCluster::Live, "My Channels", "staged");
    staged_custom.channels[0].header.id = "200".intern();
    let mut staged_news = test_group(XtreamCluster::Live, "My News", "staged");
    staged_news.channels[0].header.id = "100".intern();

    let groups = super::super::apply_staged_overlay_groups(
        &provider,
        StagedInputType::M3u,
        ClusterFlags::Live,
        vec![provider_news, provider_sports, provider_kids, provider_vod],
        vec![staged_custom, staged_news],
    );

    assert_eq!(groups.len(), 3);
    assert_eq!(groups[0].title.as_ref(), "My Channels");
    assert_eq!(groups[0].channels[0].header.id.as_ref(), "200");
    assert_eq!(groups[0].channels[0].header.input_name.as_ref(), "provider");
    assert_eq!(groups[0].channels[0].header.url.as_ref(), "http://provider.example/direct/200.ts");
    assert_eq!(groups[1].title.as_ref(), "My News");
    assert_eq!(groups[1].channels[0].header.id.as_ref(), "100");
    assert_eq!(groups[2].title.as_ref(), "Provider VOD");
}

#[test]
fn staged_xtream_overlay_falls_back_to_the_secondary_stream_overlap() {
    let provider = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("real-user".to_string()),
        password: Some("real-pass".to_string()),
        ..Default::default()
    };
    let mut provider_alpha = test_group(XtreamCluster::Live, "Alpha", "provider");
    provider_alpha.id = 5;
    provider_alpha.channels[0].header.id = "100".intern();
    let mut provider_beta = test_group(XtreamCluster::Live, "Beta", "provider");
    provider_beta.id = 7;
    provider_beta.channels[0].header.id = "200".intern();
    let mut beta_second = provider_beta.channels[0].clone();
    beta_second.header.id = "201".intern();
    provider_beta.channels.push(beta_second);

    // Both staged groups overlap Beta; the stronger one takes it, so the other has to fall back to the
    // provider category it also owns streams of instead of becoming a new category.
    let mut staged_strong = test_group(XtreamCluster::Live, "Beta Rewritten", "staged");
    staged_strong.id = 0;
    staged_strong.channels[0].header.id = "200".intern();
    let mut strong_second = staged_strong.channels[0].clone();
    strong_second.header.id = "201".intern();
    staged_strong.channels.push(strong_second);

    let mut staged_weak = test_group(XtreamCluster::Live, "Alpha Rewritten", "staged");
    staged_weak.id = 0;
    staged_weak.channels[0].header.id = "200".intern();
    let mut weak_second = staged_weak.channels[0].clone();
    weak_second.header.id = "201".intern();
    staged_weak.channels.push(weak_second);
    let mut weak_third = staged_weak.channels[0].clone();
    weak_third.header.id = "100".intern();
    staged_weak.channels.push(weak_third);

    let groups = apply_staged_overlay_groups(
        &provider,
        ClusterFlags::Live,
        vec![provider_alpha, provider_beta],
        vec![staged_strong, staged_weak],
    );

    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].title.as_ref(), "Alpha Rewritten");
    assert_eq!(groups[0].id, 5);
    assert_eq!(groups[0].channels.len(), 3);
    assert_eq!(groups[1].title.as_ref(), "Beta Rewritten");
    assert_eq!(groups[1].id, 7);
    assert_eq!(groups[1].channels.len(), 2);
    assert_unique_category_ids(&groups);
}

#[test]
fn staged_xtream_overlay_gives_a_new_category_a_free_category_id() {
    let provider = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("real-user".to_string()),
        password: Some("real-pass".to_string()),
        ..Default::default()
    };
    let mut provider_sports = test_group(XtreamCluster::Live, "Sports", "provider");
    provider_sports.id = 2;
    provider_sports.channels[0].header.id = "200".intern();

    // Staged groups without a category id must not reach persistence as a shared zero key.
    let mut staged_kids = test_group(XtreamCluster::Live, "Kids", "staged");
    staged_kids.id = 0;
    staged_kids.channels[0].header.id = "300".intern();
    let mut staged_movies = test_group(XtreamCluster::Live, "Movies", "staged");
    staged_movies.id = 0;
    staged_movies.channels[0].header.id = "400".intern();

    let groups = apply_staged_overlay_groups(
        &provider,
        ClusterFlags::Live,
        vec![provider_sports],
        vec![staged_kids, staged_movies],
    );

    assert_eq!(groups.len(), 3);
    assert_eq!(groups[0].title.as_ref(), "Sports");
    assert_eq!(groups[0].id, 2);
    assert_eq!(groups[1].title.as_ref(), "Kids");
    assert_eq!(groups[1].id, 3);
    assert_eq!(groups[2].title.as_ref(), "Movies");
    assert_eq!(groups[2].id, 4);
    assert_unique_category_ids(&groups);
}

#[test]
fn staged_xtream_overlay_empty_staged_group_without_provider_fallback_is_omitted() {
    let provider = ConfigInput {
        name: "provider".intern(),
        input_type: InputType::Xtream,
        url: "http://provider.example".to_string(),
        username: Some("real-user".to_string()),
        password: Some("real-pass".to_string()),
        ..Default::default()
    };
    let mut provider_action = test_group(XtreamCluster::Video, "Action", "provider");
    provider_action.channels[0].header.id = "10".intern();

    let mut staged_horror = test_group(XtreamCluster::Video, "Horror", "staged");
    staged_horror.id = 2;
    staged_horror.channels[0].header.id = "non-numeric".intern();

    let groups = apply_staged_overlay_groups(&provider, ClusterFlags::Vod, vec![provider_action], vec![staged_horror]);

    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].title.as_ref(), "Action");
}

#[test]
fn staged_overlay_is_skipped_when_provider_playlist_is_cached() {
    let result = PlaylistDownloadResult::new(vec![], vec![], true, false);

    assert!(!should_apply_staged_overlay(&result));
}

#[test]
fn xtream_base_and_selector_category_projection_are_independent() {
    let selected_uuid = UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000011");
    let rejected_uuid = UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000012");
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
    let dto = TraktConfigDto {
        enabled: true,
        catalog_selection: TraktCatalogSelection::Curated,
        include_xtream_base_categories: false,
        api: TraktApiConfigDto::default(),
        lists: vec![TraktListConfigDto {
            user: "alice".to_string(),
            list_slug: "watchlist".to_string(),
            category_name: Some("Curated".to_string()),
            create_xtream_category: true,
            content_type: TraktContentType::Vod,
            tmdb_only: true,
            fuzzy_match_threshold: 100,
        }],
        charts: Vec::new(),
    };
    let config = TraktConfig::from(&dto);

    let views =
        build_curated_playlist_views(playlist.clone(), &evaluation, &CurationConfig::from(&config), false, true);

    assert_eq!(views.base.len(), 1);
    assert_eq!(views.base[0].channels.len(), 1);
    assert!(
        target_watch_view(&views.base, views.xtream.as_deref()).iter().any(|group| group.title.as_ref() == "Curated"),
        "configured Trakt watches observe the Xtream category appearance"
    );
    let xtream = views.xtream.expect("complete curation has an Xtream view");
    assert_eq!(xtream.len(), 1);
    assert_eq!(xtream[0].title.as_ref(), "Curated");
    assert_eq!(xtream[0].channels.len(), 1);
    assert_ne!(xtream[0].channels[0].header.uuid, selected_uuid);

    let mut compatible_dto = dto;
    compatible_dto.catalog_selection = TraktCatalogSelection::Full;
    compatible_dto.include_xtream_base_categories = true;
    compatible_dto.lists[0].create_xtream_category = false;
    compatible_dto.lists[0].category_name = None;
    let compatible = build_curated_playlist_views(
        playlist,
        &evaluation,
        &CurationConfig::from(&TraktConfig::from(&compatible_dto)),
        false,
        true,
    );
    assert_eq!(compatible.base[0].channels.len(), 2);
    assert_eq!(compatible.xtream.expect("Xtream view").len(), 1, "selection-only selector creates no category");
}
