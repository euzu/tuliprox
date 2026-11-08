use super::*;
use shared::model::{OverrideFieldOp, OverrideItemKind, OverrideProvenanceDto, OverrideScopeDto, PlaylistItemType};
use std::collections::BTreeMap;

#[test]
fn patch_from_form_only_changed_fields() {
    let initial = PlaylistOverrideDto {
        name: "Initial Name".to_string(),
        title: "Initial Title".to_string(),
        group: "Initial Group".to_string(),
        chno: "1".to_string(),
        ..Default::default()
    };
    let mut current = initial.clone();
    current.name = "Updated Name".to_string();
    current.chno = "42".to_string();

    let deltas = FieldDeltas::default();
    let patch = build_item_patch(&initial, &current, &deltas, OverrideItemKind::Live);

    assert_eq!(patch.name, Some(OverrideFieldOp::Set("Updated Name".to_string())));
    assert_eq!(patch.chno, Some(OverrideFieldOp::Set(42)));
    assert_eq!(patch.title, None);
    assert_eq!(patch.group, None);
    assert_eq!(patch.logo, None);
    assert_eq!(patch.epg_channel_id, None);
    assert_eq!(patch.audio_track, None);
}

#[test]
fn reset_in_target_falls_back_to_input_label() {
    let mut input_override = BTreeMap::new();
    input_override.insert("group".to_string(), serde_json::json!("Input Group Value"));

    let provenance = OverrideProvenanceDto {
        provider: None,
        input_override: Some(input_override),
        target_override: None,
        processed: None,
        pending_per_target: vec![],
        ambiguous: false,
    };

    let fallback = reset_fallback(OverrideScopeDto::Target, "group", &provenance);
    assert_eq!(fallback.source, FallbackSource::InputOverride);
    assert_eq!(
        fallback.value.as_deref(),
        Some("Input Group Value"),
        "Reset in target scope must fall back to the input override value"
    );
}

#[test]
fn reset_fallback_reports_cleared_values() {
    let mut input_override = BTreeMap::new();
    input_override.insert("group".to_string(), serde_json::Value::Null);

    let provenance = OverrideProvenanceDto {
        provider: None,
        input_override: Some(input_override),
        target_override: None,
        processed: None,
        pending_per_target: vec![],
        ambiguous: false,
    };

    let fallback = reset_fallback(OverrideScopeDto::Target, "group", &provenance);
    assert_eq!(fallback.source, FallbackSource::InputOverride);
    assert_eq!(fallback.value, None);
}

#[test]
fn rating_prefill_is_dot_formatted_and_parsed_comma_tolerant() {
    let initial = PlaylistOverrideDto { rating: "7.5".to_string(), ..Default::default() };
    let mut current = initial.clone();

    // A user typing the localized comma form must produce the same patch.
    current.rating = "8,25".to_string();
    let patch = build_item_patch(&initial, &current, &FieldDeltas::default(), OverrideItemKind::Vod);
    assert_eq!(patch.rating, Some(OverrideFieldOp::Set(8.25)));

    let mut current = initial.clone();
    current.rating = "9.5".to_string();
    let patch = build_item_patch(&initial, &current, &FieldDeltas::default(), OverrideItemKind::Vod);
    assert_eq!(patch.rating, Some(OverrideFieldOp::Set(9.5)));
}

#[test]
fn patch_playlist_item_applies_form_fields_and_sets_pending() {
    use shared::model::{PlaylistItemType, UiPlaylistItem, XtreamCluster};
    use std::sync::Arc;

    let original = UiPlaylistItem {
        virtual_id: 101,
        provider_id: Arc::from("chan-1"),
        name: Arc::from("Old Name"),
        title: Arc::from("Old Title"),
        group: Arc::from("Old Group"),
        logo: Arc::from("http://old/logo.png"),
        url: Arc::from("http://stream/1"),
        item_type: PlaylistItemType::Live,
        xtream_cluster: XtreamCluster::Live,
        category_id: 1,
        rating: 0.0,
        input_name: Arc::from("main-input"),
        epg_channel_id: Some(Arc::from("epg-1")),
        explorer_meta: None,
    };

    let form = PlaylistOverrideDto {
        name: "New Name".to_string(),
        title: "New Title".to_string(),
        group: "New Group".to_string(),
        logo: "http://new/logo.png".to_string(),
        epg_channel_id: "epg-new".to_string(),
        ..Default::default()
    };
    let deltas = FieldDeltas::default();

    let patched = patch_playlist_item(&original, &form, &deltas, None, true);

    assert_eq!(&*patched.name, "New Name");
    assert_eq!(&*patched.title, "New Title");
    assert_eq!(&*patched.group, "New Group");
    assert_eq!(&*patched.logo, "http://new/logo.png");
    assert_eq!(patched.epg_channel_id.as_deref(), Some("epg-new"));

    let meta = patched.explorer_meta.expect("explorer_meta should be present");
    assert!(meta.override_state.edited);
    assert!(meta.override_state.pending);
}

#[test]
fn patch_playlist_item_resets_fields_and_clears_edited_when_no_deltas_remain() {
    use shared::model::{ItemFieldsDto, PlaylistItemType, UiPlaylistItem, XtreamCluster};
    use std::sync::Arc;

    let original = UiPlaylistItem {
        virtual_id: 102,
        provider_id: Arc::from("chan-2"),
        name: Arc::from("Overridden Name"),
        title: Arc::from("Overridden Title"),
        group: Arc::from("Overridden Group"),
        logo: Arc::from("http://custom/logo.png"),
        url: Arc::from("http://stream/2"),
        item_type: PlaylistItemType::Live,
        xtream_cluster: XtreamCluster::Live,
        category_id: 1,
        rating: 0.0,
        input_name: Arc::from("main-input"),
        epg_channel_id: None,
        explorer_meta: Some(shared::model::UiItemExplorerMeta {
            override_state: shared::model::UiOverrideState { edited: true, pending: false, ambiguous: false },
            epg_state: None,
            ..Default::default()
        }),
    };

    let provider_base = ItemFieldsDto {
        name: "Base Name".to_string(),
        title: "Base Title".to_string(),
        group: "Base Group".to_string(),
        logo: Some("http://base/logo.png".to_string()),
        logo_small: None,
        epg_channel_id: Some("epg-base".to_string()),
        chno: 10,
        parent_code: String::new(),
        audio_track: String::new(),
        time_shift: String::new(),
        rec: String::new(),
    };

    let provenance = OverrideProvenanceDto {
        provider: Some(provider_base),
        input_override: None,
        target_override: None,
        processed: None,
        pending_per_target: vec![],
        ambiguous: false,
    };

    let mut deltas = FieldDeltas::default();
    deltas.reset_fields.insert("name".to_string());
    deltas.reset_fields.insert("title".to_string());
    deltas.reset_fields.insert("group".to_string());
    deltas.reset_fields.insert("logo".to_string());
    deltas.reset_fields.insert("epg_channel_id".to_string());

    let form = PlaylistOverrideDto::default();

    let patched = patch_playlist_item(&original, &form, &deltas, Some(&provenance), false);

    assert_eq!(&*patched.name, "Base Name");
    assert_eq!(&*patched.title, "Base Title");
    assert_eq!(&*patched.group, "Base Group");
    assert_eq!(&*patched.logo, "http://base/logo.png");
    assert_eq!(patched.epg_channel_id.as_deref(), Some("epg-base"));

    let meta = patched.explorer_meta.expect("explorer_meta should be present");
    assert!(!meta.override_state.edited, "edited should be false when no deltas remain");
    assert!(meta.override_state.pending, "pending should be true until next update");
}

#[test]
fn replace_item_in_categories_replaces_single_rc_and_preserves_others() {
    use shared::model::{PlaylistItemType, UiPlaylistCategories, UiPlaylistGroup, UiPlaylistItem, XtreamCluster};
    use std::{rc::Rc, sync::Arc};

    let item1 = Rc::new(UiPlaylistItem {
        virtual_id: 1,
        provider_id: Arc::from("p1"),
        name: Arc::from("Channel 1"),
        title: Arc::from("Title 1"),
        group: Arc::from("News"),
        logo: Arc::from(""),
        url: Arc::from(""),
        item_type: PlaylistItemType::Live,
        xtream_cluster: XtreamCluster::Live,
        category_id: 1,
        rating: 0.0,
        input_name: Arc::from("in"),
        epg_channel_id: None,
        explorer_meta: None,
    });

    let item2 = Rc::new(UiPlaylistItem {
        virtual_id: 2,
        provider_id: Arc::from("p2"),
        name: Arc::from("Channel 2"),
        title: Arc::from("Title 2"),
        group: Arc::from("News"),
        logo: Arc::from(""),
        url: Arc::from(""),
        item_type: PlaylistItemType::Live,
        xtream_cluster: XtreamCluster::Live,
        category_id: 1,
        rating: 0.0,
        input_name: Arc::from("in"),
        epg_channel_id: None,
        explorer_meta: None,
    });

    let other_item = Rc::new(UiPlaylistItem {
        virtual_id: 3,
        provider_id: Arc::from("p3"),
        name: Arc::from("Sport 1"),
        title: Arc::from("Sport 1"),
        group: Arc::from("Sports"),
        logo: Arc::from(""),
        url: Arc::from(""),
        item_type: PlaylistItemType::Live,
        xtream_cluster: XtreamCluster::Live,
        category_id: 2,
        rating: 0.0,
        input_name: Arc::from("in"),
        epg_channel_id: None,
        explorer_meta: None,
    });

    let group1 = Rc::new(UiPlaylistGroup {
        id: 1,
        title: Arc::from("News"),
        channels: vec![item1.clone(), item2.clone()],
        xtream_cluster: XtreamCluster::Live,
    });

    let group2 = Rc::new(UiPlaylistGroup {
        id: 2,
        title: Arc::from("Sports"),
        channels: vec![other_item.clone()],
        xtream_cluster: XtreamCluster::Live,
    });

    let categories = UiPlaylistCategories { live: Some(vec![group1.clone(), group2.clone()]), vod: None, series: None };

    let mut patched_item1 = (*item1).clone();
    patched_item1.title = Arc::from("Title 1 Patched");

    let updated_categories = replace_item_in_categories(&categories, patched_item1);

    let updated_live = updated_categories.live.as_ref().unwrap();
    assert_eq!(updated_live.len(), 2);

    // Group 2 is untouched and keeps the same Rc
    assert!(Rc::ptr_eq(&updated_live[1], &group2));
    assert!(Rc::ptr_eq(&updated_live[1].channels[0], &other_item));

    // Group 1 has a new Rc
    assert!(!Rc::ptr_eq(&updated_live[0], &group1));

    // Channel 1 in Group 1 is patched
    assert_eq!(&*updated_live[0].channels[0].title, "Title 1 Patched");
    assert!(!Rc::ptr_eq(&updated_live[0].channels[0], &item1));

    // Channel 2 in Group 1 was untouched and keeps the same Rc
    assert!(Rc::ptr_eq(&updated_live[0].channels[1], &item2));
}

#[test]
fn replace_item_in_categories_matches_only_virtual_id_even_if_provider_id_collides() {
    let item_input_a = Rc::new(UiPlaylistItem {
        virtual_id: 101,
        provider_id: Arc::from("shared-provider-id"),
        name: Arc::from("Channel A"),
        title: Arc::from("Channel A"),
        group: Arc::from("News"),
        logo: Arc::from(""),
        url: Arc::from(""),
        item_type: PlaylistItemType::Live,
        xtream_cluster: XtreamCluster::Live,
        category_id: 1,
        rating: 0.0,
        input_name: Arc::from("Provider A"),
        epg_channel_id: None,
        explorer_meta: None,
    });

    let item_input_b = Rc::new(UiPlaylistItem {
        virtual_id: 202,
        provider_id: Arc::from("shared-provider-id"),
        name: Arc::from("Channel B"),
        title: Arc::from("Channel B"),
        group: Arc::from("News"),
        logo: Arc::from(""),
        url: Arc::from(""),
        item_type: PlaylistItemType::Live,
        xtream_cluster: XtreamCluster::Live,
        category_id: 1,
        rating: 0.0,
        input_name: Arc::from("Provider B"),
        epg_channel_id: None,
        explorer_meta: None,
    });

    let group = Rc::new(UiPlaylistGroup {
        id: 1,
        title: Arc::from("News"),
        channels: vec![item_input_a.clone(), item_input_b.clone()],
        xtream_cluster: XtreamCluster::Live,
    });

    let categories = UiPlaylistCategories { live: Some(vec![group]), vod: None, series: None };

    let mut patch_item_a = (*item_input_a).clone();
    patch_item_a.name = Arc::from("Channel A (Modified)");

    let updated = replace_item_in_categories(&categories, patch_item_a);
    let channels = &updated.live.as_ref().unwrap()[0].channels;

    assert_eq!(&*channels[0].name, "Channel A (Modified)");
    assert_eq!(channels[0].virtual_id, 101);
    assert_eq!(&*channels[1].name, "Channel B");
    assert_eq!(channels[1].virtual_id, 202);
    // item_b is identical Rc
    assert!(Rc::ptr_eq(&channels[1], &item_input_b));
}

#[test]
fn replace_item_in_categories_moves_item_to_group_creates_missing_and_drops_empty() {
    let item1 = Rc::new(UiPlaylistItem {
        virtual_id: 1,
        provider_id: Arc::from("p1"),
        name: Arc::from("Lonely Item"),
        title: Arc::from("Lonely Item"),
        group: Arc::from("OldGroup"),
        logo: Arc::from(""),
        url: Arc::from(""),
        item_type: PlaylistItemType::Live,
        xtream_cluster: XtreamCluster::Live,
        category_id: 10,
        rating: 0.0,
        input_name: Arc::from("in"),
        epg_channel_id: None,
        explorer_meta: None,
    });

    let group1 = Rc::new(UiPlaylistGroup {
        id: 10,
        title: Arc::from("OldGroup"),
        channels: vec![item1.clone()],
        xtream_cluster: XtreamCluster::Live,
    });

    let categories = UiPlaylistCategories { live: Some(vec![group1]), vod: None, series: None };

    let mut moved_item = (*item1).clone();
    moved_item.group = Arc::from("BrandNewGroup");

    let updated = replace_item_in_categories(&categories, moved_item);
    let live_groups = updated.live.as_ref().unwrap();

    // OldGroup had only 1 item and became empty -> dropped
    assert_eq!(live_groups.len(), 1);
    assert_eq!(&*live_groups[0].title, "BrandNewGroup");
    assert_eq!(live_groups[0].channels.len(), 1);
    assert_eq!(&*live_groups[0].channels[0].name, "Lonely Item");
    assert_eq!(&*live_groups[0].channels[0].group, "BrandNewGroup");
}

#[test]
fn apply_bulk_patches_to_categories_updates_multiple_items_and_rebuilds_groups() {
    let item1 = Rc::new(UiPlaylistItem {
        virtual_id: 1,
        provider_id: Arc::from("p1"),
        name: Arc::from("Item 1"),
        title: Arc::from("Item 1"),
        group: Arc::from("Group 1"),
        logo: Arc::from(""),
        url: Arc::from(""),
        item_type: PlaylistItemType::Live,
        xtream_cluster: XtreamCluster::Live,
        category_id: 1,
        rating: 0.0,
        input_name: Arc::from("in"),
        epg_channel_id: None,
        explorer_meta: None,
    });
    let item2 = Rc::new(UiPlaylistItem {
        virtual_id: 2,
        provider_id: Arc::from("p2"),
        name: Arc::from("Item 2"),
        title: Arc::from("Item 2"),
        group: Arc::from("Group 1"),
        logo: Arc::from(""),
        url: Arc::from(""),
        item_type: PlaylistItemType::Live,
        xtream_cluster: XtreamCluster::Live,
        category_id: 1,
        rating: 0.0,
        input_name: Arc::from("in"),
        epg_channel_id: None,
        explorer_meta: None,
    });

    let group = Rc::new(UiPlaylistGroup {
        id: 1,
        title: Arc::from("Group 1"),
        channels: vec![item1.clone(), item2.clone()],
        xtream_cluster: XtreamCluster::Live,
    });

    let categories = UiPlaylistCategories { live: Some(vec![group]), vod: None, series: None };

    let mut patched1 = (*item1).clone();
    patched1.name = Arc::from("Patched 1");
    let mut patched2 = (*item2).clone();
    patched2.group = Arc::from("Moved Group 2");

    let patch_map = bulk_patch_map(vec![patched1, patched2]);

    let updated = apply_bulk_patches_to_categories(&categories, &patch_map);
    let groups = updated.live.as_ref().unwrap();
    assert_eq!(groups.len(), 2);

    let g1 = groups.iter().find(|g| g.title.as_ref() == "Group 1").unwrap();
    assert_eq!(g1.channels.len(), 1);
    assert_eq!(&*g1.channels[0].name, "Patched 1");

    let g2 = groups.iter().find(|g| g.title.as_ref() == "Moved Group 2").unwrap();
    assert_eq!(g2.channels.len(), 1);
    assert_eq!(&*g2.channels[0].name, "Item 2");
}

#[test]
fn find_duplicate_chno_uses_precomputed_map() {
    let item1 = Rc::new(UiPlaylistItem {
        virtual_id: 1,
        provider_id: Arc::from("ch1-pid"),
        name: Arc::from("10 BBC One"),
        title: Arc::from("BBC One"),
        group: Arc::from("UK"),
        logo: Arc::from(""),
        url: Arc::from(""),
        item_type: PlaylistItemType::Live,
        xtream_cluster: XtreamCluster::Live,
        category_id: 1,
        rating: 0.0,
        input_name: Arc::from("in"),
        epg_channel_id: None,
        explorer_meta: Some(shared::model::UiItemExplorerMeta { chno: Some(10), ..Default::default() }),
    });

    let group = Rc::new(UiPlaylistGroup {
        id: 1,
        title: Arc::from("UK"),
        channels: vec![item1.clone()],
        xtream_cluster: XtreamCluster::Live,
    });

    let categories = UiPlaylistCategories { live: Some(vec![group]), vod: None, series: None };

    let map = build_channel_no_map(Some(&categories));
    assert!(map.contains_key(&10));

    // Looking up 10 for another channel finds duplicate
    let dup = find_duplicate_chno("10", 2, &map);
    assert_eq!(dup, Some("10 BBC One (BBC One)".to_string()));

    // Looking up 10 for the same channel ignores itself, even when another input shares its provider id
    let dup_self = find_duplicate_chno("10", 1, &map);
    assert_eq!(dup_self, None);

    // Looking up unknown channel number returns None
    let dup_none = find_duplicate_chno("99", 2, &map);
    assert_eq!(dup_none, None);
}

#[test]
fn patch_from_patch_updates_channel_number_meta() {
    let item = UiPlaylistItem {
        virtual_id: 7,
        provider_id: Arc::from("7"),
        name: Arc::from("Ch"),
        title: Arc::from("Ch"),
        group: Arc::from("G"),
        logo: Arc::from(""),
        url: Arc::from(""),
        item_type: PlaylistItemType::Live,
        xtream_cluster: XtreamCluster::Live,
        category_id: 1,
        rating: 0.0,
        input_name: Arc::from("in"),
        epg_channel_id: None,
        explorer_meta: Some(shared::model::UiItemExplorerMeta { chno: Some(10), ..Default::default() }),
    };
    let set = ItemOverridePatch { chno: Some(OverrideFieldOp::Set(42)), ..Default::default() };
    let updated = patch_playlist_item_from_patch(&item, &set, None, true);
    assert_eq!(updated.explorer_meta.as_ref().and_then(|m| m.chno), Some(42));

    let clear = ItemOverridePatch { chno: Some(OverrideFieldOp::Clear), ..Default::default() };
    let cleared = patch_playlist_item_from_patch(&item, &clear, None, true);
    assert_eq!(cleared.explorer_meta.as_ref().and_then(|m| m.chno), None);
}

#[test]
fn apply_bulk_patches_to_categories_ignores_same_virtual_id_in_other_cluster() {
    let make_item = |name: &str, group: &str, item_type: PlaylistItemType, cluster: XtreamCluster| {
        Rc::new(UiPlaylistItem {
            virtual_id: 1,
            provider_id: Arc::from(name),
            name: Arc::from(name),
            title: Arc::from(name),
            group: Arc::from(group),
            logo: Arc::from(""),
            url: Arc::from(""),
            item_type,
            xtream_cluster: cluster,
            category_id: 1,
            rating: 0.0,
            input_name: Arc::from("in"),
            epg_channel_id: None,
            explorer_meta: None,
        })
    };
    let live = make_item("Live", "Live Group", PlaylistItemType::Live, XtreamCluster::Live);
    let vod = make_item("Movie", "Vod Group", PlaylistItemType::Video, XtreamCluster::Video);
    let group_of = |item: &Rc<UiPlaylistItem>, cluster: XtreamCluster| {
        vec![Rc::new(UiPlaylistGroup {
            id: 1,
            title: Arc::clone(&item.group),
            channels: vec![Rc::clone(item)],
            xtream_cluster: cluster,
        })]
    };
    let categories = UiPlaylistCategories {
        live: Some(group_of(&live, XtreamCluster::Live)),
        vod: Some(group_of(&vod, XtreamCluster::Video)),
        series: None,
    };

    let mut patched = (*live).clone();
    patched.name = Arc::from("Live Patched");
    let patch_map = bulk_patch_map(vec![patched]);

    let updated = apply_bulk_patches_to_categories(&categories, &patch_map);
    assert_eq!(&*updated.live.as_ref().unwrap()[0].channels[0].name, "Live Patched");
    let vod_groups = updated.vod.as_ref().unwrap();
    assert_eq!(vod_groups.len(), 1);
    assert_eq!(vod_groups[0].channels.len(), 1);
    assert_eq!(&*vod_groups[0].channels[0].name, "Movie");
}

#[test]
fn bulk_patch_map_keeps_same_virtual_id_of_different_clusters_apart() {
    let make_item = |name: &str, group: &str, item_type: PlaylistItemType, cluster: XtreamCluster| {
        Rc::new(UiPlaylistItem {
            virtual_id: 1,
            provider_id: Arc::from(name),
            name: Arc::from(name),
            title: Arc::from(name),
            group: Arc::from(group),
            logo: Arc::from(""),
            url: Arc::from(""),
            item_type,
            xtream_cluster: cluster,
            category_id: 1,
            rating: 0.0,
            input_name: Arc::from("in"),
            epg_channel_id: None,
            explorer_meta: None,
        })
    };
    let live = make_item("Live", "Live Group", PlaylistItemType::Live, XtreamCluster::Live);
    let vod = make_item("Movie", "Vod Group", PlaylistItemType::Video, XtreamCluster::Video);
    let group_of = |item: &Rc<UiPlaylistItem>, cluster: XtreamCluster| {
        vec![Rc::new(UiPlaylistGroup {
            id: 1,
            title: Arc::clone(&item.group),
            channels: vec![Rc::clone(item)],
            xtream_cluster: cluster,
        })]
    };
    let categories = UiPlaylistCategories {
        live: Some(group_of(&live, XtreamCluster::Live)),
        vod: Some(group_of(&vod, XtreamCluster::Video)),
        series: None,
    };

    let mut patched_live = (*live).clone();
    patched_live.name = Arc::from("Live Patched");
    let mut patched_vod = (*vod).clone();
    patched_vod.name = Arc::from("Movie Patched");
    let patch_map = bulk_patch_map(vec![patched_live, patched_vod]);
    assert_eq!(patch_map.len(), 2);

    let updated = apply_bulk_patches_to_categories(&categories, &patch_map);
    assert_eq!(&*updated.live.as_ref().unwrap()[0].channels[0].name, "Live Patched");
    assert_eq!(&*updated.vod.as_ref().unwrap()[0].channels[0].name, "Movie Patched");
}
