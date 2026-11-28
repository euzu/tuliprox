use super::*;
use shared::model::UiPlaylistItem;

fn s(val: &str) -> String { val.to_string() }

fn make_group(title: &str, count: usize) -> Rc<UiPlaylistGroup> {
    let channels = (0..count)
        .map(|i| {
            Rc::new(UiPlaylistItem {
                virtual_id: u32::try_from(i + 1).unwrap_or(1),
                group: Arc::from(title),
                title: Arc::from(format!("{title} {i}")),
                ..Default::default()
            })
        })
        .collect();
    Rc::new(UiPlaylistGroup { id: 1, title: Arc::from(title), channels, xtream_cluster: XtreamCluster::Live })
}

#[test]
fn merge_group_order_puts_stored_first_then_remaining_in_sidebar_order() {
    let stored = vec![s("Sports"), s("News")];
    let current = vec![s("Movies"), s("News"), s("Kids"), s("Sports")];
    assert_eq!(merge_group_order(&stored, &current), vec![s("Sports"), s("News"), s("Movies"), s("Kids")]);
}

#[test]
fn merge_group_order_drops_absent_and_duplicate_entries() {
    let stored = vec![s("Deleted"), s("Sports"), s("Sports"), s("News")];
    let current = vec![s("Movies"), s("Sports"), s("News"), s("Sports")];
    assert_eq!(merge_group_order(&stored, &current), vec![s("Sports"), s("News"), s("Movies")]);
}

#[test]
fn merge_group_order_handles_empty_stored_or_empty_current() {
    let current = vec![s("Alpha"), s("Beta")];
    assert_eq!(merge_group_order(&[], &current), vec![s("Alpha"), s("Beta")]);
    assert_eq!(merge_group_order(&[s("Alpha")], &[]), Vec::<String>::new());
}

#[test]
fn move_group_up_and_down_swap_adjacent_items_and_respect_bounds() {
    let order = vec![s("A"), s("B"), s("C")];
    assert_eq!(move_group_up(&order, 0), None);
    assert_eq!(move_group_up(&order, 1), Some(vec![s("B"), s("A"), s("C")]));
    assert_eq!(move_group_up(&order, 2), Some(vec![s("A"), s("C"), s("B")]));
    assert_eq!(move_group_up(&order, 3), None);

    assert_eq!(move_group_down(&order, 0), Some(vec![s("B"), s("A"), s("C")]));
    assert_eq!(move_group_down(&order, 1), Some(vec![s("A"), s("C"), s("B")]));
    assert_eq!(move_group_down(&order, 2), None);
    assert_eq!(move_group_down(&order, 3), None);
}

#[test]
fn move_group_to_handles_drag_drop_forward_backward_and_invalid_indices() {
    let order = vec![s("A"), s("B"), s("C"), s("D")];
    // Dragging forward from 0 to 2 places A at index 2.
    assert_eq!(move_group_to(&order, 0, 2), Some(vec![s("B"), s("C"), s("A"), s("D")]));
    // Dragging backward from 3 to 1 places D at index 1.
    assert_eq!(move_group_to(&order, 3, 1), Some(vec![s("A"), s("D"), s("B"), s("C")]));
    // Same index or out of bounds is a no-op.
    assert_eq!(move_group_to(&order, 1, 1), None);
    assert_eq!(move_group_to(&order, 4, 1), None);
    assert_eq!(move_group_to(&order, 1, 4), None);
    assert_eq!(move_group_to(&[], 0, 0), None);
}

#[test]
fn insert_group_after_places_new_group_directly_after_anchor() {
    let order = vec![s("News"), s("Sports"), s("Movies")];
    assert_eq!(
        insert_group_after(&order, "Documentaries", "News"),
        vec![s("News"), s("Documentaries"), s("Sports"), s("Movies")]
    );
    assert_eq!(
        insert_group_after(&order, "Documentaries", "Movies"),
        vec![s("News"), s("Sports"), s("Movies"), s("Documentaries")]
    );
    // Unknown anchor falls back to the end; existing or empty group names do not duplicate.
    assert_eq!(
        insert_group_after(&order, "Documentaries", "Missing"),
        vec![s("News"), s("Sports"), s("Movies"), s("Documentaries")]
    );
    assert_eq!(insert_group_after(&order, "  Sports  ", "News"), order);
    assert_eq!(insert_group_after(&order, "   ", "News"), order);
}

#[test]
fn reorder_categories_groups_updates_only_target_cluster_and_preserves_unlisted() {
    let categories = UiPlaylistCategories {
        live: Some(vec![make_group("A", 1), make_group("B", 2), make_group("C", 3)]),
        vod: Some(vec![make_group("V1", 1), make_group("V2", 1)]),
        series: None,
    };
    let updated = reorder_categories_groups(&categories, XtreamCluster::Live, &[s("C"), s("A")]);
    let live_titles: Vec<&str> = updated.live.as_ref().unwrap().iter().map(|g| g.title.as_ref()).collect();
    let vod_titles: Vec<&str> = updated.vod.as_ref().unwrap().iter().map(|g| g.title.as_ref()).collect();
    assert_eq!(live_titles, vec!["C", "A", "B"]);
    assert_eq!(vod_titles, vec!["V1", "V2"]);
    assert!(updated.series.is_none());
}

#[test]
fn target_group_cluster_maps_all_xtream_clusters() {
    assert_eq!(target_group_cluster(XtreamCluster::Live), TargetGroupCluster::Live);
    assert_eq!(target_group_cluster(XtreamCluster::Video), TargetGroupCluster::Vod);
    assert_eq!(target_group_cluster(XtreamCluster::Series), TargetGroupCluster::Series);
}

#[test]
fn group_order_i18n_keys_exist_in_all_locales() {
    let locales = [
        ("ar", include_str!("../../../../../public/assets/i18n/ar.json")),
        ("de", include_str!("../../../../../public/assets/i18n/de.json")),
        ("en", include_str!("../../../../../public/assets/i18n/en.json")),
        ("es", include_str!("../../../../../public/assets/i18n/es.json")),
        ("ru", include_str!("../../../../../public/assets/i18n/ru.json")),
        ("tr", include_str!("../../../../../public/assets/i18n/tr.json")),
    ];
    for (lang, raw) in locales {
        let json: serde_json::Value = serde_json::from_str(raw).unwrap();
        let conflict = json["MESSAGES"]["PLAYLIST_EXPLORER"]["GROUP_ORDER_CONFLICT"]
            .as_str()
            .unwrap_or_else(|| panic!("{lang}.json missing MESSAGES.PLAYLIST_EXPLORER.GROUP_ORDER_CONFLICT"));
        assert!(conflict.contains("source.yml"), "{lang}.json GROUP_ORDER_CONFLICT should mention source.yml");

        let too_many = json["MESSAGES"]["PLAYLIST_EXPLORER"]["GROUP_ORDER_TOO_MANY_GROUPS"]
            .as_str()
            .unwrap_or_else(|| panic!("{lang}.json missing MESSAGES.PLAYLIST_EXPLORER.GROUP_ORDER_TOO_MANY_GROUPS"));
        assert!(too_many.contains("{max}"), "{lang}.json GROUP_ORDER_TOO_MANY_GROUPS should interpolate {{max}}");

        for err_key in ["too_many_groups", "empty_group_name", "duplicate_group_name"] {
            let msg = json["error"][err_key].as_str().unwrap_or_else(|| panic!("{lang}.json missing error.{err_key}"));
            assert!(!msg.trim().is_empty(), "{lang}.json error.{err_key} must not be empty");
        }
    }
}

#[test]
fn reorder_move_disabled_respects_bounds_and_busy_states() {
    assert_eq!(reorder_move_disabled(0, 0, false, false), (true, true));
    assert_eq!(reorder_move_disabled(0, 1, false, false), (true, true));
    assert_eq!(reorder_move_disabled(0, 3, false, false), (true, false));
    assert_eq!(reorder_move_disabled(1, 3, false, false), (false, false));
    assert_eq!(reorder_move_disabled(2, 3, false, false), (false, true));
    assert_eq!(reorder_move_disabled(3, 3, false, false), (true, true));
    assert_eq!(reorder_move_disabled(1, 3, true, false), (true, true));
    assert_eq!(reorder_move_disabled(1, 3, false, true), (true, true));
}

#[test]
fn should_accept_reorder_drag_requires_internal_source_and_idle_state() {
    assert!(!should_accept_reorder_drag(None, false, false));
    assert!(should_accept_reorder_drag(Some(0), false, false));
    assert!(should_accept_reorder_drag(Some(2), false, false));
    assert!(!should_accept_reorder_drag(Some(0), true, false));
    assert!(!should_accept_reorder_drag(Some(0), false, true));
}

#[test]
fn drag_leave_clears_highlight_only_when_leaving_the_row() {
    // Moving between children of the row keeps the highlight.
    assert!(!clears_reorder_drag_over(true, Some(2), 2));
    // Leaving the row clears the highlight of that row.
    assert!(clears_reorder_drag_over(false, Some(2), 2));
    // Events of other rows and an already cleared highlight leave the state alone.
    assert!(!clears_reorder_drag_over(false, Some(1), 2));
    assert!(!clears_reorder_drag_over(false, None, 2));
}

#[test]
fn is_sidebar_request_current_rejects_unmounted_or_stale_generations() {
    let base = CatalogGeneration {
        source_revision: 1,
        catalog_revision: 2,
        target_name: Some("main".to_string()),
        cluster: XtreamCluster::Live,
    };
    assert!(is_sidebar_request_current(true, 5, 5, &base, &base));
    assert!(!is_sidebar_request_current(false, 5, 5, &base, &base));
    assert!(!is_sidebar_request_current(true, 5, 6, &base, &base));

    let local_edit = CatalogGeneration { catalog_revision: 3, ..base.clone() };
    assert!(!is_sidebar_request_current(true, 5, 5, &base, &local_edit));

    let reloaded = CatalogGeneration { source_revision: 2, ..base.clone() };
    assert!(!is_sidebar_request_current(true, 5, 5, &base, &reloaded));

    let other_target = CatalogGeneration { target_name: Some("backup".to_string()), ..base.clone() };
    assert!(!is_sidebar_request_current(true, 5, 5, &base, &other_target));

    let other_cluster = CatalogGeneration { cluster: XtreamCluster::Video, ..base.clone() };
    assert!(!is_sidebar_request_current(true, 5, 5, &base, &other_cluster));
}

#[test]
fn select_group_order_conflict_message_prefers_non_empty_server_message() {
    assert_eq!(
        select_group_order_conflict_message("Unsupported sort layout", "Fallback conflict"),
        "Unsupported sort layout"
    );
    assert_eq!(
        select_group_order_conflict_message("  Invalid sort filter  ", "Fallback conflict"),
        "Invalid sort filter"
    );
    assert_eq!(select_group_order_conflict_message("   ", "Fallback conflict"), "Fallback conflict");
    assert_eq!(select_group_order_conflict_message("", "Fallback conflict"), "Fallback conflict");
}

#[test]
fn resolve_conflict_reload_outcome_merges_stored_order_or_formats_error() {
    let current = vec![s("Sports"), s("News"), s("Kids")];
    assert_eq!(
        resolve_conflict_reload_outcome(
            "Revision changed",
            Ok(Some(vec![s("Kids"), s("Removed")])),
            &current,
            "Load failed: {error}",
        ),
        GroupOrderSaveOutcome::ConflictReloaded {
            merged_groups: vec![s("Kids"), s("Sports"), s("News")],
            error: "Revision changed".to_string(),
        }
    );
    assert_eq!(
        resolve_conflict_reload_outcome("Conflict", Ok(None), &current, "Load failed: {error}"),
        GroupOrderSaveOutcome::ConflictReloaded { merged_groups: current.clone(), error: "Conflict".to_string() }
    );
    assert_eq!(
        resolve_conflict_reload_outcome("Conflict", Err("timeout".to_string()), &current, "Load failed: {error}"),
        GroupOrderSaveOutcome::Failed("Load failed: timeout".to_string())
    );
}

#[test]
fn format_group_order_field_errors_with_formats_entries_and_fallback() {
    let translate_fn = |key: &str| match key {
        "error.duplicate_group_name" => "Duplicate group".to_string(),
        "error.too_many_groups" => "Too many groups".to_string(),
        other => other.to_string(),
    };
    assert_eq!(format_group_order_field_errors_with(&[], translate_fn, "Validation Error"), "Validation Error");
    let errs = vec![
        FieldError::new("groups", "error.duplicate_group_name").with_entry("Sports"),
        FieldError::new("groups", "error.too_many_groups"),
    ];
    assert_eq!(
        format_group_order_field_errors_with(&errs, translate_fn, "Validation Error"),
        "Duplicate group (Sports); Too many groups"
    );
}

#[test]
fn reorder_mode_is_disabled_above_group_limit_and_respects_busy_states() {
    assert!(!is_reorder_over_limit(0));
    assert!(!is_reorder_over_limit(MAX_REORDER_GROUPS));
    assert!(is_reorder_over_limit(MAX_REORDER_GROUPS + 1));

    assert!(!can_enter_reorder_mode(false, 10));
    assert!(!can_enter_reorder_mode(true, 0));
    assert!(can_enter_reorder_mode(true, 1));
    assert!(can_enter_reorder_mode(true, MAX_REORDER_GROUPS));
    assert!(!can_enter_reorder_mode(true, MAX_REORDER_GROUPS + 1));

    assert!(is_reorder_toggle_disabled(true, 0, false, false));
    assert!(!is_reorder_toggle_disabled(true, MAX_REORDER_GROUPS, false, false));
    assert!(is_reorder_toggle_disabled(true, MAX_REORDER_GROUPS + 1, false, false));
    assert!(is_reorder_toggle_disabled(true, 5, true, false));
    assert!(is_reorder_toggle_disabled(true, 5, false, true));

    assert!(!is_reorder_save_disabled(true, false, false, false, 1));
    assert!(is_reorder_save_disabled(false, false, false, false, 1));
    assert!(is_reorder_save_disabled(true, true, false, false, 1));
    assert!(is_reorder_save_disabled(true, false, true, false, 1));
    assert!(is_reorder_save_disabled(true, false, false, true, 1));
    assert!(is_reorder_save_disabled(true, false, false, false, 0));
}

#[test]
fn build_reorder_group_data_and_extract_titles_collect_titles_and_counts() {
    let groups = vec![make_group("News", 3), make_group("Sports", 7)];
    let data = build_reorder_group_data(Some(&groups));
    assert_eq!(data.titles, vec![s("News"), s("Sports")]);
    assert_eq!(data.counts.get("News"), Some(&3));
    assert_eq!(data.counts.get("Sports"), Some(&7));
    assert_eq!(extract_group_titles(Some(&groups)), vec![s("News"), s("Sports")]);

    let empty = build_reorder_group_data(None);
    assert_eq!(empty.titles, Vec::<String>::new());
    assert_eq!(empty.counts, HashMap::new());
    assert_eq!(extract_group_titles(None), Vec::<String>::new());
}
