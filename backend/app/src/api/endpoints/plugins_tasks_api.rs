//! `POST /api/v1/plugins/{id}/tasks/{task}/run`, under `SystemWrite`: run a task now and answer its
//! `PluginTaskRunDto`. Never exposed through MCP.
//!
//! `200` carries the run, including a run that failed (`error` set). `404` is
//! an unknown plugin or task, and plugins off. `409` is a run that did not
//! start because the previous one is still going (or the plugin is not
//! runnable); the body says why.

use crate::api::{
    model::AppState,
    tasks::{run_plugin_task, PluginTaskResult, TaskTrigger},
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use std::sync::Arc;

pub(super) async fn post_plugin_task_run(
    State(app_state): State<Arc<AppState>>,
    Path((id, task)): Path<(String, String)>,
) -> Response {
    match run_plugin_task(&app_state, &id, &task, TaskTrigger::Manual, None).await {
        PluginTaskResult::Ran(run) => Json(run).into_response(),
        PluginTaskResult::Skipped(reason) => (StatusCode::CONFLICT, reason).into_response(),
        PluginTaskResult::Unknown(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::{super::plugins_api::plugins_api_register_unprotected, *};
    use crate::{api::model::create_test_app_state, model::Config};
    use axum::{body::Body, http::Request};
    use tempfile::TempDir;
    use tower::ServiceExt;

    #[tokio::test]
    async fn plugins_off_answers_a_manual_run_with_404() {
        let dir = TempDir::new().unwrap();
        let config = Config { storage_dir: dir.path().to_string_lossy().to_string(), ..Config::default() };
        let router = plugins_api_register_unprotected().with_state(create_test_app_state(config));
        let request = Request::builder().method("POST").uri("/plugins/demo/tasks/nightly/run").body(Body::empty());
        let response = router.oneshot(request.unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
