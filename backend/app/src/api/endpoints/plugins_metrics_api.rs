//! `GET /api/v1/plugins/metrics`, under
//! `SystemRead`: the throttled `collect-metrics` snapshot of every plugin.
//! The same snapshot rides the status payload ([`plugin_metrics`]).
//!
//! Plugins off answers `[]`.

use crate::api::model::AppState;
#[cfg(feature = "plugins")]
use axum::extract::State;
use axum::Json;
use shared::model::PluginMetricDto;
#[cfg(feature = "plugins")]
use std::sync::Arc;

/// The plugin metrics snapshot; empty without the `plugins` feature or with
/// plugins disabled. Compiles in both builds, so the status check needs no `cfg`.
#[cfg(feature = "plugins")]
pub(crate) async fn plugin_metrics(app_state: &AppState) -> Vec<PluginMetricDto> {
    match app_state.plugins.as_ref() {
        Some(plugins) => plugins.metrics_snapshot().await,
        None => Vec::new(),
    }
}

#[cfg(not(feature = "plugins"))]
#[allow(clippy::unused_async, reason = "same signature as the `plugins` build")]
pub(crate) async fn plugin_metrics(_app_state: &AppState) -> Vec<PluginMetricDto> { Vec::new() }

#[cfg(feature = "plugins")]
pub(super) async fn get_plugins_metrics(State(app_state): State<Arc<AppState>>) -> Json<Vec<PluginMetricDto>> {
    Json(plugin_metrics(&app_state).await)
}

#[cfg(not(feature = "plugins"))]
pub(super) async fn get_plugins_metrics() -> Json<Vec<PluginMetricDto>> { Json(Vec::new()) }
