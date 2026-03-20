use crate::{
    api::model::AppState,
    auth::{generate_password_from_input, validator_admin, verify_token, AuthBearer},
    model::{RbacGroup, WebAuthConfig, WebUiUser},
    utils,
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json, Router,
};
use log::{info, warn};
use rand::Rng;
use serde::Deserialize;
use serde_json::json;
use shared::model::{
    permission::{permission_from_name, Permission, PermissionSet, PERMISSION_NAMES},
    RbacGroupDto, WebUiUserDto,
};
use std::{
    collections::HashSet,
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};

#[derive(Debug, Deserialize)]
struct CreateUserRequest {
    username: String,
    password: String,
    #[serde(default)]
    groups: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct UpdateUserRequest {
    password: Option<String>,
    #[serde(default)]
    groups: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CreateGroupRequest {
    name: String,
    #[serde(default)]
    permissions: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
struct PermissionInfo {
    name: &'static str,
    reserved: bool,
}

fn create_temp_path(file_path: &FsPath) -> PathBuf {
    let file_name =
        file_path.file_name().and_then(|value| value.to_str()).filter(|value| !value.is_empty()).unwrap_or("rbac");
    let temp_name = format!(".{file_name}.tmp-{}-{}", std::process::id(), rand::rng().random::<u64>());
    match file_path.parent() {
        Some(parent) => parent.join(temp_name),
        None => PathBuf::from(temp_name),
    }
}

async fn replace_file_atomic(source: &FsPath, target: &FsPath) -> std::io::Result<()> {
    match tokio::fs::rename(source, target).await {
        Ok(()) => Ok(()),
        Err(err) => {
            #[cfg(windows)]
            {
                if target.exists() {
                    tokio::fs::remove_file(target).await?;
                    return tokio::fs::rename(source, target).await;
                }
            }
            Err(err)
        }
    }
}

async fn write_text_file_atomic(path: &FsPath, content: &str) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let temp_path = create_temp_path(path);
    tokio::fs::write(&temp_path, content).await?;
    if let Err(err) = replace_file_atomic(&temp_path, path).await {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(err);
    }
    Ok(())
}

fn serialize_users_file(users: &[WebUiUser]) -> String {
    users.iter()
        .map(|user| {
            if user.groups.is_empty() || (user.groups.len() == 1 && user.groups[0].eq_ignore_ascii_case("admin")) {
                format!("{}:{}", user.username, user.password_hash)
            } else {
                format!("{}:{}:{}", user.username, user.password_hash, user.groups.join(","))
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn serialize_groups_file(groups: &[RbacGroup]) -> String {
    groups.iter()
        .map(|group| {
            let permissions = PERMISSION_NAMES
                .iter()
                .filter_map(|(name, permission)| group.permissions.contains(*permission).then_some(*name))
                .collect::<Vec<_>>()
                .join(",");
            format!("{}:{permissions}", group.name)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_name(value: &str) -> String { value.trim().to_string() }

fn normalize_groups(groups: &[String]) -> Vec<String> {
    let mut normalized = Vec::with_capacity(groups.len());
    let mut seen = HashSet::with_capacity(groups.len());
    for group in groups {
        let group = group.trim();
        if group.is_empty() {
            continue;
        }
        let key = group.to_ascii_lowercase();
        if seen.insert(key) {
            normalized.push(group.to_string());
        }
    }
    if normalized.iter().any(|group| group.eq_ignore_ascii_case("admin")) {
        return vec![String::from("admin")];
    }
    normalized
}

fn validate_group_names(groups: &[String], available_groups: &[RbacGroup]) -> Result<(), String> {
    for group in groups {
        if group.eq_ignore_ascii_case("admin") {
            continue;
        }
        if !available_groups.iter().any(|candidate| candidate.name.eq_ignore_ascii_case(group)) {
            return Err(format!("Unknown group '{group}'"));
        }
    }
    Ok(())
}

fn normalize_permissions(permission_names: &[String]) -> PermissionSet {
    let mut permissions = PermissionSet::new();
    for permission_name in permission_names {
        let trimmed = permission_name.trim();
        if trimmed.is_empty() {
            continue;
        }
        match permission_from_name(trimmed) {
            Some(permission) => permissions.set(permission),
            None => warn!("RBAC API: unknown permission '{trimmed}' ignored"),
        }
    }
    permissions
}

fn validate_permission_dependencies(permission_names: &[String]) -> Result<(), Vec<String>> {
    let normalized = normalize_permissions(permission_names);
    let invalid_domains = PERMISSION_NAMES
        .iter()
        .filter_map(|(name, permission)| {
            name.strip_suffix(".write").and_then(|domain| {
                normalized.contains(*permission).then(|| {
                    let read_name = format!("{domain}.read");
                    permission_from_name(&read_name)
                        .filter(|read_permission| !normalized.contains(*read_permission))
                        .map(|_| domain.to_string())
                })?
            })
        })
        .collect::<Vec<_>>();

    if invalid_domains.is_empty() {
        Ok(())
    } else {
        Err(invalid_domains)
    }
}

fn user_has_admin_group(user: &WebUiUser) -> bool { user.groups.iter().any(|group| group.eq_ignore_ascii_case("admin")) }

fn count_admin_users(users: &[WebUiUser]) -> usize { users.iter().filter(|user| user_has_admin_group(user)).count() }

fn resolve_auth_paths(web_auth: &WebAuthConfig, config_path: &str) -> (PathBuf, PathBuf) {
    let userfile_name = if utils::is_blank_or_default_user_file_path(&web_auth.userfile) {
        utils::get_default_user_file_path(config_path)
    } else {
        web_auth.userfile.as_ref().map_or_else(String::new, std::borrow::ToOwned::to_owned)
    };
    let groupfile_name = if utils::is_blank_or_default_user_group_file_path(&web_auth.groupfile) {
        utils::get_default_user_group_file_path(config_path)
    } else {
        web_auth.groupfile.as_ref().map_or_else(String::new, std::borrow::ToOwned::to_owned)
    };

    let resolve_path = |file_name: &str| {
        let path = PathBuf::from(file_name);
        if path.is_absolute() || utils::path_exists(&path) {
            path
        } else {
            PathBuf::from(config_path).join(file_name)
        }
    };

    let userfile_path = if utils::is_blank_or_default_user_file_path(&web_auth.userfile) {
        PathBuf::from(&userfile_name)
    } else {
        resolve_path(&userfile_name)
    };
    let groupfile_path = if utils::is_blank_or_default_user_group_file_path(&web_auth.groupfile) {
        PathBuf::from(&groupfile_name)
    } else {
        resolve_path(&groupfile_name)
    };

    (userfile_path, groupfile_path)
}

fn current_web_auth_snapshot(app_state: &AppState) -> Result<(WebAuthConfig, String), StatusCode> {
    let paths = app_state.app_config.paths.load();
    let config = app_state.app_config.config.load();
    let Some(web_auth) = config.web_ui.as_ref().and_then(|web_ui| web_ui.auth.as_ref()) else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    Ok((web_auth.clone(), paths.config_path.clone()))
}

fn current_username(app_state: &AppState, token: &str) -> Option<String> {
    let config = app_state.app_config.config.load();
    let web_auth = config.web_ui.as_ref()?.auth.as_ref()?;
    verify_token(token, web_auth.secret.as_bytes()).map(|token_data| token_data.claims.username)
}

fn store_reprepared_web_auth(app_state: &AppState) -> Result<(), String> {
    let paths = app_state.app_config.paths.load();
    let mut config = (*app_state.app_config.config.load_full()).clone();
    let Some(web_ui) = config.web_ui.as_mut() else {
        return Err("Web UI auth is not configured".to_string());
    };
    let Some(auth) = web_ui.auth.as_mut() else {
        return Err("Web UI auth is not configured".to_string());
    };
    auth.prepare(paths.config_path.as_str()).map_err(|err| err.to_string())?;
    app_state.app_config.config.store(Arc::new(config));
    Ok(())
}

async fn list_users(State(app_state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = app_state.app_config.config.load();
    let users: Vec<WebUiUserDto> = config
        .web_ui
        .as_ref()
        .and_then(|web_ui| web_ui.auth.as_ref())
        .and_then(|auth| auth.t_users.as_ref())
        .map(|users| {
            users.iter()
                .map(|user| WebUiUserDto {
                    username: user.username.clone(),
                    groups: user.groups.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    Json(users)
}

async fn list_groups(State(app_state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = app_state.app_config.config.load();
    let mut groups = vec![RbacGroupDto {
        name: "admin".to_string(),
        permissions: vec!["*".to_string()],
        builtin: true,
    }];

    if let Some(parsed_groups) = config
        .web_ui
        .as_ref()
        .and_then(|web_ui| web_ui.auth.as_ref())
        .and_then(|auth| auth.t_groups.as_ref())
    {
        for group in parsed_groups {
            let permissions = PERMISSION_NAMES
                .iter()
                .filter_map(|(name, permission)| group.permissions.contains(*permission).then_some((*name).to_string()))
                .collect();
            groups.push(RbacGroupDto {
                name: group.name.clone(),
                permissions,
                builtin: false,
            });
        }
    }

    Json(groups)
}

async fn list_permissions() -> impl IntoResponse {
    let permissions = PERMISSION_NAMES
        .iter()
        .map(|(name, permission)| PermissionInfo {
            name,
            reserved: matches!(permission, Permission::EpgWrite),
        })
        .collect::<Vec<_>>();
    Json(permissions)
}

async fn create_user(
    State(app_state): State<Arc<AppState>>,
    Json(request): Json<CreateUserRequest>,
) -> impl IntoResponse {
    let username = normalize_name(&request.username);
    if username.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "Username cannot be empty"}))).into_response();
    }
    if request.password.len() < 8 {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "Password must be at least 8 characters"}))).into_response();
    }

    let (web_auth, config_path) = match current_web_auth_snapshot(&app_state) {
        Ok(value) => value,
        Err(status) => return status.into_response(),
    };
    let mut users = web_auth.t_users.clone().unwrap_or_default();
    let groups = normalize_groups(&request.groups);
    let available_groups = web_auth.t_groups.clone().unwrap_or_default();

    if users.iter().any(|user| user.username.eq_ignore_ascii_case(&username)) {
        return (StatusCode::CONFLICT, Json(json!({"error": format!("User '{username}' already exists")}))).into_response();
    }
    if let Err(err) = validate_group_names(&groups, &available_groups) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": err}))).into_response();
    }

    let password = request.password;
    let hash = match tokio::task::spawn_blocking(move || generate_password_from_input(&password)).await {
        Ok(Ok(hash)) => hash,
        Ok(Err(err)) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": err.to_string()}))).into_response();
        }
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Password hashing task failed: {err}")})),
            )
                .into_response();
        }
    };

    users.push(WebUiUser {
        username: username.clone(),
        password_hash: hash,
        groups,
    });

    let (userfile_path, _) = resolve_auth_paths(&web_auth, &config_path);
    let _lock = app_state.app_config.file_locks.write_lock(&userfile_path).await;
    if let Err(err) = write_text_file_atomic(&userfile_path, &serialize_users_file(&users)).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": err.to_string()}))).into_response();
    }
    if let Err(err) = store_reprepared_web_auth(&app_state) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": err}))).into_response();
    }

    info!("RBAC API: created web UI user '{username}'");
    StatusCode::CREATED.into_response()
}

async fn update_user(
    State(app_state): State<Arc<AppState>>,
    Path(username): Path<String>,
    Json(request): Json<UpdateUserRequest>,
) -> impl IntoResponse {
    let username = normalize_name(&username);
    if username.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "Username cannot be empty"}))).into_response();
    }
    if request.password.as_ref().is_some_and(|password| password.len() < 8) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "Password must be at least 8 characters"}))).into_response();
    }

    let (web_auth, config_path) = match current_web_auth_snapshot(&app_state) {
        Ok(value) => value,
        Err(status) => return status.into_response(),
    };
    let mut users = web_auth.t_users.clone().unwrap_or_default();
    let available_groups = web_auth.t_groups.clone().unwrap_or_default();
    let groups = normalize_groups(&request.groups);
    if let Err(err) = validate_group_names(&groups, &available_groups) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": err}))).into_response();
    }

    let Some(user_index) = users.iter().position(|user| user.username.eq_ignore_ascii_case(&username)) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": format!("User '{username}' not found")}))).into_response();
    };

    let removing_admin = user_has_admin_group(&users[user_index]) && !groups.iter().any(|group| group.eq_ignore_ascii_case("admin"));
    if removing_admin && count_admin_users(&users) == 1 {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "Cannot remove admin group from the last admin user"})),
        )
            .into_response();
    }

    let new_hash = if let Some(password) = request.password {
        match tokio::task::spawn_blocking(move || generate_password_from_input(&password)).await {
            Ok(Ok(hash)) => Some(hash),
            Ok(Err(err)) => {
                return (StatusCode::BAD_REQUEST, Json(json!({"error": err.to_string()}))).into_response();
            }
            Err(err) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("Password hashing task failed: {err}")})),
                )
                    .into_response();
            }
        }
    } else {
        None
    };

    users[user_index].groups = groups;
    if let Some(hash) = new_hash {
        users[user_index].password_hash = hash;
    }

    let (userfile_path, _) = resolve_auth_paths(&web_auth, &config_path);
    let _lock = app_state.app_config.file_locks.write_lock(&userfile_path).await;
    if let Err(err) = write_text_file_atomic(&userfile_path, &serialize_users_file(&users)).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": err.to_string()}))).into_response();
    }
    if let Err(err) = store_reprepared_web_auth(&app_state) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": err}))).into_response();
    }

    info!("RBAC API: updated web UI user '{username}'");
    StatusCode::OK.into_response()
}

async fn delete_user(
    State(app_state): State<Arc<AppState>>,
    AuthBearer(token): AuthBearer,
    Path(username): Path<String>,
) -> impl IntoResponse {
    let username = normalize_name(&username);
    if username.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "Username cannot be empty"}))).into_response();
    }

    if current_username(&app_state, &token).is_some_and(|current_username| current_username.eq_ignore_ascii_case(&username)) {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "Admin cannot delete themselves"}))).into_response();
    }

    let (web_auth, config_path) = match current_web_auth_snapshot(&app_state) {
        Ok(value) => value,
        Err(status) => return status.into_response(),
    };
    let mut users = web_auth.t_users.clone().unwrap_or_default();
    let Some(user_index) = users.iter().position(|user| user.username.eq_ignore_ascii_case(&username)) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": format!("User '{username}' not found")}))).into_response();
    };

    if user_has_admin_group(&users[user_index]) && count_admin_users(&users) == 1 {
        return (StatusCode::CONFLICT, Json(json!({"error": "Cannot delete the last admin user"}))).into_response();
    }

    users.remove(user_index);

    let (userfile_path, _) = resolve_auth_paths(&web_auth, &config_path);
    let _lock = app_state.app_config.file_locks.write_lock(&userfile_path).await;
    if let Err(err) = write_text_file_atomic(&userfile_path, &serialize_users_file(&users)).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": err.to_string()}))).into_response();
    }
    if let Err(err) = store_reprepared_web_auth(&app_state) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": err}))).into_response();
    }

    info!("RBAC API: deleted web UI user '{username}'");
    StatusCode::OK.into_response()
}

async fn create_group(
    State(app_state): State<Arc<AppState>>,
    Json(request): Json<CreateGroupRequest>,
) -> impl IntoResponse {
    let name = normalize_name(&request.name);
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "Group name cannot be empty"}))).into_response();
    }
    if name.eq_ignore_ascii_case("admin") {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "Group 'admin' is reserved"}))).into_response();
    }
    if let Err(domains) = validate_permission_dependencies(&request.permissions) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("Write permissions require matching read permissions for: {}", domains.join(", "))})),
        )
            .into_response();
    }

    let (web_auth, config_path) = match current_web_auth_snapshot(&app_state) {
        Ok(value) => value,
        Err(status) => return status.into_response(),
    };
    let mut groups = web_auth.t_groups.clone().unwrap_or_default();
    if groups.iter().any(|group| group.name.eq_ignore_ascii_case(&name)) {
        return (StatusCode::CONFLICT, Json(json!({"error": format!("Group '{name}' already exists")}))).into_response();
    }

    groups.push(RbacGroup {
        name: name.clone(),
        permissions: normalize_permissions(&request.permissions),
    });

    let (_, groupfile_path) = resolve_auth_paths(&web_auth, &config_path);
    let _lock = app_state.app_config.file_locks.write_lock(&groupfile_path).await;
    if let Err(err) = write_text_file_atomic(&groupfile_path, &serialize_groups_file(&groups)).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": err.to_string()}))).into_response();
    }
    if let Err(err) = store_reprepared_web_auth(&app_state) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": err}))).into_response();
    }

    info!("RBAC API: created group '{name}'");
    StatusCode::CREATED.into_response()
}

async fn update_group(
    State(app_state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(request): Json<CreateGroupRequest>,
) -> impl IntoResponse {
    let name = normalize_name(&name);
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "Group name cannot be empty"}))).into_response();
    }
    if name.eq_ignore_ascii_case("admin") {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "Group 'admin' cannot be modified"}))).into_response();
    }
    if let Err(domains) = validate_permission_dependencies(&request.permissions) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("Write permissions require matching read permissions for: {}", domains.join(", "))})),
        )
            .into_response();
    }

    let (web_auth, config_path) = match current_web_auth_snapshot(&app_state) {
        Ok(value) => value,
        Err(status) => return status.into_response(),
    };
    let mut groups = web_auth.t_groups.clone().unwrap_or_default();
    let Some(group_index) = groups.iter().position(|group| group.name.eq_ignore_ascii_case(&name)) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": format!("Group '{name}' not found")}))).into_response();
    };

    groups[group_index].permissions = normalize_permissions(&request.permissions);

    let (_, groupfile_path) = resolve_auth_paths(&web_auth, &config_path);
    let _lock = app_state.app_config.file_locks.write_lock(&groupfile_path).await;
    if let Err(err) = write_text_file_atomic(&groupfile_path, &serialize_groups_file(&groups)).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": err.to_string()}))).into_response();
    }
    if let Err(err) = store_reprepared_web_auth(&app_state) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": err}))).into_response();
    }

    info!("RBAC API: updated group '{name}'");
    StatusCode::OK.into_response()
}

async fn delete_group(
    State(app_state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let name = normalize_name(&name);
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "Group name cannot be empty"}))).into_response();
    }
    if name.eq_ignore_ascii_case("admin") {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "Group 'admin' cannot be deleted"}))).into_response();
    }

    let (web_auth, config_path) = match current_web_auth_snapshot(&app_state) {
        Ok(value) => value,
        Err(status) => return status.into_response(),
    };
    let users = web_auth.t_users.clone().unwrap_or_default();
    let mut groups = web_auth.t_groups.clone().unwrap_or_default();
    let Some(group_index) = groups.iter().position(|group| group.name.eq_ignore_ascii_case(&name)) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": format!("Group '{name}' not found")}))).into_response();
    };

    let assigned_users = users
        .iter()
        .filter(|user| user.groups.iter().any(|group| group.eq_ignore_ascii_case(&name)))
        .map(|user| user.username.clone())
        .collect::<Vec<_>>();
    if !assigned_users.is_empty() {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!("Group '{name}' is still assigned to users"),
                "users": assigned_users,
            })),
        )
            .into_response();
    }

    groups.remove(group_index);

    let (_, groupfile_path) = resolve_auth_paths(&web_auth, &config_path);
    let _lock = app_state.app_config.file_locks.write_lock(&groupfile_path).await;
    if let Err(err) = write_text_file_atomic(&groupfile_path, &serialize_groups_file(&groups)).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": err.to_string()}))).into_response();
    }
    if let Err(err) = store_reprepared_web_auth(&app_state) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": err}))).into_response();
    }

    info!("RBAC API: deleted group '{name}'");
    StatusCode::OK.into_response()
}

pub fn rbac_api_register(app_state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/rbac/users", axum::routing::get(list_users))
        .route("/rbac/users", axum::routing::post(create_user))
        .route("/rbac/users/{username}", axum::routing::put(update_user))
        .route("/rbac/users/{username}", axum::routing::delete(delete_user))
        .route("/rbac/groups", axum::routing::get(list_groups))
        .route("/rbac/groups", axum::routing::post(create_group))
        .route("/rbac/groups/{name}", axum::routing::put(update_group))
        .route("/rbac/groups/{name}", axum::routing::delete(delete_group))
        .route("/rbac/permissions", axum::routing::get(list_permissions))
        .route_layer(axum::middleware::from_fn_with_state(app_state, validator_admin))
}

#[cfg(test)]
mod tests {
    use super::{resolve_auth_paths, validate_permission_dependencies};
    use crate::model::WebAuthConfig;
    use crate::utils;

    #[test]
    fn rejects_write_permission_without_matching_read_permission() {
        let result = validate_permission_dependencies(&["config.write".to_string()]);

        assert_eq!(result, Err(vec!["config".to_string()]));
    }

    #[test]
    fn resolve_auth_paths_uses_default_group_file_path_for_groups() {
        let web_auth = WebAuthConfig {
            enabled: true,
            issuer: "test".to_string(),
            secret: "secret".to_string(),
            token_ttl_mins: 60,
            userfile: None,
            groupfile: None,
            t_users: None,
            t_groups: None,
        };

        let config_path = "config";
        let (_, groupfile_path) = resolve_auth_paths(&web_auth, config_path);

        assert_eq!(groupfile_path, std::path::PathBuf::from(utils::get_default_user_group_file_path(config_path)));
    }
}
