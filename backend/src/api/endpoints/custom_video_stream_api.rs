use crate::{
    api::{api_utils::create_api_proxy_user, model::{create_custom_video_stream_response, AppState, CustomVideoStreamType}},
    auth::{verify_access_token, Fingerprint},
};
use axum::response::IntoResponse;
use std::{str::FromStr, sync::Arc};
use crate::auth::resolve_api_user_context;
use url::form_urlencoded;

async fn cvs_api(
    fingerprint: Fingerprint,
    axum::extract::Path((username, password, stream_type)): axum::extract::Path<(String, String, String)>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let cvs_type = stream_type.strip_suffix(".ts").unwrap_or(&stream_type);

    let Ok(custom_video_type) = CustomVideoStreamType::from_str(cvs_type) else {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };

    let api_proxy_user = create_api_proxy_user(&app_state);
    if username == api_proxy_user.username && password == api_proxy_user.password {
        let token = raw_query.as_deref().and_then(|query| {
            form_urlencoded::parse(query.as_bytes())
                .find_map(|(key, value)| (key == "token").then(|| value.into_owned()))
        });
        let Some(token) = token.as_deref() else {
            return app_state.app_config.get_auth_error_status().into_response();
        };
        if !verify_access_token(token, &app_state.app_config.access_token_secret) {
            return app_state.app_config.get_auth_error_status().into_response();
        }
        return create_custom_video_stream_response(&app_state, &fingerprint.addr, custom_video_type).into_response();
    }

    let Some((user, target)) = app_state.app_config.get_target_for_user(&username, &password) else {
        return app_state.app_config.get_auth_error_status().into_response();
    };

    if let Err(e) = resolve_api_user_context(user.clone(), target.clone(), fingerprint.clone(), &app_state) {
        return e.into_player_response(app_state.app_config.get_auth_error_status());
    }

    create_custom_video_stream_response(&app_state, &fingerprint.addr, custom_video_type).into_response()
}

pub fn cvs_api_register() -> axum::Router<Arc<AppState>> {
    axum::Router::new().route("/cvs/{username}/{password}/{stream_type}", axum::routing::get(cvs_api))
}

#[cfg(test)]
mod tests {
    use super::cvs_api_register;
    use crate::{
        api::model::{
            ActiveProviderManager, ActiveUserManager, AppState, CancelTokens, ConnectionManager, DownloadQueue,
            EventManager, MetadataUpdateManager, PlaylistStorageState, SharedStreamManager, TransportStreamBuffer,
            UpdateGuard,
        },
        model::{
            AppConfig, Config, ConfigInput, CustomStreamResponse, MediaToolCapabilities, SourcesConfig,
        },
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use axum::{body::Body, http::{Request, StatusCode}, Router};
    use crate::utils::{FileLockManager, GeoIp};
    use std::{collections::HashMap, sync::Arc};
    use tower::ServiceExt;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;
    use shared::model::{ConfigPaths, InputFetchMethod, InputType};

    fn create_test_app_config_with_channel_unavailable() -> AppConfig {
        let input = Arc::new(ConfigInput {
            id: 1,
            name: "provider_1".into(),
            input_type: InputType::Xtream,
            headers: HashMap::default(),
            url: "http://provider-1.example".to_string(),
            username: Some("user1".to_string()),
            password: Some("pass1".to_string()),
            enabled: true,
            priority: 0,
            max_connections: 1,
            method: InputFetchMethod::default(),
            aliases: None,
            ..ConfigInput::default()
        });
        let sources = SourcesConfig { inputs: vec![input], ..SourcesConfig::default() };

        let app_cfg = AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(sources)),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            api_proxy: Arc::new(ArcSwapOption::default()),
            file_locks: Arc::new(FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(MediaToolCapabilities::new()),
        };

        let mut ts_packet = vec![0_u8; 188];
        ts_packet[0] = 0x47;
        app_cfg.custom_stream_response.store(Some(Arc::new(CustomStreamResponse {
            channel_unavailable: Some(TransportStreamBuffer::new(ts_packet)),
            user_connections_exhausted: None,
            provider_connections_exhausted: None,
            low_priority_preempted: None,
            user_account_expired: None,
            panel_api_provisioning: None,
        })));
        app_cfg
    }

    fn create_test_app_state() -> Arc<AppState> {
        let app_cfg = Arc::new(create_test_app_config_with_channel_unavailable());
        let event_manager = Arc::new(EventManager::new());
        let active_provider = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let shared_stream_manager = Arc::new(SharedStreamManager::new(Arc::clone(&active_provider)));
        active_provider.set_shared_stream_manager(Arc::clone(&shared_stream_manager));

        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let config = app_cfg.config.load();
        let active_users = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));
        let connection_manager = Arc::new(ConnectionManager::new(
            &active_users,
            &active_provider,
            &shared_stream_manager,
            &event_manager,
            None,
        ));

        let tokens = CancelTokens {
            scheduler: CancellationToken::new(),
            hdhomerun: CancellationToken::new(),
            file_watch: CancellationToken::new(),
            provider_dns: CancellationToken::new(),
            metadata: CancellationToken::new(),
            qos_aggregation: CancellationToken::new(),
            downloads: CancellationToken::new(),
        };
        let metadata_manager = Arc::new(MetadataUpdateManager::new(tokens.metadata.clone()));
        let (manual_update_sender, _) = mpsc::channel::<Arc<crate::model::ProcessTargets>>(1);

        Arc::new(AppState {
            forced_targets: Arc::new(ArcSwap::from_pointee(crate::model::ProcessTargets {
                enabled: false,
                inputs: Vec::new(),
                targets: Vec::new(),
                target_names: Vec::new(),
            })),
            app_config: app_cfg,
            http_client: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
            http_client_no_redirect: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
            downloads: Arc::new(DownloadQueue::new()),
            cache: Arc::new(ArcSwapOption::default()),
            shared_stream_manager,
            active_users,
            active_provider,
            connection_manager,
            event_manager,
            cancel_tokens: Arc::new(ArcSwap::from_pointee(tokens)),
            playlists: Arc::new(PlaylistStorageState::new()),
            geoip,
            update_guard: UpdateGuard::new(),
            metadata_manager,
            manual_update_sender,
        })
    }

    #[tokio::test]
    async fn cvs_api_internal_api_proxy_user_returns_bad_request_for_channel_unavailable() {
        let app_state = create_test_app_state();
        let router = cvs_api_register().with_state(app_state);
        let token = crate::auth::create_access_token(&[0; 32], 30);
        let request = Request::builder()
            .method("GET")
            .uri(format!("/cvs/api_user/api_user/channel_unavailable.ts?token={token}"))
            .body(Body::empty())
            .expect("request");

        let response = Router::into_service(router).oneshot(request).await.expect("response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn cvs_api_rejects_internal_api_proxy_user_without_token() {
        let app_state = create_test_app_state();
        let router = cvs_api_register().with_state(app_state);
        let request = Request::builder()
            .method("GET")
            .uri("/cvs/api_user/api_user/channel_unavailable.ts")
            .body(Body::empty())
            .expect("request");

        let response = Router::into_service(router).oneshot(request).await.expect("response");

        assert!(response.status().is_client_error());
    }
}
