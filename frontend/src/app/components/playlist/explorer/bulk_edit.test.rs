use super::*;
use shared::model::{
    OverrideFieldOp, OverrideItemKind, PlaylistItemType, UiItemExplorerMeta, UiOverrideState, XtreamCluster,
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
        epg_channel_id: Some(Arc::from("epg-1")),
        explorer_meta: Some(UiItemExplorerMeta {
            override_state: UiOverrideState { edited: false, pending: false, ambiguous: false },
            epg_state: None,
            ..Default::default()
        }),
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

    let patched = patch_playlist_item_bulk(&original, &patch);

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

    let patched = patch_playlist_item_bulk(&original, &patch);
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

    let cleared = patch_playlist_item_bulk(&patched, &clear_patch);
    assert_eq!(&*cleared.name, "");
    assert_eq!(&*cleared.title, "");
    assert_eq!(&*cleared.group, "");
    assert_eq!(&*cleared.logo, "");
    assert_eq!(cleared.epg_channel_id, None);
    assert_eq!(cleared.rating, 0.0);
}
