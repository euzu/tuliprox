use super::*;
use shared::model::{OverrideFieldOp, OverrideItemKind, OverrideProvenanceDto, OverrideScopeDto};
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
fn name_edit_also_sets_title_when_equal() {
    let mut form = PlaylistOverrideDto {
        name: "Channel News".to_string(),
        title: "Channel News".to_string(),
        ..Default::default()
    };
    let mut deltas = FieldDeltas::default();

    // When also_set_title is true (default when name == title before edit)
    handle_name_change("Channel News HD", true, &mut form, &mut deltas);
    assert_eq!(form.name, "Channel News HD");
    assert_eq!(form.title, "Channel News HD", "Title should follow name when also_set_title is true");

    // When also_set_title is false
    handle_name_change("Channel News 4K", false, &mut form, &mut deltas);
    assert_eq!(form.name, "Channel News 4K");
    assert_eq!(form.title, "Channel News HD", "Title should remain unchanged when also_set_title is false");
}
