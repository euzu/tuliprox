use crate::api::model::AppState;
use crate::api::panel_api::{sync_panel_api_alias_pool_for_target, target_has_alias_pool_min};
use crate::model::{ApiProxyConfig, ProxyUserCredentials, TargetUser};
use crate::repository::store_api_user;
use axum::response::IntoResponse;
use axum::Router;
use serde_json::json;
use shared::model::{ApiProxyConfigDto, ProxyUserCredentialsDto};
use shared::utils::{concat_path_leading_slash, mask_credentials};
use std::path::PathBuf;
use std::sync::Arc;

#[allow(clippy::too_many_lines)]
async fn save_config_api_proxy_user(
    method: axum::http::Method,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(target_name): axum::extract::Path<String>,
    axum::extract::Json(mut credential): axum::extract::Json<ProxyUserCredentialsDto>,
) -> impl axum::response::IntoResponse + Send {
    let virtual_file = PathBuf::from("api_proxy");
    let _lock = app_state
        .app_config
        .file_locks
        .write_lock(&virtual_file)
        .await;

    credential.prepare();
    if let Err(err) = credential.validate() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": err.to_string()})),
        )
            .into_response();
    }

    let is_update = method == axum::http::Method::PUT;

    let mut api_proxy = if let Some(old) = app_state.app_config.api_proxy.load().clone() {
        (*old).clone()
    } else {
        ApiProxyConfig::default()
    };

    // ---------- Search for existing Target and existing User ----------
    let mut existing_target_index: Option<usize> = None; // index of target (target_name), falls vorhanden
    let mut existing_user_target_index: Option<usize> = None; // index of existing users target
    let mut existing_user_index: Option<usize> = None; // index of the user in the targets credentials list

    for (t_idx, target_user) in api_proxy.user.iter().enumerate() {
        if target_user.target == target_name {
            existing_target_index = Some(t_idx);
        }
        for (u_idx, user) in target_user.credentials.iter().enumerate() {
            if let (Some(u), Some(c)) = (&user.token, &credential.token) {
                if u == c && user.username != credential.username {
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        axum::Json(
                            json!({"error": format!("Duplicate token {}", mask_credentials(c))}),
                        ),
                    )
                        .into_response();
                }
            }
            if user.username == credential.username {
                // if not an update und username exists -> Error (duplicate username)
                if !is_update {
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        axum::Json(json!({"error": format!("Duplicate username {}", &credential.username)}))
                    ).into_response();
                }

                // mark position of user (for update / move)
                existing_user_target_index = Some(t_idx);
                existing_user_index = Some(u_idx);
            }
        }
    }

    // ---------- if update but no user found -> Error ----------
    if is_update && existing_user_index.is_none() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": format!("User {} not found", credential.username)})),
        )
            .into_response();
    }

    // ---------- create target if new target does not exist ----------
    let target_idx = if let Some(idx) = existing_target_index {
        idx
    } else {
        api_proxy.user.push(TargetUser {
            target: target_name.clone(),
            credentials: vec![],
        });
        api_proxy.user.len() - 1
    };

    if is_update {
        let mut remove_empty_target = false;
        // existing_user_index and existing_user_target_index exists at this point
        let user_idx = existing_user_index.unwrap();
        let user_target_idx = existing_user_target_index.unwrap();

        if user_target_idx == target_idx {
            // Update
            api_proxy.user[user_target_idx].credentials[user_idx] =
                ProxyUserCredentials::from(&credential);
        } else {
            // Move: remove from old target and insert into new target
            api_proxy.user[user_target_idx].credentials.remove(user_idx);
            api_proxy.user[target_idx]
                .credentials
                .push(ProxyUserCredentials::from(&credential));
            remove_empty_target = api_proxy.user[user_target_idx].credentials.is_empty();
        }

        if remove_empty_target {
            api_proxy.user.retain(|t| !t.credentials.is_empty());
        }
    } else {
        // new user
        api_proxy.user[target_idx]
            .credentials
            .push(ProxyUserCredentials::from(&credential));
    }

    let new_api_proxy = Arc::new(api_proxy);

    if new_api_proxy.use_user_db {
        if let Err(err) = store_api_user(&app_state.app_config, &new_api_proxy.user).await {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": err.to_string()})),
            )
                .into_response();
        }
    } else {
        let config = app_state.app_config.config.load();
        let backup_dir = config.get_backup_dir();
        let paths = app_state.app_config.paths.load();
        if let Some(err) = crate::api::endpoints::v1_api_config::intern_save_config_api_proxy(
            backup_dir.as_ref(),
            &ApiProxyConfigDto::from(&*new_api_proxy),
            paths.api_proxy_file_path.as_str(),
        )
        .await
        {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": err.to_string()})),
            )
                .into_response();
        }
    }

    // Update state after successful save
    app_state
        .app_config
        .api_proxy
        .store(Some(Arc::clone(&new_api_proxy)));

    if target_has_alias_pool_min(&app_state, &target_name) {
        let app_state_clone = Arc::clone(&app_state);
        let target_name_clone = target_name.clone();
        tokio::spawn(async move {
            sync_panel_api_alias_pool_for_target(&app_state_clone, &target_name_clone).await;
        });
    }

    axum::http::StatusCode::OK.into_response()
}

async fn delete_config_api_proxy_user(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path((target_name, username)): axum::extract::Path<(String, String)>,
) -> impl axum::response::IntoResponse + Send {
    if let Some(old_api_proxy) = app_state.app_config.api_proxy.load().clone() {
        let mut api_proxy = (*old_api_proxy).clone();
        let mut modified = false;

        for target_user in &mut api_proxy.user {
            if target_user.target == target_name {
                let count = target_user.credentials.len();
                target_user
                    .credentials
                    .retain(|user| user.username != username);
                modified = count != target_user.credentials.len();
                break;
            }
        }
        if modified {
            let new_api_proxy = Arc::new(api_proxy);
            if new_api_proxy.use_user_db {
                if let Err(err) = store_api_user(&app_state.app_config, &new_api_proxy.user).await {
                    return (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(json!({"error": err.to_string()})),
                    )
                        .into_response();
                }
            } else {
                let config = app_state.app_config.config.load();
                let backup_dir = config.get_backup_dir();
                let paths = app_state.app_config.paths.load();
                if let Some(err) =
                    crate::api::endpoints::v1_api_config::intern_save_config_api_proxy(
                        backup_dir.as_ref(),
                        &ApiProxyConfigDto::from(&*new_api_proxy),
                        paths.api_proxy_file_path.as_str(),
                    )
                    .await
                {
                    return (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(json!({"error": err.to_string()})),
                    )
                        .into_response();
                }
            }
            app_state
                .app_config
                .api_proxy
                .store(Some(Arc::clone(&new_api_proxy)));
        } else {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(
                    json!({"error": format!("User not found {username} in target {target_name}")}),
                ),
            )
                .into_response();
        }
    }
    axum::http::StatusCode::OK.into_response()
}

pub fn v1_api_user_register(router: Router<Arc<AppState>>, web_ui_path: &str) -> axum::Router<Arc<AppState>> {
    router
        .route(
            &concat_path_leading_slash(web_ui_path, "/user/{target}"),
            axum::routing::post(save_config_api_proxy_user),
        )
        .route(
            &concat_path_leading_slash(web_ui_path, "/user/{target}"),
            axum::routing::put(save_config_api_proxy_user),
        )
        .route(
            &concat_path_leading_slash(web_ui_path, "/user/{target}/{username}"),
            axum::routing::delete(delete_config_api_proxy_user),
        )
}
