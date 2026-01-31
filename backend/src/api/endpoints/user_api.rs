use crate::api::api_utils::try_unwrap_body;
use crate::api::api_utils::{get_user_target_by_username, get_username_from_auth_header};
use crate::api::model::AppState;
use crate::auth::validator_user;
use crate::auth::AuthBearer;
use crate::model::PlaylistXtreamCategory;
use crate::model::{AppConfig, ConfigTarget};
use crate::repository::{iter_raw_m3u_target_playlist, load_user_bouquet_as_json, save_user_bouquet};
use crate::repository::xtream_get_playlist_categories;
use axum::response::IntoResponse;
use bytes::Bytes;
use futures::{stream, StreamExt};
use log::error;
use shared::model::{PlaylistBouquetDto, TargetType, XtreamCluster};
use std::collections::HashSet;
use std::sync::Arc;
use shared::utils::concat_path_leading_slash;

fn get_categories_from_xtream(categories: Option<Vec<PlaylistXtreamCategory>>) -> Vec<String> {
    let mut groups: Vec<String> = Vec::new();
    if let Some(cats) = categories {
        for category in cats {
            groups.push(category.name.clone());
        }
    }
    groups
}


async fn get_categories_from_m3u_playlist(target: &ConfigTarget, config: &AppConfig) -> Vec<Arc<str>> {
    let mut groups = Vec::new();
    if let Some((_guard, iter)) = iter_raw_m3u_target_playlist(config, target, None).await {
        let mut unique_groups = HashSet::new();
        for item in iter {
            if !unique_groups.contains(&item.group) {
                unique_groups.insert(item.group.clone());
                groups.push(item.group.clone());
            }
        }
    }
    groups
}

async fn playlist_categories(
    AuthBearer(token): AuthBearer,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl axum::response::IntoResponse + Send {
    if let Some(username) = get_username_from_auth_header(&token, &app_state) {
        if let Some((user, target)) = get_user_target_by_username(username.as_str(), &app_state) {
            if user.permission_denied(&app_state) {
                return axum::http::StatusCode::FORBIDDEN.into_response();
            }
            let target_name = &target.name;
            let xtream_stream = if target.has_output(TargetType::Xtream) {
                let config = &app_state.app_config.config.load();
                let live_categories = get_categories_from_xtream(xtream_get_playlist_categories(config, target_name, XtreamCluster::Live).await);
                let vod_categories = get_categories_from_xtream(xtream_get_playlist_categories(config, target_name, XtreamCluster::Video).await);
                let series_categories = get_categories_from_xtream(xtream_get_playlist_categories(config, target_name, XtreamCluster::Series).await);
                stream::iter(vec![
                    Ok::<Bytes, String>(Bytes::from(r#"{"live": "#)),
                    Ok::<Bytes, String>(Bytes::from(serde_json::to_string(&live_categories).unwrap_or("[]".to_string()))),
                    Ok::<Bytes, String>(Bytes::from(r#", "vod": "#.to_string())),
                    Ok::<Bytes, String>(Bytes::from(serde_json::to_string(&vod_categories).unwrap_or("[]".to_string()))),
                    Ok::<Bytes, String>(Bytes::from(r#", "series": "#)),
                    Ok::<Bytes, String>(Bytes::from(serde_json::to_string(&series_categories).unwrap_or("[]".to_string()))),
                    Ok::<Bytes, String>(Bytes::from(r"}")),
                ])
            } else {
                stream::iter(vec![Ok::<Bytes, String>(Bytes::from(r#"{"live":[],"vod":[],"series":[]}"#))])
            };

            let m3u_stream = if target.has_output(TargetType::M3u) {
                let live_categories = get_categories_from_m3u_playlist(&target, &app_state.app_config).await;
                stream::iter(vec![
                    Ok::<Bytes, String>(Bytes::from(r#"{"live": "#)),
                    Ok::<Bytes, String>(Bytes::from(serde_json::to_string(&live_categories).unwrap_or("[]".to_string()))),
                    Ok::<Bytes, String>(Bytes::from(r#","vod":[],"series":[]}"#)),
                ])
            } else {
                stream::iter(vec![Ok::<Bytes, String>(Bytes::from(r#"{"live":[],"vod":[],"series":[]}"#))])
            };

            let json_stream = stream::once(async { Ok::<Bytes, String>(Bytes::from(r#"{"xtream": "#)) })
                .chain(xtream_stream)
                .chain(stream::once(async { Ok::<Bytes, String>(Bytes::from(r#", "m3u": "#)) }))
                .chain(m3u_stream)
                .chain(stream::once(async { Ok::<Bytes, String>(Bytes::from("}")) }));

            return try_unwrap_body!(axum::response::Response::builder()
                .status(axum::http::StatusCode::OK)
                .header("Content-Type", mime::APPLICATION_JSON.to_string())
                .body(axum::body::Body::from_stream(json_stream)));
        }
    }
    axum::http::StatusCode::BAD_REQUEST.into_response()
}

async fn save_playlist_bouquet(
    AuthBearer(token): AuthBearer,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(bouquet): axum::extract::Json<PlaylistBouquetDto>,
) -> impl axum::response::IntoResponse + Send {
    if let Some(username) = get_username_from_auth_header(&token, &app_state) {
        if let Some((user, target)) = get_user_target_by_username(username.as_str(), &app_state) {
            if user.permission_denied(&app_state) {
                return axum::http::StatusCode::FORBIDDEN.into_response();
            }
            let config = &app_state.app_config.config.load();
            match save_user_bouquet(config, &target.name, &username, &bouquet).await {
                Ok(()) => {
                    return axum::http::StatusCode::OK.into_response();
                }
                Err(err) => {
                    error!("Saving bouquet for {username} failed: {err}");
                }
            }
        }
    }
    axum::http::StatusCode::BAD_REQUEST.into_response()
}

async fn playlist_bouquet(
    AuthBearer(token): AuthBearer,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl axum::response::IntoResponse + Send {
    if let Some(username) = get_username_from_auth_header(&token, &app_state) {
        if let Some((user, _target)) = get_user_target_by_username(username.as_str(), &app_state) {
            if user.permission_denied(&app_state) {
                return axum::http::StatusCode::FORBIDDEN.into_response();
            }
            let config = &app_state.app_config.config.load();
            let xtream = load_user_bouquet_as_json(config, &username, TargetType::Xtream).await;
            let m3u = load_user_bouquet_as_json(config, &username, TargetType::M3u).await;
            return try_unwrap_body!(axum::response::Response::builder()
                .status(axum::http::StatusCode::OK)
                .header("Content-Type", mime::APPLICATION_JSON.to_string())
                .body(axum::body::Body::from(format!(r#"{{"xtream": {}, "m3u": {} }}"#,
                    xtream.unwrap_or("null".to_string()),
                    m3u.unwrap_or("null".to_string())))));
        }
    }
    try_unwrap_body!(axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header("Content-Type", mime::APPLICATION_JSON.to_string())
        .body(axum::body::Body::from("{}")))
}

pub fn user_api_register(app_state: Arc<AppState>, web_ui_path: &str) -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .nest(
            &concat_path_leading_slash(web_ui_path, "/api/v1/user"),
            axum::Router::new()
                .route("/playlist/categories", axum::routing::get(playlist_categories))
                .route("/playlist/bouquet", axum::routing::get(playlist_bouquet))
                .route("/playlist/bouquet", axum::routing::post(save_playlist_bouquet))
                .route_layer(axum::middleware::from_fn_with_state(app_state, validator_user)),
        )


    // cfg.service(web::scope("/api/v1/user")
    //     .wrap(HttpAuthentication::with_fn(validator_user))
    //     .wrap(Compress::default())
    //     .route("/playlist/categories", web::get().to(playlist_categories))
    //     .route("/playlist/bouquet", web::get().to(playlist_bouquet))
    //     .route("/playlist/bouquet", web::post().to(save_playlist_bouquet)));
}
