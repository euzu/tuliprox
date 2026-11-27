use super::*;
use crate::api::test_support::stalker_portal::{spawn_stalker_mock_server_with, StalkerMockConfig, StalkerMockServer};
use axum::Router;
use futures::future::BoxFuture;
use std::sync::Mutex;
use tuliprox_session::SessionProviderHeaders;

const STALKER_VIRTUAL_ID: u32 = 101;
const STALKER_STREAM_REF: &str = "101";

type AsyncHook = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

#[derive(Clone)]
enum PlaylistReply {
    Ok { body: String, cookies: Vec<String> },
    Status(StatusCode),
    StatusAfterHook { status: StatusCode, hook: AsyncHook },
    HangUntil(Arc<tokio::sync::Notify>),
}

impl PlaylistReply {
    fn media_playlist(seq: u64) -> Self {
        Self::Ok {
            body: format!(
                "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:{seq}\n#EXTINF:4.0,\nseg-{seq}.ts\n"
            ),
            cookies: Vec::new(),
        }
    }

    fn media_playlist_with_cookies(seq: u64, cookies: Vec<String>) -> Self {
        Self::Ok {
            body: format!(
                "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:{seq}\n#EXTINF:4.0,\nseg-{seq}.ts\n"
            ),
            cookies,
        }
    }
}

struct PlaylistMock {
    base_url: String,
    replies: Arc<Mutex<HashMap<String, PlaylistReply>>>,
    counts: Arc<Mutex<HashMap<String, usize>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for PlaylistMock {
    fn drop(&mut self) { self.task.abort(); }
}

impl PlaylistMock {
    async fn spawn() -> Self {
        let replies = Arc::new(Mutex::new(HashMap::from([
            ("1".to_string(), PlaylistReply::media_playlist(1)),
            ("2".to_string(), PlaylistReply::media_playlist(2)),
        ])));
        let counts = Arc::new(Mutex::new(HashMap::new()));
        let replies_for_task = Arc::clone(&replies);
        let counts_for_task = Arc::clone(&counts);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("playlist mock binds");
        let addr = listener.local_addr().expect("playlist mock addr");
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let replies = Arc::clone(&replies_for_task);
                let counts = Arc::clone(&counts_for_task);
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 4096];
                    let Ok(read) = socket.read(&mut buf).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    let request = String::from_utf8_lossy(&buf[..read]);
                    let path = request.lines().next().and_then(|line| line.split_whitespace().nth(1)).unwrap_or("/");
                    let token = url::Url::parse(&format!("http://127.0.0.1{path}"))
                        .ok()
                        .and_then(|parsed| parsed.query_pairs().find(|(k, _)| k == "t").map(|(_, v)| v.into_owned()))
                        .unwrap_or_default();
                    {
                        let mut guard = counts.lock().expect("counts lock");
                        *guard.entry(token.clone()).or_insert(0) += 1;
                    }
                    let reply = {
                        let guard = replies.lock().expect("replies lock");
                        guard.get(&token).cloned().unwrap_or(PlaylistReply::Status(StatusCode::NOT_FOUND))
                    };
                    match reply {
                        PlaylistReply::Ok { body, cookies } => {
                            let mut headers = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\n",
                                body.len()
                            );
                            for cookie in cookies {
                                let _ = write!(headers, "Set-Cookie: {cookie}\r\n");
                            }
                            headers.push_str("Connection: close\r\n\r\n");
                            let _ = socket.write_all(headers.as_bytes()).await;
                            let _ = socket.write_all(body.as_bytes()).await;
                        }
                        PlaylistReply::Status(status) => {
                            let reason = status.canonical_reason().unwrap_or("Status");
                            let response = format!(
                                "HTTP/1.1 {} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                                status.as_u16()
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                        }
                        PlaylistReply::StatusAfterHook { status, hook } => {
                            hook().await;
                            let reason = status.canonical_reason().unwrap_or("Status");
                            let response = format!(
                                "HTTP/1.1 {} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                                status.as_u16()
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                        }
                        PlaylistReply::HangUntil(notify) => {
                            let _ = tokio::time::timeout(Duration::from_secs(5), notify.notified()).await;
                        }
                    }
                });
            }
        });
        Self { base_url: format!("http://{addr}"), replies, counts, task }
    }

    fn set_reply(&self, token: &str, reply: PlaylistReply) {
        self.replies.lock().expect("replies lock").insert(token.to_string(), reply);
    }

    fn request_count(&self, token: &str) -> usize {
        self.counts.lock().expect("counts lock").get(token).copied().unwrap_or(0)
    }
}

struct StalkerRetryFixture {
    _temp_dir: tempfile::TempDir,
    playlist: PlaylistMock,
    portal: StalkerMockServer,
    app_state: Arc<AppState>,
    input: Arc<ConfigInput>,
    target: ConfigTarget,
    user: Arc<ProxyUserCredentials>,
    fingerprint: Fingerprint,
}

impl StalkerRetryFixture {
    async fn setup(input_name: &str, same_link_on_refresh: bool) -> Self {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let playlist = PlaylistMock::spawn().await;
        let playlist_base = playlist.base_url.clone();
        let portal = spawn_stalker_mock_server_with(StalkerMockConfig {
            create_link: Some(Arc::new(move |call_index, _params| {
                let t = if same_link_on_refresh { 1 } else { call_index };
                format!("{playlist_base}/live/101.m3u8?t={t}")
            })),
        })
        .await;
        let (app_state, input, target) = test_stalker_app_state(input_name, &portal.base_url, temp_dir.path());
        import_stalker_catalog(&app_state, input_name).await;
        let user = app_state.app_config.get_user_credentials("hls-user").expect("hls-user configured");
        let fingerprint = test_fingerprint();
        Self { _temp_dir: temp_dir, playlist, portal, app_state, input, target, user, fingerprint }
    }

    async fn call_entry(&self, user_session: Option<&crate::api::model::UserSession>) -> Response<Body> {
        let entry_path =
            super::super::build_virtual_hls_entry_path(&self.target, &self.input, &self.user, STALKER_VIRTUAL_ID);
        super::super::handle_hls_stream_request(
            &self.fingerprint,
            &self.app_state,
            &self.user,
            &self.target,
            user_session,
            None,
            "http://streams.example/live/101.m3u8",
            None,
            test_hls_entry_stream_context(STALKER_VIRTUAL_ID, STALKER_STREAM_REF, None),
            &self.input,
            &HeaderMap::new(),
            UserConnectionPermission::Allowed,
            Some(ConnectionKind::Normal),
            &entry_path,
            super::super::HlsRequestStage::Entry,
        )
        .await
        .into_response()
    }

    async fn pin_initial_session(&self) -> String {
        let response = self.call_entry(None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("initial playlist utf8");
        let session_token = extract_session_token_from_playlist(&self.app_state, &body);
        assert_eq!(self.portal.create_link_call_count(), 1);
        assert_eq!(self.playlist.request_count("1"), 1);
        assert_eq!(self.playlist.request_count("2"), 0);
        let session = self
            .app_state
            .active_users
            .get_user_session(&self.user.username, &session_token)
            .await
            .expect("initial session pinned");
        assert_eq!(session.stream_url.as_ref(), self.link_url(1));
        session_token
    }

    fn link_url(&self, token_num: usize) -> String { format!("{}/live/101.m3u8?t={token_num}", self.playlist.base_url) }

    fn resource_retry_max_attempts(&self) -> usize {
        self.app_state.app_config.config.load().reverse_proxy.as_ref().map_or_else(
            || crate::model::ResourceRetryConfig::get_default_retry_values().0 as usize,
            |config| config.resource_retry.get_retry_values().0 as usize,
        )
    }
}

async fn import_stalker_catalog(app_state: &Arc<AppState>, input_name: &str) {
    let router = crate::api::endpoints::v1_api_playlist::v1_api_playlist_register_protected(Router::new())
        .with_state(Arc::clone(app_state));
    let body = serde_json::json!({ "Input": input_name }).to_string();
    let request = Request::builder()
        .method(Method::POST)
        .uri("/playlist/live")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("catalog import request builds");
    let response = router.oneshot(request).await.expect("catalog import responds");
    assert_eq!(response.status(), StatusCode::OK);
}

fn extract_session_token_from_playlist(app_state: &Arc<AppState>, playlist: &str) -> String {
    let rewritten_uri =
        playlist.lines().find(|line| line.contains("/hls/hls-user/")).expect("rewritten HLS URI in playlist");
    let token_segment = rewritten_uri.rsplit('/').next().expect("sealed token segment");
    get_hls_session_token_and_url_from_token(&app_state.get_encrypt_secret(), token_segment)
        .expect("sealed token decodes")
        .session_token
        .expect("sealed token carries session token")
}

fn assert_channel_unavailable_not_found(response: &Response<Body>) {
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response.headers().get(shared::model::HEADER_TULIPROX_FALLBACK).and_then(|value| value.to_str().ok()),
        Some("channel_unavailable")
    );
}

#[tokio::test]
async fn pinned_stalker_playlist_refreshes_once_on_403_and_updates_session_stream_url() {
    let fixture = StalkerRetryFixture::setup("stalker-retry-403", false).await;
    let session_token = fixture.pin_initial_session().await;

    fixture.playlist.set_reply("1", PlaylistReply::Status(StatusCode::FORBIDDEN));
    let session = fixture
        .app_state
        .active_users
        .get_user_session(&fixture.user.username, &session_token)
        .await
        .expect("pinned session exists");

    let response = fixture.call_entry(Some(&session)).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fixture.portal.create_link_call_count(), 2);
    assert_eq!(fixture.playlist.request_count("1"), 2);
    assert_eq!(fixture.playlist.request_count("2"), 1);

    let body = String::from_utf8(response_body(response).await.to_vec()).expect("refreshed playlist utf8");
    assert!(body.starts_with("#EXTM3U"), "{body}");
    assert!(body.contains("#EXT-X-MEDIA-SEQUENCE:2"), "{body}");
    assert!(!body.contains("\nseg-2.ts"), "segment URI must be rewritten: {body}");
    let rewritten_segment =
        body.lines().find(|line| line.contains("/hls/hls-user/")).expect("rewritten segment URI in response");
    let decoded = get_hls_session_token_and_url_from_token(
        &fixture.app_state.get_encrypt_secret(),
        rewritten_segment.rsplit('/').next().expect("segment token"),
    )
    .expect("rewritten segment token decodes");
    assert!(decoded.url.ends_with("/live/seg-2.ts"), "rewritten segment must target fresh playlist: {}", decoded.url);

    let updated_session = fixture
        .app_state
        .active_users
        .get_user_session(&fixture.user.username, &session_token)
        .await
        .expect("session remains live");
    assert_eq!(updated_session.stream_url.as_ref(), fixture.link_url(2));
}

#[tokio::test]
async fn pinned_stalker_playlist_does_not_retry_on_503() {
    let fixture = StalkerRetryFixture::setup("stalker-retry-503", false).await;
    let session_token = fixture.pin_initial_session().await;

    fixture.playlist.set_reply("1", PlaylistReply::Status(StatusCode::SERVICE_UNAVAILABLE));
    let session = fixture
        .app_state
        .active_users
        .get_user_session(&fixture.user.username, &session_token)
        .await
        .expect("pinned session exists");

    let response = fixture.call_entry(Some(&session)).await;

    assert_channel_unavailable_not_found(&response);
    assert_eq!(fixture.portal.create_link_call_count(), 1);
    // 1 initial pin request plus resource retry attempts on the cached link without re-resolving.
    assert_eq!(fixture.playlist.request_count("1"), 1 + fixture.resource_retry_max_attempts());
    assert_eq!(fixture.playlist.request_count("2"), 0);
}

#[tokio::test]
async fn pinned_stalker_playlist_does_not_retry_on_timeout() {
    let fixture = StalkerRetryFixture::setup("stalker-retry-timeout", false).await;
    let proxy = &fixture.app_state.hls.proxy;
    proxy.update_origin_preferences(
        proxy.min_origin_target_duration_secs(),
        proxy.segment_fetch_policy().timeouts.config().segment,
        50,
    );
    let session_token = fixture.pin_initial_session().await;

    let gate = Arc::new(tokio::sync::Notify::new());
    fixture.playlist.set_reply("1", PlaylistReply::HangUntil(Arc::clone(&gate)));
    let session = fixture
        .app_state
        .active_users
        .get_user_session(&fixture.user.username, &session_token)
        .await
        .expect("pinned session exists");

    let response = fixture.call_entry(Some(&session)).await;
    gate.notify_waiters();

    assert_channel_unavailable_not_found(&response);
    assert_eq!(fixture.portal.create_link_call_count(), 1);
    // 1 initial pin request plus resource retry attempts on the cached link without re-resolving.
    assert_eq!(fixture.playlist.request_count("1"), 1 + fixture.resource_retry_max_attempts());
    assert_eq!(fixture.playlist.request_count("2"), 0);
}

#[tokio::test]
async fn pinned_stalker_playlist_does_not_refetch_when_portal_returns_same_link() {
    let fixture = StalkerRetryFixture::setup("stalker-retry-same-link", true).await;
    let session_token = fixture.pin_initial_session().await;

    fixture.playlist.set_reply("1", PlaylistReply::Status(StatusCode::FORBIDDEN));
    let session = fixture
        .app_state
        .active_users
        .get_user_session(&fixture.user.username, &session_token)
        .await
        .expect("pinned session exists");

    let response = fixture.call_entry(Some(&session)).await;

    assert_channel_unavailable_not_found(&response);
    assert_eq!(fixture.portal.create_link_call_count(), 2);
    assert_eq!(fixture.playlist.request_count("1"), 2);
    assert_eq!(fixture.playlist.request_count("2"), 0);
}

#[tokio::test]
async fn pinned_stalker_playlist_does_not_overwrite_rebound_session_or_store_retry_cookies() {
    let fixture = StalkerRetryFixture::setup("stalker-retry-rebind", false).await;
    let session_token = fixture.pin_initial_session().await;

    let app_state = Arc::clone(&fixture.app_state);
    let user = Arc::clone(&fixture.user);
    let token_for_hook = session_token.clone();
    let addr = fixture.fingerprint.addr;
    let rebound_url = "http://switched-provider.example/live/999.m3u8";
    let hook: AsyncHook = Arc::new(move || {
        let app_state = Arc::clone(&app_state);
        let user = Arc::clone(&user);
        let token = token_for_hook.clone();
        Box::pin(async move {
            app_state
                .active_users
                .create_user_session(CreateUserSessionParams {
                    user: &user,
                    session_token: &token,
                    virtual_id: STALKER_VIRTUAL_ID,
                    provider: "switched-provider",
                    stream_url: rebound_url,
                    addr: &addr,
                    connection_permission: UserConnectionPermission::Allowed,
                    connection_kind: Some(ConnectionKind::Normal),
                    socket_bound: PlaylistItemType::LiveHls.uses_socket_bound_session(),
                })
                .await;
        })
    });

    fixture.playlist.set_reply("1", PlaylistReply::StatusAfterHook { status: StatusCode::FORBIDDEN, hook });
    fixture.playlist.set_reply(
        "2",
        PlaylistReply::media_playlist_with_cookies(2, vec!["retry_cookie=should_not_persist; Path=/".to_string()]),
    );
    let session = fixture
        .app_state
        .active_users
        .get_user_session(&fixture.user.username, &session_token)
        .await
        .expect("pinned session exists");

    let response = fixture.call_entry(Some(&session)).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fixture.portal.create_link_call_count(), 2);
    assert_eq!(fixture.playlist.request_count("1"), 2);
    assert_eq!(fixture.playlist.request_count("2"), 1);

    let final_session = fixture
        .app_state
        .active_users
        .get_user_session(&fixture.user.username, &session_token)
        .await
        .expect("rebound session remains live");
    assert_ne!(final_session.identity(), session.identity());
    assert_eq!(final_session.provider.as_ref(), "switched-provider");
    assert_eq!(final_session.stream_url.as_ref(), rebound_url);
    assert!(
        final_session.provider_session_cookies.is_empty(),
        "retry response cookies must not be stored after session identity change"
    );
    assert_eq!(
        fixture
            .app_state
            .active_users
            .provider_headers_for_session_identity(
                &fixture.user.username,
                &session_token,
                final_session.identity(),
                &fixture.link_url(2),
            )
            .await,
        SessionProviderHeaders::NoHeaders
    );
}
