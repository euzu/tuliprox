use crate::{
    build_epg_group_index, epg_group_index_remove, epg_group_index_store,
    error_macros::{cant_open_result, cant_query_result},
    m3u_get_epg_file_path_for_target, xtream_get_epg_file_path_for_target, xtream_get_storage_path, BPlusTree,
    BPlusTreeQuery,
};
use shared::{
    error::TuliproxError,
    model::{EpgChannel, EpgOutputOptions, PlaylistGroup},
};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};
use tokio::task;
use tuliprox_core::{
    model::{Config, ConfigTarget, Epg, TargetOutput},
    utils::{canonicalize_output_epg_id, debug_if_enabled, EpgIdOutputCase, FileLockManager},
};

pub const XML_PREAMBLE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE tv SYSTEM "xmltv.dtd">
"#;

// Due to a bug in quick_xml we cannot write the DOCTYPE via event; quotes are escaped and the XML becomes invalid.
// Keep the manual header/doctype write workaround below.
//
// // XML Header via events (DO NOT USE, kept for documentation):
// writer.write_event_async(quick_xml::events::Event::Decl(quick_xml::events::BytesDecl::new("1.0", Some("utf-8"), None)))
//     .await.map_err(|e| TuliproxError::RepositoryEpg(format!("failed to write XML header: {}", e)))?;
//
// // DOCTYPE via events (DO NOT USE):
// writer.write_event_async(quick_xml::events::Event::DocType(quick_xml::events::BytesText::new(r#"tv SYSTEM "xmltv.dtd""#)))
//     .await.map_err(|e| TuliproxError::RepositoryEpg(format!("failed to write doctype: {}", e)))?;
type EpgRenameMap = HashMap<Arc<str>, Arc<str>>;
type EpgOrderMap = HashMap<Arc<str>, u64>;
type EpgMaps = (EpgRenameMap, EpgOrderMap);

/// Keeps the source ordinal as the primary order while traversal order makes equal or synthesized ordinals unique.
fn epg_order_rank(source_ordinal: u32, traversal_ordinal: u32) -> u64 {
    let ordinal = if source_ordinal > 0 { source_ordinal } else { traversal_ordinal };
    (u64::from(ordinal) << u32::BITS) | u64::from(traversal_ordinal)
}

/// A channel is written to the target EPG db only when it has programmes.
fn is_written_epg_channel(channel: &EpgChannel) -> bool { !channel.programmes.is_empty() }

/// Canonical keys `epg_write_file` writes for `epg`, or `None` when it writes nothing.
fn epg_written_keys(epg: &Epg, output_case: EpgIdOutputCase) -> Option<HashSet<Arc<str>>> {
    if epg.children.is_empty() {
        return None;
    }
    Some(
        epg.children
            .iter()
            .filter(|channel| is_written_epg_channel(channel))
            .map(|channel| canonicalize_output_epg_id(&channel.id, output_case))
            .collect(),
    )
}

pub fn epg_write_file<S: std::hash::BuildHasher>(
    target_name: &str,
    epg: &Epg,
    path: &Path,
    rename_map: &HashMap<Arc<str>, Arc<str>, S>,
    order_map: Option<&HashMap<Arc<str>, u64, S>>,
    epg_output: &EpgOutputOptions,
) -> Result<(), TuliproxError> {
    if epg.children.is_empty() {
        return Ok(());
    }

    let mut tree = BPlusTree::<Arc<str>, EpgChannel>::new();
    let output_case = EpgIdOutputCase::from_lowercase(epg_output.lowercase_ids);
    for channel in &epg.children {
        if !is_written_epg_channel(channel) {
            continue;
        }

        let mut chan = (**channel).clone();
        let output_id = canonicalize_output_epg_id(&chan.id, output_case);
        if let Some(title) = rename_map.get(&output_id) {
            chan.title = Some(Arc::clone(title));
        }

        chan.id = Arc::clone(&output_id);
        chan.programmes.sort_by_key(|programme| programme.start);
        tree.insert(output_id, chan);
    }

    let result = if let Some(order) = order_map {
        tree.store_with_index(path, |chan| order.get(&chan.id).copied().unwrap_or(u64::MAX))
    } else {
        tree.store(path)
    };

    result.map_err(|err| {
        TuliproxError::RepositoryEpg(format!(
            "Failed to write epg for target {}: {} - {err}",
            target_name,
            path.display()
        ))
    })?;

    debug_if_enabled!("Epg for target {} written to {}", target_name, path.display());
    Ok(())
}

fn build_epg_maps(playlist: Option<&[PlaylistGroup]>, output_case: EpgIdOutputCase) -> EpgMaps {
    let estimated_len = playlist.map_or(0, |groups| groups.iter().map(|g| g.channels.len()).sum());
    let mut rename_map = HashMap::with_capacity(estimated_len);
    let mut order_map = HashMap::with_capacity(estimated_len);
    let mut sequential_ordinal = 0u32;
    if let Some(pl) = playlist {
        for group in pl {
            for channel in &group.channels {
                sequential_ordinal += 1;
                let rank = epg_order_rank(channel.header.source_ordinal, sequential_ordinal);
                if let Some(epg_id) = &channel.header.epg_channel_id {
                    if !epg_id.is_empty() {
                        let output_id = canonicalize_output_epg_id(epg_id, output_case);
                        match output_case {
                            // Preserve the legacy exact-key last-match behavior when the feature is disabled.
                            EpgIdOutputCase::Preserve => {
                                rename_map.insert(Arc::clone(&output_id), Arc::clone(&channel.header.name));
                            }
                            // Canonical collisions use playlist traversal order: the first entry wins.
                            EpgIdOutputCase::LowercaseAscii => {
                                rename_map
                                    .entry(Arc::clone(&output_id))
                                    .or_insert_with(|| Arc::clone(&channel.header.name));
                            }
                        }
                        order_map
                            .entry(output_id)
                            .and_modify(|current: &mut u64| *current = (*current).min(rank))
                            .or_insert(rank);
                    }
                }
            }
        }
    }
    (rename_map, order_map)
}

#[cfg(test)]
fn build_epg_rename_map(
    playlist: Option<&[PlaylistGroup]>,
    output_case: EpgIdOutputCase,
) -> HashMap<Arc<str>, Arc<str>> {
    build_epg_maps(playlist, output_case).0
}

pub async fn epg_write_for_target(
    cfg: &Config,
    target: &ConfigTarget,
    target_path: &Path,
    epg: Option<&Epg>,
    output: &TargetOutput,
    playlist: Option<&[PlaylistGroup]>,
) -> Result<(), TuliproxError> {
    if !output.target_type().supports_epg() {
        // Formats without EPG support are skipped here via the shared capability
        // table rather than a silent empty match arm.
        return Ok(());
    }
    if let Some(epg_data) = epg {
        let epg_output =
            target.options.as_ref().map_or_else(EpgOutputOptions::default, |options| options.epg_output.clone());
        let output_case = EpgIdOutputCase::from_lowercase(epg_output.lowercase_ids);
        let (rename_map, order_map) = build_epg_maps(playlist, output_case);
        let epg_path = match output {
            TargetOutput::Xtream(_) => match xtream_get_storage_path(cfg, &target.name) {
                Some(path) => xtream_get_epg_file_path_for_target(&path),
                None => {
                    return Err(TuliproxError::RepositoryEpg(format!(
                        "failed to write epg for target: {}, storage path not found",
                        target.name
                    )))
                }
            },
            TargetOutput::M3u(_) => m3u_get_epg_file_path_for_target(target_path),
            TargetOutput::Strm(_) | TargetOutput::HdHomeRun(_) => return Ok(()),
        };
        debug_if_enabled!("writing {} epg to {}", output.target_type(), epg_path.display());
        // The group index is small and built from the borrowed playlist here, so the playlist is
        // not cloned into the blocking task. It is only written together with the EPG db.
        let group_index = playlist
            .zip(epg_written_keys(epg_data, output_case))
            .map(|(playlist, keys)| build_epg_group_index(playlist, &keys, output_case));
        let target_name = target.name.clone();
        let target_name_err = target_name.clone();
        let epg_data = epg_data.clone();
        tokio::task::spawn_blocking(move || {
            // The old index goes before the EPG db is replaced and the new one is published after
            // it, so a failure at any step leaves no index rather than one of another EPG db.
            // Readers treat a missing index as "not built yet". An empty EPG keeps the old db.
            if !epg_data.children.is_empty() {
                epg_group_index_remove(&epg_path)?;
            }
            epg_write_file(&target_name, &epg_data, &epg_path, &rename_map, Some(&order_map), &epg_output)?;
            if let Some(group_index) = group_index {
                epg_group_index_store(group_index, &epg_path)?;
            }
            Ok::<_, TuliproxError>(())
        })
        .await
        .map_err(|err| {
            TuliproxError::RepositoryEpg(format!("Failed to write epg for target {target_name_err}: {err}"))
        })??;
    }
    Ok(())
}

/// Queries EPG channels by exact target-storage keys.
///
/// Callers must prepare each key according to the target's configured output case. Results preserve
/// the order of `storage_keys` and contain `None` for keys that are not present in the database.
pub async fn epg_query_channels_by_storage_key(
    file_locks: &FileLockManager,
    epg_path: &Path,
    storage_keys: Vec<Arc<str>>,
) -> Result<Vec<(Arc<str>, Option<EpgChannel>)>, TuliproxError> {
    let file_lock = file_locks.read_lock(epg_path).await;
    let epg_path = epg_path.to_path_buf();

    task::spawn_blocking(move || {
        let _guard = file_lock;
        let mut query = BPlusTreeQuery::<Arc<str>, EpgChannel>::try_new(&epg_path)
            .map_err(|e| cant_open_result!(RepositoryEpg, "epg", &epg_path, e))?;

        let mut results = Vec::with_capacity(storage_keys.len());
        for storage_key in &storage_keys {
            let channel =
                query.query(storage_key).map_err(|e| cant_query_result!(RepositoryEpg, "epg", &epg_path, e))?;
            results.push((Arc::clone(storage_key), channel));
        }
        Ok(results)
    })
    .await
    .map_err(|e| TuliproxError::RepositoryEpg(format!("epg query task panicked: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BPlusTree;
    use arc_swap::ArcSwapOption;
    use shared::{
        model::{
            EpgCategory, EpgChannel, EpgProgramme, PlaylistItem, PlaylistItemHeader, ProcessingOrder, XtreamCluster,
        },
        utils::Internable,
    };
    use tempfile::TempDir;
    use tuliprox_core::{
        model::{IcsEpgSourceConfig, M3uTargetOutput, XtreamTargetFlagsSet, XtreamTargetOutput},
        utils::FileLockManager,
    };
    use tuliprox_parser::ics::parse_ics_file_to_channel;

    fn target_with_m3u_and_xtream() -> ConfigTarget {
        ConfigTarget {
            curation: None,
            id: 1,
            enabled: true,
            name: "ics-target".to_string(),
            options: None,
            sort: None,
            filter: tuliprox_core::model::StagedFilter::default(),
            output: vec![
                TargetOutput::M3u(M3uTargetOutput {
                    filename: None,
                    include_type_in_url: false,
                    mask_redirect_url: false,
                    filter: None,
                }),
                TargetOutput::Xtream(XtreamTargetOutput {
                    flags: XtreamTargetFlagsSet::new(),
                    trakt: None,
                    filter: None,
                }),
            ],
            rename: None,
            mapping_ids: None,
            mapping: Arc::new(ArcSwapOption::new(None)),
            favourites: None,
            processing_order: ProcessingOrder::default(),
            execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
            watch: None,
            use_memory_cache: false,
        }
    }

    #[tokio::test]
    async fn epg_query_channels_by_storage_key_returns_found_channels_in_order() {
        let tmp = TempDir::new().expect("temp dir created");
        let path = tmp.path().join("epg.db");

        // Write two channels directly via BPlusTree
        let mut tree = BPlusTree::<Arc<str>, EpgChannel>::new();
        tree.insert(
            "ch1".intern(),
            EpgChannel { id: "ch1".intern(), title: Some("Channel 1".intern()), icon: None, programmes: Vec::new() },
        );
        tree.insert(
            "ch2".intern(),
            EpgChannel { id: "ch2".intern(), title: Some("Channel 2".intern()), icon: None, programmes: Vec::new() },
        );
        tree.store(&path).expect("store epg");

        let file_locks = FileLockManager::new();
        let storage_keys = vec!["ch1".intern(), "ch2".intern(), "ch3".intern()];

        let results = epg_query_channels_by_storage_key(&file_locks, &path, storage_keys)
            .await
            .expect("EPG storage-key query succeeds");

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].0.as_ref(), "ch1");
        assert!(results[0].1.is_some());
        assert_eq!(results[1].0.as_ref(), "ch2");
        assert!(results[1].1.is_some());
        assert_eq!(results[2].0.as_ref(), "ch3");
        assert!(results[2].1.is_none());
    }

    fn epg_with_channels(ids: &[&str]) -> Epg {
        Epg {
            priority: 0,
            logo_override: false,
            attributes: None,
            children: ids
                .iter()
                .map(|id| {
                    Arc::new(EpgChannel {
                        id: id.intern(),
                        title: None,
                        icon: None,
                        programmes: vec![EpgProgramme::new(0, 60, id.intern())],
                    })
                })
                .collect(),
        }
    }

    fn live_group(title: &str, channels: &[(u32, &str)]) -> PlaylistGroup {
        PlaylistGroup {
            id: 1,
            title: title.intern(),
            channels: channels
                .iter()
                .map(|(virtual_id, epg_id)| PlaylistItem {
                    header: PlaylistItemHeader {
                        virtual_id: shared::model::VirtualId::new(*virtual_id),
                        name: format!("ch{virtual_id}").intern(),
                        epg_channel_id: Some(epg_id.intern()),
                        xtream_cluster: XtreamCluster::Live,
                        ..PlaylistItemHeader::default()
                    },
                })
                .collect(),
            xtream_cluster: XtreamCluster::Live,
        }
    }

    async fn write_target_epg(
        tmp: &TempDir,
        playlist: Option<&[PlaylistGroup]>,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let config = Config { storage_dir: tmp.path().to_string_lossy().into_owned(), ..Config::default() };
        let target = target_with_m3u_and_xtream();
        let target_path = crate::get_target_storage_path(&config, &target.name).expect("target storage path");
        let m3u_path = m3u_get_epg_file_path_for_target(&target_path);
        let xtream_storage = xtream_get_storage_path(&config, &target.name).expect("xtream storage path");
        let xtream_path = xtream_get_epg_file_path_for_target(&xtream_storage);
        std::fs::create_dir_all(m3u_path.parent().expect("m3u parent")).expect("create m3u storage");
        std::fs::create_dir_all(xtream_path.parent().expect("xtream parent")).expect("create xtream storage");
        let epg = epg_with_channels(&["a", "b"]);
        for output in &target.output {
            epg_write_for_target(&config, &target, &target_path, Some(&epg), output, playlist)
                .await
                .expect("write target EPG");
        }
        (m3u_path, xtream_path)
    }

    #[tokio::test]
    async fn target_epg_write_creates_group_index_next_to_each_epg_db() {
        let tmp = TempDir::new().expect("temp dir created");
        let playlist = vec![live_group("Sport", &[(7, "b"), (8, "missing")]), live_group("News", &[(3, "a")])];
        let (m3u_path, xtream_path) = write_target_epg(&tmp, Some(&playlist)).await;

        for epg_path in [&m3u_path, &xtream_path] {
            let mut groups = BPlusTreeQuery::<u32, crate::EpgGroupEntry>::try_new(&crate::epg_groups_path(epg_path))
                .expect("groups index written");
            let names: Vec<_> = groups
                .iter()
                .map(|entry| entry.map(|(_, group)| (group.name.to_string(), group.channel_count)))
                .collect::<std::io::Result<_>>()
                .expect("groups readable");
            assert_eq!(names, vec![("Sport".to_string(), 1), ("News".to_string(), 1)]);
            assert!(crate::epg_group_channels_path(epg_path).exists());
        }
    }

    #[tokio::test]
    async fn target_epg_write_without_playlist_writes_no_group_index() {
        let tmp = TempDir::new().expect("temp dir created");
        let (m3u_path, xtream_path) = write_target_epg(&tmp, None).await;
        for epg_path in [&m3u_path, &xtream_path] {
            assert!(epg_path.exists());
            assert!(!crate::epg_groups_path(epg_path).exists());
            assert!(!crate::epg_group_channels_path(epg_path).exists());
        }
    }

    #[tokio::test]
    async fn target_epg_rewrite_without_playlist_removes_stale_group_index() {
        let tmp = TempDir::new().expect("temp dir created");
        let playlist = vec![live_group("News", &[(3, "a")])];
        write_target_epg(&tmp, Some(&playlist)).await;
        let (m3u_path, xtream_path) = write_target_epg(&tmp, None).await;
        for epg_path in [&m3u_path, &xtream_path] {
            assert!(epg_path.exists());
            assert!(!crate::epg_groups_path(epg_path).exists());
            assert!(!crate::epg_group_channels_path(epg_path).exists());
        }
    }

    #[tokio::test]
    async fn target_epg_write_keeps_old_db_when_stale_group_index_cannot_be_removed() {
        let tmp = TempDir::new().expect("temp dir created");
        let config = Config { storage_dir: tmp.path().to_string_lossy().into_owned(), ..Config::default() };
        let target = target_with_m3u_and_xtream();
        let target_path = crate::get_target_storage_path(&config, &target.name).expect("target storage path");
        let epg_path = m3u_get_epg_file_path_for_target(&target_path);
        // A directory cannot be removed as a file, so invalidating the old index fails.
        std::fs::create_dir_all(crate::epg_groups_path(&epg_path)).expect("create blocking groups path");
        let output = target.output.iter().find(|output| matches!(output, TargetOutput::M3u(_))).expect("m3u output");
        let playlist = vec![live_group("News", &[(3, "a")])];

        let result = epg_write_for_target(
            &config,
            &target,
            &target_path,
            Some(&epg_with_channels(&["a"])),
            output,
            Some(&playlist),
        )
        .await;

        assert!(result.is_err());
        assert!(!epg_path.exists());
        assert!(!crate::epg_group_channels_path(&epg_path).exists());
    }

    #[tokio::test]
    async fn imported_ics_epg_uses_shared_m3u_and_xtream_target_write_read_path() {
        let tmp = TempDir::new().expect("temp dir created");
        let ics_path = tmp.path().join("calendar.ics");
        std::fs::write(
            &ics_path,
            concat!(
                "BEGIN:VCALENDAR\r\n",
                "VERSION:2.0\r\n",
                "BEGIN:VEVENT\r\n",
                "UID:f1-session\r\n",
                "DTSTART:20300101T120000Z\r\n",
                "DTEND:20300101T130000Z\r\n",
                "SUMMARY:Formula 1 Practice\r\n",
                "DESCRIPTION:Imported from ICS\r\n",
                "CATEGORIES:Motorsport,Practice\r\n",
                "END:VEVENT\r\n",
                "END:VCALENDAR\r\n",
            ),
        )
        .expect("write ICS fixture");

        let channel = parse_ics_file_to_channel(
            &ics_path,
            "f1.calendar".intern(),
            Some("Formula 1".intern()),
            &IcsEpgSourceConfig::default(),
        )
        .await
        .expect("parse ICS fixture");
        let epg = Epg { priority: 0, logo_override: false, attributes: None, children: vec![Arc::new(channel)] };

        let config = Config { storage_dir: tmp.path().to_string_lossy().into_owned(), ..Config::default() };
        let target = target_with_m3u_and_xtream();
        let target_path = crate::get_target_storage_path(&config, &target.name).expect("target storage path");
        let m3u_path = m3u_get_epg_file_path_for_target(&target_path);
        let xtream_storage = xtream_get_storage_path(&config, &target.name).expect("xtream storage path");
        let xtream_path = xtream_get_epg_file_path_for_target(&xtream_storage);
        std::fs::create_dir_all(m3u_path.parent().expect("m3u parent")).expect("create m3u storage");
        std::fs::create_dir_all(xtream_path.parent().expect("xtream parent")).expect("create xtream storage");

        for output in &target.output {
            epg_write_for_target(&config, &target, &target_path, Some(&epg), output, None)
                .await
                .expect("write target EPG");
        }

        let locks = FileLockManager::new();
        for epg_path in [&m3u_path, &xtream_path] {
            let results = epg_query_channels_by_storage_key(&locks, epg_path, vec!["f1.calendar".intern()])
                .await
                .expect("read target EPG");
            let stored = results[0].1.as_ref().expect("stored ICS channel");
            assert_eq!(stored.title.as_deref(), Some("Formula 1"));
            assert_eq!(stored.programmes.len(), 1);
            assert_eq!(stored.programmes[0].title.as_deref(), Some("Formula 1 Practice"));
            assert_eq!(
                stored.programmes[0].categories,
                vec![
                    EpgCategory { value: "Motorsport".intern(), lang: None },
                    EpgCategory { value: "Practice".intern(), lang: None },
                ],
            );
            assert!(!stored.programmes[0].is_live);
            assert!(!stored.programmes[0].is_new);
        }
    }

    fn mixed_case_epg() -> Epg {
        Epg {
            priority: 0,
            logo_override: false,
            attributes: None,
            children: vec![Arc::new(EpgChannel {
                id: "Example.Channel".intern(),
                title: Some("Guide Name".intern()),
                icon: None,
                programmes: vec![
                    EpgProgramme::new(20, 30, "Example.Channel".intern()),
                    EpgProgramme::new(10, 20, "Example.Channel".intern()),
                ],
            })],
        }
    }

    fn rename_playlist(channels: &[(&str, &str)]) -> Vec<PlaylistGroup> {
        vec![PlaylistGroup {
            id: 1,
            title: "Live".intern(),
            channels: channels
                .iter()
                .map(|(epg_id, name)| PlaylistItem {
                    header: PlaylistItemHeader {
                        name: name.intern(),
                        epg_channel_id: Some(epg_id.intern()),
                        xtream_cluster: XtreamCluster::Live,
                        ..PlaylistItemHeader::default()
                    },
                })
                .collect(),
            xtream_cluster: XtreamCluster::Live,
        }]
    }

    #[test]
    fn epg_write_file_lowercases_visible_id_and_renames_with_canonical_key() {
        let tmp = TempDir::new().expect("temp dir created");
        let path = tmp.path().join("epg.db");
        let playlist = rename_playlist(&[("example.CHANNEL", "Mapped Name")]);
        let rename_map = build_epg_rename_map(Some(&playlist), EpgIdOutputCase::LowercaseAscii);
        let options = EpgOutputOptions { lowercase_ids: true, ..EpgOutputOptions::default() };

        epg_write_file("target", &mixed_case_epg(), &path, &rename_map, None, &options)
            .expect("canonical EPG should be written");

        let mut query = BPlusTreeQuery::<Arc<str>, EpgChannel>::try_new(&path).expect("EPG DB should open");
        let channel = query
            .query(&"example.channel".intern())
            .expect("canonical EPG key should be queryable")
            .expect("canonical EPG channel should exist");
        assert_eq!(channel.id.as_ref(), "example.channel");
        assert_eq!(channel.title.as_deref(), Some("Mapped Name"));
        assert_eq!(channel.programmes.iter().map(|programme| programme.start).collect::<Vec<_>>(), vec![10, 20]);
        assert!(query.query(&"Example.Channel".intern()).expect("mixed-case EPG key query should succeed").is_none());
    }

    #[test]
    fn epg_write_file_preserves_guide_case_and_exact_rename_behavior_when_disabled() {
        let tmp = TempDir::new().expect("temp dir created");
        let path = tmp.path().join("epg.db");
        let playlist = rename_playlist(&[("example.channel", "Mapped Name")]);
        let rename_map = build_epg_rename_map(Some(&playlist), EpgIdOutputCase::Preserve);

        epg_write_file("target", &mixed_case_epg(), &path, &rename_map, None, &EpgOutputOptions::default())
            .expect("case-preserving EPG should be written");

        let mut query = BPlusTreeQuery::<Arc<str>, EpgChannel>::try_new(&path).expect("EPG DB should open");
        let channel = query
            .query(&"Example.Channel".intern())
            .expect("source-case EPG key should be queryable")
            .expect("guide-case EPG channel should exist");
        assert_eq!(channel.id.as_ref(), "Example.Channel");
        assert_eq!(channel.title.as_deref(), Some("Guide Name"));
        assert!(query.query(&"example.channel".intern()).expect("lowercase EPG key query should succeed").is_none());
    }

    #[test]
    fn epg_write_file_keeps_same_case_rename_behavior_when_disabled() {
        let tmp = TempDir::new().expect("temp dir created");
        let path = tmp.path().join("epg.db");
        let playlist = rename_playlist(&[("Example.Channel", "Mapped Name")]);
        let rename_map = build_epg_rename_map(Some(&playlist), EpgIdOutputCase::Preserve);

        epg_write_file("target", &mixed_case_epg(), &path, &rename_map, None, &EpgOutputOptions::default())
            .expect("case-preserving EPG should be written");

        let mut query = BPlusTreeQuery::<Arc<str>, EpgChannel>::try_new(&path).expect("EPG DB should open");
        let channel = query
            .query(&"Example.Channel".intern())
            .expect("source-case EPG key should be queryable")
            .expect("EPG channel should exist");
        assert_eq!(channel.id.as_ref(), "Example.Channel");
        assert_eq!(channel.title.as_deref(), Some("Mapped Name"));
    }

    #[test]
    fn epg_write_file_preserves_legacy_key_order_when_disabled() {
        let tmp = TempDir::new().expect("temp dir created");
        let path = tmp.path().join("epg.db");
        let epg = Epg {
            priority: 0,
            logo_override: false,
            attributes: None,
            children: vec![
                Arc::new(EpgChannel {
                    id: "Z.Channel".intern(),
                    title: Some("First Network".intern()),
                    icon: None,
                    programmes: vec![EpgProgramme::new(10, 20, "Z.Channel".intern())],
                }),
                Arc::new(EpgChannel {
                    id: "a.channel".intern(),
                    title: Some("Second Network".intern()),
                    icon: None,
                    programmes: vec![EpgProgramme::new(10, 20, "a.channel".intern())],
                }),
            ],
        };

        epg_write_file("target", &epg, &path, &HashMap::new(), None, &EpgOutputOptions::default())
            .expect("case-preserving EPG should be written");

        let mut query = BPlusTreeQuery::<Arc<str>, EpgChannel>::try_new(&path).expect("EPG DB should open");
        let stored_ids = query
            .iter()
            .collect::<std::io::Result<Vec<_>>>()
            .expect("EPG entries should be readable")
            .into_iter()
            .map(|(_, channel)| channel.id)
            .collect::<Vec<_>>();

        assert_eq!(stored_ids.iter().map(AsRef::as_ref).collect::<Vec<_>>(), vec!["Z.Channel", "a.channel"]);
    }

    #[test]
    fn epg_write_file_resolves_ordinal_collisions_and_places_unreferenced_channels_last() {
        use crate::{get_file_path_for_db_index, open_playlist_reader};

        let tmp = TempDir::new().expect("temp dir created");
        let path = tmp.path().join("epg.db");
        let epg = Epg {
            priority: 0,
            logo_override: false,
            attributes: None,
            children: vec![
                Arc::new(EpgChannel {
                    id: "Z.Channel".intern(),
                    title: Some("Z Channel".intern()),
                    icon: None,
                    programmes: vec![EpgProgramme::new(10, 20, "Z.Channel".intern())],
                }),
                Arc::new(EpgChannel {
                    id: "a.channel".intern(),
                    title: Some("A Channel".intern()),
                    icon: None,
                    programmes: vec![EpgProgramme::new(10, 20, "a.channel".intern())],
                }),
                Arc::new(EpgChannel {
                    id: "unreferenced.channel".intern(),
                    title: Some("Unreferenced Channel".intern()),
                    icon: None,
                    programmes: vec![EpgProgramme::new(10, 20, "unreferenced.channel".intern())],
                }),
            ],
        };

        let mut playlist = rename_playlist(&[
            ("Z.Channel", "Z Channel"),
            ("a.channel", "A Channel"),
            ("a.channel", "Duplicate A Channel"),
        ]);
        for (channel, ordinal) in playlist[0].channels.iter_mut().zip([0, 1, 3]) {
            channel.header.source_ordinal = ordinal;
        }
        let (rename_map, order_map) = build_epg_maps(Some(&playlist), EpgIdOutputCase::Preserve);

        assert_eq!(order_map.get("Z.Channel"), Some(&epg_order_rank(0, 1)));
        assert_eq!(order_map.get("a.channel"), Some(&epg_order_rank(1, 2)));
        assert_ne!(order_map.get("Z.Channel"), order_map.get("a.channel"));
        assert!(!order_map.contains_key("unreferenced.channel"));

        epg_write_file("target", &epg, &path, &rename_map, Some(&order_map), &EpgOutputOptions::default())
            .expect("EPG with index should be written");

        let index_path = get_file_path_for_db_index(&path);
        assert!(index_path.exists(), "epg.db.index sidecar file must exist");

        let reader = open_playlist_reader::<Arc<str>, EpgChannel, u64>(&path, &index_path, None)
            .expect("open_playlist_reader should open sorted index");
        let stored_ids = reader
            .collect::<std::io::Result<Vec<_>>>()
            .expect("EPG entries should be readable in sorted order")
            .into_iter()
            .map(|(_, channel)| channel.id)
            .collect::<Vec<_>>();

        assert_eq!(
            stored_ids.iter().map(AsRef::as_ref).collect::<Vec<_>>(),
            vec!["Z.Channel", "a.channel", "unreferenced.channel"],
            "Channels should follow playlist order with unreferenced EPG channels last"
        );
    }

    #[test]
    fn canonical_rename_collision_uses_first_playlist_entry() {
        let playlist = rename_playlist(&[("Example.Channel", "First Name"), ("example.channel", "Second Name")]);

        let rename_map = build_epg_rename_map(Some(&playlist), EpgIdOutputCase::LowercaseAscii);

        assert_eq!(rename_map.get("example.channel").map(AsRef::as_ref), Some("First Name"));
    }
}
