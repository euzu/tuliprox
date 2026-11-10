use super::*;
use crate::parser::xmltv::TVGuide;
use arc_swap::{ArcSwap, ArcSwapOption};
use shared::{
    foundation::get_filter,
    model::{
        ConfigInputAliasDto, ConfigInputDto, ConfigPaths, ConfigRenameDto, ConfigTargetDto, FieldDelta, ItemField,
        ItemOverride, MappingStage, OverrideItemKind, PlaylistClusterBouquetDto, PlaylistItem, PlaylistItemHeader,
        PlaylistItemType, StreamProperties, TargetBouquetDto, UUIDType, VideoStreamDetailProperties,
        VideoStreamProperties, XtreamCluster,
    },
    utils::Internable,
};
use std::{collections::HashMap, path::Path, sync::Arc};
use tempfile::tempdir;
use tuliprox_core::{
    model::{
        ConfigInput, ConfigRename, ConfigTarget, InputAliasMap, MediaToolCapabilities, OverrideKey, OverrideLookup,
        SourcesConfig, TargetBouquetFilter,
    },
    utils::FileLockManager,
};
use tuliprox_repository::{MemoryPlaylistSource, OverrideScope};

fn test_app_config(storage_dir: &Path, config_dir: &Path) -> Arc<AppConfig> {
    Arc::new(AppConfig {
        config: Arc::new(ArcSwap::from_pointee(tuliprox_core::model::Config {
            storage_dir: storage_dir.to_string_lossy().into_owned(),
            ..Default::default()
        })),
        sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
        hdhomerun: Arc::new(ArcSwapOption::default()),
        api_proxy: Arc::new(ArcSwapOption::default()),
        file_locks: Arc::new(FileLockManager::default()),
        failover: Arc::default(),
        paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
            home_path: String::new(),
            config_path: config_dir.to_string_lossy().into_owned(),
            storage_path: storage_dir.to_string_lossy().into_owned(),
            config_file_path: String::new(),
            sources_file_path: String::new(),
            mapping_file_path: None,
            mapping_files_used: None,
            template_file_path: None,
            template_files_used: None,
            failover_file_path: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        })),
        custom_stream_response: Arc::new(ArcSwapOption::default()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(MediaToolCapabilities::new()),
        plugins: Arc::default(),
    })
}

fn test_processing_context(
    app_config: Arc<AppConfig>,
    override_status: Option<Arc<tuliprox_repository::OverrideStatusStore>>,
) -> PlaylistProcessingContext<shared::model::NoopSink> {
    PlaylistProcessingContext {
        client: reqwest::Client::new(),
        run_id: "override-test-run".into(),
        execution_order: PlaylistUpdateRunOrder::from(1),
        config: app_config,
        user_targets: Arc::new(ProcessTargets {
            enabled: false,
            inputs: Vec::new(),
            targets: Vec::new(),
            target_names: Vec::new(),
        }),
        events: shared::model::NoopSink,
        playlist_state: None,
        disabled_headers: None,
        processed_inputs: Arc::default(),
        input_completions: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        input_locks: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        provider_manager: None,
        metadata_manager: None,
        pre_processed_inputs: None,
        stalker_refresh_mode: StalkerRefreshMode::Complete,
        partial_refresh: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        had_quality_rejections: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        maintenance: None,
        input_refresh: None,
        library_update_mode: LibraryUpdateMode::ExistingCatalog,
        override_status,
        plugins: shared::model::NoPlugins,
    }
}

fn make_live_item(id: &str, input_name: &str, name: &str, group: &str) -> PlaylistItem {
    let mut header = PlaylistItemHeader {
        id: id.intern(),
        input_name: input_name.intern(),
        name: name.intern(),
        title: name.intern(),
        group: group.intern(),
        url: format!("http://example.com/stream/{id}.ts").intern(),
        item_type: PlaylistItemType::Live,
        xtream_cluster: XtreamCluster::Live,
        ..Default::default()
    };
    header.gen_uuid();
    PlaylistItem { header }
}

#[tokio::test]
async fn no_override_files_output_identical() {
    let input = ConfigInput { name: "test-input".intern(), ..ConfigInput::default() };
    let item1 = make_live_item("1", "test-input", "Alpha", "News");
    let item2 = make_live_item("2", "test-input", "Beta", "Sports");

    let source1 = MemoryPlaylistSource::new(vec![
        PlaylistGroup {
            id: 1,
            title: "News".intern(),
            channels: vec![item1.clone()],
            xtream_cluster: XtreamCluster::Live,
        },
        PlaylistGroup {
            id: 2,
            title: "Sports".intern(),
            channels: vec![item2.clone()],
            xtream_cluster: XtreamCluster::Live,
        },
    ])
    .into_source();
    let mut fetched1 = FetchedPlaylist { input: &input, source: source1, epg: None };
    let mut duplicates1 = std::collections::HashSet::new();
    let target = ConfigTarget::from(&ConfigTargetDto::default());
    let pipe = get_processing_pipe(&target);

    let (mut processed1, _) =
        execute_pipe(&target, &pipe, &mut fetched1, &mut duplicates1, false, None, &NoPlugins, None)
            .await
            .expect("execution without overrides must succeed");
    let golden_groups = processed1.source.take_groups();
    let golden_serialized = serde_json::to_string(&golden_groups).expect("serialize golden");

    let source2 = MemoryPlaylistSource::new(vec![
        PlaylistGroup { id: 1, title: "News".intern(), channels: vec![item1], xtream_cluster: XtreamCluster::Live },
        PlaylistGroup { id: 2, title: "Sports".intern(), channels: vec![item2], xtream_cluster: XtreamCluster::Live },
    ])
    .into_source();
    let mut fetched2 = FetchedPlaylist { input: &input, source: source2, epg: None };
    let mut duplicates2 = std::collections::HashSet::new();

    // Set has overrides for test-input, but for key 999 which matches no item.
    // This exercises the full override collection path without modifying any item.
    let aliases = InputAliasMap::from_inputs(&[Arc::new(input.clone())]);
    let mut non_matching_map = HashMap::new();
    let mut lookup = HashMap::new();
    lookup.insert(
        OverrideKey::new(OverrideItemKind::Live, "999"),
        Arc::new(ItemOverride { name: FieldDelta::Set("Nonexistent".to_string()), ..Default::default() }),
    );
    non_matching_map.insert(Arc::from("test-input"), OverrideLookup::new(lookup));
    let non_matching_set = TargetOverrideSet::new(non_matching_map, HashMap::new(), aliases, Vec::new());

    let (mut processed2, _) =
        execute_pipe(&target, &pipe, &mut fetched2, &mut duplicates2, false, None, &NoPlugins, Some(&non_matching_set))
            .await
            .expect("execution with non-matching overrides must succeed");
    let test_groups = processed2.source.take_groups();
    let test_serialized = serde_json::to_string(&test_groups).expect("serialize test");

    assert_eq!(golden_serialized, test_serialized, "output through override collection path must be byte-identical");
}

#[tokio::test]
async fn input_override_visible_to_filter_and_mapper() {
    let input = ConfigInput { name: "live-input".intern(), ..ConfigInput::default() };
    let item = make_live_item("101", "live-input", "Old Name", "News");

    let source = MemoryPlaylistSource::new(vec![PlaylistGroup {
        id: 1,
        title: "News".intern(),
        channels: vec![item],
        xtream_cluster: XtreamCluster::Live,
    }])
    .into_source();
    let mut fetched = FetchedPlaylist { input: &input, source, epg: None };
    let mut duplicates = std::collections::HashSet::new();

    let mapping = super::mapping_stage::build_mapping(
        "test_map",
        MappingStage::Processing,
        r#"@name = concat(@Name, " Mapped")"#,
    );
    let mut target = super::mapping_stage::build_target(vec![mapping], false);
    target.filter = get_filter(r#"name = "Overridden Name""#, None).expect("filter").into();
    let pipe = get_processing_pipe(&target);

    let key = OverrideKey::new(OverrideItemKind::Live, "101");
    let mut input_map = HashMap::new();
    let mut lookup_map = HashMap::new();
    lookup_map.insert(
        key,
        Arc::new(ItemOverride { name: FieldDelta::Set("Overridden Name".to_string()), ..Default::default() }),
    );
    input_map.insert(Arc::from("live-input"), OverrideLookup::new(lookup_map));

    let aliases = InputAliasMap::from_inputs(&[Arc::new(input.clone())]);
    let set = TargetOverrideSet::new(input_map, HashMap::new(), aliases, Vec::new());

    let (mut processed, outcome) =
        execute_pipe(&target, &pipe, &mut fetched, &mut duplicates, false, None, &NoPlugins, Some(&set))
            .await
            .expect("pipeline execution must succeed");

    assert_eq!(outcome.filter.map(|f| f.retained), Some(1));
    let groups = processed.source.take_groups();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].channels[0].header.name.as_ref(), "Overridden Name Mapped");
}

#[test]
fn target_wins_over_input_same_field() {
    let input = ConfigInput { name: "src".intern(), ..ConfigInput::default() };
    let item = make_live_item("1", "src", "Original", "News");
    let uuid = item.header.uuid;

    let key = OverrideKey::new(OverrideItemKind::Live, "1");
    let mut input_map = HashMap::new();
    let mut in_lookup = HashMap::new();
    in_lookup.insert(
        key.clone(),
        Arc::new(ItemOverride {
            name: FieldDelta::Set("From Input".to_string()),
            logo: FieldDelta::Set("http://input.png".to_string()),
            ..Default::default()
        }),
    );
    input_map.insert(Arc::from("src"), OverrideLookup::new(in_lookup));

    let mut target_map = HashMap::new();
    let mut tgt_lookup = HashMap::new();
    tgt_lookup
        .insert(key, Arc::new(ItemOverride { name: FieldDelta::Set("From Target".to_string()), ..Default::default() }));
    target_map.insert(Arc::from("src"), OverrideLookup::new(tgt_lookup));

    let aliases = InputAliasMap::from_inputs(&[Arc::new(input)]);
    let set = TargetOverrideSet::new(input_map, target_map, aliases, Vec::new());

    let mut items = vec![item];
    let outcome = apply_overrides(&mut items, &set);

    assert_eq!(outcome.applied, 1);
    assert_eq!(items[0].header.name.as_ref(), "From Target");
    assert_eq!(items[0].header.logo.as_ref(), "http://input.png");
    assert!(outcome.target_locked.contains_key(&uuid));
}

#[test]
fn two_targets_different_group_same_input_name() {
    let input = ConfigInput { name: "shared-src".intern(), ..ConfigInput::default() };
    let key = OverrideKey::new(OverrideItemKind::Live, "10");

    let mut input_map = HashMap::new();
    let mut in_lookup = HashMap::new();
    in_lookup.insert(
        key.clone(),
        Arc::new(ItemOverride { name: FieldDelta::Set("Common Name".to_string()), ..Default::default() }),
    );
    input_map.insert(Arc::from("shared-src"), OverrideLookup::new(in_lookup));

    let aliases = InputAliasMap::from_inputs(&[Arc::new(input)]);

    let mut first_target_overrides = HashMap::new();
    let mut first_lookup = HashMap::new();
    first_lookup.insert(
        key.clone(),
        Arc::new(ItemOverride { group: FieldDelta::Set("Favoriten".to_string()), ..Default::default() }),
    );
    first_target_overrides.insert(Arc::from("shared-src"), OverrideLookup::new(first_lookup));
    let set_a = TargetOverrideSet::new(input_map.clone(), first_target_overrides, aliases.clone(), Vec::new());

    let mut second_target_overrides = HashMap::new();
    let mut second_lookup = HashMap::new();
    second_lookup
        .insert(key, Arc::new(ItemOverride { group: FieldDelta::Set("Live-TV".to_string()), ..Default::default() }));
    second_target_overrides.insert(Arc::from("shared-src"), OverrideLookup::new(second_lookup));
    let set_b = TargetOverrideSet::new(input_map, second_target_overrides, aliases, Vec::new());

    let mut item_a = vec![make_live_item("10", "shared-src", "Original", "General")];
    apply_overrides(&mut item_a, &set_a);
    assert_eq!(item_a[0].header.name.as_ref(), "Common Name");
    assert_eq!(item_a[0].header.group.as_ref(), "Favoriten");

    let mut item_b = vec![make_live_item("10", "shared-src", "Original", "General")];
    apply_overrides(&mut item_b, &set_b);
    assert_eq!(item_b[0].header.name.as_ref(), "Common Name");
    assert_eq!(item_b[0].header.group.as_ref(), "Live-TV");
}

#[tokio::test]
async fn no_override_dir_no_allocation() {
    let temp = tempdir().expect("tempdir");
    let non_existent = temp.path().join("overrides_missing");

    let input = Arc::new(ConfigInput { id: 1, name: "my-input".intern(), ..ConfigInput::default() });
    let mut target = ConfigTarget::from(&ConfigTargetDto::default());
    target.id = 10;
    target.name = "target-1".to_string();

    let set = TargetOverrideSet::load_from_dir(&non_existent, &target, &[input], &["my-input"]).await;

    assert!(set.is_empty());
    assert_eq!(set.input.len(), 0);
    assert_eq!(set.target.len(), 0);
    assert_eq!(set.revisions.len(), 0);
}

#[test]
fn duplicate_ids_marked_ambiguous_not_applied() {
    let input = ConfigInput { name: "src".intern(), ..ConfigInput::default() };
    let item1 = make_live_item("dup-1", "src", "Original 1", "News");
    let item2 = make_live_item("dup-1", "src", "Original 2", "News");

    let key = OverrideKey::new(OverrideItemKind::Live, "dup-1");
    let mut input_map = HashMap::new();
    let mut in_lookup = HashMap::new();
    in_lookup.insert(
        key.clone(),
        Arc::new(ItemOverride { name: FieldDelta::Set("Overridden".to_string()), ..Default::default() }),
    );
    input_map.insert(Arc::from("src"), OverrideLookup::new(in_lookup));

    let aliases = InputAliasMap::from_inputs(&[Arc::new(input)]);
    let set = TargetOverrideSet::new(input_map, HashMap::new(), aliases, Vec::new());

    let mut items = vec![item1, item2];
    let outcome = apply_overrides(&mut items, &set);

    assert_eq!(outcome.applied, 0);
    assert_eq!(outcome.ambiguous.len(), 1);
    assert_eq!(outcome.ambiguous[0], key);
    assert_eq!(items[0].header.name.as_ref(), "Original 1");
    assert_eq!(items[1].header.name.as_ref(), "Original 2");
}

#[tokio::test]
async fn bouquet_filter_sees_edited_group() {
    let input = ConfigInput { name: "input-bq".intern(), ..ConfigInput::default() };
    let item = make_live_item("99", "input-bq", "Channel", "Excluded");

    let source = MemoryPlaylistSource::new(vec![PlaylistGroup {
        id: 1,
        title: "Excluded".intern(),
        channels: vec![item],
        xtream_cluster: XtreamCluster::Live,
    }])
    .into_source();
    let mut fetched = FetchedPlaylist { input: &input, source, epg: None };
    let mut duplicates = std::collections::HashSet::new();
    let target = ConfigTarget::from(&ConfigTargetDto::default());

    let bouquet_dto = PlaylistClusterBouquetDto { live: Some(vec!["Allowed".to_string()]), vod: None, series: None };
    let filter = TargetBouquetFilter::from_dto(TargetBouquetDto::whitelist(bouquet_dto)).expect("bouquet filter");

    let key = OverrideKey::new(OverrideItemKind::Live, "99");
    let mut target_map = HashMap::new();
    let mut tgt_lookup = HashMap::new();
    tgt_lookup
        .insert(key, Arc::new(ItemOverride { group: FieldDelta::Set("Allowed".to_string()), ..Default::default() }));
    target_map.insert(Arc::from("input-bq"), OverrideLookup::new(tgt_lookup));

    let aliases = InputAliasMap::from_inputs(&[Arc::new(input.clone())]);
    let set = TargetOverrideSet::new(HashMap::new(), target_map, aliases, Vec::new());

    let (mut processed, _) =
        execute_pipe(&target, &vec![], &mut fetched, &mut duplicates, false, Some(&filter), &NoPlugins, Some(&set))
            .await
            .expect("pipe execution must succeed");

    let groups = processed.source.take_groups();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].channels.len(), 1);
    assert_eq!(groups[0].channels[0].header.group.as_ref(), "Allowed");
}

#[test]
fn reapply_target_overrides_and_regroup_moved_items() {
    let mut item_restored = make_live_item("1", "src", "Original", "News");
    let uuid_restored = item_restored.header.uuid;

    // Item not in locked map (e.g. was moved by mapper, but has no target override)
    let mut item_unlocked = make_live_item("2", "src", "Other", "News");
    item_unlocked.header.group = "Other Group".intern();

    let target_ov = Arc::new(ItemOverride {
        group: FieldDelta::Set("Favorites".to_string()),
        name: FieldDelta::Set("Favorite 1".to_string()),
        ..Default::default()
    });

    let mut locked = HashMap::new();
    locked.insert(uuid_restored, Arc::clone(&target_ov));

    item_restored.header.group = "Mapped Group".intern();
    item_restored.header.name = "Mapped Name".intern();

    let mut items = vec![item_restored, item_unlocked];
    let group_restored = reapply_target_overrides(items.iter_mut(), &locked);
    assert!(group_restored);
    assert_eq!(items[0].header.group.as_ref(), "Favorites");
    assert_eq!(items[0].header.name.as_ref(), "Favorite 1");

    let mut groups =
        vec![PlaylistGroup { id: 1, title: "News".intern(), channels: items, xtream_cluster: XtreamCluster::Live }];

    regroup_moved_items(&mut groups, &locked);

    // item_restored moved to Favorites; item_unlocked stayed in News because it was not in locked map.
    // Group News is NOT empty because item_unlocked is in it.
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].title.as_ref(), "News");
    assert_eq!(groups[0].channels.len(), 1);
    assert_eq!(groups[0].channels[0].header.id.as_ref(), "2");
    assert_eq!(groups[1].title.as_ref(), "Favorites");
    assert_eq!(groups[1].channels.len(), 1);
    assert_eq!(groups[1].channels[0].header.name.as_ref(), "Favorite 1");

    // If all items in a group are moved, the empty group must be dropped.
    let mut lone_group = vec![PlaylistGroup {
        id: 5,
        title: "Temp".intern(),
        channels: vec![make_live_item("3", "src", "Single", "Temp")],
        xtream_cluster: XtreamCluster::Live,
    }];
    lone_group[0].channels[0].header.group = "New Destination".intern();
    let lone_uuid = lone_group[0].channels[0].header.uuid;
    let mut lone_locked = HashMap::new();
    lone_locked.insert(
        lone_uuid,
        Arc::new(ItemOverride { group: FieldDelta::Set("New Destination".to_string()), ..Default::default() }),
    );

    regroup_moved_items(&mut lone_group, &lone_locked);
    assert_eq!(lone_group.len(), 1, "Temp group must be dropped because it became empty");
    assert_eq!(lone_group[0].title.as_ref(), "New Destination");
}

#[test]
fn nil_uuid_falls_back_to_get_uuid_for_insert_and_lookup() {
    let input = ConfigInput { name: "src".intern(), ..ConfigInput::default() };
    let mut item = make_live_item("1", "src", "Original", "News");
    item.header.uuid = UUIDType::default(); // nil header UUID
    let expected_uuid = item.get_uuid();
    assert_ne!(expected_uuid, UUIDType::default());

    let key = OverrideKey::new(OverrideItemKind::Live, "1");
    let mut target_map = HashMap::new();
    let mut tgt_lookup = HashMap::new();
    tgt_lookup
        .insert(key, Arc::new(ItemOverride { name: FieldDelta::Set("Target Name".to_string()), ..Default::default() }));
    target_map.insert(Arc::from("src"), OverrideLookup::new(tgt_lookup));

    let aliases = InputAliasMap::from_inputs(&[Arc::new(input)]);
    let set = TargetOverrideSet::new(HashMap::new(), target_map, aliases, Vec::new());

    let mut items = vec![item];
    let outcome = apply_overrides(&mut items, &set);

    assert_eq!(outcome.applied, 1);
    assert_eq!(items[0].header.name.as_ref(), "Target Name");
    assert!(outcome.target_locked.contains_key(&expected_uuid), "fallback UUID must be inserted into target_locked");

    // Tamper with name
    items[0].header.name = "Tampered".intern();
    // Reapply: lookup uses resolve_item_uuid which falls back to item.get_uuid()
    let restored = reapply_target_overrides(items.iter_mut(), &outcome.target_locked);
    assert!(restored || items[0].header.name.as_ref() == "Target Name");
    assert_eq!(items[0].header.name.as_ref(), "Target Name");
}

#[test]
fn nil_uuid_warns_when_both_nil() {
    assert_eq!(
        super::overrides::resolve_item_uuid_from_parts(UUIDType::default(), UUIDType::default(), "test-nil"),
        None
    );
}

#[tokio::test]
async fn alias_runtime_name_input_override_applies() {
    let input_dto = ConfigInputDto {
        name: "parent-src".intern(),
        aliases: Some(vec![ConfigInputAliasDto { name: "alias-src".intern(), ..Default::default() }]),
        ..Default::default()
    };
    let input = ConfigInput::from(&input_dto);

    let item = make_live_item("item-42", "alias-src", "Original Name", "News");

    let key = OverrideKey::new(OverrideItemKind::Live, "item-42");
    let mut input_map = HashMap::new();
    let mut in_lookup = HashMap::new();
    in_lookup.insert(
        key,
        Arc::new(ItemOverride { name: FieldDelta::Set("From Parent Override".to_string()), ..Default::default() }),
    );
    // Overrides in input scope are keyed by parent name
    input_map.insert(Arc::from("parent-src"), OverrideLookup::new(in_lookup));

    let aliases = InputAliasMap::from_inputs(&[Arc::new(input)]);
    let set = TargetOverrideSet::new(input_map, HashMap::new(), aliases, Vec::new());

    let mut items = vec![item];
    let outcome = apply_overrides(&mut items, &set);

    assert_eq!(outcome.applied, 1);
    assert_eq!(items[0].header.name.as_ref(), "From Parent Override");
}

#[tokio::test]
async fn target_file_alias_section_applies_to_parent() {
    let temp = tempdir().expect("tempdir");
    let overrides_dir = temp.path().join("overrides");
    tokio::fs::create_dir_all(overrides_dir.join("target")).await.expect("create dir");

    // Target file defines overrides under the alias name section
    let target_yaml = r"
schema_version: 1
inputs:
  alias-src:
    live:
      ch-1:
        name: Alias Target Name
";
    tokio::fs::write(overrides_dir.join("target/my-target.yml"), target_yaml.as_bytes())
        .await
        .expect("write target override file");

    let input_dto = ConfigInputDto {
        name: "parent-src".intern(),
        aliases: Some(vec![ConfigInputAliasDto { name: "alias-src".intern(), ..Default::default() }]),
        ..Default::default()
    };
    let input = Arc::new(ConfigInput::from(&input_dto));

    let mut target = ConfigTarget::from(&ConfigTargetDto::default());
    target.name = "my-target".to_string();

    let set = TargetOverrideSet::load_from_dir(&overrides_dir, &target, &[input], &["alias-src"]).await;

    assert!(!set.is_empty());
    // Mapped to parent-src
    assert!(set.target.contains_key("parent-src"));

    let mut item = vec![make_live_item("ch-1", "alias-src", "Original", "News")];
    let outcome = apply_overrides(&mut item, &set);
    assert_eq!(outcome.applied, 1);
    assert_eq!(item[0].header.name.as_ref(), "Alias Target Name");
}

#[tokio::test]
async fn load_from_dir_skips_malformed_preserves_valid() {
    let temp = tempdir().expect("tempdir");
    let overrides_dir = temp.path().join("overrides");
    tokio::fs::create_dir_all(overrides_dir.join("input")).await.expect("create dir");

    let valid_yaml = r"
schema_version: 1
live:
  item1:
    name: Valid Channel
";
    tokio::fs::write(overrides_dir.join("input/parent-valid.yml"), valid_yaml.as_bytes())
        .await
        .expect("write valid file");

    let malformed_yaml = "not: valid: yaml: [";
    tokio::fs::write(overrides_dir.join("input/parent-malformed.yml"), malformed_yaml.as_bytes())
        .await
        .expect("write malformed file");

    let input_valid = Arc::new(ConfigInput { id: 1, name: "parent-valid".intern(), ..ConfigInput::default() });
    let input_malformed = Arc::new(ConfigInput { id: 2, name: "parent-malformed".intern(), ..ConfigInput::default() });

    let mut target = ConfigTarget::from(&ConfigTargetDto::default());
    target.name = "test-target".to_string();

    let set = TargetOverrideSet::load_from_dir(
        &overrides_dir,
        &target,
        &[input_valid, input_malformed],
        &["parent-valid", "parent-malformed"],
    )
    .await;

    assert!(set.input.contains_key("parent-valid"));
    assert!(!set.input.contains_key("parent-malformed"));
    assert_eq!(set.revisions.len(), 1);
    assert_eq!(set.revisions[0].0, OverrideScope::Input("parent-valid".to_string()));
}

#[tokio::test]
async fn target_group_override_filtered_in_any_processing_order() {
    let input = ConfigInput { name: "src".intern(), ..ConfigInput::default() };
    let item = make_live_item("1", "src", "Channel 1", "News");
    let key = OverrideKey::new(OverrideItemKind::Live, "1");

    let mut target_map = HashMap::new();
    let mut lookup = HashMap::new();
    lookup
        .insert(key, Arc::new(ItemOverride { group: FieldDelta::Set("Favoriten".to_string()), ..Default::default() }));
    target_map.insert(Arc::from("src"), OverrideLookup::new(lookup));
    let aliases = InputAliasMap::from_inputs(&[Arc::new(input.clone())]);
    let set = TargetOverrideSet::new(HashMap::new(), target_map, aliases, Vec::new());

    // Pipelines with frm, mfr, rmf
    let orders = [
        vec![TransformStage::Filter, TransformStage::Rename, TransformStage::Map],
        vec![TransformStage::Map, TransformStage::Filter, TransformStage::Rename],
        vec![TransformStage::Rename, TransformStage::Map, TransformStage::Filter],
    ];

    for pipe_order in orders {
        // Filter excluding "Favoriten" must remove the item
        let mut target_excluding_fav = super::mapping_stage::build_target(
            vec![super::mapping_stage::build_mapping(
                "rename_group",
                MappingStage::Processing,
                r#"@group = "RenamedGroup""#,
            )],
            false,
        );
        target_excluding_fav.execution_plan.transform_stages = pipe_order.clone();
        target_excluding_fav.filter = get_filter(r#"group != "Favoriten""#, None).expect("filter").into();

        let source = MemoryPlaylistSource::new(vec![PlaylistGroup {
            id: 1,
            title: "News".intern(),
            channels: vec![item.clone()],
            xtream_cluster: XtreamCluster::Live,
        }])
        .into_source();
        let mut fpl = FetchedPlaylist { input: &input, source, epg: None };
        let mut duplicates = std::collections::HashSet::new();

        let (mut processed, _) = execute_pipe(
            &target_excluding_fav,
            &pipe_order,
            &mut fpl,
            &mut duplicates,
            false,
            None,
            &NoPlugins,
            Some(&set),
        )
        .await
        .expect("pipeline execution");
        let groups = processed.source.take_groups();
        assert!(
            groups.is_empty() || groups.iter().all(|g| g.channels.is_empty()),
            "filter excluding Favoriten must remove the item in order {pipe_order:?}"
        );

        // Filter excluding provider group "News" must keep the item
        let mut target_excluding_news = super::mapping_stage::build_target(
            vec![super::mapping_stage::build_mapping(
                "rename_group",
                MappingStage::Processing,
                r#"@group = "RenamedGroup""#,
            )],
            false,
        );
        target_excluding_news.execution_plan.transform_stages = pipe_order.clone();
        target_excluding_news.filter = get_filter(r#"group != "News""#, None).expect("filter").into();

        let source = MemoryPlaylistSource::new(vec![PlaylistGroup {
            id: 1,
            title: "News".intern(),
            channels: vec![item.clone()],
            xtream_cluster: XtreamCluster::Live,
        }])
        .into_source();
        let mut fpl = FetchedPlaylist { input: &input, source, epg: None };
        let mut duplicates = std::collections::HashSet::new();

        let (mut processed, _) = execute_pipe(
            &target_excluding_news,
            &pipe_order,
            &mut fpl,
            &mut duplicates,
            false,
            None,
            &NoPlugins,
            Some(&set),
        )
        .await
        .expect("pipeline execution");
        let groups = processed.source.take_groups();
        assert_eq!(
            groups.iter().map(|g| g.channels.len()).sum::<usize>(),
            1,
            "filter excluding News must keep the item in order {pipe_order:?}"
        );
        assert_eq!(groups[0].channels[0].header.group.as_ref(), "Favoriten");
    }
}

#[tokio::test]
async fn rename_and_both_mapper_stages_cannot_change_target_field() {
    let input = ConfigInput { name: "src".intern(), ..ConfigInput::default() };
    let item = make_live_item("1", "src", "OriginalName", "News");
    let key = OverrideKey::new(OverrideItemKind::Live, "1");

    let mut target_map = HashMap::new();
    let mut lookup = HashMap::new();
    lookup.insert(
        key,
        Arc::new(ItemOverride {
            name: FieldDelta::Set("TargetName".to_string()),
            logo: FieldDelta::Set("http://target.png".to_string()),
            ..Default::default()
        }),
    );
    target_map.insert(Arc::from("src"), OverrideLookup::new(lookup));
    let aliases = InputAliasMap::from_inputs(&[Arc::new(input.clone())]);
    let set = TargetOverrideSet::new(HashMap::new(), target_map, aliases, Vec::new());

    let processing_mapping = super::mapping_stage::build_mapping(
        "early_map",
        MappingStage::Processing,
        r#"@name = "MappedName"; @logo = "http://mapped.png""#,
    );
    let after_epg_mapping = super::mapping_stage::build_mapping(
        "late_map",
        MappingStage::AfterEpg,
        r#"@name = "LateMapped"; @logo = "http://late.png""#,
    );

    let mut target = super::mapping_stage::build_target(vec![processing_mapping, after_epg_mapping], false);
    target.rename = Some(vec![ConfigRename::from(&ConfigRenameDto {
        field: ItemField::Name,
        pattern: ".*".to_string(),
        new_name: "RenamedName".to_string(),
        t_pattern: None,
    })]);
    let pipe = vec![TransformStage::Rename, TransformStage::Map];

    let source = MemoryPlaylistSource::new(vec![PlaylistGroup {
        id: 1,
        title: "News".intern(),
        channels: vec![item],
        xtream_cluster: XtreamCluster::Live,
    }])
    .into_source();
    let mut fpl = FetchedPlaylist { input: &input, source, epg: None };
    let mut duplicates = std::collections::HashSet::new();

    let (mut processed, _) =
        execute_pipe(&target, &pipe, &mut fpl, &mut duplicates, false, None, &NoPlugins, Some(&set))
            .await
            .expect("pipeline execution");

    // Late mapper stage
    if let Some(mut groups) = map_playlist_at_stage(&mut processed.source, &target, MappingStage::AfterEpg, None) {
        let locked = set.target_locked();
        reapply_to_groups(&mut groups, &locked);
        processed.source = MemoryPlaylistSource::new(groups).into_source();
    }

    let groups = processed.source.take_groups();
    assert_eq!(groups[0].channels[0].header.name.as_ref(), "TargetName");
    assert_eq!(groups[0].channels[0].header.logo.as_ref(), "http://target.png");
}

#[test]
fn edited_chno_survives_numbering() {
    let input = ConfigInput { name: "src".intern(), ..ConfigInput::default() };
    let item1 = make_live_item("1", "src", "Channel 1", "News");
    let item2 = make_live_item("2", "src", "Channel 2", "News");

    let mut target_map = HashMap::new();
    let mut tgt_lookup = HashMap::new();
    tgt_lookup.insert(
        OverrideKey::new(OverrideItemKind::Live, "1"),
        Arc::new(ItemOverride { chno: FieldDelta::Set(42), ..Default::default() }),
    );
    tgt_lookup.insert(
        OverrideKey::new(OverrideItemKind::Live, "2"),
        Arc::new(ItemOverride { chno: FieldDelta::Clear, ..Default::default() }),
    );
    target_map.insert(Arc::from("src"), OverrideLookup::new(tgt_lookup));
    let aliases = InputAliasMap::from_inputs(&[Arc::new(input)]);
    let set = TargetOverrideSet::new(HashMap::new(), target_map, aliases, Vec::new());

    let mut items = vec![item1, item2];
    let outcome = apply_overrides(&mut items, &set);
    assert_eq!(outcome.applied, 2);
    assert_eq!(items[0].header.chno, 42);
    assert_eq!(items[1].header.chno, 0);

    let mut groups =
        vec![PlaylistGroup { id: 1, title: "News".intern(), channels: items, xtream_cluster: XtreamCluster::Live }];

    // Numbering preserves non-zero and assigns auto numbers to zero chnos
    assign_channel_no_playlist(&mut groups);
    assert_eq!(groups[0].channels[0].header.chno, 42);
    let auto_chno = groups[0].channels[1].header.chno;
    assert_ne!(auto_chno, 0);
    assert_ne!(auto_chno, 42);

    // On passes after numbering, skip chno: auto-assigned chno stays, set chno stays
    reapply_to_groups_skip_chno(&mut groups, &outcome.target_locked);
    assert_eq!(groups[0].channels[0].header.chno, 42);
    assert_eq!(groups[0].channels[1].header.chno, auto_chno);
}

#[tokio::test]
async fn edited_epg_id_drives_assignment_and_is_not_cleared_or_recased() {
    let input = ConfigInput { name: "src".intern(), ..ConfigInput::default() };
    let mut item = make_live_item("1", "src", "BBC One", "News");
    item.header.epg_channel_id = Some("Custom.Epg".intern());

    let key = OverrideKey::new(OverrideItemKind::Live, "1");
    let mut target_map = HashMap::new();
    let mut lookup = HashMap::new();
    lookup.insert(
        key,
        Arc::new(ItemOverride { epg_channel_id: FieldDelta::Set("Custom.Epg".to_string()), ..Default::default() }),
    );
    target_map.insert(Arc::from("src"), OverrideLookup::new(lookup));
    let aliases = InputAliasMap::from_inputs(&[Arc::new(input.clone())]);
    let set = TargetOverrideSet::new(HashMap::new(), target_map, aliases, Vec::new());
    set.target_locked().insert(
        item.header.uuid,
        Arc::new(ItemOverride { epg_channel_id: FieldDelta::Set("Custom.Epg".to_string()), ..Default::default() }),
    );

    let groups = vec![PlaylistGroup {
        id: 1,
        title: "News".intern(),
        channels: vec![item],
        xtream_cluster: XtreamCluster::Live,
    }];

    // Guide has lowercase "custom.epg"
    let guide_channel = Arc::new(shared::model::EpgChannel {
        id: "custom.epg".intern(),
        title: Some("BBC One".intern()),
        icon: None,
        programmes: Vec::new(),
    });
    let epg_sources =
        vec![Epg { children: vec![guide_channel], priority: 0, logo_override: false, ..Default::default() }];

    let mut fpl = FetchedPlaylist {
        input: &input,
        source: MemoryPlaylistSource::new(groups).into_source(),
        epg: Some(TVGuide::new(Vec::new())),
    };

    crate::processor::epg::adopt_guide_live_epg_id_case(&mut fpl, &epg_sources);
    crate::processor::epg::clear_invalid_live_epg_ids(&mut fpl, &epg_sources);

    let locked = set.target_locked();
    reapply_to_fpl(&mut fpl, &locked);

    let groups = fpl.source.take_groups();
    assert_eq!(groups[0].channels[0].header.epg_channel_id.as_deref(), Some("Custom.Epg"));
}

#[test]
fn edited_tmdb_used_by_resolve_but_user_plot_wins() {
    let input = ConfigInput { name: "src".intern(), ..ConfigInput::default() };
    let mut header = PlaylistItemHeader {
        id: "vod-1".intern(),
        input_name: "src".intern(),
        name: "Inception".intern(),
        title: "Inception".intern(),
        group: "Movies".intern(),
        item_type: PlaylistItemType::Video,
        xtream_cluster: XtreamCluster::Video,
        additional_properties: Some(StreamProperties::Video(Box::new(VideoStreamProperties {
            details: Some(VideoStreamDetailProperties {
                plot: Some(Arc::from("Initial raw plot")),
                ..Default::default()
            }),
            ..Default::default()
        }))),
        ..Default::default()
    };
    header.gen_uuid();
    let item = PlaylistItem { header };

    let key = OverrideKey::new(OverrideItemKind::Vod, "vod-1");
    let mut target_map = HashMap::new();
    let mut tgt_lookup = HashMap::new();
    tgt_lookup.insert(
        key,
        Arc::new(ItemOverride {
            tmdb: FieldDelta::Set(27205),
            plot: FieldDelta::Set("User custom plot wins".to_string()),
            ..Default::default()
        }),
    );
    target_map.insert(Arc::from("src"), OverrideLookup::new(tgt_lookup));
    let aliases = InputAliasMap::from_inputs(&[Arc::new(input)]);
    let set = TargetOverrideSet::new(HashMap::new(), target_map, aliases, Vec::new());

    let mut items = vec![item];
    let outcome = apply_overrides(&mut items, &set);
    assert_eq!(outcome.applied, 1);

    // Override was applied: tmdb is 27205, plot is user plot
    if let Some(StreamProperties::Video(v)) = &items[0].header.additional_properties {
        assert_eq!(v.tmdb, Some(27205));
        assert_eq!(v.details.as_ref().and_then(|d| d.plot.as_deref()), Some("User custom plot wins"));
    } else {
        panic!("expected Video stream properties");
    }

    // Resolve stub: resolve step uses the overridden tmdb ID (27205) to fetch/overwrite details
    if let Some(StreamProperties::Video(v)) = &mut items[0].header.additional_properties {
        if v.tmdb == Some(27205) {
            if let Some(details) = v.details.as_mut() {
                details.plot = Some(Arc::from("TMDB overview that would overwrite"));
            }
        }
    }

    // Post-resolve reapply: user plot wins over the resolved plot
    let mut groups =
        vec![PlaylistGroup { id: 1, title: "Movies".intern(), channels: items, xtream_cluster: XtreamCluster::Video }];

    reapply_to_groups(&mut groups, &outcome.target_locked);

    let channel = &groups[0].channels[0];
    if let Some(StreamProperties::Video(v)) = &channel.header.additional_properties {
        let details = v.details.as_ref().expect("video details");
        assert_eq!(v.tmdb, Some(27205));
        assert_eq!(details.plot.as_deref(), Some("User custom plot wins"));
    } else {
        panic!("expected Video stream properties");
    }
}

#[test]
fn late_mapper_moves_group_item_returns_to_target_group() {
    let input = ConfigInput { name: "src".intern(), ..ConfigInput::default() };
    let item = make_live_item("1", "src", "Channel 1", "News");
    let key = OverrideKey::new(OverrideItemKind::Live, "1");

    let mut target_map = HashMap::new();
    let mut lookup = HashMap::new();
    lookup.insert(
        key,
        Arc::new(ItemOverride { group: FieldDelta::Set("TargetGroup".to_string()), ..Default::default() }),
    );
    target_map.insert(Arc::from("src"), OverrideLookup::new(lookup));
    let aliases = InputAliasMap::from_inputs(&[Arc::new(input.clone())]);
    let set = TargetOverrideSet::new(HashMap::new(), target_map, aliases, Vec::new());
    set.target_locked().insert(
        item.header.uuid,
        Arc::new(ItemOverride { group: FieldDelta::Set("TargetGroup".to_string()), ..Default::default() }),
    );

    let late_mapper =
        super::mapping_stage::build_mapping("late_move", MappingStage::AfterEpg, r#"@group = "LateGroup""#);
    let target = super::mapping_stage::build_target(vec![late_mapper], false);

    let mut source = MemoryPlaylistSource::new(vec![PlaylistGroup {
        id: 1,
        title: "TargetGroup".intern(),
        channels: vec![item],
        xtream_cluster: XtreamCluster::Live,
    }])
    .into_source();

    let mut groups = map_playlist_at_stage(&mut source, &target, MappingStage::AfterEpg, None)
        .expect("late mapper should produce groups");
    assert_eq!(groups[0].title.as_ref(), "LateGroup");

    let locked = set.target_locked();
    reapply_to_groups(&mut groups, &locked);

    assert_eq!(groups[0].title.as_ref(), "TargetGroup");
    assert_eq!(groups[0].channels[0].header.group.as_ref(), "TargetGroup");
}

#[tokio::test]
async fn mapper_changing_id_does_not_break_reapply() {
    let input = ConfigInput { name: "src".intern(), ..ConfigInput::default() };
    let item = make_live_item("1", "src", "Original", "News");
    let key = OverrideKey::new(OverrideItemKind::Live, "1");

    let mut target_map = HashMap::new();
    let mut lookup = HashMap::new();
    lookup
        .insert(key, Arc::new(ItemOverride { name: FieldDelta::Set("LockedName".to_string()), ..Default::default() }));
    target_map.insert(Arc::from("src"), OverrideLookup::new(lookup));
    let aliases = InputAliasMap::from_inputs(&[Arc::new(input.clone())]);
    let set = TargetOverrideSet::new(HashMap::new(), target_map, aliases, Vec::new());

    let mapper = super::mapping_stage::build_mapping(
        "change_id_and_name",
        MappingStage::Processing,
        r#"@id = "completely_different_id"; @name = "TamperedName""#,
    );
    let target = super::mapping_stage::build_target(vec![mapper], false);
    let pipe = vec![TransformStage::Map];

    let source = MemoryPlaylistSource::new(vec![PlaylistGroup {
        id: 1,
        title: "News".intern(),
        channels: vec![item],
        xtream_cluster: XtreamCluster::Live,
    }])
    .into_source();
    let mut fpl = FetchedPlaylist { input: &input, source, epg: None };
    let mut duplicates = std::collections::HashSet::new();

    let (mut processed, _) =
        execute_pipe(&target, &pipe, &mut fpl, &mut duplicates, false, None, &NoPlugins, Some(&set))
            .await
            .expect("pipeline execution");
    let groups = processed.source.take_groups();
    assert_eq!(groups[0].channels[0].header.name.as_ref(), "LockedName");
}

#[tokio::test]
async fn end_to_end_pipeline_test() {
    let temp = tempdir().expect("tempdir");
    let storage_dir = temp.path().join("storage");
    let config_dir = temp.path().join("config");
    let override_dir = config_dir.join("overrides");
    tokio::fs::create_dir_all(&storage_dir).await.unwrap();
    tokio::fs::create_dir_all(&override_dir).await.unwrap();

    let app_config = test_app_config(&storage_dir, &config_dir);
    let ctx = test_processing_context(app_config, None);

    let input = Arc::new(ConfigInput { name: "prov".intern(), ..ConfigInput::default() });
    ctx.config.sources.store(Arc::new(tuliprox_core::model::SourcesConfig {
        inputs: vec![Arc::clone(&input)],
        ..Default::default()
    }));

    // Provider item
    let item = make_live_item("item1", "prov", "RawName", "RawGroup");

    // Write input and target override files to disk
    let repo = tuliprox_repository::OverrideRepository::new(&override_dir);
    let in_scope = OverrideScope::Input("prov".to_string());
    let in_path = repo.file_path(&in_scope).unwrap();
    tokio::fs::create_dir_all(in_path.parent().unwrap()).await.unwrap();
    tokio::fs::write(
        &in_path,
        "schema_version: 1\nlive:\n  item1:\n    name: InputName\n    epg_channel_id: GuideChannel\n",
    )
    .await
    .unwrap();

    let tgt_scope = OverrideScope::Target("end-to-end-target".to_string());
    let tgt_path = repo.file_path(&tgt_scope).unwrap();
    tokio::fs::create_dir_all(tgt_path.parent().unwrap()).await.unwrap();
    tokio::fs::write(
        &tgt_path,
        "schema_version: 1\ninputs:\n  prov:\n    live:\n      item1:\n        group: TargetGroup\n        logo: http://target.png\n",
    )
    .await
    .unwrap();

    let input_overrides = Arc::new(InputOverrideSet::load(&ctx.config).await);

    // Early mapper changes name to "MappedName"
    let early_mapper =
        super::mapping_stage::build_mapping("early", MappingStage::Processing, r#"@name = "MappedName""#);
    // Late mapper tries to change logo to "http://latemapped.png"
    let late_mapper =
        super::mapping_stage::build_mapping("late", MappingStage::AfterEpg, r#"@logo = "http://latemapped.png""#);

    let mut target = super::mapping_stage::build_target(vec![early_mapper, late_mapper], false);
    target.name = "end-to-end-target".to_string();
    target.processing_order = shared::model::ProcessingOrder::Mfr;
    target.execution_plan.transform_stages = vec![TransformStage::Map, TransformStage::Filter, TransformStage::Rename];
    target.filter = get_filter(r#"group = "TargetGroup" and name = "MappedName""#, None).expect("filter").into();
    target.output = vec![tuliprox_core::model::TargetOutput::M3u(tuliprox_core::model::M3uTargetOutput::from(
        &shared::model::M3uTargetOutputDto { filename: Some("output.m3u".into()), ..Default::default() },
    ))];

    let source = MemoryPlaylistSource::new(vec![PlaylistGroup {
        id: 1,
        title: "RawGroup".intern(),
        channels: vec![item],
        xtream_cluster: XtreamCluster::Live,
    }])
    .into_source();
    let fpl = FetchedPlaylist { input: &input, source, epg: None };
    let mut playlists = vec![fpl];
    let mut stats = HashMap::new();
    let mut errors = Vec::new();
    let analytics_info = Arc::new(collect_source_analytics_info(&mut playlists));

    let prepared = prepare_playlist_for_target(
        &ctx,
        &mut playlists,
        &target,
        &mut stats,
        &mut errors,
        ClusterFlags::empty(),
        false,
        &HashMap::new(),
        &analytics_info,
        &input_overrides,
    )
    .await
    .expect("prepare_playlist_for_target");

    let (result, final_errors, _) = finalize_prepared_target(Arc::new(ctx), prepared).await;
    assert!(result.is_ok(), "finalization must succeed: {final_errors:?}");

    let m3u_path = storage_dir.join("output.m3u");
    assert!(m3u_path.exists());
    let content = std::fs::read_to_string(&m3u_path).expect("read m3u");
    assert!(content.contains("MappedName"), "Name must be MappedName (renamed by mapper)");
    assert!(content.contains("TargetGroup"), "Group must be TargetGroup (locked override survived)");
    assert!(
        content.contains("http://target.png"),
        "Logo must be http://target.png (locked override survived late mapper)"
    );
    assert!(content.contains("GuideChannel"), "EPG ID must be GuideChannel (input override survived)");
}

#[test]
fn duplicate_manual_chno_logged() {
    let mut item1 = make_live_item("1", "src", "Channel 1", "News");
    let mut item2 = make_live_item("2", "src", "Channel 2", "News");
    item1.header.chno = 7;
    item2.header.chno = 7;

    let mut locked = HashMap::new();
    locked.insert(item1.header.uuid, Arc::new(ItemOverride { chno: FieldDelta::Set(7), ..Default::default() }));
    locked.insert(item2.header.uuid, Arc::new(ItemOverride { chno: FieldDelta::Set(7), ..Default::default() }));

    let groups = vec![PlaylistGroup {
        id: 1,
        title: "News".intern(),
        channels: vec![item1, item2],
        xtream_cluster: XtreamCluster::Live,
    }];

    let duplicates = check_duplicate_manual_chno("test-target", &groups, &locked);
    assert_eq!(duplicates, vec![7]);
}

#[tokio::test]
async fn status_hook_records_on_success_and_not_on_failure_through_process_targets() {
    let temp = tempdir().expect("tempdir");
    let storage_dir = temp.path().join("storage");
    let config_dir = temp.path().join("config");
    let override_dir = config_dir.join("overrides");
    tokio::fs::create_dir_all(&storage_dir).await.unwrap();
    tokio::fs::create_dir_all(&override_dir).await.unwrap();

    // Write input override file using OverrideRepository
    let repo = tuliprox_repository::OverrideRepository::new(&override_dir);
    let scope = OverrideScope::Input("prov".to_string());
    let path = repo.file_path(&scope).unwrap();
    tokio::fs::create_dir_all(path.parent().unwrap()).await.unwrap();
    tokio::fs::write(&path, "schema_version: 1\nlive:\n  item1:\n    name: Overridden\n").await.unwrap();

    let app_config = test_app_config(&storage_dir, &config_dir);
    let store = Arc::new(tuliprox_repository::OverrideStatusStore::new(&storage_dir));
    let ctx = Arc::new(test_processing_context(app_config, Some(Arc::clone(&store))));

    let input = Arc::new(ConfigInput { name: "prov".intern(), ..ConfigInput::default() });
    ctx.config.sources.store(Arc::new(tuliprox_core::model::SourcesConfig {
        inputs: vec![Arc::clone(&input)],
        ..Default::default()
    }));

    let item = make_live_item("item1", "prov", "RawName", "RawGroup");

    // Target ok: valid output target
    let mut target_ok = ConfigTarget::from(&ConfigTargetDto::default());
    target_ok.name = "target-ok".to_string();
    target_ok.output = vec![tuliprox_core::model::TargetOutput::M3u(tuliprox_core::model::M3uTargetOutput::from(
        &shared::model::M3uTargetOutputDto { filename: Some("ok.m3u".into()), ..Default::default() },
    ))];

    let source = MemoryPlaylistSource::new(vec![PlaylistGroup {
        id: 1,
        title: "RawGroup".intern(),
        channels: vec![item.clone()],
        xtream_cluster: XtreamCluster::Live,
    }])
    .into_source();
    let mut playlists = vec![FetchedPlaylist { input: &input, source, epg: None }];
    let mut stats = HashMap::new();
    let mut errors = Vec::new();

    let target_ok_arc = Arc::new(target_ok);
    let results = process_targets(
        &ctx,
        &mut playlists,
        &[&target_ok_arc],
        &mut stats,
        &mut errors,
        ClusterFlags::empty(),
        false,
        &HashMap::new(),
    )
    .await;

    assert_eq!(results.len(), 1);
    assert!(results[0].success);

    let scope = OverrideScope::Input("prov".to_string());
    assert!(store.applied_revision("target-ok", &scope).await.is_some(), "successful run must record status");

    // Target fail: output filter drops all items with accepted_empty_clusters, causing validation failure
    let mut target_fail = ConfigTarget::from(&ConfigTargetDto::default());
    target_fail.name = "target-fail".to_string();
    let non_matching_filter = get_filter(r#"name = "Nonexistent""#, None).expect("filter");
    target_fail.output = vec![tuliprox_core::model::TargetOutput::M3u(tuliprox_core::model::M3uTargetOutput {
        filename: Some("fail.m3u".into()),
        include_type_in_url: false,
        mask_redirect_url: false,
        filter: Some(non_matching_filter),
    })];

    let source_fail = MemoryPlaylistSource::new(vec![PlaylistGroup {
        id: 1,
        title: "RawGroup".intern(),
        channels: vec![item],
        xtream_cluster: XtreamCluster::Live,
    }])
    .into_source();
    let mut playlists_fail = vec![FetchedPlaylist { input: &input, source: source_fail, epg: None }];
    let target_fail_arc = Arc::new(target_fail);
    let results_fail = process_targets(
        &ctx,
        &mut playlists_fail,
        &[&target_fail_arc],
        &mut stats,
        &mut errors,
        ClusterFlags::Live,
        false,
        &HashMap::new(),
    )
    .await;

    assert_eq!(results_fail.len(), 1);
    assert!(!results_fail[0].success);
    assert_eq!(store.applied_revision("target-fail", &scope).await, None, "failing run must record nothing");
}

#[tokio::test]
async fn test_b6_deleted_files_and_schema_only_file_not_pending() {
    use tuliprox_repository::{OverrideRepository, OverrideRevision, OverrideStatusStore};

    let temp_overrides = tempfile::tempdir().expect("tempdir");
    let temp_storage = tempfile::tempdir().expect("tempdir");

    let repo = OverrideRepository::new(temp_overrides.path());
    let store = Arc::new(OverrideStatusStore::new(temp_storage.path()));

    let target_name = "target-1";
    let input_scope = OverrideScope::Input("input-a".to_string());

    // 1. Create a schema-only file
    let schema_only_yaml = "schema_version: 1\n";
    let file_path = repo.file_path(&input_scope).expect("path");
    tokio::fs::create_dir_all(file_path.parent().unwrap()).await.expect("create dir");
    tokio::fs::write(&file_path, schema_only_yaml.as_bytes()).await.expect("write schema-only file");
    let rev1 = OverrideRevision::from_bytes(schema_only_yaml.as_bytes());

    let mut target = ConfigTarget::from(&ConfigTargetDto::default());
    target.name = target_name.to_string();
    let input = Arc::new(ConfigInput { name: "input-a".intern(), ..ConfigInput::default() });

    // Verify it is initially pending before the run
    assert!(store.is_pending(target_name, &input_scope, &rev1).await);

    // Load overrides for target
    let set =
        TargetOverrideSet::load_from_dir(temp_overrides.path(), &target, &[Arc::clone(&input)], &["input-a"]).await;
    // Revisions must not be dropped!
    assert_eq!(set.revisions.len(), 1, "revisions must contain the schema-only file revision");

    // Record success
    let app_config = test_app_config(temp_overrides.path(), temp_storage.path());
    let ctx = test_processing_context(app_config, Some(Arc::clone(&store)));
    super::super::target::record_target_overrides_status(&ctx, target_name, Some(&set)).await;

    // After run, schema-only file is NOT pending!
    assert!(
        !store.is_pending(target_name, &input_scope, &rev1).await,
        "schema-only file must not be pending after run"
    );

    // 2. Now delete the override file
    repo.delete_file(&input_scope, &rev1).await.expect("delete file");
    let current_rev = OverrideRevision::empty();

    // Target previously had a record, so before the next run it is pending because recorded != current (rev1 != empty)
    assert!(store.is_pending(target_name, &input_scope, &current_rev).await);

    // Next run: no files exist
    let set_empty =
        TargetOverrideSet::load_from_dir(temp_overrides.path(), &target, &[Arc::clone(&input)], &["input-a"]).await;
    assert!(set_empty.has_no_files());

    super::super::target::record_target_overrides_status(&ctx, target_name, Some(&set_empty)).await;

    // After next run, deleted files are NOT pending!
    assert!(
        !store.is_pending(target_name, &input_scope, &current_rev).await,
        "deleted file must not be pending after next run"
    );
}

#[tokio::test]
async fn test_b7_filter_revisions_to_consumed_inputs() {
    use tuliprox_repository::OverrideRepository;

    let temp_overrides = tempfile::tempdir().expect("tempdir");
    let repo = OverrideRepository::new(temp_overrides.path());

    let scope_a = OverrideScope::Input("input-a".to_string());
    let scope_b = OverrideScope::Input("input-b".to_string());

    let yaml_a = "schema_version: 1\nlive:\n  1:\n    name: A\n";
    let yaml_b = "schema_version: 1\nlive:\n  2:\n    name: B\n";

    let path_a = repo.file_path(&scope_a).expect("path a");
    let path_b = repo.file_path(&scope_b).expect("path b");
    tokio::fs::create_dir_all(path_a.parent().unwrap()).await.expect("create dir");

    tokio::fs::write(&path_a, yaml_a.as_bytes()).await.expect("write a");
    tokio::fs::write(&path_b, yaml_b.as_bytes()).await.expect("write b");

    let mut target = ConfigTarget::from(&ConfigTargetDto::default());
    target.name = "target-only-a".to_string();

    let input_a = Arc::new(ConfigInput { name: "input-a".intern(), ..ConfigInput::default() });
    let input_b = Arc::new(ConfigInput { name: "input-b".intern(), ..ConfigInput::default() });

    // Target only consumes input-a
    let set = TargetOverrideSet::load_from_dir(
        temp_overrides.path(),
        &target,
        &[Arc::clone(&input_a), Arc::clone(&input_b)],
        &["input-a"],
    )
    .await;

    // Must only have revisions and applied_files for input-a, NOT input-b
    assert_eq!(set.revisions.len(), 1);
    assert_eq!(set.revisions[0].0, scope_a);
    assert_eq!(set.applied_files.len(), 1);
    assert!(set.applied_files[0].contains("input-a"));
}

#[test]
fn test_b8_resolve_item_uuid_lazy() {
    let mut item = make_live_item("1", "src", "Original", "News");
    let non_nil_uuid = item.get_uuid();
    item.header.uuid = non_nil_uuid;

    // When item.header.uuid is non-nil, resolve_item_uuid returns it directly without evaluating fallback
    assert_eq!(super::overrides::resolve_item_uuid(&item), Some(non_nil_uuid));
}

#[test]
fn test_b9_apply_overrides_only_inserts_existing_keys() {
    let mut input_map = HashMap::new();
    let mut lookup = HashMap::new();
    let ov = Arc::new(ItemOverride { name: FieldDelta::Set("Overridden".to_string()), ..Default::default() });
    let key = OverrideKey::new(OverrideItemKind::Live, "1");
    lookup.insert(key, ov);
    input_map.insert(Arc::from("src"), OverrideLookup::from(lookup));

    let input = ConfigInput { name: "src".intern(), ..ConfigInput::default() };
    let aliases = InputAliasMap::from_inputs(&[Arc::new(input)]);
    let set = TargetOverrideSet::new(input_map, HashMap::new(), aliases, Vec::new());

    let mut items = vec![
        make_live_item("1", "src", "Item 1", "News"), // has override
        make_live_item("2", "src", "Item 2", "News"), // NO override
        make_live_item("3", "src", "Item 3", "News"), // NO override
    ];

    let outcome = apply_overrides(&mut items, &set);
    assert_eq!(outcome.applied, 1);
    assert_eq!(items[0].header.name.as_ref(), "Overridden");
    assert_eq!(items[1].header.name.as_ref(), "Item 2");
    assert_eq!(items[2].header.name.as_ref(), "Item 3");
}

#[test]
fn test_b10_regroup_preserves_pre_existing_empty_groups() {
    let mut groups = vec![
        PlaylistGroup { id: 1, title: "EmptyBefore".intern(), channels: vec![], xtream_cluster: XtreamCluster::Live },
        PlaylistGroup {
            id: 2,
            title: "SourceGroup".intern(),
            channels: vec![make_live_item("1", "src", "Item 1", "SourceGroup")],
            xtream_cluster: XtreamCluster::Live,
        },
    ];

    // Move item 1 from SourceGroup to TargetGroup
    groups[1].channels[0].header.group = "TargetGroup".intern();
    let uuid = groups[1].channels[0].header.uuid;

    let mut locked = HashMap::new();
    locked.insert(
        uuid,
        Arc::new(ItemOverride { group: FieldDelta::Set("TargetGroup".to_string()), ..Default::default() }),
    );

    regroup_moved_items(&mut groups, &locked);

    // SourceGroup became empty, so it must be removed.
    // EmptyBefore was empty before the move, so it must NOT be removed!
    let titles: Vec<&str> = groups.iter().map(|g| g.title.as_ref()).collect();
    assert!(titles.contains(&"EmptyBefore"), "Pre-existing empty group must be preserved");
    assert!(!titles.contains(&"SourceGroup"), "Group emptied by move must be removed");
    assert!(titles.contains(&"TargetGroup"), "New group created by move must exist");
}

#[tokio::test]
#[ignore = "manual benchmark test"]
async fn benchmark_pipeline_100k_items() {
    let mut items = Vec::with_capacity(100_000);
    for i in 0..100_000 {
        let mut item = make_live_item(&i.to_string(), "bench", "Name", "Group");
        item.header.chno = 0;
        items.push(item);
    }

    let rename = ConfigRename::from(&ConfigRenameDto {
        field: ItemField::Name,
        pattern: "^Name$".to_string(),
        new_name: "Renamed".to_string(),
        t_pattern: None,
    });
    let mapping = super::mapping_stage::build_mapping("map_rule", MappingStage::Processing, r#"@name = "Mapped""#);
    let mut target = super::mapping_stage::build_target(vec![mapping], false);
    target.rename = Some(vec![rename]);
    let pipe = vec![TransformStage::Rename, TransformStage::Map];

    let items_clean = items.clone();
    let items_locked = items;

    let start_clean = std::time::Instant::now();
    let (groups_clean, _) = execute_pipeline_on_items(items_clean, &target, "bench", &pipe, &NoPlugins, None)
        .await
        .expect("no plugin aborts");
    let dur_clean = start_clean.elapsed();

    let mut locked = HashMap::new();
    for item in groups_clean.iter().flat_map(|g| &g.channels).take(100) {
        locked.insert(
            item.header.uuid,
            Arc::new(ItemOverride { name: FieldDelta::Set("Locked".to_string()), ..Default::default() }),
        );
    }

    let start_locked = std::time::Instant::now();
    let (groups_locked, _) =
        execute_pipeline_on_items(items_locked, &target, "bench", &pipe, &NoPlugins, Some(&locked))
            .await
            .expect("no plugin aborts");
    let dur_locked = start_locked.elapsed();

    assert_eq!(groups_clean.iter().map(|g| g.channels.len()).sum::<usize>(), 100_000);
    assert_eq!(groups_locked.iter().map(|g| g.channels.len()).sum::<usize>(), 100_000);

    eprintln!("Benchmark 100k items: clean = {dur_clean:?}, locked (100 overrides) = {dur_locked:?}");
}
