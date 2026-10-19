use crate::api::model::AppState;
use log::warn;
use shared::model::{ConfigType, EventMessage, PlaylistUpdateState};
use std::{collections::HashSet, sync::Arc};
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use tuliprox_repository::target_stream_item::failover_groups_enabled;

/// Restricts the member health probe store to the members of all failover groups.
/// Returns the number of member keys.
pub(in crate::api) async fn refresh_member_keys(app_state: &AppState) -> usize {
    let targets: Vec<_> = app_state
        .app_config
        .sources
        .load()
        .sources
        .iter()
        .flat_map(|source| source.targets.iter())
        .filter(|target| failover_groups_enabled(target))
        .map(Arc::clone)
        .collect();
    let mut keys = HashSet::new();
    for target in targets {
        let data =
            app_state.playlists.failover_groups.get_or_load(&app_state.app_config, &app_state.playlists, &target).await;
        keys.extend(data.member_keys());
    }
    let count = keys.len();
    app_state.member_health.set_member_keys(keys);
    count
}

/// Refreshes the member keys at startup, after every playlist update and after a sources or
/// config reload, until `cancel` fires.
pub(in crate::api) fn exec_failover_group_member_keys(
    app_state: &Arc<AppState>,
    cancel: &CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let app_state = Arc::clone(app_state);
    let cancel = cancel.clone();
    tokio::spawn(async move {
        let mut rx = app_state.event_manager.get_event_channel();
        refresh_member_keys(&app_state).await;
        loop {
            let event = tokio::select! {
                () = cancel.cancelled() => break,
                event = rx.recv() => event,
            };
            match event {
                Ok(EventMessage::PlaylistUpdate(summary)) if summary.state != PlaylistUpdateState::Failure => {
                    refresh_member_keys(&app_state).await;
                }
                Ok(EventMessage::ConfigChange(ConfigType::Sources | ConfigType::Config)) => {
                    refresh_member_keys(&app_state).await;
                }
                Ok(_) => {}
                Err(RecvError::Lagged(skipped)) => {
                    warn!("Failover group member key refresh lagged by {skipped} events");
                    refresh_member_keys(&app_state).await;
                }
                Err(RecvError::Closed) => break,
            }
        }
    })
}
