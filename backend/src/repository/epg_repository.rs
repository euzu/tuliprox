use crate::model::Epg;
use crate::model::{Config, ConfigTarget, TargetOutput};
use crate::repository::m3u_repository::m3u_get_epg_file_path;
use shared::model::PlaylistGroup;
use crate::repository::xtream_repository::{xtream_get_epg_file_path, xtream_get_storage_path};
use crate::utils::{async_file_writer, debug_if_enabled};
use shared::error::{notify_err, TuliproxError};
use std::collections::HashMap;
use std::path::Path;
use tokio::io::AsyncWriteExt;


const XML_PREAMBLE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
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
pub async fn epg_write_file(target: &ConfigTarget, epg: &Epg, path: &Path, playlist: Option<&[PlaylistGroup]>) -> Result<(), TuliproxError> {
    let file = tokio::fs::File::create(path).await
        .map_err(|e| notify_err!("failed to create epg file {}: {}", path.display(), e))?;

    // Use a larger buffer for sequential writes to reduce syscalls
    let mut buf_writer = async_file_writer(file);

    // Header/DOCTYPE workaround: write both lines in a single call for fewer syscalls
    buf_writer
        .write_all(XML_PREAMBLE.as_bytes())
        .await
        .map_err(|e| notify_err!("failed to write XML preamble {}: {}", path.display(), e))?;

    // Build a temporary rename map with zero allocations (uses references)
    let mut rename_map: HashMap<&str, &str> = HashMap::new();
    if let Some(pl) = playlist {
        for group in pl {
            for channel in &group.channels {
                if let Some(epg_id) = &channel.header.epg_channel_id {
                    if !epg_id.is_empty() {
                        rename_map.insert(epg_id.as_str(), channel.header.name.as_str());
                    }
                }
            }
        }
    }

    // EPG content streamed via quick_xml writer (compact output)
    let mut writer = quick_xml::writer::Writer::new(buf_writer);
    epg
        .write_to_async(&mut writer, if rename_map.is_empty() { None } else { Some(&rename_map) })
        .await
        .map_err(|e| notify_err!("failed to write epg {}: {}", path.display(), e))?;

    // Ensure buffers are flushed to the OS and capture any I/O error
    let mut buf_writer = writer.into_inner();
    buf_writer
        .flush()
        .await
        .map_err(|e| notify_err!("failed to flush epg {}: {}", path.display(), e))?;

    buf_writer.shutdown().await.map_err(|e| notify_err!("failed to write epg {}: {}", path.display(), e))?;

    debug_if_enabled!("Epg for target {} written to {}", target.name, path.display());
    Ok(())
}

pub async fn epg_write(cfg: &Config, target: &ConfigTarget, target_path: &Path, epg: Option<&Epg>, output: &TargetOutput, playlist: Option<&[PlaylistGroup]>) -> Result<(), TuliproxError> {
    if let Some(epg_data) = epg {
        match output {
            TargetOutput::Xtream(_) => {
                match xtream_get_storage_path(cfg, &target.name) {
                    Some(path) => {
                        let epg_path = xtream_get_epg_file_path(&path);
                        debug_if_enabled!("writing xtream epg to {}", epg_path.display());
                        epg_write_file(target, epg_data, &epg_path, playlist).await?;
                    }
                    None => return Err(notify_err!("failed to write epg for target: {}, storage path not found", target.name)),
                }
            }
            TargetOutput::M3u(_) => {
                let path = m3u_get_epg_file_path(target_path);
                debug_if_enabled!("writing m3u epg to {}", path.display());
                epg_write_file(target, epg_data, &path, playlist).await?;
            }
            TargetOutput::Strm(_) | TargetOutput::HdHomeRun(_) => {}
        }
    }
    Ok(())
}
