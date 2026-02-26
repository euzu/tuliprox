use crate::{
    api::model::{create_custom_video_stream_response, AppState, CustomVideoStreamType},
    auth::Fingerprint,
};
use axum::response::IntoResponse;
use std::{str::FromStr, sync::Arc};

async fn cvs_api(
    fingerprint: Fingerprint,
    axum::extract::Path((username, password, stream_type)): axum::extract::Path<(String, String, String)>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let cvs_type = stream_type.strip_suffix(".ts").unwrap_or(&stream_type);

    let Ok(custom_video_type) = CustomVideoStreamType::from_str(cvs_type) else {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };

    let Some((user, _target)) = app_state.app_config.get_target_for_user(&username, &password) else {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    };

    if user.permission_denied(&app_state) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }

    create_custom_video_stream_response(&app_state, &fingerprint.addr, custom_video_type).await.into_response()
}

pub fn cvs_api_register() -> axum::Router<Arc<AppState>> {
    axum::Router::new().route("/cvs/{username}/{password}/{stream_type}", axum::routing::get(cvs_api))
}
