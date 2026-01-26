use crate::model::{Config, ConfigTarget, TargetOutput, XmlTagIcon};
use crate::model::{Epg, EPG_ATTRIB_CHANNEL, EPG_ATTRIB_ID, EPG_TAG_CHANNEL, EPG_TAG_DISPLAY_NAME, EPG_TAG_ICON, EPG_TAG_PROGRAMME};
use crate::repository::{m3u_get_epg_file_path_for_target, BPlusTree};
use crate::repository::{xtream_get_epg_file_path_for_target, xtream_get_storage_path};
use crate::utils::{debug_if_enabled, parse_xmltv_time};
use shared::error::{notify_err, TuliproxError};
use shared::model::{EpgChannel, EpgProgramme, PlaylistGroup};
use shared::utils::Internable;
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
    let tag_channel = EPG_TAG_CHANNEL.intern();
    let tag_programme = EPG_TAG_PROGRAMME.intern();
    let tag_display_name = EPG_TAG_DISPLAY_NAME.intern();
    let tag_icon = EPG_TAG_ICON.intern();
    let tag_title = "title".intern();
    let tag_desc = "desc".intern();
    let epg_id_attrib = EPG_ATTRIB_ID.intern();
    let channel_id_attrib = EPG_ATTRIB_CHANNEL.intern();
    let start_attrib = "start".intern();
    let stop_attrib = "stop".intern();

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

    let mut channels: HashMap<Arc<str>, EpgChannel> =
        epg.children
            .iter()
            .filter(|tag| tag.name == tag_channel)
            .filter_map(|tag| {
                let channel_id = tag.get_attribute_value(&epg_id_attrib)?;
                let mut title = rename_map.get(channel_id).map(|v| Arc::clone(v));
                let mut icon = match tag.icon {
                    XmlTagIcon::Src(ref url) => Some(Arc::clone(url)),
                    XmlTagIcon::Undefined | XmlTagIcon::Exists => None,
                };
                if let Some(children) = tag.children.as_ref() {
                    for child in children {
                        if child.name == tag_display_name {
                            if title.is_none() {
                                title.clone_from(&child.value);
                            }
                        } else if icon.is_none() && child.name == tag_icon {
                            icon.clone_from(&child.value);
                        }
                    }
                }
                let channel = EpgChannel {
                    id: Arc::clone(channel_id),
                    title,
                    icon,
                    programmes: vec![],
                };

                Some((Arc::clone(channel_id), channel))
            })
            .collect();
    drop(rename_map);

    epg.children.iter().filter(|tag| tag.name == tag_programme).for_each(|tag| {
        if let Some(attribs) = tag.attributes.as_ref() {
            let opt_channel_id = attribs.get(&channel_id_attrib);
            let opt_start = attribs.get(&start_attrib);
            let opt_stop = attribs.get(&stop_attrib);
            if let (Some(channel_id), Some(start), Some(stop)) = (opt_channel_id, opt_start, opt_stop) {
                if let (Some(start_time), Some(stop_time)) = (parse_xmltv_time(start), parse_xmltv_time(stop)) {
                    if let Some(channel) = channels.get_mut(channel_id) {
                        let mut title = None;
                        let mut desc = None;
                        if let Some(children) = tag.children.as_ref() {
                            for child in children {
                                if child.name == tag_title {
                                    title.clone_from(&child.value);
                                } else if child.name == tag_desc {
                                    desc.clone_from(&child.value);
                                }
                            }
                            channel.programmes.push(EpgProgramme::new_all(start_time, stop_time, Arc::clone(channel_id), title, desc));
                        }
                    }
                }
            }
        }
    });

    let mut tree = BPlusTree::<Arc<str>, EpgChannel>::new();
    for (key, mut channel) in channels {
        channel.programmes.sort_by_key(|p| p.start);
        tree.insert(key, channel);
    }
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
