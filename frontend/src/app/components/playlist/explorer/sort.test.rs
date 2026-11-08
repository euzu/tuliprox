use super::*;
use shared::model::{ExplorerEpgState, PlaylistItemType, UiItemExplorerMeta, UiOverrideState, XtreamCluster};
use std::sync::Arc;

#[allow(clippy::too_many_arguments)]
fn create_test_item(
    virtual_id: u32,
    provider_id: &str,
    name: &str,
    title: &str,
    rating: f64,
    cluster: XtreamCluster,
    epg_channel_id: Option<&str>,
    epg_state: Option<ExplorerEpgState>,
    edited: bool,
) -> Rc<UiPlaylistItem> {
    Rc::new(UiPlaylistItem {
        virtual_id,
        provider_id: Arc::from(provider_id),
        name: Arc::from(name),
        title: Arc::from(title),
        group: Arc::from("Group A"),
        logo: Arc::from(""),
        url: Arc::from(""),
        item_type: PlaylistItemType::Live,
        xtream_cluster: cluster,
        category_id: 1,
        rating,
        input_name: Arc::from("input-1"),
        epg_channel_id: epg_channel_id.map(Arc::from),
        explorer_meta: Some(UiItemExplorerMeta {
            override_state: UiOverrideState { edited, pending: false, ambiguous: false },
            epg_state,
            ..Default::default()
        }),
    })
}

#[test]
fn default_sort_per_kind() {
    assert_eq!(default_sort_for_kind(XtreamCluster::Live), (ExplorerSort::ChannelNo, false));
    assert_eq!(default_sort_for_kind(XtreamCluster::Video), (ExplorerSort::Added, true));
    assert_eq!(default_sort_for_kind(XtreamCluster::Series), (ExplorerSort::LastModified, true));
}

#[test]
fn sort_missing_values_last_both_directions() {
    let item_rated_high = create_test_item(1, "p1", "High", "High", 8.5, XtreamCluster::Video, None, None, false);
    let item_unrated = create_test_item(2, "p2", "Unrated", "Unrated", 0.0, XtreamCluster::Video, None, None, false);
    let item_rated_low = create_test_item(3, "p3", "Low", "Low", 6.0, XtreamCluster::Video, None, None, false);

    // Ascending: Low (6.0), High (8.5), Unrated (missing -> last)
    let mut items_asc = vec![Rc::clone(&item_rated_high), Rc::clone(&item_unrated), Rc::clone(&item_rated_low)];
    sort_items(&mut items_asc, ExplorerSort::Rating, false);
    assert_eq!(items_asc[0].provider_id.as_ref(), "p3");
    assert_eq!(items_asc[1].provider_id.as_ref(), "p1");
    assert_eq!(items_asc[2].provider_id.as_ref(), "p2");

    // Descending: High (8.5), Low (6.0), Unrated (missing -> last)
    let mut items_desc = vec![Rc::clone(&item_rated_high), Rc::clone(&item_unrated), Rc::clone(&item_rated_low)];
    sort_items(&mut items_desc, ExplorerSort::Rating, true);
    assert_eq!(items_desc[0].provider_id.as_ref(), "p1");
    assert_eq!(items_desc[1].provider_id.as_ref(), "p3");
    assert_eq!(items_desc[2].provider_id.as_ref(), "p2");

    // Test with Year (parsed from title)
    let movie_2024 =
        create_test_item(4, "p4", "Movie A", "Movie A (2024)", 0.0, XtreamCluster::Video, None, None, false);
    let movie_noyear = create_test_item(5, "p5", "Movie B", "Movie B", 0.0, XtreamCluster::Video, None, None, false);
    let movie_2020 =
        create_test_item(6, "p6", "Movie C", "Movie C (2020)", 0.0, XtreamCluster::Video, None, None, false);

    let mut year_asc = vec![Rc::clone(&movie_2024), Rc::clone(&movie_noyear), Rc::clone(&movie_2020)];
    sort_items(&mut year_asc, ExplorerSort::Year, false);
    assert_eq!(year_asc[0].provider_id.as_ref(), "p6"); // 2020
    assert_eq!(year_asc[1].provider_id.as_ref(), "p4"); // 2024
    assert_eq!(year_asc[2].provider_id.as_ref(), "p5"); // missing year -> last

    let mut year_desc = vec![Rc::clone(&movie_2024), Rc::clone(&movie_noyear), Rc::clone(&movie_2020)];
    sort_items(&mut year_desc, ExplorerSort::Year, true);
    assert_eq!(year_desc[0].provider_id.as_ref(), "p4"); // 2024
    assert_eq!(year_desc[1].provider_id.as_ref(), "p6"); // 2020
    assert_eq!(year_desc[2].provider_id.as_ref(), "p5"); // missing year -> last
}

#[test]
fn sort_ties_by_provider_id() {
    let item_b = create_test_item(1, "provider-20", "Same", "Same", 7.5, XtreamCluster::Video, None, None, false);
    let item_a = create_test_item(2, "provider-10", "Same", "Same", 7.5, XtreamCluster::Video, None, None, false);

    let mut items = vec![Rc::clone(&item_b), Rc::clone(&item_a)];
    sort_items(&mut items, ExplorerSort::Title, false);
    assert_eq!(items[0].provider_id.as_ref(), "provider-10");
    assert_eq!(items[1].provider_id.as_ref(), "provider-20");

    let mut items_desc = vec![Rc::clone(&item_b), Rc::clone(&item_a)];
    sort_items(&mut items_desc, ExplorerSort::Title, true);
    assert_eq!(items_desc[0].provider_id.as_ref(), "provider-10");
    assert_eq!(items_desc[1].provider_id.as_ref(), "provider-20");
}

#[test]
fn without_epg_group_counts_three_statuses() {
    let item_missing_id = create_test_item(
        1,
        "p1",
        "Ch 1",
        "Ch 1",
        0.0,
        XtreamCluster::Live,
        None,
        Some(ExplorerEpgState::MissingId),
        false,
    );
    let item_not_in_sources = create_test_item(
        2,
        "p2",
        "Ch 2",
        "Ch 2",
        0.0,
        XtreamCluster::Live,
        Some("ch2.id"),
        Some(ExplorerEpgState::NotInSources),
        false,
    );
    let item_no_programme = create_test_item(
        3,
        "p3",
        "Ch 3",
        "Ch 3",
        0.0,
        XtreamCluster::Live,
        Some("ch3.id"),
        Some(ExplorerEpgState::NoProgramme),
        false,
    );
    let item_ok = create_test_item(
        4,
        "p4",
        "Ch 4",
        "Ch 4",
        0.0,
        XtreamCluster::Live,
        Some("ch4.id"),
        Some(ExplorerEpgState::Ok),
        false,
    );

    assert!(is_without_epg(&item_missing_id));
    assert!(is_without_epg(&item_not_in_sources));
    assert!(is_without_epg(&item_no_programme));
    assert!(!is_without_epg(&item_ok));

    let channels = vec![item_missing_id, item_not_in_sources, item_no_programme, item_ok];
    assert_eq!(count_without_epg(&channels), 3);
}

#[test]
fn epg_badge_mapping_for_all_states() {
    assert_eq!(epg_badge_label_key(Some(ExplorerEpgState::MissingId)), Some("LABEL.EXPLORER_EPG_MISSING_ID"));
    assert_eq!(epg_badge_label_key(Some(ExplorerEpgState::NotInSources)), Some("LABEL.EXPLORER_EPG_NOT_IN_SOURCES"));
    assert_eq!(epg_badge_label_key(Some(ExplorerEpgState::NoProgramme)), Some("LABEL.EXPLORER_EPG_NO_PROGRAMME"));
    assert_eq!(epg_badge_label_key(Some(ExplorerEpgState::Ok)), None);
    assert_eq!(epg_badge_label_key(None), None);
}

#[test]
fn new_window_only_accepts_real_recent_timestamps() {
    let now = 1_700_000_000u64;
    assert!(!is_within_new_window(None, now));
    assert!(is_within_new_window(Some(now - 60), now));
    assert!(is_within_new_window(Some(now - NEW_ITEM_WINDOW_SECS + 1), now));
    assert!(!is_within_new_window(Some(now - NEW_ITEM_WINDOW_SECS - 1), now));
    assert!(!is_within_new_window(Some(now + 60), now), "future timestamps are provider data errors");
}

fn create_timestamp_item(
    virtual_id: u32,
    provider_id: &str,
    cluster: XtreamCluster,
    added: Option<u64>,
    last_modified: Option<u64>,
) -> Rc<UiPlaylistItem> {
    Rc::new(UiPlaylistItem {
        virtual_id,
        provider_id: Arc::from(provider_id),
        name: Arc::from(provider_id),
        title: Arc::from(provider_id),
        group: Arc::from("Group A"),
        logo: Arc::from(""),
        url: Arc::from(""),
        item_type: PlaylistItemType::Live,
        xtream_cluster: cluster,
        category_id: 1,
        rating: 0.0,
        input_name: Arc::from("input-1"),
        epg_channel_id: None,
        explorer_meta: Some(UiItemExplorerMeta {
            override_state: UiOverrideState::default(),
            epg_state: None,
            override_id: None,
            added,
            last_modified,
            chno: None,
            ..Default::default()
        }),
    })
}

#[test]
fn sort_vod_by_added_desc_and_asc_missing_last() {
    let item_200 = create_timestamp_item(1, "p200", XtreamCluster::Video, Some(200), None);
    let item_missing = create_timestamp_item(2, "pmissing", XtreamCluster::Video, None, None);
    let item_100 = create_timestamp_item(3, "p100", XtreamCluster::Video, Some(100), None);

    // Descending: newest first (200, 100, None last)
    let mut items_desc = vec![Rc::clone(&item_100), Rc::clone(&item_missing), Rc::clone(&item_200)];
    sort_items(&mut items_desc, ExplorerSort::Added, true);
    assert_eq!(items_desc[0].provider_id.as_ref(), "p200");
    assert_eq!(items_desc[1].provider_id.as_ref(), "p100");
    assert_eq!(items_desc[2].provider_id.as_ref(), "pmissing");

    // Ascending: oldest first (100, 200, None last)
    let mut items_asc = vec![Rc::clone(&item_100), Rc::clone(&item_missing), Rc::clone(&item_200)];
    sort_items(&mut items_asc, ExplorerSort::Added, false);
    assert_eq!(items_asc[0].provider_id.as_ref(), "p100");
    assert_eq!(items_asc[1].provider_id.as_ref(), "p200");
    assert_eq!(items_asc[2].provider_id.as_ref(), "pmissing");
}

#[test]
fn sort_series_by_last_modified_with_fallback_to_added() {
    let series_modified_300 = create_timestamp_item(1, "s300", XtreamCluster::Series, Some(100), Some(300));
    let series_added_fallback_250 = create_timestamp_item(2, "s250", XtreamCluster::Series, Some(250), None);
    let series_missing = create_timestamp_item(3, "smissing", XtreamCluster::Series, None, None);

    // Descending: s300 (last_modified 300), s250 (fallback added 250), smissing (missing -> last)
    let mut items_desc =
        vec![Rc::clone(&series_missing), Rc::clone(&series_added_fallback_250), Rc::clone(&series_modified_300)];
    sort_items(&mut items_desc, ExplorerSort::LastModified, true);
    assert_eq!(items_desc[0].provider_id.as_ref(), "s300");
    assert_eq!(items_desc[1].provider_id.as_ref(), "s250");
    assert_eq!(items_desc[2].provider_id.as_ref(), "smissing");

    // Ascending: s250 (fallback added 250), s300 (last_modified 300), smissing (missing -> last)
    let mut items_asc =
        vec![Rc::clone(&series_missing), Rc::clone(&series_added_fallback_250), Rc::clone(&series_modified_300)];
    sort_items(&mut items_asc, ExplorerSort::LastModified, false);
    assert_eq!(items_asc[0].provider_id.as_ref(), "s250");
    assert_eq!(items_asc[1].provider_id.as_ref(), "s300");
    assert_eq!(items_asc[2].provider_id.as_ref(), "smissing");
}

#[test]
fn channel_number_comes_from_meta_not_from_the_name() {
    let with_meta = |vid: u32, name: &str, chno: Option<u32>| {
        let mut item =
            (*create_test_item(vid, &format!("p{vid}"), name, name, 0.0, XtreamCluster::Live, None, None, false))
                .clone();
        if let Some(meta) = item.explorer_meta.as_mut() {
            meta.chno = chno;
        }
        Rc::new(item)
    };
    // A leading number in the name is part of the name ("7 RADIO"), not the channel number.
    let radio = with_meta(1, "7 RADIO ARTA", Some(202_362));
    let first = with_meta(2, "ZDF", Some(2));
    let unset = with_meta(3, "1 Unset", None);
    assert_eq!(item_chno(&radio), Some(202_362));
    assert_eq!(item_chno(&unset), None);

    let mut items = vec![Rc::clone(&unset), Rc::clone(&radio), Rc::clone(&first)];
    sort_items(&mut items, ExplorerSort::ChannelNo, false);
    let order: Vec<&str> = items.iter().map(|i| i.provider_id.as_ref()).collect();
    assert_eq!(order, ["p2", "p1", "p3"]);
}

#[test]
fn sort_ids_round_trip_and_start_direction() {
    for sort in ExplorerSort::ALL {
        assert_eq!(ExplorerSort::from_id(sort.id()), Some(sort));
    }
    assert_eq!(ExplorerSort::from_id("unknown"), None);
    assert!(ExplorerSort::Added.default_desc());
    assert!(ExplorerSort::LastModified.default_desc());
    assert!(!ExplorerSort::ChannelNo.default_desc());
    assert!(!ExplorerSort::Title.default_desc());
}

#[test]
fn rating_filter_uses_selected_threshold_only_when_active() {
    let item = create_test_item(1, "p1", "Movie", "Movie", 6.5, XtreamCluster::Video, None, None, false);
    assert!(passes_rating_filter(&item, false, 9));
    assert!(passes_rating_filter(&item, true, 6));
    assert!(!passes_rating_filter(&item, true, 7));
}

#[test]
fn items_without_channel_number_keep_playlist_order() {
    // An input has no channel numbers: sorting must not reshuffle it by provider id.
    let a = create_test_item(1, "z-last-id", "A", "A", 0.0, XtreamCluster::Live, None, None, false);
    let b = create_test_item(2, "a-first-id", "B", "B", 0.0, XtreamCluster::Live, None, None, false);
    let mut items = vec![Rc::clone(&a), Rc::clone(&b)];
    sort_items(&mut items, ExplorerSort::ChannelNo, false);
    let order: Vec<&str> = items.iter().map(|i| i.name.as_ref()).collect();
    assert_eq!(order, ["A", "B"]);
}
