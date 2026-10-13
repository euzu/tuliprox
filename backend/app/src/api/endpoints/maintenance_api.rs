//! Operator control over input maintenance.
//!
//! Every call answers with the complete state, so a client never has to merge.

use crate::api::{auth_middleware::permission_layer, model::AppState};
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use log::error;
use shared::model::{
    normalize_maintenance_reason, InputMaintenanceRequestDto, InputMaintenanceStateDto, Permission,
    MAINTENANCE_WILDCARD,
};
use std::sync::Arc;
use tuliprox_core::model::MaintenanceWrite;

pub fn maintenance_api_register_with_permissions(app_state: &Arc<AppState>) -> Router<Arc<AppState>> {
    let read_routes = Router::new()
        .route("/maintenance", get(get_maintenance))
        .layer(permission_layer!(app_state, Permission::MaintenanceRead));
    let write_routes = Router::new()
        .route("/maintenance/{input}", axum::routing::put(put_maintenance).delete(delete_maintenance))
        .layer(permission_layer!(app_state, Permission::MaintenanceWrite));
    Router::new().merge(read_routes).merge(write_routes)
}

pub fn maintenance_api_register_unprotected() -> Router<Arc<AppState>> {
    Router::new()
        .route("/maintenance", get(get_maintenance))
        .route("/maintenance/{input}", axum::routing::put(put_maintenance).delete(delete_maintenance))
}

fn current_state(app_state: &AppState) -> InputMaintenanceStateDto {
    let sources = app_state.app_config.sources.load();
    InputMaintenanceStateDto {
        entries: (*app_state.input_maintenance.snapshot()).clone(),
        inputs: sources.inputs.iter().map(|input| input.name.to_string()).collect(),
    }
}

fn respond(app_state: &AppState, write: MaintenanceWrite) -> axum::response::Response {
    if let Err(err) = app_state.publish_input_maintenance(write) {
        error!("Could not persist maintenance state: {err}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    Json(current_state(app_state)).into_response()
}

async fn get_maintenance(State(app_state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(current_state(&app_state))
}

/// The body is optional: a bare `PUT` puts the input into maintenance without a remark.
async fn put_maintenance(
    State(app_state): State<Arc<AppState>>,
    Path(input): Path<String>,
    body: Bytes,
) -> axum::response::Response {
    let request = if body.is_empty() {
        InputMaintenanceRequestDto::default()
    } else {
        match serde_json::from_slice::<InputMaintenanceRequestDto>(&body) {
            Ok(request) => request,
            Err(err) => return (StatusCode::BAD_REQUEST, err.to_string()).into_response(),
        }
    };
    let reason = match normalize_maintenance_reason(request.reason.as_deref()) {
        Ok(reason) => reason,
        Err(err) => return (StatusCode::BAD_REQUEST, err.to_string()).into_response(),
    };
    let now = chrono::Utc::now().timestamp();
    let write = if input == MAINTENANCE_WILDCARD {
        let sources = app_state.app_config.sources.load();
        let names: Vec<&str> = sources.inputs.iter().map(|input| input.name.as_ref()).collect();
        app_state.input_maintenance.add_all(&names, reason, now).await
    } else if app_state.is_known_input_name(&input) {
        app_state.input_maintenance.add(&input, reason, now).await
    } else {
        return StatusCode::NOT_FOUND.into_response();
    };
    respond(&app_state, write)
}

async fn delete_maintenance(
    State(app_state): State<Arc<AppState>>,
    Path(input): Path<String>,
) -> axum::response::Response {
    let write = if input == MAINTENANCE_WILDCARD {
        app_state.input_maintenance.remove_all().await
    } else if app_state.is_known_input_name(&input) {
        app_state.input_maintenance.remove(&input).await
    } else {
        return StatusCode::NOT_FOUND.into_response();
    };
    respond(&app_state, write)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::model::create_test_app_state,
        auth::create_jwt_web_user,
        model::{Config, SourcesConfig},
    };
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use shared::model::{
        ConfigInputDto, SourcesConfigDto, UserId, WebAuthConfigDto, WebUiConfigDto, MAINTENANCE_REASON_MAX_CHARS,
    };
    use tempfile::TempDir;
    use tower::ServiceExt;

    fn app_state_with(config: Config) -> Arc<AppState> {
        let state = create_test_app_state(config);
        let dto = SourcesConfigDto {
            inputs: ["provider_a", "provider_b"]
                .iter()
                .map(|name| ConfigInputDto {
                    name: (*name).to_string().into(),
                    url: "http://localhost/playlist.m3u".to_string(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        state.app_config.sources.store(Arc::new(SourcesConfig::try_from(&dto).unwrap()));
        state
    }

    fn app_state(dir: &TempDir) -> Arc<AppState> {
        app_state_with(Config { storage_dir: dir.path().to_string_lossy().to_string(), ..Config::default() })
    }

    async fn call(router: &Router, method: &str, uri: &str, body: Option<&str>) -> (StatusCode, Vec<u8>) {
        let mut builder = Request::builder().method(method).uri(uri);
        if body.is_some() {
            builder = builder.header(axum::http::header::CONTENT_TYPE, "application/json");
        }
        let request = builder.body(body.map_or_else(Body::empty, |b| Body::from(b.to_string()))).unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        (status, response.into_body().collect().await.unwrap().to_bytes().to_vec())
    }

    fn state_of(body: &[u8]) -> InputMaintenanceStateDto { serde_json::from_slice(body).unwrap() }

    #[tokio::test]
    async fn get_lists_known_inputs_and_empty_entries() {
        let dir = TempDir::new().unwrap();
        let router = maintenance_api_register_unprotected().with_state(app_state(&dir));
        let (status, body) = call(&router, "GET", "/maintenance", None).await;
        assert_eq!(status, StatusCode::OK);
        let state = state_of(&body);
        assert!(state.entries.is_empty());
        assert_eq!(state.inputs, vec!["provider_a".to_string(), "provider_b".to_string()]);
    }

    #[tokio::test]
    async fn put_adds_with_trimmed_reason_and_delete_removes() {
        let dir = TempDir::new().unwrap();
        let state = app_state(&dir);
        let router = maintenance_api_register_unprotected().with_state(Arc::clone(&state));
        let (status, body) = call(&router, "PUT", "/maintenance/provider_a", Some(r#"{"reason":"  down "}"#)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(state_of(&body).entries["provider_a"].reason.as_deref(), Some("down"));
        assert!(state.input_maintenance.is_in_maintenance("provider_a"));

        let (status, body) = call(&router, "DELETE", "/maintenance/provider_a", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(state_of(&body).entries.is_empty());
    }

    #[tokio::test]
    async fn put_without_body_is_accepted() {
        let dir = TempDir::new().unwrap();
        let router = maintenance_api_register_unprotected().with_state(app_state(&dir));
        let (status, body) = call(&router, "PUT", "/maintenance/provider_a", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(state_of(&body).entries["provider_a"].reason, None);
    }

    #[tokio::test]
    async fn invalid_json_body_is_bad_request() {
        let dir = TempDir::new().unwrap();
        let router = maintenance_api_register_unprotected().with_state(app_state(&dir));
        let (status, _) = call(&router, "PUT", "/maintenance/provider_a", Some("{not json")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn wildcard_adds_and_removes_all_known_inputs() {
        let dir = TempDir::new().unwrap();
        let router = maintenance_api_register_unprotected().with_state(app_state(&dir));
        let (_, body) = call(&router, "PUT", "/maintenance/%2A", Some(r#"{"reason":"outage"}"#)).await;
        assert_eq!(state_of(&body).entries.len(), 2);
        let (_, body) = call(&router, "DELETE", "/maintenance/*", None).await;
        assert!(state_of(&body).entries.is_empty());
    }

    #[tokio::test]
    async fn unknown_input_is_not_found_and_changes_nothing() {
        let dir = TempDir::new().unwrap();
        let state = app_state(&dir);
        let router = maintenance_api_register_unprotected().with_state(Arc::clone(&state));
        let (status, _) = call(&router, "PUT", "/maintenance/nope", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = call(&router, "DELETE", "/maintenance/nope", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(state.input_maintenance.snapshot().is_empty());
    }

    #[tokio::test]
    async fn too_long_reason_is_bad_request() {
        let dir = TempDir::new().unwrap();
        let router = maintenance_api_register_unprotected().with_state(app_state(&dir));
        let body = format!(r#"{{"reason":"{}"}}"#, "x".repeat(MAINTENANCE_REASON_MAX_CHARS + 1));
        let (status, _) = call(&router, "PUT", "/maintenance/provider_a", Some(&body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn delete_of_known_input_not_in_maintenance_is_ok() {
        let dir = TempDir::new().unwrap();
        let router = maintenance_api_register_unprotected().with_state(app_state(&dir));
        let (status, _) = call(&router, "DELETE", "/maintenance/provider_b", None).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn persist_failure_is_internal_error() {
        let dir = TempDir::new().unwrap();
        let state = app_state(&dir);
        let blocked = dir.path().join(crate::model::INPUT_MAINTENANCE_FILE);
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("blocker"), b"x").unwrap();
        let router = maintenance_api_register_unprotected().with_state(Arc::clone(&state));
        let (status, _) = call(&router, "PUT", "/maintenance/provider_a", None).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!state.input_maintenance.is_in_maintenance("provider_a"));
    }

    #[tokio::test]
    async fn permissions_gate_read_and_write() -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let web_ui = WebUiConfigDto {
            auth: Some(WebAuthConfigDto {
                enabled: true,
                issuer: "maintenance-test".to_owned(),
                secret: "maintenance-test-secret".to_owned(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let config = Config {
            web_ui: Some((&web_ui).into()),
            storage_dir: dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };
        let Some(web_auth) = config.web_ui.as_ref().and_then(|web_ui| web_ui.auth.as_ref()) else {
            return Err("missing web auth".into());
        };
        let reader =
            create_jwt_web_user(web_auth, "reader", Permission::MaintenanceRead.into(), 0, UserId::from("web:reader"))?;
        let system =
            create_jwt_web_user(web_auth, "system", Permission::SystemRead.into(), 0, UserId::from("web:system"))?;
        let state = app_state_with(config);
        let router = maintenance_api_register_with_permissions(&state).with_state(Arc::clone(&state));
        let request = |method: &str, uri: &str, token: &str| {
            Request::builder()
                .method(method)
                .uri(uri)
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
        };

        let response = router.clone().oneshot(request("GET", "/maintenance", &reader)?).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let response = router.clone().oneshot(request("GET", "/maintenance", &system)?).await?;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response = router.clone().oneshot(request("PUT", "/maintenance/provider_a", &reader)?).await?;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(!state.input_maintenance.is_in_maintenance("provider_a"));
        Ok(())
    }
}
