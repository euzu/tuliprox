use crate::processor::playlist::target::{run_finalize_until_persist, run_finalize_until_persist_with_locked};
use shared::{
    model::{
        ConfigFavouritesDto, ConfigTargetDto, ConfigTargetOptions, FieldDelta, ItemOverride, PlaylistGroup,
        PlaylistItem, PlaylistItemHeader, PlaylistItemType, Prepare, XtreamCluster,
    },
    utils::{create_alias_uuid, Internable},
};
use std::{collections::HashMap, sync::Arc};
use tuliprox_core::model::ConfigTarget;

fn live(input: &str, stream_id: &str, title: &str) -> PlaylistItem {
    let mut header = PlaylistItemHeader {
        input_name: input.intern(),
        id: stream_id.intern(),
        title: title.intern(),
        name: title.intern(),
        url: format!("http://{input}.invalid/live/u/p/{stream_id}.ts").intern(),
        item_type: PlaylistItemType::Live,
        xtream_cluster: XtreamCluster::Live,
        ..Default::default()
    };
    header.freeze_input_stream_id();
    header.gen_uuid();
    PlaylistItem { header }
}

fn live_with_chno(input: &str, stream_id: &str, title: &str, chno: u32) -> PlaylistItem {
    let mut item = live(input, stream_id, title);
    item.header.chno = chno;
    item
}

fn group(title: &str, channels: Vec<PlaylistItem>) -> PlaylistGroup {
    PlaylistGroup { id: 1, title: title.intern(), channels, xtream_cluster: XtreamCluster::Live }
}

fn target_with_options(options_json: &str) -> ConfigTarget {
    let mut options: ConfigTargetOptions = serde_json::from_str(options_json).expect("valid options");
    options.prepare().expect("prepared");
    let dto = ConfigTargetDto { name: "T".to_string(), options: Some(options), ..Default::default() };
    ConfigTarget::from(&dto)
}

fn target_with_options_and_favourites(options_json: &str, group_name: &str, filter_str: &str) -> ConfigTarget {
    let mut options: ConfigTargetOptions = serde_json::from_str(options_json).expect("valid options");
    options.prepare().expect("prepared");
    let mut fav = ConfigFavouritesDto {
        cluster: XtreamCluster::Live,
        group: group_name.intern(),
        filter: filter_str.to_string(),
        match_as_ascii: false,
        t_filter: None,
    };
    fav.prepare(None).expect("prepared filter");
    let dto = ConfigTargetDto {
        name: "T".to_string(),
        options: Some(options),
        favourites: Some(vec![fav]),
        ..Default::default()
    };
    ConfigTarget::from(&dto)
}

#[tokio::test]
async fn favourites_copy_the_group_item_instead_of_joining_the_group() {
    let target = target_with_options_and_favourites(
        r#"{"failover_groups":{"enabled":true}}"#,
        "My Favourites",
        r#"Caption ~ "ZDF.*""#,
    );
    let playlist = vec![group("News", vec![live("solis", "1", "ZDF HD"), live("korath", "7", "ZDF FHD")])];
    let (finalized, pending) = run_finalize_until_persist(&target, playlist).await;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].members.len(), 2);
    let fav = finalized.iter().find(|g| g.title.as_ref() == "My Favourites").expect("favourites group");
    assert_eq!(fav.channels.len(), 1);
    assert_eq!(fav.channels[0].header.failover_group_uuid, Some(pending[0].group_uuid));
}

#[tokio::test]
async fn locked_representative_override_survives_grouping() {
    let target = target_with_options(r#"{"failover_groups":{"enabled":true,"name_strip_quality":true}}"#);
    // The representative already carries the applied target override.
    let representative = live_with_chno("solis", "1", "ZDF HD", 42);
    let representative_uuid = representative.header.uuid;
    let playlist = vec![group("News", vec![representative, live("korath", "7", "ZDF FHD")])];

    let item_override = Arc::new(ItemOverride {
        chno: FieldDelta::Set(42),
        name: FieldDelta::Set("ZDF HD".to_string()),
        title: FieldDelta::Set("ZDF HD".to_string()),
        ..Default::default()
    });
    let mut locked = HashMap::new();
    locked.insert(representative_uuid, Arc::clone(&item_override));

    let (finalized, pending) = run_finalize_until_persist_with_locked(&target, playlist, locked).await;
    assert_eq!(pending.len(), 1);
    let group_uuid = pending[0].group_uuid;
    let channel = finalized
        .iter()
        .flat_map(|group| &group.channels)
        .find(|channel| channel.header.failover_group_uuid == Some(group_uuid))
        .expect("group channel");
    assert_eq!(channel.header.uuid, group_uuid, "the group channel carries the group uuid");
    assert_eq!(channel.header.chno, 42, "the representative's locked channel number must survive grouping");
    assert_eq!(channel.header.name.as_ref(), "ZDF HD", "the representative's locked name must survive grouping");
}

#[tokio::test]
async fn group_copies_inherit_the_representative_lock_like_other_alias_copies() {
    let target = target_with_options(r#"{"failover_groups":{"enabled":true}}"#);
    let mut representative = live_with_chno("solis", "1", "ZDF HD", 42);
    representative.header.logo = "http://locked.example/zdf.png".intern();
    let representative_uuid = representative.header.uuid;
    let mut copy = representative.clone();
    copy.header.group = "Favourites".intern();
    copy.header.uuid = create_alias_uuid(&representative_uuid, "Favourites");
    let playlist =
        vec![group("News", vec![representative, live("korath", "7", "ZDF FHD")]), group("Favourites", vec![copy])];

    let item_override = Arc::new(ItemOverride {
        logo: FieldDelta::Set("http://locked.example/zdf.png".to_string()),
        ..Default::default()
    });
    let mut locked = HashMap::new();
    locked.insert(representative_uuid, Arc::clone(&item_override));

    let (finalized, pending) = run_finalize_until_persist_with_locked(&target, playlist, locked).await;
    assert_eq!(pending.len(), 1);
    let group_copy = finalized
        .iter()
        .filter(|group| group.title.as_ref() == "Favourites")
        .flat_map(|group| &group.channels)
        .find(|channel| channel.header.failover_group_uuid == Some(pending[0].group_uuid))
        .expect("group copy");
    assert_eq!(group_copy.header.logo.as_ref(), "http://locked.example/zdf.png");
}

#[tokio::test]
async fn grouping_runs_before_deduplicate_and_dedup_ignores_group_members() {
    let target = target_with_options(r#"{"failover_groups":{"enabled":true},"deduplicate":{}}"#);
    let playlist = vec![group(
        "News",
        vec![
            live("solis", "1", "ZDF HD"),
            live("korath", "7", "ZDF FHD"),
            live("veyra", "3", "ARD HD"),
            live("veyra", "4", "ARD"),
        ],
    )];
    let (finalized, pending) = run_finalize_until_persist(&target, playlist).await;
    assert_eq!(pending.len(), 2);
    assert_eq!(finalized[0].channels.len(), 2);
}

#[tokio::test]
async fn grouping_uses_global_config_with_target_overrides() {
    let dir = tempfile::tempdir().expect("tempdir");
    let app_config = super::curation_effect_gate::app_config(dir.path());
    let mut global: shared::model::FailoverGroupsConfig =
        serde_json::from_str(r#"{"match_normalize":{"ignore_tokens":["backup"]},"name_strip_quality":true}"#)
            .expect("global config");
    global.prepare().expect("prepared");
    app_config.config.store(std::sync::Arc::new(tuliprox_core::model::Config {
        storage_dir: dir.path().to_string_lossy().into_owned(),
        failover_groups: global,
        ..tuliprox_core::model::Config::default()
    }));
    let target = target_with_options(r#"{"failover_groups":{"enabled":true,"name_strip_quality":false}}"#);
    let mut playlist = vec![group("News", vec![live("a", "1", "ZDF HD"), live("b", "2", "ZDF HD backup")])];
    let mut step = tuliprox_core::utils::StepMeasure::new("test", |_, _| {});
    let mut errors = Vec::new();

    let pending = crate::processor::playlist::target::process_target_failover_groups(
        &app_config,
        &target,
        &mut playlist,
        &mut step,
        &mut errors,
    );

    assert!(errors.is_empty());
    assert_eq!(pending.groups.len(), 1, "the global ignore token joins both channels");
    let names: Vec<&str> = playlist[0].channels.iter().map(|item| item.header.name.as_ref()).collect();
    assert_eq!(names, ["ZDF HD"], "the target keeps quality words");
}

fn snapshot_with_excluded(target: &str, input: &str, stream_id: &str) -> tuliprox_core::model::FailoverOverrides {
    let mut overrides = shared::model::TargetFailoverOverridesDto::new(target);
    overrides
        .excluded
        .push(shared::model::ChannelMemberRefDto { input: input.to_string(), stream_id: stream_id.to_string() });
    let entry = tuliprox_core::model::TargetOverridesEntry {
        etag: overrides.etag().into(),
        overrides,
        file: std::path::PathBuf::from("not-on-disk.yml"),
        findings: Vec::new(),
    };
    tuliprox_core::model::FailoverOverrides {
        sections: std::collections::HashMap::from([(target.into(), std::sync::Arc::new(entry))]),
        ..Default::default()
    }
}

#[tokio::test]
async fn processing_reads_snapshot_not_disk() {
    let dir = tempfile::tempdir().expect("tempdir");
    let app_config = super::curation_effect_gate::app_config(dir.path());
    let target = target_with_options(r#"{"failover_groups":{"enabled":true}}"#);
    let mut step = tuliprox_core::utils::StepMeasure::new("test", |_, _| {});
    let channels = || vec![group("News", vec![live("solis", "1", "ZDF HD"), live("korath", "7", "ZDF FHD")])];

    let mut playlist = channels();
    let mut errors = Vec::new();
    let without = crate::processor::playlist::target::process_target_failover_groups(
        &app_config,
        &target,
        &mut playlist,
        &mut step,
        &mut errors,
    );
    assert_eq!(without.groups.len(), 1, "without a section both channels are grouped");

    app_config.failover.store(std::sync::Arc::new(snapshot_with_excluded("T", "solis", "1")));
    let mut playlist = channels();
    let with = crate::processor::playlist::target::process_target_failover_groups(
        &app_config,
        &target,
        &mut playlist,
        &mut step,
        &mut errors,
    );

    assert!(errors.is_empty());
    assert!(with.groups.is_empty(), "the excluded channel from the snapshot prevents grouping");
    assert_eq!(playlist[0].channels.len(), 2);
}

#[tokio::test]
async fn processing_reports_failover_load_error_and_groups_without_overrides() {
    let dir = tempfile::tempdir().expect("tempdir");
    let app_config = super::curation_effect_gate::app_config(dir.path());
    app_config.failover.store(std::sync::Arc::new(tuliprox_core::model::FailoverOverrides {
        error: Some("bad failover file".to_string()),
        ..Default::default()
    }));
    let target = target_with_options(r#"{"failover_groups":{"enabled":true}}"#);
    let mut playlist = vec![group("News", vec![live("solis", "1", "ZDF HD"), live("korath", "7", "ZDF FHD")])];
    let mut step = tuliprox_core::utils::StepMeasure::new("test", |_, _| {});
    let mut errors = Vec::new();

    let pending = crate::processor::playlist::target::process_target_failover_groups(
        &app_config,
        &target,
        &mut playlist,
        &mut step,
        &mut errors,
    );

    assert_eq!(pending.groups.len(), 1, "auto grouping keeps working");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].to_string().contains("bad failover file"), "{}", errors[0]);
}

#[tokio::test]
async fn meta_records_applied_section_and_etag() {
    let dir = tempfile::tempdir().expect("tempdir");
    let app_config = super::curation_effect_gate::app_config(dir.path());
    let target = target_with_options(r#"{"failover_groups":{"enabled":true}}"#);
    let mut step = tuliprox_core::utils::StepMeasure::new("test", |_, _| {});
    let channels = || vec![group("News", vec![live("solis", "1", "ZDF HD"), live("korath", "7", "ZDF FHD")])];

    let mut errors = Vec::new();
    let mut playlist = channels();
    let without = crate::processor::playlist::target::process_target_failover_groups(
        &app_config,
        &target,
        &mut playlist,
        &mut step,
        &mut errors,
    );
    assert!(without.enabled);
    assert_eq!(without.catalog.len(), 2);
    assert_eq!(without.meta, tuliprox_repository::FailoverGroupMeta::default());

    let snapshot = snapshot_with_excluded("T", "solis", "1");
    let entry = std::sync::Arc::clone(&snapshot.sections["T"]);
    app_config.failover.store(std::sync::Arc::new(snapshot));
    let mut playlist = channels();
    let with = crate::processor::playlist::target::process_target_failover_groups(
        &app_config,
        &target,
        &mut playlist,
        &mut step,
        &mut errors,
    );
    assert!(errors.is_empty());
    assert_eq!(with.meta.overrides_etag.as_deref(), Some(entry.etag.as_ref()));
    assert_eq!(with.meta.applied_overrides.as_ref(), Some(&entry.overrides));
    assert_eq!(with.meta.overrides_error, None);
    assert!(with.meta.display_stale_groups.is_empty(), "a publication shows the current representatives");
}

#[tokio::test]
async fn meta_records_failover_load_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let app_config = super::curation_effect_gate::app_config(dir.path());
    app_config.failover.store(std::sync::Arc::new(tuliprox_core::model::FailoverOverrides {
        error: Some("bad failover file".to_string()),
        ..Default::default()
    }));
    let target = target_with_options(r#"{"failover_groups":{"enabled":true}}"#);
    let mut playlist = vec![group("News", vec![live("solis", "1", "ZDF HD")])];
    let mut step = tuliprox_core::utils::StepMeasure::new("test", |_, _| {});
    let mut errors = Vec::new();
    let batch = crate::processor::playlist::target::process_target_failover_groups(
        &app_config,
        &target,
        &mut playlist,
        &mut step,
        &mut errors,
    );
    assert_eq!(batch.meta.overrides_error.as_deref(), Some("bad failover file"));
    assert_eq!(batch.meta.overrides_etag, None);
    assert_eq!(batch.meta.applied_overrides, None);
}

#[tokio::test]
async fn target_without_failover_groups_yields_disabled_batch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let app_config = super::curation_effect_gate::app_config(dir.path());
    let target = target_with_options("{}");
    let mut playlist = vec![group("News", vec![live("solis", "1", "ZDF HD")])];
    let mut step = tuliprox_core::utils::StepMeasure::new("test", |_, _| {});
    let mut errors = Vec::new();
    let batch = crate::processor::playlist::target::process_target_failover_groups(
        &app_config,
        &target,
        &mut playlist,
        &mut step,
        &mut errors,
    );
    assert!(!batch.enabled && batch.catalog.is_empty() && batch.groups.is_empty());
}

#[tokio::test]
async fn mapper_copies_become_group_copies_in_pipeline() {
    let target = target_with_options(r#"{"failover_groups":{"enabled":true}}"#);
    let orig = live("solis", "1", "ZDF HD");
    let mut copy = orig.clone();
    copy.header.group = "Favourites".intern();
    copy.header.uuid = shared::utils::create_alias_uuid(&orig.header.uuid, "Favourites");

    let playlist = vec![group("News", vec![orig, live("korath", "7", "ZDF FHD")]), group("Favourites", vec![copy])];
    let (finalized, pending) = run_finalize_until_persist(&target, playlist).await;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].members.len(), 2);
    let fav = finalized.iter().find(|g| g.title.as_ref() == "Favourites").expect("favourites group");
    assert_eq!(fav.channels.len(), 1);
    assert_eq!(fav.channels[0].header.failover_group_uuid, Some(pending[0].group_uuid));
    assert_eq!(fav.channels[0].header.uuid, shared::utils::create_alias_uuid(&pending[0].group_uuid, "Favourites"));
}

#[tokio::test]
async fn group_channels_follow_the_channel_sort_order() {
    let mut options: ConfigTargetOptions =
        serde_json::from_str(r#"{"failover_groups":{"enabled":true}}"#).expect("valid options");
    options.prepare().expect("prepared");
    let mut dto: ConfigTargetDto = serde_json::from_value(serde_json::json!({
        "name": "T",
        "output": [{"type": "m3u"}],
        "sort": {"rules": [{"target": "channel", "field": "caption", "order": "asc", "filter": "Group ~ \".*\""}]}
    }))
    .expect("target dto");
    dto.options = Some(options);
    dto.prepare(1, None, None).expect("prepared target");
    let target = ConfigTarget::from(&dto);
    let playlist = vec![group(
        "US",
        vec![
            live_with_chno("solis", "1", "NBC", 1),
            live_with_chno("solis", "2", "CBS", 2),
            live("solis", "3", "ABC"),
            live_with_chno("korath", "7", "CBS", 7),
            live_with_chno("korath", "8", "NBC", 8),
        ],
    )];
    let (finalized, pending) = run_finalize_until_persist(&target, playlist).await;
    assert_eq!(pending.len(), 2);
    let titles: Vec<&str> = finalized[0].channels.iter().map(|c| c.header.title.as_ref()).collect();
    assert_eq!(titles, vec!["ABC", "CBS", "NBC"]);
    let chnos: Vec<u32> = finalized[0].channels.iter().map(|c| c.header.chno).collect();
    assert_eq!(chnos, vec![1, 2, 3]);
}

#[tokio::test]
async fn group_channels_honour_sort_order_with_sequence_regex_and_provider_chno() {
    let mut options: ConfigTargetOptions =
        serde_json::from_str(r#"{"failover_groups":{"enabled":true}}"#).expect("valid options");
    options.prepare().expect("prepared");
    let mut dto: ConfigTargetDto = serde_json::from_value(serde_json::json!({
        "name": "T",
        "output": [{"type": "m3u"}],
        "sort": {"rules": [{
            "target": "channel",
            "field": "caption",
            "order": "asc",
            "sequence": ["^NBC$"],
            "filter": "Group ~ \".*\""
        }]}
    }))
    .expect("target dto");
    dto.options = Some(options);
    dto.prepare(1, None, None).expect("prepared target");
    let target = ConfigTarget::from(&dto);
    let playlist = vec![group(
        "US",
        vec![
            live_with_chno("solis", "1", "NBC", 1),
            live_with_chno("solis", "2", "CBS", 2),
            live("solis", "3", "ABC"),
            live_with_chno("korath", "7", "CBS", 7),
            live_with_chno("korath", "8", "NBC", 8),
        ],
    )];
    let (finalized, pending) = run_finalize_until_persist(&target, playlist).await;
    assert_eq!(pending.len(), 2);
    let titles: Vec<&str> = finalized[0].channels.iter().map(|c| c.header.title.as_ref()).collect();
    assert_eq!(titles, vec!["NBC", "ABC", "CBS"]);
    let chnos: Vec<u32> = finalized[0].channels.iter().map(|c| c.header.chno).collect();
    assert_eq!(chnos, vec![1, 2, 3]);
}

#[tokio::test]
async fn favourite_copy_receives_the_locked_representative_chno() {
    let target = target_with_options_and_favourites(
        r#"{"failover_groups":{"enabled":true}}"#,
        "My Favourites",
        r#"Caption ~ "ZDF.*""#,
    );
    let representative = live_with_chno("solis", "1", "ZDF HD", 42);
    let representative_uuid = representative.header.uuid;
    let playlist = vec![group("News", vec![representative, live("korath", "7", "ZDF FHD")])];

    let item_override = Arc::new(ItemOverride { chno: FieldDelta::Set(42), ..Default::default() });
    let mut locked = HashMap::new();
    locked.insert(representative_uuid, item_override);

    let (finalized, pending) = run_finalize_until_persist_with_locked(&target, playlist, locked).await;
    assert_eq!(pending.len(), 1);
    let group_uuid = pending[0].group_uuid;

    let group_channel = finalized
        .iter()
        .flat_map(|group| &group.channels)
        .find(|channel| channel.header.uuid == group_uuid)
        .expect("group channel");
    assert_eq!(group_channel.header.chno, 42);

    let favourite_copy = finalized
        .iter()
        .find(|group| group.title.as_ref() == "My Favourites")
        .expect("favourites group")
        .channels
        .iter()
        .find(|channel| channel.header.failover_group_uuid == Some(group_uuid))
        .expect("favourite copy of the group channel");
    assert_eq!(
        favourite_copy.header.chno, 42,
        "a copy of the group channel must inherit the representative's locked number"
    );
}

#[test]
fn duplicate_chno_warning_ignores_group_copies() {
    use crate::processor::playlist::target::check_duplicate_manual_chno;
    use shared::model::UUIDType;

    let group_uuid = UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000001");
    let mut group_channel = live_with_chno("solis", "1", "ZDF HD", 42);
    group_channel.header.uuid = group_uuid;
    group_channel.header.failover_group_uuid = Some(group_uuid);
    let mut copy = group_channel.clone();
    copy.header.uuid = create_alias_uuid(&group_uuid, "My Favourites");
    let playlist = vec![group("News", vec![group_channel]), group("My Favourites", vec![copy])];
    let locked =
        HashMap::from([(group_uuid, Arc::new(ItemOverride { chno: FieldDelta::Set(42), ..Default::default() }))]);

    assert_eq!(check_duplicate_manual_chno("T", &playlist, &locked), Vec::<u32>::new());
}
