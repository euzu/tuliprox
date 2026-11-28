use super::*;
use shared::model::{
    OverrideFieldOp, OverrideItemKind, OverrideScopeDto, PlaylistItemType, UiItemExplorerMeta, UiOverrideState,
    UiRatingOrigin, XtreamCluster,
};
use std::sync::Arc;

fn make_test_item(
    vid: u32,
    pid: &str,
    name: &str,
    input_name: &str,
    item_type: PlaylistItemType,
) -> Rc<UiPlaylistItem> {
    Rc::new(UiPlaylistItem {
        virtual_id: vid,
        provider_id: Arc::from(pid),
        name: Arc::from(name),
        title: Arc::from(name),
        group: Arc::from("Group 1"),
        logo: Arc::from("http://logo.png"),
        url: Arc::from("http://stream"),
        item_type,
        xtream_cluster: match item_type {
            PlaylistItemType::Live => XtreamCluster::Live,
            PlaylistItemType::Video => XtreamCluster::Video,
            PlaylistItemType::Series => XtreamCluster::Series,
            _ => XtreamCluster::Live,
        },
        category_id: 1,
        rating: 0.0,
        input_name: Arc::from(input_name),
        input_stream_id: "".into(),
        epg_channel_id: Some(Arc::from("epg-1")),
        explorer_meta: Some(UiItemExplorerMeta {
            override_state: UiOverrideState { edited: false, pending: false, ambiguous: false },
            epg_state: None,
            ..Default::default()
        }),
        ..Default::default()
    })
}

#[test]
fn bulk_form_data_defaults_to_unchanged() {
    let form = BulkFormData::default();
    assert_eq!(form.group.action, BulkFieldAction::Unchanged);
    assert_eq!(form.logo.action, BulkFieldAction::Unchanged);
    assert_eq!(form.logo_small.action, BulkFieldAction::Unchanged);
    assert_eq!(form.epg_channel_id.action, BulkFieldAction::Unchanged);
    assert_eq!(form.genre.action, BulkFieldAction::Unchanged);
    assert_eq!(form.rating.action, BulkFieldAction::Unchanged);
    assert_eq!(form.plot.action, BulkFieldAction::Unchanged);
    assert_eq!(form.cast.action, BulkFieldAction::Unchanged);
    assert_eq!(form.director.action, BulkFieldAction::Unchanged);
    assert_eq!(form.release_date.action, BulkFieldAction::Unchanged);
    assert!(!form.has_changes(), "Default bulk form data must have no changes");

    let live_patch = form.build_patch(OverrideItemKind::Live);
    assert_eq!(live_patch, ItemOverridePatch::default(), "Default bulk patch must be empty for Live");
    let vod_patch = form.build_patch(OverrideItemKind::Vod);
    assert_eq!(vod_patch, ItemOverridePatch::default(), "Default bulk patch must be empty for VOD");
}

#[test]
fn bulk_disables_unique_fields() {
    for &field in &["chno", "name", "title", "tmdb"] {
        assert!(
            !is_bulk_field_allowed(field, OverrideItemKind::Live),
            "Unique field '{field}' must not be allowed in Live bulk edit"
        );
        assert!(
            !is_bulk_field_allowed(field, OverrideItemKind::Vod),
            "Unique field '{field}' must not be allowed in VOD bulk edit"
        );
        assert!(
            !is_bulk_field_allowed(field, OverrideItemKind::Series),
            "Unique field '{field}' must not be allowed in Series bulk edit"
        );
    }

    assert!(is_bulk_field_allowed("group", OverrideItemKind::Live));
    assert!(is_bulk_field_allowed("logo", OverrideItemKind::Live));
    assert!(is_bulk_field_allowed("epg_channel_id", OverrideItemKind::Live));
    assert!(is_bulk_field_allowed("group", OverrideItemKind::Vod));
    assert!(is_bulk_field_allowed("genre", OverrideItemKind::Vod));
    assert!(is_bulk_field_allowed("plot", OverrideItemKind::Vod));
}

#[test]
fn bulk_groups_by_parent_input() {
    let item1 = make_test_item(1, "p1", "Item 1", "input-a", PlaylistItemType::Live);
    let item2 = make_test_item(2, "p2", "Item 2", "input-a", PlaylistItemType::Live);
    let item3 = make_test_item(3, "p3", "Item 3", "input-b", PlaylistItemType::Live);

    let items = vec![item1, item2, item3];
    let patch = ItemOverridePatch { group: Some(OverrideFieldOp::Set("Bulk Group".to_string())), ..Default::default() };

    let grouped = group_patches_by_parent_input(&items, &patch);

    assert_eq!(grouped.len(), 2, "Should group into exactly 2 parent inputs");
    assert_eq!(grouped.get("input-a").map(Vec::len), Some(2));
    assert_eq!(grouped.get("input-b").map(Vec::len), Some(1));

    let mut revisions = HashMap::new();
    revisions.insert("input-a".to_string(), "rev-a-1".to_string());
    revisions.insert("input-b".to_string(), "rev-b-1".to_string());

    let requests = prepare_bulk_input_save_requests(&revisions, &items, &patch);
    assert_eq!(requests.len(), 2, "Must produce one request per parent input");

    let req_a = requests.iter().find(|r| r.name == "input-a").expect("Request for input-a missing");
    assert_eq!(req_a.scope, OverrideScopeDto::Input);
    assert_eq!(req_a.base_revision, "rev-a-1");
    assert_eq!(req_a.patches.len(), 2);

    let req_b = requests.iter().find(|r| r.name == "input-b").expect("Request for input-b missing");
    assert_eq!(req_b.scope, OverrideScopeDto::Input);
    assert_eq!(req_b.base_revision, "rev-b-1");
    assert_eq!(req_b.patches.len(), 1);
}

#[test]
fn bulk_partial_failure_reported_per_input() {
    let results = vec![
        BulkInputResult {
            input: "input-a".to_string(),
            count: 2,
            status: BulkInputStatus::Saved("rev-a-2".to_string()),
        },
        BulkInputResult {
            input: "input-b".to_string(),
            count: 1,
            status: BulkInputStatus::Conflict("Stale revision".to_string()),
        },
    ];

    let summary = summarize_bulk_results(results);
    assert!(!summary.is_all_success(), "Must not report blanket success on partial failure");
    assert_eq!(summary.results.len(), 2);
    assert_eq!(summary.successful_inputs(), vec!["input-a".to_string()]);
    assert_eq!(summary.results[1].status, BulkInputStatus::Conflict("Stale revision".to_string()));
}

#[test]
fn bulk_keeps_other_item_deltas() {
    let original = make_test_item(1, "p1", "Preserved Channel Name", "input-a", PlaylistItemType::Live);
    let patch =
        ItemOverridePatch { group: Some(OverrideFieldOp::Set("New Bulk Group".to_string())), ..Default::default() };

    let patched =
        patch_playlist_item_bulk(&original, &patch, OverrideScopeDto::Input, None, ImdbRatingSource::ImdbThenProvider);

    assert_eq!(&*patched.group, "New Bulk Group");
    // Existing fields like name, title, epg_channel_id must NOT be altered or wiped
    assert_eq!(&*patched.name, "Preserved Channel Name");
    assert_eq!(&*patched.title, "Preserved Channel Name");
    assert_eq!(patched.epg_channel_id.as_deref(), Some("epg-1"));

    let meta = patched.explorer_meta.expect("explorer_meta must exist");
    assert!(meta.override_state.edited);
    assert!(meta.override_state.pending);
}

#[test]
fn bulk_patch_supports_all_ui_fields() {
    let original = make_test_item(1, "p1", "Old Name", "input-a", PlaylistItemType::Live);
    let patch = ItemOverridePatch {
        name: Some(OverrideFieldOp::Set("Bulk Name".to_string())),
        title: Some(OverrideFieldOp::Set("Bulk Title".to_string())),
        group: Some(OverrideFieldOp::Set("Bulk Group".to_string())),
        logo: Some(OverrideFieldOp::Set("http://bulk/logo.png".to_string())),
        epg_channel_id: Some(OverrideFieldOp::Set("epg-bulk".to_string())),
        rating: Some(OverrideFieldOp::Set(8.5)),
        ..Default::default()
    };

    let patched =
        patch_playlist_item_bulk(&original, &patch, OverrideScopeDto::Input, None, ImdbRatingSource::ImdbThenProvider);
    assert_eq!(&*patched.name, "Bulk Name");
    assert_eq!(&*patched.title, "Bulk Title");
    assert_eq!(&*patched.group, "Bulk Group");
    assert_eq!(&*patched.logo, "http://bulk/logo.png");
    assert_eq!(patched.epg_channel_id.as_deref(), Some("epg-bulk"));
    assert_eq!(patched.rating, 8.5);

    let clear_patch = ItemOverridePatch {
        name: Some(OverrideFieldOp::Clear),
        title: Some(OverrideFieldOp::Clear),
        group: Some(OverrideFieldOp::Clear),
        logo: Some(OverrideFieldOp::Clear),
        epg_channel_id: Some(OverrideFieldOp::Clear),
        rating: Some(OverrideFieldOp::Clear),
        ..Default::default()
    };

    let cleared = patch_playlist_item_bulk(
        &patched,
        &clear_patch,
        OverrideScopeDto::Input,
        None,
        ImdbRatingSource::ImdbThenProvider,
    );
    assert_eq!(&*cleared.name, "");
    assert_eq!(&*cleared.title, "");
    assert_eq!(&*cleared.group, "");
    assert_eq!(&*cleared.logo, "");
    assert_eq!(cleared.epg_channel_id, None);
    assert_eq!(cleared.rating, 0.0);
}

#[test]
fn bulk_rating_reset_leaves_rating_unchanged_when_no_provenance() {
    let mut original = (*make_test_item(1, "p1", "Item", "input-a", PlaylistItemType::Live)).clone();
    original.rating = 8.5;
    original.effective_rating = Some(8.5);
    original.rating_origin = Some(UiRatingOrigin::Provider);

    let patch = ItemOverridePatch { rating: Some(OverrideFieldOp::Reset), ..Default::default() };

    let patched =
        patch_playlist_item_bulk(&original, &patch, OverrideScopeDto::Input, None, ImdbRatingSource::ImdbThenProvider);
    assert_eq!(patched.rating, 8.5);
    assert_eq!(patched.effective_rating, Some(8.5));
    assert_eq!(patched.rating_origin, Some(UiRatingOrigin::Provider));
}

#[test]
fn bulk_inputs_list_class_matches_stylesheet() {
    let source = include_str!("bulk_edit.rs").split("#[cfg(test)]").next().unwrap();
    assert!(source.contains("class=\"tp__bulk-inputs-list\""));
    let styles = include_str!("../../../../../scss/app/components/playlist/_playlist_explorer.scss");
    assert!(styles.contains(".tp__bulk-inputs-list"));
}

#[test]
fn bulk_fetches_provenance_only_for_resets() {
    let set = ItemOverridePatch { group: Some(OverrideFieldOp::Set("G".to_string())), ..Default::default() };
    assert!(!patch_needs_provenance(&set));
    let clear = ItemOverridePatch { rating: Some(OverrideFieldOp::Clear), ..Default::default() };
    assert!(!patch_needs_provenance(&clear));
    let group_reset = ItemOverridePatch { group: Some(OverrideFieldOp::Reset), ..Default::default() };
    assert!(patch_needs_provenance(&group_reset));
    let rating_reset = ItemOverridePatch { rating: Some(OverrideFieldOp::Reset), ..Default::default() };
    assert!(patch_needs_provenance(&rating_reset));
}

#[test]
fn pending_new_group_order_insertion_requires_target_permission_new_group_and_real_selection() {
    let existing = vec!["News".to_string(), "Sports".to_string()];
    let new_group_patch =
        ItemOverridePatch { group: Some(OverrideFieldOp::Set("  Docs  ".to_string())), ..Default::default() };

    // Valid: target scope, source.write, new group name, real group selected.
    assert_eq!(
        pending_new_group_order_insertion(
            OverrideScopeDto::Target,
            true,
            "main",
            &new_group_patch,
            &existing,
            Some("News"),
        ),
        Some(("Docs".to_string(), "News".to_string()))
    );

    // Input scope -> nothing extra.
    assert_eq!(
        pending_new_group_order_insertion(
            OverrideScopeDto::Input,
            true,
            "main",
            &new_group_patch,
            &existing,
            Some("News"),
        ),
        None
    );

    // Missing source.write -> nothing extra.
    assert_eq!(
        pending_new_group_order_insertion(
            OverrideScopeDto::Target,
            false,
            "main",
            &new_group_patch,
            &existing,
            Some("News"),
        ),
        None
    );

    // No real group selected -> nothing extra.
    assert_eq!(
        pending_new_group_order_insertion(OverrideScopeDto::Target, true, "main", &new_group_patch, &existing, None),
        None
    );

    // Group already exists in the cluster -> nothing extra.
    let existing_group_patch =
        ItemOverridePatch { group: Some(OverrideFieldOp::Set("Sports".to_string())), ..Default::default() };
    assert_eq!(
        pending_new_group_order_insertion(
            OverrideScopeDto::Target,
            true,
            "main",
            &existing_group_patch,
            &existing,
            Some("News"),
        ),
        None
    );

    // Non-Set group op -> nothing extra.
    let reset_patch = ItemOverridePatch { group: Some(OverrideFieldOp::Reset), ..Default::default() };
    assert_eq!(
        pending_new_group_order_insertion(
            OverrideScopeDto::Target,
            true,
            "main",
            &reset_patch,
            &existing,
            Some("News"),
        ),
        None
    );
}

#[test]
fn bulk_edit_disables_interactions_while_saving_and_guards_stale_generation() {
    assert!(is_bulk_panel_interactive(false));
    assert!(!is_bulk_panel_interactive(true));

    let base = CatalogGeneration {
        source_revision: 1,
        catalog_revision: 2,
        target_name: Some("main".to_string()),
        cluster: XtreamCluster::Live,
    };
    assert!(is_bulk_request_active(true, &base, &base));
    assert!(!is_bulk_request_active(false, &base, &base));

    // Local catalog_revision bumps from item saves in the same source remain active.
    let after_local_edit = CatalogGeneration { catalog_revision: 3, ..base.clone() };
    assert!(is_bulk_request_active(true, &base, &after_local_edit));

    let after_reload = CatalogGeneration { source_revision: 2, ..base.clone() };
    assert!(!is_bulk_request_active(true, &base, &after_reload));

    let other_target = CatalogGeneration { target_name: Some("backup".to_string()), ..base.clone() };
    assert!(!is_bulk_request_active(true, &base, &other_target));

    let other_cluster = CatalogGeneration { cluster: XtreamCluster::Video, ..base.clone() };
    assert!(!is_bulk_request_active(true, &base, &other_cluster));
}

#[test]
fn confirmed_bulk_save_finishes_requests_and_only_gates_ui_reconciliation() {
    assert_eq!(BulkSaveProgress::after_await(true), BulkSaveProgress::Current);
    assert!(BulkSaveProgress::after_await(true).reconciles_ui());
    assert_eq!(BulkSaveProgress::after_await(false), BulkSaveProgress::StalePersist);
    assert!(!BulkSaveProgress::after_await(false).reconciles_ui());

    // Every request ran after the confirmation, so the results reflect what was committed.
    let summary = summarize_bulk_results(vec![
        BulkInputResult {
            input: "input-a".to_string(),
            count: 2,
            status: BulkInputStatus::Saved("rev-a-2".to_string()),
        },
        BulkInputResult {
            input: "input-b".to_string(),
            count: 1,
            status: BulkInputStatus::Saved("rev-b-2".to_string()),
        },
    ]);
    assert!(summary.is_all_success());
    assert_eq!(summary.successful_inputs(), vec!["input-a".to_string(), "input-b".to_string()]);
}

#[test]
fn target_bulk_save_places_the_new_group_even_when_the_panel_went_stale() {
    let current = BulkSaveProgress::Current;
    let stale = BulkSaveProgress::StalePersist;

    assert_eq!(plan_target_bulk_save(true, current), TargetBulkSavePlan { patch_items: true, place_group: true });
    assert_eq!(
        plan_target_bulk_save(true, stale),
        TargetBulkSavePlan { patch_items: false, place_group: true },
        "the overrides committed, so the new group is placed without a UI reconciliation"
    );
    assert_eq!(
        plan_target_bulk_save(false, current),
        TargetBulkSavePlan { patch_items: false, place_group: false },
        "a failed override save must not place the new group"
    );
    assert_eq!(plan_target_bulk_save(false, stale), TargetBulkSavePlan { patch_items: false, place_group: false });
}

#[test]
fn placement_failure_stays_separate_from_committed_item_results() {
    let summary = summarize_bulk_results(vec![BulkInputResult {
        input: "main".to_string(),
        count: 3,
        status: BulkInputStatus::Saved("rev-9".to_string()),
    }]);
    assert!(summary.is_all_success());

    let failed = BulkPlacementState::failed("Docs".to_string(), "News".to_string(), "Save failed: timeout".to_string());
    assert_eq!(failed.failure(), Some("Save failed: timeout"));
    assert_eq!(failed.retry_target(), Some(("Docs", "News")));
    assert!(summary.is_all_success(), "a placement failure must not rewrite the committed item results");

    let retrying = failed.retrying();
    assert_eq!(retrying.failure(), None);
    assert_eq!(retrying.retry_target(), None, "a running retry cannot start a second time");
    assert_eq!(retrying.retrying(), retrying);

    assert_eq!(BulkPlacementState::default(), BulkPlacementState::Idle);
    assert_eq!(BulkPlacementState::Idle.retrying(), BulkPlacementState::Idle);
}

#[test]
fn placement_failure_maps_save_outcomes() {
    assert_eq!(placement_failure(&GroupOrderSaveOutcome::Saved { groups: vec![], pending: false }), None);
    assert_eq!(placement_failure(&GroupOrderSaveOutcome::Stale), None);
    assert_eq!(
        placement_failure(&GroupOrderSaveOutcome::Failed("Save failed: offline".to_string())),
        Some("Save failed: offline".to_string())
    );
    assert_eq!(
        placement_failure(&GroupOrderSaveOutcome::ConflictReloaded {
            merged_groups: vec![],
            error: "Conflict".to_string(),
        }),
        Some("Conflict".to_string())
    );
}

#[test]
fn placement_retry_order_uses_the_latest_stored_order() {
    let stored = vec!["Sports".to_string(), "News".to_string()];
    let current = vec!["News".to_string(), "Sports".to_string(), "Kids".to_string()];
    assert_eq!(
        placement_retry_order(&stored, &current, "Docs", "News"),
        vec!["Sports".to_string(), "News".to_string(), "Docs".to_string(), "Kids".to_string()]
    );

    // The stored order fetched at retry time wins over the dispatch-time order, and groups that no
    // longer exist are dropped.
    let latest_stored = vec!["Kids".to_string(), "Removed".to_string()];
    assert_eq!(
        placement_retry_order(&latest_stored, &current, "Docs", "Kids"),
        vec!["Kids".to_string(), "Docs".to_string(), "News".to_string(), "Sports".to_string()]
    );
}
