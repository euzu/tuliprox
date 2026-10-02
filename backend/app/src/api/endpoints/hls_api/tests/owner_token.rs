//! Owner token flow for proxied live HLS with the shared HLS cache off: the entry route wraps a
//! media playlist in a single-variant master whose sealed variant URI keeps the playback owner
//! stable across client IP changes.

use super::*;
use crate::processing::parser::hls::{
    create_hls_resource_token, get_hls_session_token_and_url_from_token, HlsManifestSource, HlsResourceKind,
};
use std::sync::Mutex;

const VIRTUAL_ID: u32 = 12345;
const STREAM_REF: &str = "channel-a";
const MEDIA_PLAYLIST: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:7\n#EXTINF:2.0,\nseg-7.ts\n";
const MASTER_PLAYLIST: &str = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"en\",URI=\"tracks-a1/index.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=800000,AUDIO=\"aud\"\ntracks-v1/index.m3u8\n";

type OriginReply = (StatusCode, Vec<(String, String)>, Vec<u8>);
type OriginHandler = Arc<dyn Fn(&str) -> OriginReply + Send + Sync>;

/// Test origin that records every request head and answers by path.
struct RecordingOrigin {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for RecordingOrigin {
    fn drop(&mut self) { self.task.abort(); }
}

impl RecordingOrigin {
    fn requests(&self) -> Vec<String> { self.requests.lock().expect("requests lock").clone() }

    fn manifest_requests(&self) -> usize {
        self.requests().iter().filter(|request| request_path(request).contains(".m3u8")).count()
    }
}

fn request_path(request: &str) -> &str {
    request.lines().next().and_then(|line| line.split_whitespace().nth(1)).unwrap_or("/")
}

async fn spawn_recording_origin(handler: OriginHandler) -> RecordingOrigin {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_for_task = Arc::clone(&requests);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
    let addr = listener.local_addr().expect("local addr");
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let handler = Arc::clone(&handler);
            let requests = Arc::clone(&requests_for_task);
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 4096];
                let Ok(read) = socket.read(&mut buf).await else {
                    return;
                };
                if read == 0 {
                    return;
                }
                let request = String::from_utf8_lossy(&buf[..read]).to_string();
                let (status, headers, body) = handler(request_path(&request));
                requests.lock().expect("requests lock").push(request);
                let reason = status.canonical_reason().unwrap_or("Status");
                let extra_headers = headers.iter().fold(String::new(), |mut acc, (name, value)| {
                    let _ = write!(acc, "{name}: {value}\r\n");
                    acc
                });
                let response = format!(
                    "HTTP/1.1 {} {reason}\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    status.as_u16(),
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            });
        }
    });
    RecordingOrigin { base_url: format!("http://{addr}"), requests, task }
}

fn playlist_origin_handler(playlist: &'static str) -> OriginHandler {
    Arc::new(move |path| {
        if path.contains(".m3u8") || path.contains("timeshift") {
            (StatusCode::OK, Vec::new(), playlist.as_bytes().to_vec())
        } else {
            (StatusCode::OK, Vec::new(), b"segment".to_vec())
        }
    })
}

struct OwnerTokenFixture {
    origin: RecordingOrigin,
    app_state: Arc<AppState>,
    user: Arc<ProxyUserCredentials>,
    target: Arc<ConfigTarget>,
    input: ConfigInput,
    entry_url: String,
}

async fn owner_token_fixture(handler: OriginHandler) -> OwnerTokenFixture {
    owner_token_fixture_with(handler, |_| {}).await
}

async fn owner_token_fixture_with(
    handler: OriginHandler,
    configure: impl FnOnce(&mut ConfigInput),
) -> OwnerTokenFixture {
    let origin = spawn_recording_origin(handler).await;
    let mut input = ConfigInput {
        id: 1,
        name: Arc::from("owner-token-input"),
        input_type: InputType::Xtream,
        url: origin.base_url.clone(),
        username: Some("user".to_string()),
        password: Some("pass".to_string()),
        max_connections: 2,
        enabled: true,
        ..ConfigInput::default()
    };
    configure(&mut input);
    let mut target = test_m3u_hls_share_target();
    target.name = "default".to_string();
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    enable_channel_unavailable_custom_response(&app_state);
    configure_default_test_server(&app_state);
    store_test_sources_with_target(&app_state, input.clone(), target.clone());
    let entry_url = format!("{}/live/user/pass/{VIRTUAL_ID}.m3u8", origin.base_url);
    cache_test_m3u_hls_item(&app_state, &target, test_m3u_hls_item(&input, VIRTUAL_ID, STREAM_REF, &entry_url)).await;
    let user = app_state.app_config.get_user_credentials("hls-user").expect("test user should exist");
    OwnerTokenFixture { origin, app_state, user, target: Arc::new(target), input, entry_url }
}

fn client(ip: &str, port: u16) -> Fingerprint {
    Fingerprint::new(format!("{ip}|test-player"), ip.to_string(), test_addr_with_port(port))
}

impl OwnerTokenFixture {
    async fn entry(&self, fingerprint: &Fingerprint) -> Response<Body> {
        let entry_path = super::super::build_virtual_hls_entry_path(&self.target, &self.input, &self.user, VIRTUAL_ID);
        super::super::handle_hls_stream_request(
            fingerprint,
            &self.app_state,
            &self.user,
            &self.target,
            None,
            None,
            &self.entry_url,
            None,
            test_hls_entry_stream_context(VIRTUAL_ID, STREAM_REF, Some(2_500_000)),
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

    async fn entry_with_archive(&self, fingerprint: &Fingerprint, archive_reference: i64) -> Response<Body> {
        let entry_path = super::super::build_virtual_hls_entry_path(&self.target, &self.input, &self.user, VIRTUAL_ID);
        super::super::handle_hls_stream_request(
            fingerprint,
            &self.app_state,
            &self.user,
            &self.target,
            None,
            None,
            &self.entry_url,
            Some(archive_reference),
            test_hls_entry_stream_context(VIRTUAL_ID, STREAM_REF, None),
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

    async fn token_request(&self, fingerprint: &Fingerprint, token: &str, headers: HeaderMap) -> Response<Body> {
        super::super::hls_api_stream_resolved(
            fingerprint.clone(),
            headers,
            Arc::clone(&self.app_state),
            Arc::clone(&self.user),
            Arc::clone(&self.target),
            self.input.id,
            VIRTUAL_ID,
            token.to_string(),
        )
        .await
    }

    fn seal(&self, session_token: Option<&str>, url: &str, kind: HlsResourceKind, origin: Option<&str>) -> String {
        create_hls_resource_token(&self.app_state.get_encrypt_secret(), session_token, url, Some(kind), origin)
    }

    fn decode(&self, token: &str) -> crate::processing::parser::hls::HlsResourceToken {
        get_hls_session_token_and_url_from_token(&self.app_state.get_encrypt_secret(), token).expect("token decodes")
    }
}

fn token_of(uri: &str) -> &str { uri.rsplit('/').next().expect("uri has a token") }

async fn body_text(response: Response<Body>) -> String {
    String::from_utf8(response_body(response).await.to_vec()).expect("utf8 body")
}

async fn wrapped_entry(fixture: &OwnerTokenFixture, fingerprint: &Fingerprint) -> (u32, String, String) {
    let response = fixture.entry(fingerprint).await;
    assert_eq!(response.status(), StatusCode::OK);
    let (bandwidth, variant_uri) = single_variant_master_playlist(response).await;
    let token = token_of(&variant_uri).to_string();
    let decoded = fixture.decode(&token);
    let session_token = decoded.session_token.clone().expect("variant token carries the session token");
    assert_eq!(decoded.kind, Some(HlsResourceKind::Manifest(HlsManifestSource::Entry)));
    assert!(decoded.origin_provider.is_some());
    assert_eq!(decoded.url, fixture.entry_url);
    assert!(std::path::Path::new(&token).extension().is_some_and(|ext| ext == "m3u8"));
    (bandwidth, token, session_token)
}

#[tokio::test]
async fn entry_wraps_media_playlist_and_owner_survives_client_ip_change() {
    let fixture = owner_token_fixture(playlist_origin_handler(MEDIA_PLAYLIST)).await;
    let ip1 = client("10.0.0.1", 50_001);
    let ip2 = client("10.0.0.2", 50_002);

    let (bandwidth, token, session_token) = wrapped_entry(&fixture, &ip1).await;
    assert_eq!(bandwidth, 3_000_000, "BANDWIDTH comes from the item bitrate with headroom");
    assert_eq!(fixture.origin.manifest_requests(), 1);
    assert!(session_token.starts_with("10.0.0.1|test-player|hls-user|12345|hls|"));
    assert_eq!(fixture.decode(&token).origin_provider.as_deref(), Some(fixture.input.name.as_ref()));

    // Variant from the second IP is served from the hand-off cache: no second upstream fetch.
    let variant = fixture.token_request(&ip2, &token, HeaderMap::new()).await;
    assert_eq!(variant.status(), StatusCode::OK);
    let body = body_text(variant).await;
    assert!(body.contains("#EXT-X-TARGETDURATION:2"), "{body}");
    assert!(body.lines().any(|line| line.contains("/hls/hls-user/hls-pass/")), "{body}");
    assert_eq!(fixture.origin.manifest_requests(), 1);

    // Refreshes alternate between both IPs; each fetches upstream with the same owner.
    for (round, fingerprint) in [&ip1, &ip2, &ip1].into_iter().enumerate() {
        let refresh = fixture.token_request(fingerprint, &token, HeaderMap::new()).await;
        assert_eq!(refresh.status(), StatusCode::OK);
        assert_eq!(fixture.origin.manifest_requests(), round + 2);
    }

    assert!(fixture.app_state.active_users.get_and_update_user_session("hls-user", &session_token).await.is_some());
    assert!(fixture.app_state.active_provider.binding_tag_for_owner(&session_token).is_some());
    let foreign_owner = "10.0.0.2|test-player|hls-user|12345";
    assert!(fixture.app_state.active_provider.binding_tag_for_owner(foreign_owner).is_none());
    assert_eq!(fixture.app_state.active_users.user_connections("hls-user").await, 0);
}

#[tokio::test]
async fn second_variant_request_after_handoff_fetches_upstream() {
    let fixture = owner_token_fixture(playlist_origin_handler(MEDIA_PLAYLIST)).await;
    let ip1 = client("10.0.0.1", 50_011);
    let (_, token, session_token) = wrapped_entry(&fixture, &ip1).await;

    assert_eq!(fixture.token_request(&ip1, &token, HeaderMap::new()).await.status(), StatusCode::OK);
    assert_eq!(fixture.origin.manifest_requests(), 1);
    assert_eq!(fixture.token_request(&ip1, &token, HeaderMap::new()).await.status(), StatusCode::OK);
    assert_eq!(fixture.origin.manifest_requests(), 2);
    assert!(fixture.app_state.hls.playlist_handoff.take(&session_token, &fixture.entry_url).is_none());
}

#[tokio::test]
async fn upstream_master_playlist_is_not_wrapped_and_children_are_manifest_child() {
    let fixture = owner_token_fixture(playlist_origin_handler(MASTER_PLAYLIST)).await;
    let response = fixture.entry(&client("10.0.0.1", 50_021)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_text(response).await;
    assert!(body.contains("#EXT-X-MEDIA:TYPE=AUDIO"), "{body}");
    let variant = body.lines().find(|line| line.contains("/hls/hls-user/")).expect("variant line");
    let decoded = fixture.decode(token_of(variant));
    assert_eq!(decoded.kind, Some(HlsResourceKind::Manifest(HlsManifestSource::Child)));
    assert_eq!(decoded.origin_provider.as_deref(), Some(fixture.input.name.as_ref()));
}

#[tokio::test]
async fn wrapper_switch_off_returns_media_playlist_directly() {
    let fixture = owner_token_fixture(playlist_origin_handler(MEDIA_PLAYLIST)).await;
    fixture.app_state.app_config.config.store(Arc::new(Config {
        custom_stream_response_enabled: true,
        reverse_proxy: Some(ReverseProxyConfig::from(&ReverseProxyConfigDto {
            stream: Some(StreamConfigDto { hls_wrap_media_playlist: false, ..StreamConfigDto::default() }),
            ..Default::default()
        })),
        ..Default::default()
    }));
    let response = fixture.entry(&client("10.0.0.1", 50_031)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_text(response).await;
    assert!(body.contains("#EXT-X-TARGETDURATION:2"), "{body}");
    assert!(!body.contains("#EXT-X-STREAM-INF"), "{body}");
}

#[tokio::test]
async fn invalid_upstream_playlist_is_a_failed_manifest() {
    let handler: OriginHandler =
        Arc::new(|_| (StatusCode::OK, Vec::new(), b"<html><body>maintenance</body></html>".to_vec()));
    let fixture = owner_token_fixture(handler).await;
    let response = fixture.entry(&client("10.0.0.1", 50_041)).await;
    assert_eq!(response.status(), StatusCode::OK, "channel-unavailable manifest is rendered inline");
    let body = body_text(response).await;
    assert!(!body.contains("<html>"), "{body}");
    assert!(body.starts_with("#EXTM3U"), "{body}");
    let owner = "10.0.0.1|test-player|hls-user|12345";
    assert!(fixture.app_state.active_provider.binding_tag_for_owner(owner).is_none());
}

#[tokio::test]
async fn manifest_token_with_ts_url_and_range_is_served_as_playlist() {
    let fixture = owner_token_fixture(playlist_origin_handler(MEDIA_PLAYLIST)).await;
    let ip1 = client("10.0.0.1", 50_051);
    let (_, token, session_token) = wrapped_entry(&fixture, &ip1).await;
    assert_eq!(fixture.token_request(&ip1, &token, HeaderMap::new()).await.status(), StatusCode::OK);

    let timeshift_url = format!("{}/channel/timeshift_abs-1785136500.ts", fixture.origin.base_url);
    let manifest_token = fixture.seal(
        Some(&session_token),
        &timeshift_url,
        HlsResourceKind::Manifest(HlsManifestSource::Child),
        Some(fixture.input.name.as_ref()),
    );
    let mut headers = HeaderMap::new();
    headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-"));
    let response = fixture.token_request(&ip1, &manifest_token, headers).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_text(response).await;
    assert!(body.starts_with("#EXTM3U"), "{body}");
    assert!(body.contains("/hls/hls-user/hls-pass/"), "rewritten as a playlist: {body}");
    assert_eq!(fixture.app_state.active_users.user_connections("hls-user").await, 0, "Prepare consumes no slot");
}

#[tokio::test]
async fn child_manifest_from_another_provider_is_rejected_without_ending_the_session() {
    let fixture = owner_token_fixture(playlist_origin_handler(MEDIA_PLAYLIST)).await;
    let ip1 = client("10.0.0.1", 50_061);
    let (_, token, session_token) = wrapped_entry(&fixture, &ip1).await;
    assert_eq!(fixture.token_request(&ip1, &token, HeaderMap::new()).await.status(), StatusCode::OK);

    let child_url = format!("{}/cdn/child.m3u8", fixture.origin.base_url);
    let stale_child = fixture.seal(
        Some(&session_token),
        &child_url,
        HlsResourceKind::Manifest(HlsManifestSource::Child),
        Some("another-account"),
    );
    let response = fixture.token_request(&ip1, &stale_child, HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(!fixture.origin.requests().iter().any(|request| request_path(request).contains("/cdn/child")));
    assert!(fixture.app_state.active_users.get_and_update_user_session("hls-user", &session_token).await.is_some());
    assert_eq!(fixture.token_request(&ip1, &token, HeaderMap::new()).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn child_manifest_from_session_provider_is_fetched_exactly_as_sealed() {
    let fixture = owner_token_fixture(playlist_origin_handler(MEDIA_PLAYLIST)).await;
    let ip1 = client("10.0.0.1", 50_071);
    let (_, token, session_token) = wrapped_entry(&fixture, &ip1).await;
    assert_eq!(fixture.token_request(&ip1, &token, HeaderMap::new()).await.status(), StatusCode::OK);

    let child_url = format!("{}/cdn/edge/child.m3u8?sig=abc", fixture.origin.base_url);
    let child = fixture.seal(
        Some(&session_token),
        &child_url,
        HlsResourceKind::Manifest(HlsManifestSource::Child),
        Some(fixture.input.name.as_ref()),
    );
    let response = fixture.token_request(&client("10.0.0.2", 50_072), &child, HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(fixture.origin.requests().iter().any(|request| request_path(request) == "/cdn/edge/child.m3u8?sig=abc"));
}

#[tokio::test]
async fn stored_provider_session_cookie_is_sent_on_variant_manifest_fetch() {
    let handler: OriginHandler = Arc::new(|path| {
        let headers = if path.contains(".m3u8") {
            vec![("Set-Cookie".to_string(), "sid=abc; Path=/".to_string())]
        } else {
            Vec::new()
        };
        (StatusCode::OK, headers, MEDIA_PLAYLIST.as_bytes().to_vec())
    });
    let fixture = owner_token_fixture(handler).await;
    let ip1 = client("10.0.0.1", 50_081);
    let (_, token, _) = wrapped_entry(&fixture, &ip1).await;
    assert_eq!(fixture.token_request(&ip1, &token, HeaderMap::new()).await.status(), StatusCode::OK);
    assert_eq!(fixture.token_request(&ip1, &token, HeaderMap::new()).await.status(), StatusCode::OK);

    let requests = fixture.origin.requests();
    assert_eq!(requests.len(), 2);
    assert!(!requests[0].to_ascii_lowercase().contains("cookie: sid=abc"));
    assert!(requests[1].to_ascii_lowercase().contains("cookie: sid=abc"), "{}", requests[1]);
}

#[tokio::test]
async fn missing_session_is_recreated_from_entry_token_with_the_old_owner() {
    let fixture = owner_token_fixture(playlist_origin_handler(MEDIA_PLAYLIST)).await;
    let ip1 = client("10.0.0.1", 50_091);
    let ip2 = client("10.0.0.2", 50_092);
    let (_, token, session_token) = wrapped_entry(&fixture, &ip1).await;
    assert!(fixture.app_state.active_users.terminate_session("hls-user", &session_token).await);

    let response = fixture.token_request(&ip2, &token, HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_text(response).await;
    assert!(body.contains("#EXT-X-TARGETDURATION:2"), "recreate never serves the stale hand-off entry: {body}");
    assert_eq!(fixture.origin.manifest_requests(), 2);
    assert!(fixture.app_state.active_users.get_and_update_user_session("hls-user", &session_token).await.is_some());
    let foreign_owner = "10.0.0.2|test-player|hls-user|12345";
    assert!(fixture.app_state.active_provider.binding_tag_for_owner(foreign_owner).is_none());
}

#[tokio::test]
async fn recreate_rejects_hint_of_another_user_agent_or_channel() {
    let fixture = owner_token_fixture(playlist_origin_handler(MEDIA_PLAYLIST)).await;
    let manifest = HlsResourceKind::Manifest(HlsManifestSource::Entry);
    for hint in [
        "10.0.0.1|other-player|hls-user|12345|hls|abcdefghijklmnop",
        "10.0.0.1|test-player|hls-user|99999|hls|abcdefghijklmnop",
        "10.0.0.1|test-player|other-user|12345|hls|abcdefghijklmnop",
        "10.0.0.1|test-player|hls-user|12345",
    ] {
        let token = fixture.seal(Some(hint), &fixture.entry_url, manifest, Some(fixture.input.name.as_ref()));
        let response = fixture.token_request(&client("10.0.0.2", 50_101), &token, HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{hint}");
    }
    assert_eq!(fixture.origin.manifest_requests(), 0);
}

#[tokio::test]
async fn recreate_runs_entry_content_access_checks() {
    let fixture = owner_token_fixture(playlist_origin_handler(MEDIA_PLAYLIST)).await;
    let ip1 = client("10.0.0.1", 50_111);
    let (_, token, session_token) = wrapped_entry(&fixture, &ip1).await;
    assert!(fixture.app_state.active_users.terminate_session("hls-user", &session_token).await);

    let mut restricted = (*fixture.user).clone();
    restricted.output_clusters = shared::model::ClusterFlags::Vod;
    let response = super::super::hls_api_stream_resolved(
        ip1.clone(),
        HeaderMap::new(),
        Arc::clone(&fixture.app_state),
        Arc::new(restricted),
        Arc::clone(&fixture.target),
        fixture.input.id,
        VIRTUAL_ID,
        token,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "channel-unavailable manifest is rendered inline");
    assert_eq!(fixture.origin.manifest_requests(), 1);
    assert!(fixture.app_state.active_users.get_and_update_user_session("hls-user", &session_token).await.is_none());
}

#[tokio::test]
async fn tokens_without_entry_kind_keep_bad_request_without_session() {
    let fixture = owner_token_fixture(playlist_origin_handler(MEDIA_PLAYLIST)).await;
    let hint = "10.0.0.1|test-player|hls-user|12345|hls|abcdefghijklmnop";
    let origin = Some(fixture.input.name.as_ref());
    let child_url = format!("{}/cdn/child.m3u8", fixture.origin.base_url);
    let segment_url = format!("{}/channel/seg-7.ts", fixture.origin.base_url);
    for token in [
        fixture.seal(Some(hint), &child_url, HlsResourceKind::Manifest(HlsManifestSource::Child), origin),
        fixture.seal(Some(hint), &segment_url, HlsResourceKind::Media, origin),
        fixture.seal(None, &fixture.entry_url, HlsResourceKind::Manifest(HlsManifestSource::Entry), origin),
    ] {
        let response = fixture.token_request(&client("10.0.0.1", 50_121), &token, HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    assert!(fixture.origin.requests().is_empty());
}

#[tokio::test]
async fn shared_hls_cache_on_never_recreates_from_token() {
    let fixture = owner_token_fixture(playlist_origin_handler(MEDIA_PLAYLIST)).await;
    enable_hls_cache(&fixture.app_state);
    let hint = "10.0.0.1|test-player|hls-user|12345|hls|abcdefghijklmnop";
    let token = fixture.seal(
        Some(hint),
        &fixture.entry_url,
        HlsResourceKind::Manifest(HlsManifestSource::Entry),
        Some(fixture.input.name.as_ref()),
    );
    let response = fixture.token_request(&client("10.0.0.1", 50_131), &token, HeaderMap::new()).await;
    assert_ne!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(fixture.origin.requests().is_empty());
    assert!(fixture.app_state.active_users.get_and_update_user_session("hls-user", hint).await.is_none());
}

#[tokio::test]
async fn ended_session_is_not_recreated_from_token() {
    let fixture = owner_token_fixture(playlist_origin_handler(MEDIA_PLAYLIST)).await;
    let ip1 = client("10.0.0.1", 50_141);
    let (_, token, session_token) = wrapped_entry(&fixture, &ip1).await;
    assert_eq!(fixture.token_request(&ip1, &token, HeaderMap::new()).await.status(), StatusCode::OK);

    // Eviction, kick and explicit terminate mark the session ended (see the session crate tests).
    assert!(fixture.app_state.active_users.terminate_session("hls-user", &session_token).await);
    fixture.app_state.active_users.mark_session_ended(&session_token).await;
    let requests_before = fixture.origin.manifest_requests();

    let response = fixture.token_request(&client("10.0.0.2", 50_142), &token, HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(fixture.origin.manifest_requests(), requests_before);
    assert!(fixture.app_state.active_users.get_and_update_user_session("hls-user", &session_token).await.is_none());
}

#[tokio::test]
async fn invalid_body_on_variant_refresh_keeps_the_session() {
    let broken = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let broken_for_handler = Arc::clone(&broken);
    let handler: OriginHandler = Arc::new(move |_| {
        let body: &[u8] =
            if broken_for_handler.load(Ordering::SeqCst) { b"<html>busy</html>" } else { MEDIA_PLAYLIST.as_bytes() };
        (StatusCode::OK, Vec::new(), body.to_vec())
    });
    let fixture = owner_token_fixture(handler).await;
    let ip1 = client("10.0.0.1", 50_151);
    let (_, token, session_token) = wrapped_entry(&fixture, &ip1).await;
    assert_eq!(fixture.token_request(&ip1, &token, HeaderMap::new()).await.status(), StatusCode::OK);

    broken.store(true, Ordering::SeqCst);
    let response = fixture.token_request(&ip1, &token, HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(fixture.app_state.active_users.get_and_update_user_session("hls-user", &session_token).await.is_some());

    broken.store(false, Ordering::SeqCst);
    assert_eq!(fixture.token_request(&ip1, &token, HeaderMap::new()).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn nested_chunklist_playlist_is_not_wrapped_and_children_stay_manifests() {
    let fixture = owner_token_fixture(playlist_origin_handler("#EXTM3U\n#EXTINF:-1,\nchunklist_b800.m3u8\n")).await;
    let response = fixture.entry(&client("10.0.0.1", 50_161)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_text(response).await;
    assert!(!body.contains("#EXT-X-STREAM-INF"), "not wrapped: {body}");
    let chunklist = body.lines().find(|line| line.contains("/hls/hls-user/")).expect("chunklist line");
    assert_eq!(fixture.decode(token_of(chunklist)).kind, Some(HlsResourceKind::Manifest(HlsManifestSource::Child)));
}

#[tokio::test]
async fn wrapper_seals_the_entry_url_not_the_account_resolved_url() {
    let fixture = owner_token_fixture_with(playlist_origin_handler(MEDIA_PLAYLIST), |input| {
        input.priority = 10;
        input.aliases = Some(vec![crate::model::ConfigInputAlias {
            id: 2,
            name: Arc::from("owner-token-alias"),
            url: input.url.clone(),
            username: Some("alias-user".to_string()),
            password: Some("alias-pass".to_string()),
            priority: 0,
            max_connections: 2,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]);
    })
    .await;
    let response = fixture.entry(&client("10.0.0.1", 50_171)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let (_, variant_uri) = single_variant_master_playlist(response).await;
    let decoded = fixture.decode(token_of(&variant_uri));

    let fetched = fixture.origin.requests();
    assert!(
        fetched.iter().any(|request| request_path(request).contains("/alias-user/alias-pass/")),
        "entry is served by the alias account: {fetched:?}"
    );
    assert_eq!(decoded.url, fixture.entry_url, "sealed URL stays the canonical entry URL");
    assert_eq!(decoded.origin_provider.as_deref(), Some("owner-token-alias"));
}

fn enable_user_limits(app_state: &Arc<AppState>, strategies: Vec<shared::model::AdmissionStrategy>) {
    app_state.app_config.config.store(Arc::new(Config {
        custom_stream_response_enabled: true,
        user_access_control: true,
        reverse_proxy: Some(ReverseProxyConfig::from(&ReverseProxyConfigDto {
            stream: Some(StreamConfigDto {
                admission_strategies: Some(strategies),
                // No reentry window: only the ended-session marker keeps an evicted playback out.
                recent_eviction_reentry_ttl_ms: 0,
                ..StreamConfigDto::default()
            }),
            ..Default::default()
        })),
        ..Default::default()
    }));
}

async fn first_segment_token(fixture: &OwnerTokenFixture, fingerprint: &Fingerprint, variant_token: &str) -> String {
    let response = fixture.token_request(fingerprint, variant_token, HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_text(response).await;
    let segment = body.lines().find(|line| line.contains("/hls/hls-user/")).expect("segment line");
    token_of(segment).to_string()
}

fn other_device(port: u16) -> Fingerprint {
    Fingerprint::new("10.0.0.9|other-player".to_string(), "10.0.0.9".to_string(), test_addr_with_port(port))
}

/// Starts playback for a device and returns (variant token, session token, open segment response).
async fn start_playback(fixture: &OwnerTokenFixture, device: &Fingerprint) -> (String, String, Response<Body>) {
    let (_, variant_token, session_token) = wrapped_entry(fixture, device).await;
    let segment_token = first_segment_token(fixture, device, &variant_token).await;
    let segment = fixture.token_request(device, &segment_token, HeaderMap::new()).await;
    assert_eq!(segment.status(), StatusCode::OK);
    (variant_token, session_token, segment)
}

#[tokio::test]
async fn evicted_wrapped_session_is_not_recreated_and_does_not_evict_back() {
    let fixture = owner_token_fixture(playlist_origin_handler(MEDIA_PLAYLIST)).await;
    enable_user_limits(&fixture.app_state, vec![shared::model::AdmissionStrategy::EvictUserOldest]);
    let device_a = client("10.0.0.1", 50_181);
    let device_b = other_device(50_182);

    let (token_a, session_a, _segment_a) = start_playback(&fixture, &device_a).await;
    assert_eq!(fixture.app_state.active_users.user_connections("hls-user").await, 1);
    let (_, session_b, _segment_b) = start_playback(&fixture, &device_b).await;

    // B's segment evicted A: A's session is gone and marked ended, even without a reentry window.
    assert!(fixture.app_state.active_users.get_and_update_user_session("hls-user", &session_a).await.is_none());
    assert!(fixture.app_state.active_users.is_session_ended(&session_a).await);
    assert_eq!(fixture.app_state.active_users.user_connections("hls-user").await, 1);

    // A's player keeps refreshing its variant URI: no recreate, no eviction of B.
    for _ in 0..2 {
        assert_eq!(
            fixture.token_request(&device_a, &token_a, HeaderMap::new()).await.status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert!(fixture.app_state.active_users.get_and_update_user_session("hls-user", &session_a).await.is_none());
    assert!(fixture.app_state.active_users.get_and_update_user_session("hls-user", &session_b).await.is_some());
    assert_eq!(fixture.app_state.active_users.user_connections("hls-user").await, 1);
}

#[tokio::test]
async fn recreate_at_connection_limit_answers_exhausted_without_creating_a_session() {
    let fixture = owner_token_fixture(playlist_origin_handler(MEDIA_PLAYLIST)).await;
    enable_user_limits(&fixture.app_state, Vec::new());
    let device_a = client("10.0.0.1", 50_191);
    let device_b = other_device(50_192);

    // A's session expires (not ended), then B occupies the only slot.
    let (_, token_a, session_a) = wrapped_entry(&fixture, &device_a).await;
    assert!(fixture.app_state.active_users.terminate_session("hls-user", &session_a).await);
    let (_, session_b, _segment_b) = start_playback(&fixture, &device_b).await;
    assert_eq!(fixture.app_state.active_users.user_connections("hls-user").await, 1);
    let requests_before = fixture.origin.manifest_requests();

    let response = fixture.token_request(&device_a, &token_a, HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN, "same admission-failure response as the entry route");
    let body = body_text(response).await;
    assert!(!body.contains("#EXT-X-TARGETDURATION:2"), "no upstream playlist served: {body}");
    assert_eq!(fixture.origin.manifest_requests(), requests_before, "no upstream fetch");
    assert!(fixture.app_state.active_users.get_and_update_user_session("hls-user", &session_a).await.is_none());
    assert!(fixture.app_state.active_users.get_and_update_user_session("hls-user", &session_b).await.is_some());
}

fn with_alias_account(input: &mut ConfigInput) {
    input.priority = 10;
    input.aliases = Some(vec![crate::model::ConfigInputAlias {
        id: 2,
        name: Arc::from("owner-token-alias"),
        url: input.url.clone(),
        username: Some("alias-user".to_string()),
        password: Some("alias-pass".to_string()),
        priority: 0,
        max_connections: 1,
        exp_date: None,
        enabled: true,
        stalker: None,
    }]);
}

fn manifest_fetches_for(fixture: &OwnerTokenFixture, account_path: &str) -> usize {
    fixture
        .origin
        .requests()
        .iter()
        .filter(|request| request_path(request).contains(".m3u8") && request_path(request).contains(account_path))
        .count()
}

#[tokio::test]
async fn recreate_on_another_account_rewrites_every_refresh_to_that_account() {
    let fixture = owner_token_fixture_with(playlist_origin_handler(MEDIA_PLAYLIST), with_alias_account).await;
    let device = client("10.0.0.1", 50_201);
    let (_, token, session_token) = wrapped_entry(&fixture, &device).await;
    assert_eq!(manifest_fetches_for(&fixture, "/alias-user/alias-pass/"), 1, "playback starts on the alias");
    let stale_child = fixture.seal(
        Some(&session_token),
        &format!("{}/cdn/alias/child.m3u8", fixture.origin.base_url),
        HlsResourceKind::Manifest(HlsManifestSource::Child),
        Some("owner-token-alias"),
    );

    // Session and alias binding are lost, and another playback takes the alias's only slot.
    assert!(fixture.app_state.active_users.terminate_session("hls-user", &session_token).await);
    fixture.app_state.active_provider.terminate_identified_playback_owner(&session_token);
    let _alias_slot = fixture
        .app_state
        .active_provider
        .acquire_connection_with_lease_for_session(
            &fixture.input.name,
            &test_addr_with_port(50_299),
            false,
            0,
            ConnectionKind::Normal,
            Some(crate::api::model::PlaybackLeaseRef::new("other-owner", crate::model::PlaybackKind::LiveHls)),
        )
        .expect("alias slot is acquired");

    for _ in 0..3 {
        assert_eq!(fixture.token_request(&device, &token, HeaderMap::new()).await.status(), StatusCode::OK);
    }
    assert_eq!(manifest_fetches_for(&fixture, "/user/pass/"), 3, "recreate and both refreshes use the main account");
    assert_eq!(manifest_fetches_for(&fixture, "/alias-user/alias-pass/"), 1);
    let session = fixture.app_state.active_users.get_and_update_user_session("hls-user", &session_token).await;
    assert_eq!(session.map(|session| session.provider), Some(fixture.input.name.clone()));

    assert_eq!(fixture.token_request(&device, &stale_child, HeaderMap::new()).await.status(), StatusCode::NOT_FOUND);
    assert!(!fixture.origin.requests().iter().any(|request| request_path(request).contains("/cdn/alias/")));
}

fn set_session_ttls(app_state: &Arc<AppState>, hls_secs: u64, catchup_secs: u64) {
    app_state.app_config.config.store(Arc::new(Config {
        custom_stream_response_enabled: true,
        reverse_proxy: Some(ReverseProxyConfig::from(&ReverseProxyConfigDto {
            stream: Some(StreamConfigDto {
                hls_session_ttl_secs: hls_secs,
                catchup_session_ttl_secs: catchup_secs,
                provider_affinity_ttl_secs: 0,
                ..StreamConfigDto::default()
            }),
            ..Default::default()
        })),
        ..Default::default()
    }));
}

async fn lease_alive_after_hls_ttl(fixture: &OwnerTokenFixture, session_token: &str) -> bool {
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    fixture.app_state.active_provider.prune_expired_leases_now();
    fixture.app_state.active_provider.binding_tag_for_owner(session_token).is_some()
}

#[tokio::test]
async fn catchup_refresh_on_token_route_renews_the_lease_with_the_catchup_ttl() {
    let fixture = owner_token_fixture(playlist_origin_handler(MEDIA_PLAYLIST)).await;
    set_session_ttls(&fixture.app_state, 1, 30);
    let device = client("10.0.0.1", 50_211);
    let response = fixture.entry_with_archive(&device, 1_785_136_500).await;
    assert_eq!(response.status(), StatusCode::OK);
    let (_, variant_uri) = single_variant_master_playlist(response).await;
    let token = token_of(&variant_uri).to_string();
    let session_token = fixture.decode(&token).session_token.expect("session token");
    assert!(session_token.starts_with("m3u-catchup|"), "{session_token}");
    for _ in 0..2 {
        assert_eq!(fixture.token_request(&device, &token, HeaderMap::new()).await.status(), StatusCode::OK);
    }
    assert!(lease_alive_after_hls_ttl(&fixture, &session_token).await, "catch-up lease outlives the HLS TTL");
}

#[tokio::test]
async fn live_refresh_on_token_route_renews_the_lease_with_the_hls_ttl() {
    let fixture = owner_token_fixture(playlist_origin_handler(MEDIA_PLAYLIST)).await;
    set_session_ttls(&fixture.app_state, 1, 30);
    let device = client("10.0.0.1", 50_221);
    let (_, token, session_token) = wrapped_entry(&fixture, &device).await;
    for _ in 0..2 {
        assert_eq!(fixture.token_request(&device, &token, HeaderMap::new()).await.status(), StatusCode::OK);
    }
    assert!(!lease_alive_after_hls_ttl(&fixture, &session_token).await, "live lease ends with the HLS TTL");
}

#[tokio::test]
async fn media_token_from_another_account_is_rejected_and_session_account_is_kept() {
    let fixture = owner_token_fixture_with(playlist_origin_handler(MEDIA_PLAYLIST), with_alias_account).await;
    let device = client("10.0.0.1", 50_231);
    let (_, token, session_token) = wrapped_entry(&fixture, &device).await;
    let own_segment = first_segment_token(&fixture, &device, &token).await;
    assert_eq!(fixture.decode(&own_segment).origin_provider.as_deref(), Some("owner-token-alias"));
    assert_eq!(fixture.token_request(&device, &own_segment, HeaderMap::new()).await.status(), StatusCode::OK);

    let foreign_segment = fixture.seal(
        Some(&session_token),
        &format!("{}/live/user/pass/12345/seg-7.ts", fixture.origin.base_url),
        HlsResourceKind::Media,
        Some(fixture.input.name.as_ref()),
    );
    let response = fixture.token_request(&device, &foreign_segment, HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(!fixture.origin.requests().iter().any(|request| request_path(request).starts_with("/live/user/pass/")));
    assert!(fixture.app_state.active_users.get_and_update_user_session("hls-user", &session_token).await.is_some());
}

#[tokio::test]
async fn leaked_relative_path_resolves_the_same_for_kind_and_legacy_tokens() {
    let fixture = owner_token_fixture(playlist_origin_handler(MEDIA_PLAYLIST)).await;
    let device = client("10.0.0.1", 50_241);
    // Leaked relative paths are only resolved while the playback has an active stream.
    let (token, session_token, _segment) = start_playback(&fixture, &device).await;
    let legacy = shared::utils::seal_hls_resource_url(
        &fixture.app_state.get_encrypt_secret(),
        &format!("{session_token}\u{1F}{}", fixture.entry_url),
    );

    let mut fetched = Vec::new();
    for (index, sealed) in [token.trim_end_matches(".m3u8").to_string(), legacy].into_iter().enumerate() {
        let uri = format!(
            "/hls/hls-user/hls-pass/{}/{}/{VIRTUAL_ID}/{sealed}/dvr-{index}/seg.ts",
            fixture.target.id, fixture.input.id
        );
        let response = get_response(Arc::clone(&fixture.app_state), &uri, None).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        let path = fixture
            .origin
            .requests()
            .iter()
            .map(|request| request_path(request).to_string())
            .find(|path| path.contains(&format!("dvr-{index}/seg.ts")))
            .expect("leaked segment fetched upstream");
        fetched.push(path.replace(&format!("dvr-{index}"), "dvr"));
    }
    assert_eq!(fetched[0], fetched[1], "four-field and legacy tokens resolve the same origin path");
}
