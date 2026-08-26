use crate::{
    api::{
        auth_middleware::permission_layer,
        model::AppState,
        panel_api::{sync_panel_api_alias_pool_for_target, target_has_alias_pool_min},
    },
    model::{ApiProxyConfig, ProxyUserCredentials, TargetUser},
    repository::store_api_user,
};
use axum::{response::IntoResponse, Router};
use serde_json::json;
use shared::{
    model::{permission::Permission, ApiProxyConfigDto, ProxyUserCredentialsDto},
    utils::mask_credentials,
};
use std::{path::Path, sync::Arc};

#[allow(clippy::too_many_lines)]
async fn save_config_api_proxy_user(
    method: axum::http::Method,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path(target_name): axum::extract::Path<String>,
    axum::extract::Json(mut credential): axum::extract::Json<ProxyUserCredentialsDto>,
) -> impl axum::response::IntoResponse + Send {
    let api_proxy_file_path = {
        let paths = app_state.app_config.paths.load();
        paths.api_proxy_file_path.clone()
    };
    let _lock = app_state.app_config.file_locks.write_lock(Path::new(&api_proxy_file_path)).await;

    if let Err(err) = credential.prepare() {
        return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": err.to_string()}))).into_response();
    }
    if let Err(err) = credential.validate() {
        return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": err.to_string()}))).into_response();
    }

    let is_update = method == axum::http::Method::PUT;

    let mut api_proxy = if let Some(old) = app_state.app_config.api_proxy.load().clone() {
        (*old).clone()
    } else {
        ApiProxyConfig::default()
    };

    if let Some(plan) = credential.plan.as_ref() {
        if !api_proxy.plans.iter().any(|p| p.name == *plan) {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": format!("Unknown user plan {plan}")})),
            )
                .into_response();
        }
    }
    // Trial plans: new users without an explicit expiry get the trial window.
    if !is_update {
        if let Some(plan) = credential.plan.as_ref().and_then(|name| api_proxy.plans.iter().find(|p| p.name == *name)) {
            if let Some(trial_secs) = plan.t_trial_duration_secs {
                if credential.exp_date.is_none() {
                    let expires =
                        chrono::Utc::now().timestamp().saturating_add(i64::try_from(trial_secs).unwrap_or(i64::MAX));
                    credential.exp_date = Some(expires);
                }
                if credential.status.is_none() {
                    credential.status = Some(shared::model::ProxyUserStatus::Trial);
                }
            }
        }
    }
    let new_user = {
        let mut user = ProxyUserCredentials::from(&credential);
        user.resolve_plan(&api_proxy.plan_map());
        Arc::new(user)
    };

    // ---------- Search for existing Target and existing User ----------
    let mut existing_target_index: Option<usize> = None; // index of target (target_name), if present
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
                        axum::Json(json!({"error": format!("Duplicate token {}", mask_credentials(c))})),
                    )
                        .into_response();
                }
            }
            if user.username == credential.username {
                // if not an update und username exists -> Error (duplicate username)
                if !is_update {
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        axum::Json(json!({"error": format!("Duplicate username {}", &credential.username)})),
                    )
                        .into_response();
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
        api_proxy.user.push(TargetUser { target: target_name.clone(), credentials: vec![] });
        api_proxy.user.len() - 1
    };

    if is_update {
        let mut remove_empty_target = false;
        // existing_user_index and existing_user_target_index exists at this point
        let user_idx = existing_user_index.unwrap();
        let user_target_idx = existing_user_target_index.unwrap();

        if user_target_idx == target_idx {
            // Update
            api_proxy.user[user_target_idx].credentials[user_idx] = Arc::clone(&new_user);
        } else {
            // Move: remove from old target and insert into new target
            api_proxy.user[user_target_idx].credentials.remove(user_idx);
            api_proxy.user[target_idx].credentials.push(Arc::clone(&new_user));
            remove_empty_target = api_proxy.user[user_target_idx].credentials.is_empty();
        }

        if remove_empty_target {
            api_proxy.user.retain(|t| !t.credentials.is_empty());
        }
    } else {
        // new user
        api_proxy.user[target_idx].credentials.push(Arc::clone(&new_user));
    }

    let new_api_proxy = Arc::new(api_proxy);

    if new_api_proxy.use_user_db {
        if let Err(err) = store_api_user(&app_state.app_config, &new_api_proxy.user).await {
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": err.to_string()})))
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
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": err.to_string()})))
                .into_response();
        }
    }

    // Update state after successful save
    app_state.app_config.api_proxy.store(Some(Arc::clone(&new_api_proxy)));

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
                target_user.credentials.retain(|user| user.username != username);
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
            app_state.app_config.api_proxy.store(Some(Arc::clone(&new_api_proxy)));
        } else {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": format!("User not found {username} in target {target_name}")})),
            )
                .into_response();
        }
    }
    axum::http::StatusCode::OK.into_response()
}

/// Terminates a specific playback session for a user.
///
/// This is the explicit `Terminate` path from the playback state machine:
/// - Removes the session and all associated streams
/// - Releases the counted lease if held
/// - Sets lifecycle to `Expired`
async fn terminate_user_session(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Path((username, session_token)): axum::extract::Path<(String, String)>,
) -> impl axum::response::IntoResponse {
    app_state.active_users.terminate_session(&username, &session_token).await;
    app_state.active_provider.clear_provider_reservation(&session_token).await;
    (axum::http::StatusCode::NO_CONTENT).into_response()
}

pub fn v1_api_user_register(router: Router<Arc<AppState>>) -> axum::Router<Arc<AppState>> {
    router
        .route("/user/{target}", axum::routing::post(save_config_api_proxy_user))
        .route("/user/{target}", axum::routing::put(save_config_api_proxy_user))
        .route("/user/{target}/{username}", axum::routing::delete(delete_config_api_proxy_user))
        .route("/user/{username}/session/{session_token}", axum::routing::delete(terminate_user_session))
}

pub fn v1_api_user_register_with_permissions(
    router: Router<Arc<AppState>>,
    app_state: &Arc<AppState>,
) -> axum::Router<Arc<AppState>> {
    let user_write_routes = Router::new()
        .route("/{target}", axum::routing::post(save_config_api_proxy_user).put(save_config_api_proxy_user))
        .route("/{target}/{username}", axum::routing::delete(delete_config_api_proxy_user))
        .route("/{username}/session/{session_token}", axum::routing::delete(terminate_user_session))
        .layer(permission_layer!(app_state, Permission::UserWrite));

    router.nest("/user", user_write_routes)
}
