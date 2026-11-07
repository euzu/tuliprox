//! The background boot update of marketplace plugins: the app side of
//! `tuliprox_plugins::run_boot_update`.
//!
//! Spawned once after the plugins loaded; startup never waits for it. Builds
//! the HTTP client and fetch policy like the marketplace routes do and turns
//! each plugin's outcome into an `EventMessage::PluginUpdate` on the event bus
//! (`plugin.update.available` / `.applied` / `.failed`).

use crate::api::model::AppState;
use std::sync::Arc;

/// Start the boot update run in the background when `plugins.auto_update_on_boot`
/// is on. Inert with plugins off.
#[cfg(feature = "plugins")]
pub(in crate::api) fn spawn_plugin_boot_update(app_state: &Arc<AppState>) {
    let Some(plugins) = app_state.plugins.clone() else {
        return;
    };
    let Some(runtime) = app_state.app_config.plugins.load_full() else {
        return;
    };
    if !runtime.enabled || !runtime.auto_update_on_boot {
        return;
    }
    let app_state = Arc::clone(app_state);
    tokio::spawn(async move {
        let config = match boot_config(&app_state, &runtime).await {
            Ok(config) => config,
            Err(err) => {
                log::error!("Plugin boot update skipped: cannot read config.yml: {err}");
                return;
            }
        };
        let policy = tuliprox_plugins::FetchPolicy::from_env();
        // As `POST …/marketplace/install`: the public-destination client unless
        // the local-testing knob allows http:// and local hosts.
        let client = if policy.allow_http {
            app_state.http_clients.no_redirect.load_full()
        } else {
            app_state.http_clients.resource_public_no_redirect.load_full()
        };
        let events = Arc::clone(&app_state.event_manager);
        let emit = move |event: shared::model::PluginUpdateEvent| {
            let _ = events.send_event(shared::model::EventMessage::PluginUpdate(event));
        };
        tuliprox_plugins::run_boot_update(&plugins, &config, &client, policy, emit).await;
    });
}

/// Built without the `plugins` feature: there is nothing to update.
#[cfg(not(feature = "plugins"))]
pub(in crate::api) fn spawn_plugin_boot_update(_app_state: &Arc<AppState>) {}

/// The runtime `plugins:` config with every entry's settings as `config.yml`
/// has them (`${env:…}` unresolved), like the install routes' config: the
/// evaluation dry-runs the migration on them and the activation's reload
/// writes the retained prior's `prev.json` from them, which must never hold a
/// resolved secret. Repositories stay runtime (their urls are fetched).
#[cfg(feature = "plugins")]
async fn boot_config(
    app_state: &AppState,
    runtime: &tuliprox_core::model::PluginsConfig,
) -> Result<tuliprox_core::model::PluginsConfig, shared::error::TuliproxError> {
    let written = crate::api::endpoints::plugins_settings_api::written_plugins_config(&app_state.app_config).await?;
    let mut config = runtime.clone();
    for entry in &mut config.plugins {
        if let Some(as_written) = written.entry(&entry.id) {
            entry.settings.clone_from(&as_written.settings);
            entry.settings_version = as_written.settings_version;
        }
    }
    Ok(config)
}
