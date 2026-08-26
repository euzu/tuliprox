use crate::model::{AppConfig, ConfigTarget, ProxyUserCredentials};
use crate::repository::{iter_raw_m3u_target_playlist, iter_raw_xtream_target_playlist};
use shared::model::{PlaylistItem, TargetType, XtreamCluster};
use std::collections::HashSet;
use tokio_stream::StreamExt;

/// Category ids that contain at least one item visible to the user's content
/// filter. `None` when the user has no filter or the playlist is unavailable
/// (callers must not thin in that case).
pub(in crate::api) async fn collect_visible_category_ids(
    app_config: &AppConfig,
    target: &ConfigTarget,
    cluster: XtreamCluster,
    user: &ProxyUserCredentials,
) -> Option<HashSet<u32>> {
    user.t_filter.as_ref()?;
    let mut iterator = iter_raw_xtream_target_playlist(app_config, target, cluster).await?;
    let mut visible = HashSet::new();
    while let Some(entry) = iterator.next().await {
        if let Ok(item) = entry {
            if visible.contains(&item.category_id) {
                continue;
            }
            let pli = PlaylistItem::from(&item);
            if user.allows_content(&pli) {
                visible.insert(item.category_id);
            }
        }
    }
    Some(visible)
}

/// Lowercased EPG channel ids of items visible to the user's content filter,
/// across all clusters of the target's primary output. `None` when the user
/// has no filter (callers must not thin in that case).
pub(in crate::api) async fn collect_visible_epg_channel_ids(
    app_config: &AppConfig,
    target: &ConfigTarget,
    user: &ProxyUserCredentials,
) -> Option<HashSet<String>> {
    user.t_filter.as_ref()?;
    let mut visible = HashSet::new();
    if target.has_output(TargetType::Xtream) {
        for cluster in [XtreamCluster::Live, XtreamCluster::Video, XtreamCluster::Series] {
            let Some(mut iterator) = iter_raw_xtream_target_playlist(app_config, target, cluster).await else {
                continue;
            };
            while let Some(entry) = iterator.next().await {
                if let Ok(item) = entry {
                    let Some(epg_id) = item.epg_channel_id.as_ref() else {
                        continue;
                    };
                    let pli = PlaylistItem::from(&item);
                    if user.allows_content(&pli) {
                        visible.insert(epg_id.to_lowercase());
                    }
                }
            }
        }
    } else if target.has_output(TargetType::M3u) {
        let Some(mut iterator) = iter_raw_m3u_target_playlist(app_config, target, None).await else {
            return Some(visible);
        };
        while let Some(entry) = iterator.next().await {
            if let Ok(item) = entry {
                let Some(epg_id) = item.epg_channel_id.as_ref() else {
                    continue;
                };
                let pli = PlaylistItem::from(&item);
                if user.allows_content(&pli) {
                    visible.insert(epg_id.to_lowercase());
                }
            }
        }
    }
    Some(visible)
}
