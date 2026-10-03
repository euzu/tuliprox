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
    body_gate: Arc<tokio::sync::Notify>,
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
    let body_gate = Arc::new(tokio::sync::Notify::new());
    let task_body_gate = Arc::clone(&body_gate);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
    let addr = listener.local_addr().expect("local addr");
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let handler = Arc::clone(&handler);
            let requests = Arc::clone(&requests_for_task);
            let body_gate = Arc::clone(&task_body_gate);
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
                let gated = request_path(&request).contains("slow.hls.fmp4");
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
                if gated {
                    let split = 4.min(body.len());
                    let _ = socket.write_all(&body[..split]).await;
                    body_gate.notified().await;
                    let _ = socket.write_all(&body[split..]).await;
                } else {
                    let _ = socket.write_all(&body).await;
                }
            });
        }
    });
    RecordingOrigin { base_url: format!("http://{addr}"), requests, task, body_gate }
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

fn attribute_uri(line: &str) -> Option<&str> { line.split_once("URI=\"")?.1.split_once('"').map(|(uri, _)| uri) }

#[tokio::test]
async fn catchup_fmp4_master_renditions_maps_and_segments_keep_tokens_and_ranges(
) -> Result<(), Box<dyn std::error::Error>> {
    let handler: OriginHandler = Arc::new(|path| {
        let (mime, body) = if path.contains("tracks-") && path.contains(".m3u8") {
            ("application/vnd.apple.mpegurl", b"#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-TARGETDURATION:4\n#EXT-X-MAP:URI=\"init-1.hls.fmp4\",BYTERANGE=\"4@2\"\n#EXTINF:4,\n#EXT-X-BYTERANGE:4@2\ndvr-1.fmp4\n".as_slice())
        } else if path.contains(".m3u8") {
            ("application/vnd.apple.mpegurl", b"#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"audio\",URI=\"tracks-a1/timeshift_abs-1785136500.fmp4.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=12000000,AUDIO=\"aac\"\ntracks-v1/timeshift_abs-1785136500.fmp4.m3u8\n".as_slice())
        } else {
            ("video/MP2T", b"cdef".as_slice())
        };
        let mut headers = vec![("Content-Type".to_string(), mime.to_string())];
        let status = if path.contains(".m3u8") {
            StatusCode::OK
        } else {
            headers.push(("Content-Range".to_string(), "bytes 2-5/10".to_string()));
            headers.push(("Accept-Ranges".to_string(), "bytes".to_string()));
            StatusCode::PARTIAL_CONTENT
        };
        (status, headers, body.to_vec())
    });
    let mut fixture = owner_token_fixture_with(handler, |input| {
        input.options = Some(crate::model::ConfigInputOptions::from(&shared::model::ConfigInputOptionsDto {
            flussonic_hls_audio_tracks: true,
            ..shared::model::ConfigInputOptionsDto::default()
        }));
    })
    .await;
    fixture.entry_url = format!("{}/live/user/pass/{VIRTUAL_ID}.m3u8?token=archive-token", fixture.origin.base_url);
    set_session_ttls(&fixture.app_state, 1, 30);
    let device = client("10.0.0.1", 50_301);
    let response = fixture.entry_with_archive(&device, 1_785_136_500).await;
    assert_eq!(response.status(), StatusCode::OK);
    let master = body_text(response).await;
    let audio_uri = master.lines().find_map(attribute_uri).ok_or("audio URI missing")?;
    let video_uri =
        master.lines().find(|line| !line.starts_with('#') && line.contains("/hls/")).ok_or("video URI missing")?;
    let session_token = fixture.decode(token_of(video_uri)).session_token.ok_or("session token missing")?;
    assert!(session_token.starts_with("m3u-catchup|"));

    for (uri, track, mime) in [(video_uri, "tracks-v1", "video/mp4"), (audio_uri, "tracks-a1", "audio/mp4")] {
        let variant_token = token_of(uri);
        assert_eq!(fixture.decode(variant_token).kind, Some(HlsResourceKind::Manifest(HlsManifestSource::Child)));
        let response = fixture.token_request(&device, variant_token, HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let playlist = body_text(response).await;
        let map_uri = playlist.lines().find_map(attribute_uri).ok_or("map URI missing")?;
        assert!(playlist.contains("BYTERANGE=\"4@2\""));
        assert!(playlist.contains("#EXT-X-BYTERANGE:4@2"));
        let segment_uri = playlist
            .lines()
            .find(|line| !line.starts_with('#') && line.contains("/hls/"))
            .ok_or("segment URI missing")?;
        for (resource_uri, file) in [(map_uri, "init-1.hls.fmp4"), (segment_uri, "dvr-1.fmp4")] {
            let token = token_of(resource_uri);
            let decoded = fixture.decode(token);
            assert_eq!(decoded.kind, Some(HlsResourceKind::Media));
            assert_eq!(decoded.session_token.as_deref(), Some(session_token.as_str()));
            assert_eq!(decoded.origin_provider.as_deref(), Some(fixture.input.name.as_ref()));
            assert_eq!(
                decoded.url,
                format!("{}/live/user/pass/{track}/{file}?token=archive-token", fixture.origin.base_url)
            );
            let mut headers = HeaderMap::new();
            headers.insert(header::RANGE, HeaderValue::from_static("bytes=2-5"));
            let response = fixture.token_request(&device, token, headers).await;
            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(response.headers()[header::CONTENT_TYPE], mime);
            assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
            assert_eq!(response.headers()[header::CONTENT_LENGTH], "4");
            assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
            assert_eq!(response_body(response).await.as_ref(), b"cdef");
        }
    }
    let requests = fixture.origin.requests();
    let media_requests: Vec<_> = requests.iter().filter(|request| !request_path(request).contains(".m3u8")).collect();
    assert_eq!(media_requests.len(), 4);
    assert!(media_requests.iter().all(|request| request.to_ascii_lowercase().contains("\r\nrange: bytes=2-5\r\n")));
    assert!(lease_alive_after_hls_ttl(&fixture, &session_token).await);
    Ok(())
}

#[tokio::test]
async fn catchup_fmp4_init_errors_keep_upstream_status_without_ts_fallback() -> Result<(), Box<dyn std::error::Error>> {
    for (status, manual_redirects) in [
        (StatusCode::FORBIDDEN, false),
        (StatusCode::FORBIDDEN, true),
        (StatusCode::NOT_FOUND, false),
        (StatusCode::RANGE_NOT_SATISFIABLE, false),
        (StatusCode::SERVICE_UNAVAILABLE, true),
    ] {
        let handler: OriginHandler = Arc::new(move |path| {
            if path.contains(".m3u8") {
                (StatusCode::OK, Vec::new(), MEDIA_PLAYLIST.as_bytes().to_vec())
            } else {
                (
                    status,
                    vec![
                        ("Content-Range".to_string(), "bytes */10".to_string()),
                        ("Retry-After".to_string(), "0".to_string()),
                    ],
                    b"provider error".to_vec(),
                )
            }
        });
        let fixture = owner_token_fixture_with(handler, |input| {
            if manual_redirects {
                input.headers.insert("Authorization".to_string(), "Bearer fixture".to_string());
            }
        })
        .await;
        let device = client("10.0.0.1", 50_311);
        let response = fixture.entry_with_archive(&device, 1_785_136_500).await;
        assert_eq!(response.status(), StatusCode::OK);
        let (_, uri) = single_variant_master_playlist(response).await;
        let session_token = fixture.decode(token_of(&uri)).session_token.ok_or("session missing")?;
        let token = fixture.seal(
            Some(&session_token),
            &format!("{}/channel4k/tracks-v1/init-1.hls.mp4?token=archive-token", fixture.origin.base_url),
            HlsResourceKind::Media,
            Some(fixture.input.name.as_ref()),
        );
        let response = fixture.token_request(&device, &token, HeaderMap::new()).await;
        assert_eq!(response.status(), status);
        assert!(!response.headers().contains_key(header::CONTENT_TYPE));
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */10");
        assert_eq!(response.headers()[header::RETRY_AFTER], "0");
        assert!(response_body(response).await.is_empty());
        assert_eq!(
            fixture.origin.requests().iter().filter(|request| request_path(request).contains("init-1")).count(),
            if status.is_server_error() {
                fixture.app_state.app_config.config.load().reverse_proxy.as_ref().map_or_else(
                    || crate::model::ResourceRetryConfig::get_default_retry_values().0 as usize,
                    |config| config.resource_retry.get_retry_values().0 as usize,
                )
            } else {
                1
            }
        );
        assert_eq!(fixture.app_state.active_users.user_connections("hls-user").await, 0);
    }
    Ok(())
}

#[tokio::test]
async fn catchup_init_cannot_switch_account_when_manifest_account_is_full() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = owner_token_fixture_with(playlist_origin_handler(MEDIA_PLAYLIST), with_alias_account).await;
    let device = client("10.0.0.1", 50_321);
    let response = fixture.entry_with_archive(&device, 1_785_136_500).await;
    assert_eq!(response.status(), StatusCode::OK);
    let (_, uri) = single_variant_master_playlist(response).await;
    let decoded = fixture.decode(token_of(&uri));
    assert_eq!(decoded.origin_provider.as_deref(), Some("owner-token-alias"));
    let session_token = decoded.session_token.ok_or("session missing")?;
    fixture.app_state.active_provider.terminate_identified_playback_owner(&session_token);
    let pinned_provider: Arc<str> = Arc::from("owner-token-alias");
    let handle = fixture
        .app_state
        .active_provider
        .acquire_exact_connection_with_lease_for_session_await(
            &pinned_provider,
            &test_addr_with_port(50_322),
            false,
            0,
            ConnectionKind::Normal,
            Some(crate::api::model::PlaybackLeaseRef::new("another-playback", crate::model::PlaybackKind::LiveHls)),
        )
        .await
        .ok_or("alias slot missing")?;
    let _guard = tuliprox_session::ManagedProviderHandle::new(Arc::clone(&fixture.app_state.active_provider), handle);
    let token = fixture.seal(
        Some(&session_token),
        &format!("{}/channel4k/tracks-v1/init-1.hls.mp4?token=archive-token", fixture.origin.base_url),
        HlsResourceKind::Media,
        Some(&pinned_provider),
    );
    let requests_before = fixture.origin.requests().len();
    let response = fixture.token_request(&device, &token, HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(fixture.origin.requests().len(), requests_before, "no init fetch through a different account");
    assert!(response_body(response).await.is_empty());
    Ok(())
}

#[tokio::test]
async fn live_ts_range_segment_on_full_account_fails_without_fallback() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = owner_token_fixture_with(playlist_origin_handler(MEDIA_PLAYLIST), with_alias_account).await;
    let device = client("10.0.0.1", 50_341);
    let response = fixture.entry(&device).await;
    assert_eq!(response.status(), StatusCode::OK);
    let (_, uri) = single_variant_master_playlist(response).await;
    let decoded = fixture.decode(token_of(&uri));
    assert_eq!(decoded.origin_provider.as_deref(), Some("owner-token-alias"));
    let session_token = decoded.session_token.ok_or("session missing")?;
    fixture.app_state.active_provider.terminate_identified_playback_owner(&session_token);
    let pinned_provider: Arc<str> = Arc::from("owner-token-alias");
    let handle = fixture
        .app_state
        .active_provider
        .acquire_exact_connection_with_lease_for_session_await(
            &pinned_provider,
            &test_addr_with_port(50_342),
            false,
            0,
            ConnectionKind::Normal,
            Some(crate::api::model::PlaybackLeaseRef::new("another-playback", crate::model::PlaybackKind::LiveHls)),
        )
        .await
        .ok_or("alias slot missing")?;
    let _guard = tuliprox_session::ManagedProviderHandle::new(Arc::clone(&fixture.app_state.active_provider), handle);
    let token = fixture.seal(
        Some(&session_token),
        &format!("{}/channel4k/segment-1.ts", fixture.origin.base_url),
        HlsResourceKind::Media,
        Some(&pinned_provider),
    );
    let requests_before = fixture.origin.requests().len();
    let mut headers = HeaderMap::new();
    headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-187"));
    let response = fixture.token_request(&device, &token, headers).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(fixture.origin.requests().len(), requests_before, "no segment fetch through a different account");
    assert!(response_body(response).await.is_empty(), "a ranged TS segment must not receive a TS fallback body");
    Ok(())
}

#[tokio::test]
async fn catchup_range_init_obeys_user_admission_before_upstream_fetch() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = owner_token_fixture(playlist_origin_handler(MEDIA_PLAYLIST)).await;
    enable_user_limits(&fixture.app_state, Vec::new());
    let device_a = client("10.0.0.1", 50_331);
    let response = fixture.entry_with_archive(&device_a, 1_785_136_500).await;
    assert_eq!(response.status(), StatusCode::OK);
    let (_, uri) = single_variant_master_playlist(response).await;
    let session_token = fixture.decode(token_of(&uri)).session_token.ok_or("session missing")?;
    let (_, _, _open_segment) = start_playback(&fixture, &other_device(50_332)).await;
    assert_eq!(fixture.app_state.active_users.user_connections("hls-user").await, 1);
    let token = fixture.seal(
        Some(&session_token),
        &format!("{}/channel4k/tracks-v1/init-1.hls.mp4", fixture.origin.base_url),
        HlsResourceKind::Media,
        Some(fixture.input.name.as_ref()),
    );
    let requests_before = fixture.origin.requests().len();
    let mut headers = HeaderMap::new();
    headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-99"));
    let response = fixture.token_request(&device_a, &token, headers).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(fixture.origin.requests().len(), requests_before);
    assert_eq!(fixture.app_state.active_users.user_connections("hls-user").await, 1);
    Ok(())
}

#[tokio::test]
async fn catchup_entry_reservation_uses_catchup_ttl_before_first_child_request(
) -> Result<(), Box<dyn std::error::Error>> {
    let fixture = owner_token_fixture(playlist_origin_handler(MEDIA_PLAYLIST)).await;
    set_session_ttls(&fixture.app_state, 1, 30);
    let response = fixture.entry_with_archive(&client("10.0.0.1", 50_341), 1_785_136_500).await;
    assert_eq!(response.status(), StatusCode::OK);
    let (_, uri) = single_variant_master_playlist(response).await;
    let session_token = fixture.decode(token_of(&uri)).session_token.ok_or("session missing")?;
    assert!(lease_alive_after_hls_ttl(&fixture, &session_token).await);
    Ok(())
}

#[tokio::test]
async fn hls_media_cookies_follow_origin_scope_and_keep_the_response_origin() -> Result<(), Box<dyn std::error::Error>>
{
    let handler: OriginHandler = Arc::new(|path| {
        if path.contains(".m3u8") {
            (StatusCode::OK, Vec::new(), MEDIA_PLAYLIST.as_bytes().to_vec())
        } else {
            (StatusCode::OK, vec![("Set-Cookie".to_string(), "sid=media-secret; Path=/".to_string())], b"init".to_vec())
        }
    });
    let fixture = owner_token_fixture(handler).await;
    let device = client("10.0.0.1", 50_701);
    let (_, _, owner) = wrapped_entry(&fixture, &device).await;
    let cookies = "sid=entry-secret";
    for (index, origin, expected_cookie) in
        [(1, "http://entry.example", false), (2, fixture.origin.base_url.as_str(), true)]
    {
        store_cookie_header(&fixture.app_state.active_users, &owner, cookies, origin).await;
        let file = format!("init-{index}.hls.fmp4");
        let url = format!("{}/tracks-v1/{file}", fixture.origin.base_url);
        let token = fixture.seal(Some(&owner), &url, HlsResourceKind::Media, Some(fixture.input.name.as_ref()));
        let response = fixture.token_request(&device, &token, HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response_body(response).await.as_ref(), b"init");
        let requests = fixture.origin.requests();
        let init =
            requests.iter().find(|request| request_path(request).contains(&file)).ok_or("init request missing")?;
        assert_eq!(init.to_ascii_lowercase().contains("cookie: sid=entry-secret"), expected_cookie);
        let session = fixture
            .app_state
            .active_users
            .get_and_update_user_session("hls-user", &owner)
            .await
            .ok_or("session missing")?;
        // The origin-scoped cookie store is the single source of provider session headers.
        assert!(session.provider_session_headers.is_empty());
        assert_eq!(
            session.provider_session_headers_for(&url).and_then(|headers| headers.get("cookie").cloned()).as_deref(),
            Some("sid=media-secret")
        );
        assert_eq!(
            session
                .provider_session_headers_for("http://entry.example/init.mp4")
                .and_then(|headers| headers.get("cookie").cloned())
                .as_deref(),
            Some("sid=entry-secret")
        );
    }
    Ok(())
}

#[tokio::test]
async fn hls_media_redirect_cookie_is_scoped_to_final_origin() -> Result<(), Box<dyn std::error::Error>> {
    let cdn = spawn_recording_origin(Arc::new(|_| {
        (StatusCode::OK, vec![("Set-Cookie".to_string(), "sid=cdn-secret; Path=/".to_string())], b"init".to_vec())
    }))
    .await;
    let redirect_url = format!("{}/init.hls.fmp4", cdn.base_url);
    let handler: OriginHandler = Arc::new(move |path| {
        if path.contains(".m3u8") {
            (StatusCode::OK, Vec::new(), MEDIA_PLAYLIST.as_bytes().to_vec())
        } else {
            (StatusCode::FOUND, vec![("Location".to_string(), redirect_url.clone())], Vec::new())
        }
    });
    let fixture = owner_token_fixture(handler).await;
    let device = client("10.0.0.1", 50_705);
    let (_, _, owner) = wrapped_entry(&fixture, &device).await;
    let cookies = "sid=entry-secret";
    store_cookie_header(&fixture.app_state.active_users, &owner, cookies, &fixture.entry_url).await;
    let url = format!("{}/init.hls.fmp4", fixture.origin.base_url);
    let token = fixture.seal(Some(&owner), &url, HlsResourceKind::Media, Some(fixture.input.name.as_ref()));
    let response = fixture.token_request(&device, &token, HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_body(response).await.as_ref(), b"init");
    assert!(cdn.requests().iter().all(|request| !request.to_ascii_lowercase().contains("cookie:")));
    let session = fixture
        .app_state
        .active_users
        .get_and_update_user_session("hls-user", &owner)
        .await
        .ok_or("session missing")?;
    assert_eq!(
        session.provider_session_headers_for(&url).and_then(|headers| headers.get("cookie").cloned()).as_deref(),
        Some("sid=entry-secret")
    );
    assert_eq!(
        session
            .provider_session_headers_for(&format!("{}/next.mp4", cdn.base_url))
            .and_then(|headers| headers.get("cookie").cloned())
            .as_deref(),
        Some("sid=cdn-secret")
    );
    Ok(())
}

#[tokio::test]
async fn catchup_audio_request_preserves_running_video_body_and_provider_limits(
) -> Result<(), Box<dyn std::error::Error>> {
    for max_connections in [1, 2] {
        let handler: OriginHandler = Arc::new(|path| {
            if path.contains(".m3u8") {
                (StatusCode::OK, Vec::new(), MEDIA_PLAYLIST.as_bytes().to_vec())
            } else {
                (
                    StatusCode::OK,
                    vec![("Content-Type".to_string(), "video/mp4".to_string())],
                    b"abcdefghijklmnop".to_vec(),
                )
            }
        });
        let fixture = owner_token_fixture_with(handler, |input| input.max_connections = max_connections).await;
        let video_device = client("10.0.0.1", 50_711);
        let audio_device = client("10.0.0.1", 50_712);
        let master = body_text(fixture.entry_with_archive(&video_device, 1_785_136_500).await).await;
        let uri =
            master.lines().find(|line| !line.starts_with('#') && line.contains("/hls/")).ok_or("variant missing")?;
        let owner = fixture.decode(token_of(uri)).session_token.ok_or("owner missing")?;
        let video = fixture.seal(
            Some(&owner),
            &format!("{}/tracks-v1/slow.hls.fmp4", fixture.origin.base_url),
            HlsResourceKind::Media,
            Some(fixture.input.name.as_ref()),
        );
        let audio = fixture.seal(
            Some(&owner),
            &format!("{}/tracks-a1/fast.hls.fmp4", fixture.origin.base_url),
            HlsResourceKind::Media,
            Some(fixture.input.name.as_ref()),
        );
        let video_response = fixture.token_request(&video_device, &video, HeaderMap::new()).await;
        assert_eq!(video_response.status(), StatusCode::OK);
        let video_reader =
            tokio::spawn(async move { axum::body::to_bytes(video_response.into_body(), usize::MAX).await });
        let audio_request = fixture.token_request(&audio_device, &audio, HeaderMap::new());
        tokio::pin!(audio_request);
        let audio_response = if max_connections == 1 {
            assert!(
                tokio::time::timeout(Duration::from_millis(100), &mut audio_request).await.is_err(),
                "audio waits for the only provider slot"
            );
            assert_eq!(fixture.app_state.active_provider.get_provider_connections_count(), 1);
            None
        } else {
            Some(tokio::time::timeout(Duration::from_secs(2), &mut audio_request).await?)
        };
        assert!(!video_reader.is_finished(), "audio must not abort the video initialization body");
        assert!(fixture.app_state.active_provider.get_provider_connections_count() <= max_connections as usize);
        assert_eq!(fixture.app_state.active_users.user_connections("hls-user").await, 1);
        fixture.origin.body_gate.notify_one();
        let response = match audio_response {
            Some(response) => response,
            None => tokio::time::timeout(Duration::from_secs(2), &mut audio_request).await?,
        };
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response_body(response).await.as_ref(), b"abcdefghijklmnop");
        let video_body = tokio::time::timeout(Duration::from_secs(2), video_reader).await???;
        assert_eq!(video_body.as_ref(), b"abcdefghijklmnop");
        assert_eq!(
            fixture.origin.requests().iter().filter(|request| request_path(request).contains(".fmp4")).count(),
            2
        );
    }
    Ok(())
}

#[tokio::test]
async fn hls_ts_and_fmp4_resources_retry_transient_http_errors() -> Result<(), Box<dyn std::error::Error>> {
    for (extension, manual_redirects, retry_enabled) in [
        ("ts", false, true),
        ("hls.fmp4", false, true),
        ("ts", true, true),
        ("hls.fmp4", true, true),
        ("ts", false, false),
        ("hls.fmp4", false, false),
        ("ts", true, false),
        ("hls.fmp4", true, false),
    ] {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let origin_attempts = Arc::clone(&attempts);
        let handler: OriginHandler = Arc::new(move |path| {
            if path.contains(".m3u8") {
                (StatusCode::OK, Vec::new(), MEDIA_PLAYLIST.as_bytes().to_vec())
            } else if origin_attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                (StatusCode::SERVICE_UNAVAILABLE, vec![("Retry-After".to_string(), "0".to_string())], Vec::new())
            } else {
                (StatusCode::OK, Vec::new(), b"media".to_vec())
            }
        });
        let fixture = owner_token_fixture_with(handler, |input| {
            if manual_redirects {
                input.headers.insert("Authorization".to_string(), "Bearer fixture".to_string());
            }
        })
        .await;
        let config = fixture.app_state.app_config.config.load_full();
        fixture.app_state.app_config.config.store(Arc::new(Config {
            reverse_proxy: Some(ReverseProxyConfig::from(&ReverseProxyConfigDto {
                stream: Some(StreamConfigDto { retry: retry_enabled, ..Default::default() }),
                ..Default::default()
            })),
            ..config.as_ref().clone()
        }));
        let device = client("10.0.0.1", 50_721);
        let (_, _, owner) = wrapped_entry(&fixture, &device).await;
        let token = fixture.seal(
            Some(&owner),
            &format!("{}/tracks-v1/media.{extension}", fixture.origin.base_url),
            HlsResourceKind::Media,
            Some(fixture.input.name.as_ref()),
        );
        let response = fixture.token_request(&device, &token, HeaderMap::new()).await;
        if retry_enabled {
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response_body(response).await.as_ref(), b"media");
            assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        } else {
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert!(response_body(response).await.is_empty());
            assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
    }
    Ok(())
}

#[tokio::test]
async fn hls_terminated_session_cancels_capacity_wait_without_reopening() -> Result<(), Box<dyn std::error::Error>> {
    let fixture =
        owner_token_fixture_with(playlist_origin_handler(MEDIA_PLAYLIST), |input| input.max_connections = 1).await;
    enable_user_limits(&fixture.app_state, Vec::new());
    let device = client("10.0.0.1", 50_801);
    let (_, owner, initial_segment) = start_playback(&fixture, &device).await;
    assert_eq!(response_body(initial_segment).await.as_ref(), b"segment");
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.app_state.active_provider.get_provider_connections_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    fixture.app_state.active_provider.terminate_identified_playback_owner(&owner);
    let handle = fixture
        .app_state
        .active_provider
        .acquire_exact_connection_with_lease_for_session_await(
            &fixture.input.name,
            &test_addr_with_port(50_802),
            false,
            0,
            ConnectionKind::Normal,
            Some(crate::api::model::PlaybackLeaseRef::new("capacity-blocker", crate::model::PlaybackKind::LiveHls)),
        )
        .await
        .ok_or("blocker acquisition failed")?;
    let blocker = tuliprox_session::ManagedProviderHandle::new(Arc::clone(&fixture.app_state.active_provider), handle);
    let token = fixture.seal(
        Some(&owner),
        &format!("{}/tracks-v1/init.hls.fmp4", fixture.origin.base_url),
        HlsResourceKind::Media,
        Some(fixture.input.name.as_ref()),
    );
    let request = fixture.token_request(&device, &token, HeaderMap::new());
    tokio::pin!(request);
    assert!(tokio::time::timeout(Duration::from_millis(100), &mut request).await.is_err());
    fixture.app_state.active_users.terminate_sessions_for_addr("hls-user", &device.addr).await;
    assert!(fixture.app_state.active_users.is_session_ended(&owner).await);
    assert!(fixture.app_state.active_users.get_and_update_user_session("hls-user", &owner).await.is_none());
    assert_eq!(fixture.app_state.active_users.user_connections("hls-user").await, 0);
    drop(blocker);
    let response = tokio::time::timeout(Duration::from_secs(2), &mut request).await?;
    let status = response.status();
    let body = response_body(response).await;
    assert_eq!(fixture.app_state.active_users.user_connections("hls-user").await, 0);
    assert!(body.is_empty());
    assert!(!fixture.origin.requests().iter().any(|request| request_path(request).contains("init.hls.fmp4")));
    assert!(!status.is_success(), "a terminated session must not resume media after waiting");
    Ok(())
}

#[tokio::test]
async fn hls_cmaf_preemption_ends_body_without_ts_fallback() -> Result<(), Box<dyn std::error::Error>> {
    let handler: OriginHandler = Arc::new(|path| {
        if path.contains(".m3u8") {
            (StatusCode::OK, Vec::new(), MEDIA_PLAYLIST.as_bytes().to_vec())
        } else {
            (StatusCode::OK, vec![("Content-Type".to_string(), "video/mp4".to_string())], b"abcdefghijklmnop".to_vec())
        }
    });
    let fixture = owner_token_fixture_with(handler, |input| input.max_connections = 1).await;
    let config = fixture.app_state.app_config.custom_stream_response.load_full().ok_or("custom config missing")?;
    let mut custom = config.as_ref().clone();
    custom.low_priority_preempted = Some(test_custom_video_buffer());
    fixture.app_state.app_config.custom_stream_response.store(Some(Arc::new(custom)));
    set_session_ttls(&fixture.app_state, 0, 0);
    let device = client("10.0.0.1", 50_811);
    let (_, _, owner) = wrapped_entry(&fixture, &device).await;
    let token = fixture.seal(
        Some(&owner),
        &format!("{}/tracks-v1/slow.hls.fmp4", fixture.origin.base_url),
        HlsResourceKind::Media,
        Some(fixture.input.name.as_ref()),
    );
    let response = fixture.token_request(&device, &token, HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp4");
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "16");
    let (first_byte_tx, first_byte_rx) = tokio::sync::oneshot::channel();
    let reader = tokio::spawn(async move {
        let mut first_byte_tx = Some(first_byte_tx);
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            if let Ok(data) = frame?.into_data() {
                bytes.extend_from_slice(&data);
                if !bytes.is_empty() {
                    if let Some(tx) = first_byte_tx.take() {
                        let _ = tx.send(());
                    }
                }
            }
            if bytes.len() > 16 {
                break;
            }
        }
        Ok::<_, axum::Error>(bytes)
    });
    tokio::time::timeout(Duration::from_secs(2), first_byte_rx).await??;
    let handle = fixture
        .app_state
        .active_provider
        .acquire_exact_connection_with_lease_for_session_await(
            &fixture.input.name,
            &test_addr_with_port(50_812),
            false,
            -100,
            ConnectionKind::Normal,
            Some(crate::api::model::PlaybackLeaseRef::new("high-priority-owner", crate::model::PlaybackKind::LiveHls)),
        )
        .await
        .ok_or("priority acquisition failed")?;
    let _guard = tuliprox_session::ManagedProviderHandle::new(Arc::clone(&fixture.app_state.active_provider), handle);
    let body = tokio::time::timeout(Duration::from_secs(3), reader).await???;
    assert_eq!(body.as_slice(), b"abcd", "preempted MP4 ends after the already received prefix");
    Ok(())
}

#[tokio::test]
async fn hls_cdn_cookie_preserves_manifest_origin_cookie() -> Result<(), Box<dyn std::error::Error>> {
    let cdn = spawn_recording_origin(Arc::new(|_| {
        (StatusCode::OK, vec![("Set-Cookie".to_string(), "cdn_sid=media-secret; Path=/".to_string())], b"init".to_vec())
    }))
    .await;
    let fixture = owner_token_fixture(playlist_origin_handler(MEDIA_PLAYLIST)).await;
    let device = client("10.0.0.1", 50_821);
    let (_, variant, owner) = wrapped_entry(&fixture, &device).await;
    // Consume the entry handoff so the later refresh makes a real upstream request.
    let initial = fixture.token_request(&device, &variant, HeaderMap::new()).await;
    assert_eq!(initial.status(), StatusCode::OK);
    response_body(initial).await;
    let cookies = "manifest_sid=entry-secret";
    store_cookie_header(&fixture.app_state.active_users, &owner, cookies, &fixture.entry_url).await;
    let token = fixture.seal(
        Some(&owner),
        &format!("{}/init.hls.fmp4", cdn.base_url),
        HlsResourceKind::Media,
        Some(fixture.input.name.as_ref()),
    );
    let response = fixture.token_request(&device, &token, HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_body(response).await.as_ref(), b"init");
    let before = fixture.origin.manifest_requests();
    let response = fixture.token_request(&device, &variant, HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fixture.origin.manifest_requests(), before + 1, "refresh must reach the origin");
    let requests = fixture.origin.requests();
    let refresh = requests.last().ok_or("manifest refresh missing")?;
    let retained = refresh.to_ascii_lowercase().contains("cookie: manifest_sid=entry-secret");
    assert!(retained, "a CDN cookie must not overwrite the distinct manifest-origin cookie");
    Ok(())
}

/// Blocks the only slot of `provider` so the next HLS resource request waits for capacity.
async fn block_provider_slot(
    fixture: &OwnerTokenFixture,
    owner: &str,
    provider: &Arc<str>,
    port: u16,
) -> Result<tuliprox_session::ManagedProviderHandle, Box<dyn std::error::Error>> {
    fixture.app_state.active_provider.terminate_identified_playback_owner(owner);
    let handle = fixture
        .app_state
        .active_provider
        .acquire_exact_connection_with_lease_for_session_await(
            provider,
            &test_addr_with_port(port),
            false,
            0,
            ConnectionKind::Normal,
            Some(crate::api::model::PlaybackLeaseRef::new("slot-blocker", crate::model::PlaybackKind::LiveHls)),
        )
        .await
        .ok_or("blocker acquisition failed")?;
    Ok(tuliprox_session::ManagedProviderHandle::new(Arc::clone(&fixture.app_state.active_provider), handle))
}

#[tokio::test]
async fn hls_resource_waiting_for_capacity_is_rejected_after_account_switch() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = owner_token_fixture_with(playlist_origin_handler(MEDIA_PLAYLIST), |input| {
        with_alias_account(input);
        input.max_connections = 1;
    })
    .await;
    let device = client("10.0.0.1", 50_831);
    let (_, _, owner) = wrapped_entry(&fixture, &device).await;
    let session = fixture
        .app_state
        .active_users
        .get_and_update_user_session("hls-user", &owner)
        .await
        .ok_or("session missing")?;
    let old_provider = session.provider.clone();
    let blocker = block_provider_slot(&fixture, &owner, &old_provider, 50_832).await?;
    let token = fixture.seal(
        Some(&owner),
        &format!("{}/tracks-v1/old-account.hls.fmp4", fixture.origin.base_url),
        HlsResourceKind::Media,
        Some(old_provider.as_ref()),
    );
    let request = fixture.token_request(&device, &token, HeaderMap::new());
    tokio::pin!(request);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut request).await.is_err(),
        "request waits for capacity"
    );
    fixture
        .app_state
        .active_users
        .update_session_provider_binding(
            "hls-user",
            &owner,
            fixture.input.name.clone(),
            fixture.entry_url.clone().into(),
        )
        .await;
    drop(blocker);
    let response = tokio::time::timeout(Duration::from_secs(2), &mut request).await?;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(response_body(response).await.is_empty());
    assert!(
        !fixture.origin.requests().iter().any(|request| request_path(request).contains("old-account")),
        "a child URL of the previous account must not be fetched"
    );
    assert_eq!(fixture.app_state.active_provider.get_provider_connections_count(), 0, "no provider slot leaks");
    Ok(())
}

#[tokio::test]
async fn hls_resource_waiting_for_capacity_sends_cookies_rotated_during_the_wait(
) -> Result<(), Box<dyn std::error::Error>> {
    let fixture =
        owner_token_fixture_with(playlist_origin_handler(MEDIA_PLAYLIST), |input| input.max_connections = 1).await;
    let device = client("10.0.0.1", 50_841);
    let (_, _, owner) = wrapped_entry(&fixture, &device).await;
    let blocker = block_provider_slot(&fixture, &owner, &fixture.input.name, 50_842).await?;
    let url = format!("{}/tracks-v1/cookie-check.hls.fmp4", fixture.origin.base_url);
    let active_users = &fixture.app_state.active_users;
    let cookie = |value: &str| format!("sid={value}");
    store_cookie_header(active_users, &owner, &cookie("old"), &url).await;
    let token = fixture.seal(Some(&owner), &url, HlsResourceKind::Media, Some(fixture.input.name.as_ref()));
    let request = fixture.token_request(&device, &token, HeaderMap::new());
    tokio::pin!(request);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut request).await.is_err(),
        "request waits for capacity"
    );
    store_cookie_header(active_users, &owner, &cookie("fresh"), &url).await;
    drop(blocker);
    let response = tokio::time::timeout(Duration::from_secs(2), &mut request).await?;
    assert_eq!(response.status(), StatusCode::OK);
    drop(response_body(response).await);
    let requests = fixture.origin.requests();
    let sent =
        requests.iter().find(|request| request_path(request).contains("cookie-check")).ok_or("request missing")?;
    assert!(sent.contains("sid=fresh") && !sent.contains("sid=old"), "request must use the rotated cookie: {sent}");
    Ok(())
}

#[tokio::test]
async fn hls_resource_cancelled_by_session_end_during_capacity_wait_leaks_no_slot(
) -> Result<(), Box<dyn std::error::Error>> {
    let fixture =
        owner_token_fixture_with(playlist_origin_handler(MEDIA_PLAYLIST), |input| input.max_connections = 1).await;
    let device = client("10.0.0.1", 50_851);
    let (_, _, owner) = wrapped_entry(&fixture, &device).await;
    let blocker = block_provider_slot(&fixture, &owner, &fixture.input.name, 50_852).await?;
    let token = fixture.seal(
        Some(&owner),
        &format!("{}/tracks-v1/cancelled.hls.fmp4", fixture.origin.base_url),
        HlsResourceKind::Media,
        Some(fixture.input.name.as_ref()),
    );
    let request = fixture.token_request(&device, &token, HeaderMap::new());
    tokio::pin!(request);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut request).await.is_err(),
        "request waits for capacity"
    );
    assert!(fixture.app_state.active_users.terminate_session("hls-user", &owner).await);
    let response = tokio::time::timeout(Duration::from_secs(2), &mut request).await?;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(fixture.app_state.active_provider.get_provider_connections_count(), 1, "only the blocker holds a slot");
    drop(blocker);
    assert_eq!(fixture.app_state.active_provider.get_provider_connections_count(), 0);
    assert!(!fixture.origin.requests().iter().any(|request| request_path(request).contains("cancelled")));
    Ok(())
}

#[tokio::test]
async fn capacity_wait_returns_at_its_deadline_while_the_account_stays_full() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture =
        owner_token_fixture_with(playlist_origin_handler(MEDIA_PLAYLIST), |input| input.max_connections = 1).await;
    let device = client("10.0.0.1", 50_861);
    let (_, _, owner) = wrapped_entry(&fixture, &device).await;
    let _blocker = block_provider_slot(&fixture, &owner, &fixture.input.name, 50_862).await?;
    let wait = Duration::from_millis(300);
    let started = tokio::time::Instant::now();
    let acquired = crate::api::api_utils::acquire_exact_provider_handle(
        &fixture.app_state,
        &crate::api::api_utils::ExactProviderAcquire {
            provider: &fixture.input.name,
            addr: &device.addr,
            allow_grace: false,
            priority: 0,
            kind: ConnectionKind::Normal,
            lease: Some(crate::api::model::PlaybackLeaseRef::new(&owner, crate::model::PlaybackKind::LiveHls)),
        },
        Some(wait),
    )
    .await;
    let elapsed = started.elapsed();
    assert!(acquired.is_none());
    assert!(elapsed >= wait, "the wait lasts until its deadline: {elapsed:?}");
    assert!(elapsed < wait + Duration::from_millis(500), "the wait ends at its deadline: {elapsed:?}");
    Ok(())
}

#[tokio::test]
async fn pinned_manifest_refresh_waits_for_a_freed_slot_instead_of_503() -> Result<(), Box<dyn std::error::Error>> {
    let fixture =
        owner_token_fixture_with(playlist_origin_handler(MEDIA_PLAYLIST), |input| input.max_connections = 1).await;
    let device = client("10.0.0.1", 50_871);
    let (_, token, owner) = wrapped_entry(&fixture, &device).await;
    // The first variant request is served from the entry hand-off; refreshes fetch upstream.
    assert_eq!(fixture.token_request(&device, &token, HeaderMap::new()).await.status(), StatusCode::OK);
    let manifests_before = fixture.origin.manifest_requests();
    let blocker = block_provider_slot(&fixture, &owner, &fixture.input.name, 50_872).await?;
    let refresh = fixture.token_request(&device, &token, HeaderMap::new());
    tokio::pin!(refresh);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut refresh).await.is_err(),
        "refresh waits for capacity"
    );
    drop(blocker);
    let response = tokio::time::timeout(Duration::from_secs(2), &mut refresh).await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_text(response).await.contains("#EXTM3U"));
    assert_eq!(fixture.origin.manifest_requests(), manifests_before + 1);
    Ok(())
}

#[tokio::test]
async fn pinned_manifest_refresh_after_capacity_wait_sends_cookies_rotated_during_the_wait(
) -> Result<(), Box<dyn std::error::Error>> {
    let fixture =
        owner_token_fixture_with(playlist_origin_handler(MEDIA_PLAYLIST), |input| input.max_connections = 1).await;
    let device = client("10.0.0.1", 50_881);
    let (_, token, owner) = wrapped_entry(&fixture, &device).await;
    assert_eq!(fixture.token_request(&device, &token, HeaderMap::new()).await.status(), StatusCode::OK);
    let active_users = &fixture.app_state.active_users;
    store_cookie_header(active_users, &owner, "sid=old", &fixture.entry_url).await;
    let blocker = block_provider_slot(&fixture, &owner, &fixture.input.name, 50_882).await?;
    let refresh = fixture.token_request(&device, &token, HeaderMap::new());
    tokio::pin!(refresh);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut refresh).await.is_err(),
        "refresh waits for capacity"
    );
    store_cookie_header(active_users, &owner, "sid=fresh", &fixture.entry_url).await;
    drop(blocker);
    let response = tokio::time::timeout(Duration::from_secs(2), &mut refresh).await?;
    assert_eq!(response.status(), StatusCode::OK);
    drop(body_text(response).await);
    let requests = fixture.origin.requests();
    let sent =
        requests.iter().rev().find(|request| request_path(request).contains(".m3u8")).ok_or("refresh missing")?;
    assert!(sent.contains("sid=fresh") && !sent.contains("sid=old"), "refresh must use the rotated cookie: {sent}");
    Ok(())
}

/// Stores the pairs of a `Cookie` header as origin-wide provider cookies set by `source_url`.
async fn store_cookie_header(
    active_users: &tuliprox_session::ActiveUserManager,
    owner: &str,
    cookie_header: &str,
    source_url: &str,
) -> bool {
    let response = tuliprox_session::ProviderSessionHeaders {
        headers: HashMap::new(),
        cookies: cookie_header
            .split(';')
            .map(str::trim)
            .filter(|pair| !pair.is_empty())
            .map(|pair| format!("{pair}; Path=/"))
            .collect(),
    };
    active_users.update_session_provider_response_headers_from("hls-user", owner, &response, source_url).await
}

#[tokio::test]
async fn hls_resource_unfollowed_redirect_answers_bad_gateway() -> Result<(), Box<dyn std::error::Error>> {
    let handler: OriginHandler = Arc::new(|path| {
        if path.contains(".m3u8") {
            (StatusCode::OK, Vec::new(), MEDIA_PLAYLIST.as_bytes().to_vec())
        } else {
            // A redirect without Location cannot be followed and must not reach the client as 3xx.
            (StatusCode::FOUND, Vec::new(), Vec::new())
        }
    });
    // Provider credentials in headers force manual redirects, which hand unfollowed 3xx back.
    let fixture = owner_token_fixture_with(handler, |input| {
        input.headers.insert("Authorization".to_string(), "Bearer fixture".to_string());
    })
    .await;
    let device = client("10.0.0.1", 50_891);
    let (_, _, owner) = wrapped_entry(&fixture, &device).await;
    let token = fixture.seal(
        Some(&owner),
        &format!("{}/tracks-v1/redirected.hls.fmp4", fixture.origin.base_url),
        HlsResourceKind::Media,
        Some(fixture.input.name.as_ref()),
    );
    let response = fixture.token_request(&device, &token, HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(response_body(response).await.is_empty());
    Ok(())
}
