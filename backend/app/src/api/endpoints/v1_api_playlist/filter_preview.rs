use super::{FILTER_PREVIEW_DEFAULT_SAMPLES, FILTER_PREVIEW_MAX_SAMPLES};
use crate::{
    api::model::AppState,
    repository::{iter_raw_m3u_target_playlist, iter_raw_xtream_target_playlist},
};
use axum::response::IntoResponse;
use log::error;
use serde_json::json;
use shared::{
    foundation::{get_target_processing_filter_detailed, Filter, ValueProvider},
    model::{PlaylistItem, TargetType, XtreamCluster},
};
use std::sync::Arc;
use tokio_stream::StreamExt;

#[derive(serde::Deserialize)]
pub(super) struct FilterPreviewRequest {
    target: u16,
    filter: String,
    #[serde(default)]
    limit: Option<u16>,
    #[serde(default)]
    match_as_ascii: bool,
}

#[derive(serde::Serialize)]
struct FilterPreviewItem {
    name: String,
    group: String,
    item_type: String,
}

impl From<&PlaylistItem> for FilterPreviewItem {
    fn from(pli: &PlaylistItem) -> Self {
        let header = &pli.header;
        Self {
            name: header.name.to_string(),
            group: header.group.to_string(),
            item_type: header.item_type.as_str().to_string(),
        }
    }
}

#[derive(serde::Serialize, Default)]
struct FilterPreviewClusterStats {
    total: usize,
    matched: usize,
}

#[derive(serde::Serialize, Default)]
struct FilterPreviewResponse {
    total: usize,
    matched: usize,
    live: FilterPreviewClusterStats,
    vod: FilterPreviewClusterStats,
    series: FilterPreviewClusterStats,
    sample_matched: Vec<FilterPreviewItem>,
    sample_excluded: Vec<FilterPreviewItem>,
}

impl FilterPreviewResponse {
    fn observe(&mut self, pli: &PlaylistItem, filter: &Filter, match_as_ascii: bool, sample_limit: usize) {
        let cluster_stats = match pli.header.xtream_cluster {
            XtreamCluster::Live => &mut self.live,
            XtreamCluster::Video => &mut self.vod,
            XtreamCluster::Series => &mut self.series,
        };
        self.total += 1;
        cluster_stats.total += 1;
        let provider = ValueProvider { pli, match_as_ascii };
        if filter.filter(&provider) {
            self.matched += 1;
            cluster_stats.matched += 1;
            if self.sample_matched.len() < sample_limit {
                self.sample_matched.push(FilterPreviewItem::from(pli));
            }
        } else if self.sample_excluded.len() < sample_limit {
            self.sample_excluded.push(FilterPreviewItem::from(pli));
        }
    }
}

/// Dry-run a filter DSL expression against a target's stored playlist
/// without touching processing or provider fetches.
pub(super) async fn playlist_filter_preview(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(req): axum::extract::Json<FilterPreviewRequest>,
) -> impl IntoResponse + Send {
    let filter = {
        let sources = app_state.app_config.sources.load();
        match get_target_processing_filter_detailed(&req.filter, sources.templates.as_deref()) {
            Ok(filter) => {
                if filter.plugin_calls().next().is_some() {
                    return (
                        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                        axum::Json(json!({
                            "error": "plugin.<id>.<fn>() terms cannot be previewed; they run only during a playlist update",
                        })),
                    )
                        .into_response();
                }
                filter
            }
            Err((err, position)) => {
                return (
                    axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                    axum::Json(json!({
                        "error": err.to_string(),
                        "line": position.map(|p| p.line),
                        "column": position.map(|p| p.column),
                    })),
                )
                    .into_response()
            }
        }
    };
    let Some(target) = app_state.app_config.get_target_by_id(req.target) else {
        return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Unknown target"}))).into_response();
    };
    let sample_limit = usize::from(req.limit.unwrap_or(FILTER_PREVIEW_DEFAULT_SAMPLES).min(FILTER_PREVIEW_MAX_SAMPLES));

    let mut response = FilterPreviewResponse::default();
    if target.has_output(TargetType::Xtream) {
        let mut any_cluster_read = false;
        for cluster in [XtreamCluster::Live, XtreamCluster::Video, XtreamCluster::Series] {
            if let Some(mut iterator) = iter_raw_xtream_target_playlist(&app_state.app_config, &target, cluster).await {
                any_cluster_read = true;
                while let Some(entry) = iterator.next().await {
                    match entry {
                        Ok(item) => {
                            let pli = PlaylistItem::from(&item);
                            response.observe(&pli, &filter, req.match_as_ascii, sample_limit);
                        }
                        Err(err) => {
                            error!("Filter preview failed to read stored {cluster} playlist: {err}");
                            return (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(json!({"error": "Failed to read stored playlist"})),
                            )
                                .into_response();
                        }
                    }
                }
            }
        }
        if !any_cluster_read {
            return (
                axum::http::StatusCode::NOT_FOUND,
                axum::Json(json!({"error": "Stored playlist is not available, update the playlist first"})),
            )
                .into_response();
        }
    } else if target.has_output(TargetType::M3u) {
        let Some(mut iterator) = iter_raw_m3u_target_playlist(&app_state.app_config, &target, None).await else {
            return (
                axum::http::StatusCode::NOT_FOUND,
                axum::Json(json!({"error": "Stored playlist is not available, update the playlist first"})),
            )
                .into_response();
        };
        while let Some(entry) = iterator.next().await {
            match entry {
                Ok(item) => {
                    let pli = PlaylistItem::from(&item);
                    response.observe(&pli, &filter, req.match_as_ascii, sample_limit);
                }
                Err(err) => {
                    error!("Filter preview failed to read stored m3u playlist: {err}");
                    return (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(json!({"error": "Failed to read stored playlist"})),
                    )
                        .into_response();
                }
            }
        }
    } else {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "Target has no xtream or m3u output to preview"})),
        )
            .into_response();
    }

    axum::Json(response).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::model::create_test_app_state;
    use axum::http::StatusCode;

    #[tokio::test]
    async fn filter_preview_reports_parse_error_for_plugin_call() {
        let app_state = create_test_app_state(tuliprox_core::model::Config::default());
        let req = FilterPreviewRequest {
            target: 1,
            filter: r#"plugin.my-plugin.is_match("val")"#.to_string(),
            limit: None,
            match_as_ascii: false,
        };
        let response =
            playlist_filter_preview(axum::extract::State(app_state), axum::extract::Json(req)).await.into_response();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let err_msg = json["error"].as_str().unwrap();
        assert_eq!(err_msg, "plugin.<id>.<fn>() terms cannot be previewed; they run only during a playlist update");
    }
}
