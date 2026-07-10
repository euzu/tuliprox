use crate::model::{Config, ConfigTarget, TargetOutput};
use crate::model::{Epg};
use crate::repository::{m3u_get_epg_file_path_for_target, BPlusTree};
use crate::repository::{xtream_get_epg_file_path_for_target, xtream_get_storage_path, BPlusTreeQuery};
use crate::utils::{debug_if_enabled, FileLockManager};
use shared::error::{ TuliproxError};
use shared::model::{EpgChannel, PlaylistGroup};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::task;

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
pub fn epg_write_file<S: std::hash::BuildHasher>(
    target_name: &str,
    epg: &Epg,
    path: &Path,
    rename_map: &HashMap<Arc<str>, Arc<str>, S>,
) -> Result<(), TuliproxError> {
    if epg.children.is_empty() {
        return Ok(());
    }

    let mut tree = BPlusTree::<Arc<str>, EpgChannel>::new();
    for channel in &epg.children {
        if !channel.programmes.is_empty() {
            let mut chan = (**channel).clone();
            if let Some(title) = rename_map.get(&chan.id) {
                chan.title = Some(Arc::clone(title));
            }
            chan.programmes.sort_by_key(|p| p.start);
            tree.insert(Arc::clone(&channel.id), chan);
        }
    }

    tree.store(path).map_err(|err| TuliproxError::RepositoryEpg(format!("Failed to write epg for target {}: {} - {err}", target_name, path.display())))?;

    debug_if_enabled!("Epg for target {} written to {}", target_name, path.display());
    Ok(())
}

fn build_epg_rename_map(playlist: Option<&[PlaylistGroup]>) -> HashMap<Arc<str>, Arc<str>> {
    let mut rename_map = HashMap::new();
    if let Some(pl) = playlist {
        for group in pl {
            for channel in &group.channels {
                if let Some(epg_id) = &channel.header.epg_channel_id {
                    if !epg_id.is_empty() {
                        rename_map.insert(Arc::clone(epg_id), Arc::clone(&channel.header.name));
                    }
                }
            }
        }
    }
    rename_map
}

pub async fn epg_write_for_target(cfg: &Config, target: &ConfigTarget, target_path: &Path,
                                  epg: Option<&Epg>, output: &TargetOutput,
                                  playlist: Option<&[PlaylistGroup]>) -> Result<(), TuliproxError> {
    if !output.target_type().supports_epg() {
        // Formats without EPG support are skipped here via the shared capability
        // table rather than a silent empty match arm.
        return Ok(());
    }
    if let Some(epg_data) = epg {
        let rename_map = Arc::new(build_epg_rename_map(playlist));
        let epg_data = Arc::new(epg_data.clone());
        match output {
            TargetOutput::Xtream(_) => {
                match xtream_get_storage_path(cfg, &target.name) {
                    Some(path) => {
                        let epg_path = xtream_get_epg_file_path_for_target(&path);
                        debug_if_enabled!("writing xtream epg to {}", epg_path.display());
                        let target_name = target.name.clone();
                        let target_name_err = target_name.clone();
                        let rename_map = Arc::clone(&rename_map);
                        let epg_data = Arc::clone(&epg_data);
                        let epg_path = epg_path.clone();
                        tokio::task::spawn_blocking(move || {
                            epg_write_file(&target_name, &epg_data, &epg_path, &rename_map)
                        })
                        .await
                        .map_err(|err| TuliproxError::RepositoryEpg(format!(
                            "Failed to write epg for target {target_name_err}: {err}"
                        )))??;
                    }
                    None => {
                        return Err(TuliproxError::RepositoryEpg(format!(
                            "failed to write epg for target: {}, storage path not found",
                            target.name
                        )))
                    }
                }
            }
            TargetOutput::M3u(_) => {
                let path = m3u_get_epg_file_path_for_target(target_path);
                debug_if_enabled!("writing m3u epg to {}", path.display());
                let target_name = target.name.clone();
                let target_name_err = target_name.clone();
                let rename_map = Arc::clone(&rename_map);
                let epg_data = Arc::clone(&epg_data);
                let path = path.clone();
                tokio::task::spawn_blocking(move || {
                    epg_write_file(&target_name, &epg_data, &path, &rename_map)
                })
                .await
                .map_err(|err| TuliproxError::RepositoryEpg(format!(
                    "Failed to write epg for target {target_name_err}: {err}"
                )))??;
            }
            TargetOutput::Strm(_) | TargetOutput::HdHomeRun(_) => {}
        }
    }
    Ok(())
}

/// Queries EPG channels by their ids. Encapsulates lock acquisition, `BPlusTree` open, and batch lookup.
/// Returns channels in the same order as `query_ids`, with None for ids not found in the DB.
pub async fn epg_query_channels(
    file_locks: &FileLockManager,
    epg_path: &Path,
    query_ids: Vec<Arc<str>>,
) -> Result<Vec<(Arc<str>, Option<EpgChannel>)>, TuliproxError> {
    let file_lock = file_locks.read_lock(epg_path).await;
    let epg_path = epg_path.to_path_buf();

    task::spawn_blocking(move || {
        let _guard = file_lock;
        let mut query = BPlusTreeQuery::<Arc<str>, EpgChannel>::try_new(&epg_path)
            .map_err(|e| TuliproxError::RepositoryEpg(format!("failed to open epg db {}: {e}", epg_path.display())))?;

        let mut results = Vec::with_capacity(query_ids.len());
        for channel_id in &query_ids {
            let channel = query.query(channel_id)
                .map_err(|e| TuliproxError::RepositoryEpg(format!("failed to query epg db {}: {e}", epg_path.display())))?;
            results.push((Arc::clone(channel_id), channel));
        }
        Ok(results)
    })
    .await
    .map_err(|e| TuliproxError::RepositoryEpg(format!("epg query task panicked: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IcsEpgSourceConfig, M3uTargetOutput, XtreamTargetFlagsSet, XtreamTargetOutput};
    use crate::processing::parser::ics::parse_ics_file_to_channel;
    use crate::repository::BPlusTree;
    use crate::utils::FileLockManager;
    use arc_swap::ArcSwapOption;
    use shared::foundation::Filter;
    use shared::model::{EpgChannel, ProcessingOrder};
    use shared::utils::Internable;
    use tempfile::TempDir;

    fn target_with_m3u_and_xtream() -> ConfigTarget {
        ConfigTarget {
            id: 1,
            enabled: true,
            name: "ics-target".to_string(),
            options: None,
            sort: None,
            filter: Filter::default(),
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
            watch: None,
            use_memory_cache: false,
        }
    }

    #[tokio::test]
    async fn epg_query_channels_returns_found_channels_in_order() {
        let tmp = TempDir::new().expect("temp dir created");
        let path = tmp.path().join("epg.db");

        // Write two channels directly via BPlusTree
        let mut tree = BPlusTree::<Arc<str>, EpgChannel>::new();
        tree.insert("ch1".intern(), EpgChannel {
            id: "ch1".intern(),
            title: Some("Channel 1".intern()),
            icon: None,
            programmes: Vec::new(),
        });
        tree.insert("ch2".intern(), EpgChannel {
            id: "ch2".intern(),
            title: Some("Channel 2".intern()),
            icon: None,
            programmes: Vec::new(),
        });
        tree.store(&path).expect("store epg");

        let file_locks = FileLockManager::new();
        let query_ids = vec!["ch1".intern(), "ch2".intern(), "ch3".intern()];

        let results = epg_query_channels(&file_locks, &path, query_ids).await
            .expect("epg_query_channels succeeds");

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].0.as_ref(), "ch1");
        assert!(results[0].1.is_some());
        assert_eq!(results[1].0.as_ref(), "ch2");
        assert!(results[1].1.is_some());
        assert_eq!(results[2].0.as_ref(), "ch3");
        assert!(results[2].1.is_none());
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
        let epg = Epg {
            priority: 0,
            logo_override: false,
            attributes: None,
            children: vec![Arc::new(channel)],
        };

        let config = Config {
            storage_dir: tmp.path().to_string_lossy().into_owned(),
            ..Config::default()
        };
        let target = target_with_m3u_and_xtream();
        let target_path = crate::repository::get_target_storage_path(&config, &target.name)
            .expect("target storage path");
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
            let results = epg_query_channels(&locks, epg_path, vec!["f1.calendar".intern()])
                .await
                .expect("read target EPG");
            let stored = results[0].1.as_ref().expect("stored ICS channel");
            assert_eq!(stored.title.as_deref(), Some("Formula 1"));
            assert_eq!(stored.programmes.len(), 1);
            assert_eq!(stored.programmes[0].title.as_deref(), Some("Formula 1 Practice"));
        }
    }
}
