use crate::model::{Config, ConfigTarget, TargetOutput};
use crate::model::{Epg};
use crate::repository::{m3u_get_epg_file_path_for_target, BPlusTree};
use crate::repository::{xtream_get_epg_file_path_for_target, xtream_get_storage_path};
use crate::utils::{debug_if_enabled};
use shared::error::{notify_err, TuliproxError};
use shared::model::{EpgChannel, PlaylistGroup};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

pub const XML_PREAMBLE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE tv SYSTEM "xmltv.dtd">
"#;

// Due to a bug in quick_xml we cannot write the DOCTYPE via event; quotes are escaped and the XML becomes invalid.
// Keep the manual header/doctype write workaround below.
//
// // XML Header via events (DO NOT USE, kept for documentation):
// writer.write_event_async(quick_xml::events::Event::Decl(quick_xml::events::BytesDecl::new("1.0", Some("utf-8"), None)))
//     .await.map_err(|e| notify_err!("failed to write XML header: {}", e))?;
//
// // DOCTYPE via events (DO NOT USE):
// writer.write_event_async(quick_xml::events::Event::DocType(quick_xml::events::BytesText::new(r#"tv SYSTEM "xmltv.dtd""#)))
//     .await.map_err(|e| notify_err!("failed to write doctype: {}", e))?;
pub fn epg_write_file(target: &ConfigTarget, epg: &Epg, path: &Path, playlist: Option<&[PlaylistGroup]>) -> Result<(), TuliproxError> {
    if epg.children.is_empty() {
        return Ok(());
    }

    // If the epg titles differ from playlist, then we should use the ones from playlist
    // Build a temporary rename map with zero allocations (uses references)
    let mut rename_map: HashMap<&Arc<str>, &Arc<str>> = HashMap::new();
    if let Some(pl) = playlist {
        for group in pl {
            for channel in &group.channels {
                if let Some(epg_id) = &channel.header.epg_channel_id {
                    if !epg_id.is_empty() {
                        rename_map.insert(epg_id, &channel.header.name);
                    }
                }
            }
        }
    }

    let mut tree = BPlusTree::<Arc<str>, EpgChannel>::new();
    for channel in &epg.children {
        if !channel.programmes.is_empty() {
            let mut chan = (**channel).clone();
            if let Some(&title) = rename_map.get(&chan.id) {
                chan.title = Some(Arc::clone(title));
            }
            chan.programmes.sort_by_key(|p| p.start);
            tree.insert(Arc::clone(&channel.id), chan);
        }
    }
    drop(rename_map);

    tree.store(path).map_err(|err| notify_err!("Failed to write epg for target {}: {} - {err}", target.name, path.display()))?;

    debug_if_enabled!("Epg for target {} written to {}", target.name, path.display());
    Ok(())
}

pub async fn epg_write_for_target(cfg: &Config, target: &ConfigTarget, target_path: &Path,
                                  epg: Option<&Epg>, output: &TargetOutput,
                                  playlist: Option<&[PlaylistGroup]>) -> Result<(), TuliproxError> {
    if let Some(epg_data) = epg {
        match output {
            TargetOutput::Xtream(_) => {
                match xtream_get_storage_path(cfg, &target.name) {
                    Some(path) => {
                        let epg_path = xtream_get_epg_file_path_for_target(&path);
                        debug_if_enabled!("writing xtream epg to {}", epg_path.display());
                        epg_write_file(target, epg_data, &epg_path, playlist)?;
                    }
                    None => return Err(notify_err!("failed to write epg for target: {}, storage path not found", target.name)),
                }
            }
            TargetOutput::M3u(_) => {
                let path = m3u_get_epg_file_path_for_target(target_path);
                debug_if_enabled!("writing m3u epg to {}", path.display());
                epg_write_file(target, epg_data, &path, playlist)?;
            }
            TargetOutput::Strm(_) | TargetOutput::HdHomeRun(_) => {}
        }
    }
    Ok(())
}
