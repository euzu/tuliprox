//! Resolving a recording request back to the configured target and input it
//! names, and encoding that pair as a source descriptor.
//!
//! Config resolution with no I/O of its own. It lived beside the playlist
//! endpoint that first needed it, which put the recording subsystem in the
//! position of calling into `endpoints`; it belongs here, and the endpoint now
//! reads it from this module.

use futures::StreamExt;
use shared::model::{TargetType, VirtualId, XtreamCluster};
use std::sync::Arc;
use tuliprox_core::model::{AppConfig, ConfigInput, ConfigTarget, SourcesConfig};
use tuliprox_repository::{iter_raw_m3u_target_playlist, iter_raw_xtream_target_playlist};
use url::Url;

pub fn build_recording_source_descriptor(
    target_name: &str,
    input_name: &str,
    virtual_id: u32,
    cluster: XtreamCluster,
) -> Option<String> {
    let mut url = Url::parse("tuliprox-recording://source").ok()?;
    url.query_pairs_mut()
        .append_pair("target_name", target_name)
        .append_pair("input_name", input_name)
        .append_pair("virtual_id", &virtual_id.to_string())
        .append_pair("cluster", cluster.as_stream_type());
    Some(url.into())
}

#[derive(Debug, Clone)]
pub struct ResolvedRecordingConfig {
    pub target: Arc<ConfigTarget>,
    pub input: Arc<ConfigInput>,
}

pub fn resolve_recording_config(
    sources: &SourcesConfig,
    target_name: &str,
    input_name: &str,
) -> Option<ResolvedRecordingConfig> {
    let source = sources.sources.iter().find(|source| {
        source.inputs.iter().any(|configured_input| configured_input.as_ref() == input_name)
            && source.targets.iter().any(|target| target.name == target_name)
    })?;
    let target = source.targets.iter().find(|target| target.name == target_name)?.clone();
    let input = sources.inputs.iter().find(|input| input.name.as_ref() == input_name)?.clone();
    Some(ResolvedRecordingConfig { target, input })
}

pub fn resolve_recording_target(
    app_config: &AppConfig,
    target_name: &str,
    input_name: &str,
) -> Option<Arc<ConfigTarget>> {
    resolve_recording_config(app_config.sources.load().as_ref(), target_name, input_name)
        .map(|resolved| resolved.target)
}

/// Playlist group of a live channel in the target playlist.
///
/// Reads the persisted target playlist, so it costs one pass over the live
/// items. Only used when the caller has not resolved the group already.
pub async fn resolve_live_group(
    app_config: &AppConfig,
    target_name: &str,
    input_name: &str,
    virtual_id: &str,
) -> Option<String> {
    let wanted = VirtualId::new(virtual_id.parse::<u32>().ok()?);
    let target = resolve_recording_target(app_config, target_name, input_name)?;
    if target.has_output(TargetType::Xtream) {
        if let Some(mut items) = iter_raw_xtream_target_playlist(app_config, &target, XtreamCluster::Live).await {
            while let Some(entry) = items.next().await {
                match entry {
                    Ok(item) if item.virtual_id == wanted => return non_blank_group(&item.group),
                    _ => {}
                }
            }
        }
    }
    if target.has_output(TargetType::M3u) {
        if let Some(mut items) = iter_raw_m3u_target_playlist(app_config, &target, Some(XtreamCluster::Live)).await {
            while let Some(entry) = items.next().await {
                match entry {
                    Ok(item) if item.virtual_id == wanted => return non_blank_group(&item.group),
                    _ => {}
                }
            }
        }
    }
    None
}

fn non_blank_group(group: &str) -> Option<String> {
    let group = group.trim();
    (!group.is_empty()).then(|| group.to_string())
}
