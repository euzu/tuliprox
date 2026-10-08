use crate::api::{
    api_utils::json_or_bin_response,
    auth_middleware::{rejection_for, settings_owner, validator_authenticated, VerifiedClaims},
    model::AppState,
};
use axum::{
    body::{to_bytes, Bytes},
    extract::{Path, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Extension, Router,
};
use shared::{
    model::{TableLayoutPreferencesDto, SETTINGS_MAX_REQUEST_BYTES},
    utils::{bin_deserialize, CONTENT_TYPE_CBOR},
};
use std::{path::PathBuf, sync::Arc};
use tuliprox_core::utils::{FileReadGuard, FileWriteGuard};
use tuliprox_repository::user_settings_repository::{
    load_settings_unlocked, mutate_settings_unlocked, settings_path, settings_precondition, validate_settings_table,
    SettingsError, SettingsOwner,
};

fn error_response(error: SettingsError, headers: &HeaderMap) -> Response {
    let status = StatusCode::from_u16(error.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, json_or_bin_response(accept(headers), &serde_json::json!({"error": error.to_string()}))).into_response()
}

fn accept(headers: &HeaderMap) -> Option<&str> { headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()) }

fn layout_response(layout: &TableLayoutPreferencesDto, etag: &str, headers: &HeaderMap) -> Response {
    let mut response = json_or_bin_response(accept(headers), layout).into_response();
    if let Ok(value) = HeaderValue::from_str(etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    response
}

fn owner(
    state: &AppState,
    claims: Option<&Extension<VerifiedClaims>>,
) -> Result<SettingsOwner, crate::auth::AuthError> {
    settings_owner(state, claims.map(|c| &c.0 .0))
}

async fn current_owner(
    state: &AppState,
    claims: Option<&Extension<VerifiedClaims>>,
) -> Result<SettingsOwner, crate::auth::AuthError> {
    let owner = owner(state, claims)?;
    if let Some(claims) = claims {
        if state.auth.token_revocations.is_revoked(&claims.0 .0).await {
            return Err(crate::auth::AuthError::Revoked);
        }
        if !claims.0 .0.subject_id.as_ref().is_some_and(shared::model::UserId::is_api) {
            let config = state.app_config.config.load();
            if let Some(version) = config
                .web_ui
                .as_ref()
                .and_then(|ui| ui.auth.as_ref())
                .and_then(|auth| auth.pwd_version_for(&claims.0 .0.username))
            {
                crate::auth::validate_password_version(&claims.0 .0, version)?;
            }
        }
    }
    Ok(owner)
}

enum SettingsAccessError {
    Auth(crate::auth::AuthError),
    Settings(SettingsError),
}

impl SettingsAccessError {
    fn response(self, headers: &HeaderMap) -> Response {
        match self {
            Self::Auth(error) => rejection_for(error),
            Self::Settings(error) => error_response(error, headers),
        }
    }
}

enum SettingsGuard {
    Read { _guard: FileReadGuard },
    Write { _guard: FileWriteGuard },
}

struct SettingsAccess {
    owner: SettingsOwner,
    path: PathBuf,
    _guard: SettingsGuard,
}

async fn resolve_settings_access(
    state: &AppState,
    claims: Option<&Extension<VerifiedClaims>>,
    write: bool,
) -> Result<SettingsAccess, SettingsAccessError> {
    let owner = owner(state, claims).map_err(SettingsAccessError::Auth)?;
    let config_path = state.app_config.paths.load().config_path.clone();
    let path = settings_path(std::path::Path::new(&config_path), &owner).map_err(SettingsAccessError::Settings)?;
    let guard = if write {
        SettingsGuard::Write { _guard: state.app_config.file_locks.write_lock(&path).await }
    } else {
        SettingsGuard::Read { _guard: state.app_config.file_locks.read_lock(&path).await }
    };
    let current = current_owner(state, claims).await.map_err(SettingsAccessError::Auth)?;
    if current != owner || state.app_config.paths.load().config_path != config_path {
        return Err(SettingsAccessError::Settings(SettingsError::OwnerMismatch));
    }
    Ok(SettingsAccess { owner, path, _guard: guard })
}

async fn get_settings(
    State(state): State<Arc<AppState>>,
    claims: Option<Extension<VerifiedClaims>>,
    headers: HeaderMap,
) -> Response {
    let access = match resolve_settings_access(&state, claims.as_ref(), false).await {
        Ok(access) => access,
        Err(error) => return error.response(&headers),
    };
    let SettingsAccess { owner, path, .. } = &access;
    match load_settings_unlocked(path, owner).await.and_then(|entity| entity.dto(owner)) {
        Ok(dto) => {
            let mut response = json_or_bin_response(accept(&headers), &dto).into_response();
            response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
            response
        }
        Err(err) => error_response(err, &headers),
    }
}

async fn get_table(
    State(state): State<Arc<AppState>>,
    claims: Option<Extension<VerifiedClaims>>,
    Path(table): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(err) = validate_settings_table(&table) {
        return error_response(err, &headers);
    }
    let access = match resolve_settings_access(&state, claims.as_ref(), false).await {
        Ok(access) => access,
        Err(error) => return error.response(&headers),
    };
    let SettingsAccess { owner, path, .. } = &access;
    match load_settings_unlocked(path, owner).await.and_then(|entity| {
        let layout = entity.layout(&table)?;
        let etag = entity.etag_with_layout(owner, &table, &layout)?;
        Ok((layout, etag))
    }) {
        Ok((layout, etag)) => layout_response(&layout, &etag, &headers),
        Err(err) => error_response(err, &headers),
    }
}

async fn mutate_table(
    state: Arc<AppState>,
    claims: Option<Extension<VerifiedClaims>>,
    table: String,
    headers: HeaderMap,
    body: Option<Bytes>,
) -> Response {
    let mut if_matches = headers.get_all(header::IF_MATCH).iter();
    let raw_header = match (if_matches.next(), if_matches.next()) {
        (None, _) => return error_response(SettingsError::PreconditionRequired, &headers),
        (Some(header), None) => header,
        _ => return error_response(SettingsError::PreconditionInvalid, &headers),
    };
    let expected =
        match raw_header.to_str().map_err(|_| SettingsError::PreconditionInvalid).and_then(settings_precondition) {
            Ok(value) => value,
            Err(err) => return error_response(err, &headers),
        };
    let layout = if let Some(body) = body {
        let result = if headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with(CONTENT_TYPE_CBOR))
        {
            bin_deserialize::<TableLayoutPreferencesDto>(&body).map_err(|_| SettingsError::PayloadInvalid)
        } else {
            serde_json::from_slice::<TableLayoutPreferencesDto>(&body).map_err(|_| SettingsError::PayloadInvalid)
        };
        match result {
            Ok(layout) => Some(layout),
            Err(err) => return error_response(err, &headers),
        }
    } else {
        None
    };
    let access = match resolve_settings_access(&state, claims.as_ref(), true).await {
        Ok(access) => access,
        Err(error) => return error.response(&headers),
    };
    let SettingsAccess { owner, path, .. } = &access;
    match mutate_settings_unlocked(path, owner, &table, layout.as_ref(), expected, &state.app_config.file_locks).await {
        Ok((layout, etag)) => layout_response(&layout, &etag, &headers),
        Err(err) => error_response(err, &headers),
    }
}

async fn put_table(
    State(state): State<Arc<AppState>>,
    claims: Option<Extension<VerifiedClaims>>,
    Path(table): Path<String>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    match to_bytes(body, SETTINGS_MAX_REQUEST_BYTES).await {
        Ok(body) => mutate_table(state, claims, table, parts.headers, Some(body)).await,
        Err(err) => {
            let mut source = std::error::Error::source(&err);
            let mut oversized = false;
            while let Some(error) = source {
                if error.is::<http_body_util::LengthLimitError>() {
                    oversized = true;
                    break;
                }
                source = error.source();
            }
            let (status, code) = if oversized {
                (StatusCode::PAYLOAD_TOO_LARGE, "settings_request_too_large")
            } else {
                (StatusCode::BAD_REQUEST, "settings_request_failed")
            };
            (status, json_or_bin_response(accept(&parts.headers), &serde_json::json!({"error": code}))).into_response()
        }
    }
}

async fn delete_table(
    State(state): State<Arc<AppState>>,
    claims: Option<Extension<VerifiedClaims>>,
    Path(table): Path<String>,
    headers: HeaderMap,
) -> Response {
    mutate_table(state, claims, table, headers, None).await
}

pub fn user_settings_api_register(state: &Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/me/settings", get(get_settings))
        .route("/me/settings/tables/{table}", get(get_table).put(put_table).delete(delete_table))
        .layer(axum::middleware::from_fn_with_state(Arc::clone(state), validator_authenticated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::model::create_test_app_state,
        auth::{create_jwt_api_user, create_jwt_web_user},
        model::{ApiProxyConfig, Config, WebAuthConfig, WebUiUser},
    };
    use axum::{body::Body, http::Request};
    use shared::{
        model::{
            ApiProxyConfigDto, PermissionSet, ProxyUserCredentialsDto, TargetUserDto, UserId, UserSettingsDto,
            WebAuthConfigDto, WebUiConfigDto,
        },
        utils::bin_serialize,
    };
    use tower::ServiceExt;
    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn configure_path(state: &AppState, dir: &std::path::Path) {
        let mut paths = (**state.app_config.paths.load()).clone();
        paths.config_path = dir.to_string_lossy().into_owned();
        state.app_config.paths.store(Arc::new(paths));
    }

    async fn call(
        router: &Router,
        method: &str,
        table: &str,
        etag: Option<&str>,
        body: Body,
    ) -> Result<Response, Box<dyn std::error::Error>> {
        let mut request = Request::builder().method(method).uri(format!("/me/settings/tables/{table}"));
        if let Some(etag) = etag {
            request = request.header(header::IF_MATCH, etag);
        }
        Ok(router.clone().oneshot(request.body(body)?).await?)
    }

    async fn table_etag(router: &Router, table: &str) -> Result<String, Box<dyn std::error::Error>> {
        let response = call(router, "GET", table, None, Body::empty()).await?;
        assert_eq!(response.status(), StatusCode::OK);
        Ok(response.headers().get(header::ETAG).ok_or("missing etag")?.to_str()?.to_owned())
    }

    #[tokio::test]
    async fn unregistered_tables_support_the_full_settings_lifecycle() -> TestResult {
        let dir = tempfile::tempdir()?;
        let state = create_test_app_state(Config::default());
        configure_path(&state, dir.path());
        let router = user_settings_api_register(&state).with_state(state);
        assert!(!dir.path().join("user_settings").exists());
        for table in [
            "future.inventory".to_owned(),
            format!("playlist.accounts_csv.{}", "a".repeat(64)),
            format!("future.csv.{}", "b".repeat(64)),
        ] {
            let response = call(&router, "GET", &table, None, Body::empty()).await?;
            assert_eq!(response.status(), StatusCode::OK);
            let initial = response.headers().get(header::ETAG).ok_or("missing etag")?.to_str()?.to_owned();
            assert_eq!(
                serde_json::from_slice::<TableLayoutPreferencesDto>(&to_bytes(response.into_body(), 65536).await?)?,
                TableLayoutPreferencesDto::default()
            );
            let layout = TableLayoutPreferencesDto {
                column_order: vec!["name".into(), "status".into()],
                column_visibility: [("status".into(), false)].into(),
            };
            let payload = serde_json::to_vec(&layout)?;
            let response = call(&router, "PUT", &table, Some(&initial), Body::from(payload.clone())).await?;
            assert_eq!(response.status(), StatusCode::OK);
            let saved = response.headers().get(header::ETAG).ok_or("missing etag")?.to_str()?.to_owned();
            assert_ne!(saved, initial);
            let response = call(&router, "GET", &table, None, Body::empty()).await?;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers().get(header::ETAG).ok_or("missing etag")?.to_str()?, saved);
            assert_eq!(
                serde_json::from_slice::<TableLayoutPreferencesDto>(&to_bytes(response.into_body(), 65536).await?)?,
                layout
            );
            let response = router.clone().oneshot(Request::builder().uri("/me/settings").body(Body::empty())?).await?;
            assert_eq!(response.status(), StatusCode::OK);
            let dto: UserSettingsDto = serde_json::from_slice(&to_bytes(response.into_body(), 65536).await?)?;
            assert_eq!(dto.preferences.web_ui.tables, [(table.clone(), layout)].into());
            assert_eq!(dto.section_etags, [(table.clone(), saved.clone())].into());
            assert_eq!(
                call(&router, "PUT", &table, Some(&initial), Body::from(payload)).await?.status(),
                StatusCode::PRECONDITION_FAILED
            );
            assert_eq!(call(&router, "DELETE", &table, Some(&saved), Body::empty()).await?.status(), StatusCode::OK);
            assert_eq!(table_etag(&router, &table).await?, initial);
            let response = router.clone().oneshot(Request::builder().uri("/me/settings").body(Body::empty())?).await?;
            assert_eq!(response.status(), StatusCode::OK);
            let dto: UserSettingsDto = serde_json::from_slice(&to_bytes(response.into_body(), 65536).await?)?;
            assert!(dto.preferences.web_ui.tables.is_empty());
            assert!(dto.section_etags.is_empty());
            assert_eq!(call(&router, "DELETE", &table, Some(&initial), Body::empty()).await?.status(), StatusCode::OK);
        }
        Ok(())
    }

    #[tokio::test]
    async fn invalid_table_ids_are_rejected_for_reads_and_mutations() -> TestResult {
        let dir = tempfile::tempdir()?;
        let state = create_test_app_state(Config::default());
        configure_path(&state, dir.path());
        let router = user_settings_api_register(&state).with_state(state);
        let etag = table_etag(&router, "future.table").await?;
        for table in ["x".repeat(shared::model::SETTINGS_MAX_ID_BYTES + 1), "future.%0Atable".into()] {
            for method in ["GET", "PUT", "DELETE"] {
                let response = call(&router, method, &table, Some(&etag), Body::from("{}")).await?;
                assert_eq!(response.status(), StatusCode::BAD_REQUEST);
                let body: serde_json::Value = serde_json::from_slice(&to_bytes(response.into_body(), 65536).await?)?;
                assert_eq!(body["error"], "settings_id_invalid");
            }
        }
        assert!(!dir.path().join("user_settings").exists());
        Ok(())
    }

    #[tokio::test]
    async fn settings_http_roundtrip_uses_section_etags_and_bounded_payloads() -> TestResult {
        let dir = tempfile::tempdir()?;
        let state = create_test_app_state(Config::default());
        configure_path(&state, dir.path());
        let router = user_settings_api_register(&state).with_state(state.clone());
        let response = router.clone().oneshot(Request::builder().uri("/me/settings").body(Body::empty())?).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let dto: UserSettingsDto = serde_json::from_slice(&to_bytes(response.into_body(), 65536).await?)?;
        assert!(dto.shared);
        assert!(!dir.path().join("user_settings").exists());
        assert!(dto.preferences.web_ui.tables.is_empty());
        assert!(dto.section_etags.is_empty());
        let users = table_etag(&router, "users").await?;
        let inputs = table_etag(&router, "playlist.inputs").await?;
        let payload = r#"{"column_order":["username","actions"],"column_visibility":{"password":false}}"#;
        let response = call(&router, "PUT", "users", Some(&users), Body::from(payload)).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let saved = response.headers().get(header::ETAG).ok_or("missing etag")?.to_str()?.to_owned();
        assert_ne!(saved, users);
        assert_eq!(response.headers().get(header::CACHE_CONTROL).ok_or("missing cache header")?, "private, no-store");
        assert_eq!(
            call(&router, "PUT", "playlist.inputs", Some(&inputs), Body::from(payload)).await?.status(),
            StatusCode::OK
        );
        assert_eq!(
            call(&router, "PUT", "users", Some(&users), Body::from(payload)).await?.status(),
            StatusCode::PRECONDITION_FAILED
        );
        assert_eq!(
            call(&router, "PUT", "users", None, Body::from(payload)).await?.status(),
            StatusCode::PRECONDITION_REQUIRED
        );
        assert_eq!(
            call(&router, "PUT", "users", Some("*"), Body::from(payload)).await?.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(&router, "PUT", "users", Some(&saved), Body::from(r#"{"widths":{}}"#)).await?.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(&router, "PUT", "users", Some(&saved), Body::from(vec![b' '; SETTINGS_MAX_REQUEST_BYTES + 1]))
                .await?
                .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        let layout =
            TableLayoutPreferencesDto { column_order: vec!["actions".into(), "username".into()], ..Default::default() };
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/me/settings/tables/users")
                    .header(header::IF_MATCH, &saved)
                    .header(header::CONTENT_TYPE, CONTENT_TYPE_CBOR)
                    .header(header::ACCEPT, CONTENT_TYPE_CBOR)
                    .body(Body::from(bin_serialize(&layout)?))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let saved = response.headers().get(header::ETAG).ok_or("missing etag")?.to_str()?.to_owned();
        assert_eq!(
            bin_deserialize::<TableLayoutPreferencesDto>(&to_bytes(response.into_body(), 65536).await?)?,
            layout
        );
        assert_eq!(call(&router, "DELETE", "users", Some(&saved), Body::empty()).await?.status(), StatusCode::OK);
        let response = call(&router, "GET", "users", None, Body::empty()).await?;
        assert_eq!(
            serde_json::from_slice::<TableLayoutPreferencesDto>(&to_bytes(response.into_body(), 65536).await?)?,
            TableLayoutPreferencesDto::default()
        );
        Ok(())
    }

    #[tokio::test]
    async fn body_transport_failures_are_distinguished_from_streamed_size_limits() -> TestResult {
        let dir = tempfile::tempdir()?;
        let state = create_test_app_state(Config::default());
        configure_path(&state, dir.path());
        let router = user_settings_api_register(&state).with_state(state);
        let etag = table_etag(&router, "users").await?;
        for message in ["stream interrupted", "length limit reported by transport"] {
            let body =
                Body::from_stream(futures::stream::once(
                    async move { Err::<Bytes, _>(std::io::Error::other(message)) },
                ));
            let response = call(&router, "PUT", "users", Some(&etag), body).await?;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let body: serde_json::Value = serde_json::from_slice(&to_bytes(response.into_body(), 65536).await?)?;
            assert_eq!(body["error"], "settings_request_failed");
        }
        let body = Body::from_stream(futures::stream::iter([
            Ok::<_, std::io::Error>(Bytes::from(vec![b' '; SETTINGS_MAX_REQUEST_BYTES])),
            Ok(Bytes::from_static(b" ")),
        ]));
        let response = call(&router, "PUT", "users", Some(&etag), body).await?;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body: serde_json::Value = serde_json::from_slice(&to_bytes(response.into_body(), 65536).await?)?;
        assert_eq!(body["error"], "settings_request_too_large");
        assert!(!dir.path().join("user_settings").exists());
        assert_eq!(table_etag(&router, "users").await?, etag);
        Ok(())
    }

    #[tokio::test]
    async fn malformed_and_multiple_if_match_headers_are_invalid() -> TestResult {
        let dir = tempfile::tempdir()?;
        let state = create_test_app_state(Config::default());
        configure_path(&state, dir.path());
        let router = user_settings_api_register(&state).with_state(state);
        let valid = format!("\"{}\"", "a".repeat(64));
        for method in ["PUT", "DELETE"] {
            for values in [
                vec![HeaderValue::from_str(&valid)?, HeaderValue::from_str(&valid)?],
                vec![HeaderValue::from_bytes(&[0xff])?],
                vec![HeaderValue::from_bytes(&[0xff])?, HeaderValue::from_str(&valid)?],
            ] {
                let mut request = Request::builder().method(method).uri("/me/settings/tables/users");
                for value in values {
                    request = request.header(header::IF_MATCH, value);
                }
                let response = router.clone().oneshot(request.body(Body::from("{}"))?).await?;
                assert_eq!(response.status(), StatusCode::BAD_REQUEST);
                let body: serde_json::Value = serde_json::from_slice(&to_bytes(response.into_body(), 65536).await?)?;
                assert_eq!(body["error"], "settings_precondition_invalid");
            }
        }
        assert!(!dir.path().join("user_settings").exists());
        Ok(())
    }

    #[tokio::test]
    async fn settings_are_isolated_between_authenticated_users() -> TestResult {
        let dir = tempfile::tempdir()?;
        let web_ui = WebUiConfigDto {
            auth: Some(WebAuthConfigDto {
                enabled: true,
                issuer: "settings-test".into(),
                secret: "settings-test-secret".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut config = Config { web_ui: Some((&web_ui).into()), ..Default::default() };
        let auth = config.web_ui.as_mut().and_then(|ui| ui.auth.as_mut()).ok_or("missing auth")?;
        auth.t_users = Some(
            ["Alice", "Bob"]
                .into_iter()
                .map(|username| WebUiUser {
                    username: username.into(),
                    password_hash: "password-hash".into(),
                    groups: vec![],
                })
                .collect(),
        );
        let token = |username, subject| {
            create_jwt_web_user(
                auth,
                username,
                PermissionSet::new(),
                WebAuthConfig::pwd_version_from_hash("password-hash"),
                subject,
            )
        };
        let alice_token = token("Alice", UserId::web("Alice"))?;
        let bob_token = token("Bob", UserId::web("Bob"))?;
        let mismatched_token = token("Bob", UserId::web("Alice"))?;
        let state = create_test_app_state(config);
        configure_path(&state, dir.path());
        let router = user_settings_api_register(&state).with_state(state);
        let request = |method: &str, uri: &str, token: &str, etag: Option<&str>, body: Body| {
            let mut request =
                Request::builder().method(method).uri(uri).header(header::AUTHORIZATION, format!("Bearer {token}"));
            if let Some(etag) = etag {
                request = request.header(header::IF_MATCH, etag);
            }
            request.body(body)
        };
        let table_uri = "/me/settings/tables/users";
        let response = router.clone().oneshot(request("GET", table_uri, &alice_token, None, Body::empty())?).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let initial_alice_etag = response.headers().get(header::ETAG).ok_or("missing etag")?.to_str()?.to_owned();
        let alice_layout = TableLayoutPreferencesDto {
            column_order: vec!["username".into(), "actions".into()],
            column_visibility: [("password".into(), false)].into(),
        };
        let payload = serde_json::to_vec(&alice_layout)?;
        let response = router
            .clone()
            .oneshot(request("PUT", table_uri, &alice_token, Some(&initial_alice_etag), Body::from(payload.clone()))?)
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let alice_etag = response.headers().get(header::ETAG).ok_or("missing etag")?.to_str()?.to_owned();

        let response = router
            .clone()
            .oneshot(request("GET", "/me/settings?username=Alice", &bob_token, None, Body::empty())?)
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let bob: UserSettingsDto = serde_json::from_slice(&to_bytes(response.into_body(), 65536).await?)?;
        assert!(!bob.shared);
        assert!(bob.preferences.web_ui.tables.is_empty());
        assert!(bob.section_etags.is_empty());
        let response = router.clone().oneshot(request("GET", table_uri, &bob_token, None, Body::empty())?).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let bob_etag = response.headers().get(header::ETAG).ok_or("missing etag")?.to_str()?.to_owned();
        assert_ne!(bob_etag, alice_etag);
        assert_eq!(
            serde_json::from_slice::<TableLayoutPreferencesDto>(&to_bytes(response.into_body(), 65536).await?)?,
            TableLayoutPreferencesDto::default()
        );
        for method in ["PUT", "DELETE"] {
            let response = router
                .clone()
                .oneshot(request(method, table_uri, &bob_token, Some(&alice_etag), Body::from(payload.clone()))?)
                .await?;
            assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
        }
        for (method, uri) in [("GET", "/me/settings"), ("GET", table_uri), ("PUT", table_uri), ("DELETE", table_uri)] {
            let response = router
                .clone()
                .oneshot(request(method, uri, &mismatched_token, Some(&alice_etag), Body::from(payload.clone()))?)
                .await?;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let response = router
            .clone()
            .oneshot(request("PUT", table_uri, &bob_token, Some(&bob_etag), Body::from(payload))?)
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let saved_bob_etag = response.headers().get(header::ETAG).ok_or("missing etag")?.to_str()?.to_owned();
        assert_eq!(
            router
                .clone()
                .oneshot(request("DELETE", table_uri, &bob_token, Some(&saved_bob_etag), Body::empty())?)
                .await?
                .status(),
            StatusCode::OK
        );
        let response = router.oneshot(request("GET", table_uri, &alice_token, None, Body::empty())?).await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get(header::ETAG).ok_or("missing etag")?.to_str()?, alice_etag);
        assert_eq!(
            serde_json::from_slice::<TableLayoutPreferencesDto>(&to_bytes(response.into_body(), 65536).await?)?,
            alice_layout
        );
        Ok(())
    }

    #[tokio::test]
    async fn settings_auth_is_namespace_scoped_and_rechecks_deleted_users_after_waiting() -> TestResult {
        let dir = tempfile::tempdir()?;
        let web_ui = WebUiConfigDto {
            user_ui_enabled: true,
            auth: Some(WebAuthConfigDto {
                enabled: true,
                issuer: "settings-test".into(),
                secret: "settings-test-secret".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut config = Config { web_ui: Some((&web_ui).into()), ..Default::default() };
        let auth = config.web_ui.as_mut().and_then(|ui| ui.auth.as_mut()).ok_or("missing auth")?;
        auth.t_users =
            Some(vec![WebUiUser { username: "Alice".into(), password_hash: "password-hash".into(), groups: vec![] }]);
        let subject = UserId::web("Alice");
        let token = create_jwt_web_user(
            auth,
            "aLiCe",
            PermissionSet::new(),
            WebAuthConfig::pwd_version_from_hash("password-hash"),
            subject.clone(),
        )?;
        let api_token = create_jwt_api_user(auth, "Alice", UserId::api("Alice"))?;
        let state = create_test_app_state(config);
        configure_path(&state, dir.path());
        state.app_config.api_proxy.store(Some(Arc::new(ApiProxyConfig::from(&ApiProxyConfigDto {
            user: vec![TargetUserDto {
                target: "default".into(),
                credentials: vec![ProxyUserCredentialsDto {
                    username: "Alice".into(),
                    ui_enabled: true,
                    ..Default::default()
                }],
            }],
            ..Default::default()
        }))));
        let router = user_settings_api_register(&state).with_state(state.clone());
        let request = |token: &str| {
            Request::builder()
                .uri("/me/settings")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
        };
        assert_eq!(
            router.clone().oneshot(Request::builder().uri("/me/settings").body(Body::empty())?).await?.status(),
            StatusCode::UNAUTHORIZED
        );
        let response = router.clone().oneshot(request(&token)?).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let web: UserSettingsDto = serde_json::from_slice(&to_bytes(response.into_body(), 65536).await?)?;
        let response = router.clone().oneshot(request(&api_token)?).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let api: UserSettingsDto = serde_json::from_slice(&to_bytes(response.into_body(), 65536).await?)?;
        assert!(api.section_etags.is_empty());
        assert!(web.section_etags.is_empty());
        let table_request = |token: &str| {
            Request::builder()
                .uri("/me/settings/tables/users")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
        };
        let response = router.clone().oneshot(table_request(&token)?).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let etag = response.headers().get(header::ETAG).ok_or("missing etag")?.to_str()?.to_owned();
        let response = router.clone().oneshot(table_request(&api_token)?).await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_ne!(response.headers().get(header::ETAG).ok_or("missing etag")?.to_str()?, etag);
        let owner = SettingsOwner::authenticated(&subject, "Alice")?;
        let path = settings_path(dir.path(), &owner)?;
        let save_request = || {
            Request::builder()
                .method("PUT")
                .uri("/me/settings/tables/users")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::IF_MATCH, &etag)
                .body(Body::from(r#"{"column_order":["username"]}"#))
        };
        let saved = router.clone().oneshot(save_request()?).await?;
        assert_eq!(saved.status(), StatusCode::OK);
        assert!(path.exists());
        let saved_etag = saved.headers().get(header::ETAG).ok_or("missing etag")?.to_str()?;
        let guard = state.app_config.file_locks.write_lock(&path).await;
        let mut response = tokio::spawn(
            router.clone().oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/me/settings/tables/users")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::IF_MATCH, saved_etag)
                    .body(Body::from(r#"{"column_order":["actions","username"]}"#))?,
            ),
        );
        assert!(tokio::time::timeout(std::time::Duration::from_millis(20), &mut response).await.is_err());
        let mut config = (**state.app_config.config.load()).clone();
        config.web_ui.as_mut().and_then(|ui| ui.auth.as_mut()).ok_or("missing auth")?.t_users = Some(vec![]);
        state.app_config.config.store(Arc::new(config));
        tuliprox_repository::user_settings_repository::remove_settings_unlocked(&path, &owner).await?;
        drop(guard);
        assert_eq!(response.await??.status(), StatusCode::UNAUTHORIZED);
        assert!(!path.exists());
        assert_eq!(router.clone().oneshot(request(&api_token)?).await?.status(), StatusCode::OK);
        let current_config = (**state.app_config.config.load()).clone();
        let mut disabled = current_config.clone();
        disabled.web_ui.as_mut().ok_or("missing ui")?.user_ui_enabled = false;
        state.app_config.config.store(Arc::new(disabled));
        assert_eq!(router.clone().oneshot(request(&api_token)?).await?.status(), StatusCode::FORBIDDEN);
        state.app_config.config.store(Arc::new(current_config));
        let api_owner = SettingsOwner::authenticated(&UserId::api("Alice"), "Alice")?;
        let api_path = settings_path(dir.path(), &api_owner)?;
        let guard = state.app_config.file_locks.write_lock(&api_path).await;
        let mut response = tokio::spawn(router.clone().oneshot(request(&api_token)?));
        assert!(tokio::time::timeout(std::time::Duration::from_millis(20), &mut response).await.is_err());
        state.auth.token_revocations.revoke_subject(&UserId::api("Alice"), i64::MAX).await?;
        drop(guard);
        assert_eq!(response.await??.status(), StatusCode::UNAUTHORIZED);
        assert!(!api_path.exists());
        state.app_config.api_proxy.store(None);
        assert_eq!(router.clone().oneshot(request(&api_token)?).await?.status(), StatusCode::UNAUTHORIZED);
        Ok(())
    }
}
