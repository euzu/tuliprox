//! Bridges the in-process event bus onto plugins exporting `on-event` (plan §7
//! *Event queue*, §14), modelled on `notification_bridge.rs`: one
//! `subscribe_filtered` over the union of the loaded plugins' `events:`
//! masks, `Lagged` survived and reported through `record_lag`, the mask
//! rebuilt on every registry change.
//!
//! The bridge only offers events; the plugin host queues and delivers them
//! (`tuliprox_plugins::PluginSystem::offer_event`), so a slow plugin never
//! reaches the bus.

use crate::api::model::AppState;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Subscribe to the event bus and hand plugin-subscribed events to the plugin
/// host. Spawns nothing when plugins are off.
#[cfg(feature = "plugins")]
pub fn spawn_plugin_event_bridge(app_state: &Arc<AppState>, cancel_token: &CancellationToken) {
    use log::{debug, warn};
    use tokio::sync::broadcast::error::RecvError;

    let Some(plugins) = app_state.plugins.clone() else {
        return;
    };
    let event_manager = Arc::clone(&app_state.event_manager);
    let cancel_token = cancel_token.clone();
    tokio::spawn(async move {
        // Created before the first mask is read, so a reload that lands while
        // the mask is being built still wakes the loop.
        let mut changes = plugins.changes();
        'rebuild: loop {
            let mask = plugins.event_mask();
            if mask.is_empty() {
                // No plugin subscribes to anything: hold no bus subscription
                // until a registry change might add one.
                tokio::select! {
                    () = cancel_token.cancelled() => break 'rebuild,
                    updated = changes.changed() => {
                        if updated.is_err() {
                            break 'rebuild;
                        }
                    }
                }
                continue 'rebuild;
            }
            let mut receiver = event_manager.subscribe_filtered(mask);
            loop {
                tokio::select! {
                    () = cancel_token.cancelled() => break 'rebuild,
                    updated = changes.changed() => {
                        if updated.is_err() {
                            break 'rebuild;
                        }
                        continue 'rebuild;
                    }
                    message = receiver.recv() => match message {
                        Ok(message) => {
                            let payload = message.payload().to_string();
                            plugins.offer_event(message.kind(), payload, now_unix_secs());
                        }
                        // Capacity is 256. Falling behind must not stop the
                        // bridge: report the gap and keep going.
                        Err(RecvError::Lagged(skipped)) => {
                            event_manager.stats().record_lag(skipped);
                            warn!("Plugin event bridge fell behind and skipped {skipped} event(s)");
                        }
                        Err(RecvError::Closed) => break 'rebuild,
                    },
                }
            }
        }
        debug!("Plugin event bridge stopped");
    });
}

/// Built without plugins: nothing subscribes and nothing is spawned.
#[cfg(not(feature = "plugins"))]
pub fn spawn_plugin_event_bridge(_app_state: &Arc<AppState>, _cancel_token: &CancellationToken) {}

#[cfg(feature = "plugins")]
fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
}
