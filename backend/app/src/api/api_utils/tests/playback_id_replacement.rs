use super::{create_test_app_state_for_config, create_test_provider_app_config, create_test_shared_target};
use crate::{
    api::endpoints::{hls_api::hls_api_register, m3u_api::m3u_api_register, xtream_api::xtream_api_register},
    model::{ApiProxyConfig, Config, ConfigInput, ConfigSource, SourcesConfig},
};
use arc_swap::ArcSwap;
use axum::{
    body::Body,
    http::{HeaderName, HeaderValue, Request, Response, StatusCode},
};
use futures::StreamExt;
use shared::{
    model::{
        ApiProxyConfigDto, ApiProxyServerInfoDto, ConfigPaths, InputType, M3uTargetOutputDto, PlayerFeaturesDto,
        PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType, ProxyType, ProxyUserCredentialsDto,
        TargetUserDto, WebAuthConfigDto, WebUiConfigDto, XtreamCluster, XtreamTargetOutputDto,
    },
    utils::Internable,
};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tempfile::TempDir;
use tokio::{net::TcpListener, sync::Notify};
use tower::ServiceExt;
use tuliprox_repository::{persist_playlist, TargetPlaylistPersistOptions};

struct PlaybackIdTestEnv {
    _dir: TempDir,
    app_state: Arc<crate::api::model::AppState>,
    router: axum::Router,
    live_ts_vid: u32,
    live_hls_vid: u32,
    slow_ts_vid: u32,
    vod_vid: u32,
    upstream_hits: Arc<AtomicUsize>,
    slow_open_entered: Arc<Notify>,
    slow_open_release: Arc<Notify>,
    _origin_guard: Arc<Notify>,
}

async fn spawn_playback_origin(
    upstream_hits: Arc<AtomicUsize>,
    origin_guard: Arc<Notify>,
    slow_open_entered: Arc<Notify>,
    slow_open_release: Arc<Notify>,
) -> SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("origin binds");
    let origin_addr = listener.local_addr().expect("origin addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { break };
            let hits = Arc::clone(&upstream_hits);
            let guard = Arc::clone(&origin_guard);
            let entered = Arc::clone(&slow_open_entered);
            let release = Arc::clone(&slow_open_release);
            tokio::spawn(async move {
                let mut request = Vec::new();
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let mut chunk = [0_u8; 1024];
                    let Ok(read) = socket.read(&mut chunk).await else { return };
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&chunk[..read]);
                }
                let request = String::from_utf8_lossy(&request).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or_default().to_string();
                let extension = std::path::Path::new(&path)
                    .extension()
                    .and_then(std::ffi::OsStr::to_str)
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if extension == "m3u8" {
                    let manifest = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:2.0,\nseg1.ts\n";
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        manifest.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(manifest.as_bytes()).await;
                    let _ = socket.shutdown().await;
                } else if path.ends_with("/seg1.ts") {
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: 188\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&[0x47_u8; 188]).await;
                    let _ = socket.shutdown().await;
                } else if path.ends_with("/103.ts") {
                    entered.notify_one();
                    release.notified().await;
                    hits.fetch_add(1, Ordering::AcqRel);
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&[0x47_u8; 188]).await;
                    let _ = socket.flush().await;
                    guard.notified().await;
                } else if path.ends_with("/101.ts") || path.ends_with("/104.mp4") {
                    hits.fetch_add(1, Ordering::AcqRel);
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&[0x47_u8; 188]).await;
                    let _ = socket.flush().await;
                    guard.notified().await;
                } else {
                    let head = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.shutdown().await;
                }
            });
        }
    });
    origin_addr
}

impl PlaybackIdTestEnv {
    async fn new(provider_max_connections: u16, user_max_connections: u32, share_ts: bool) -> Self {
        Self::new_with_force_redirect_live(provider_max_connections, user_max_connections, share_ts, false).await
    }

    #[expect(clippy::too_many_lines, reason = "One fixture assembling provider, users, target, playlist and router.")]
    async fn new_with_force_redirect_live(
        provider_max_connections: u16,
        user_max_connections: u32,
        share_ts: bool,
        force_redirect_live: bool,
    ) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let upstream_hits = Arc::new(AtomicUsize::new(0));
        let origin_guard = Arc::new(Notify::new());
        let slow_open_entered = Arc::new(Notify::new());
        let slow_open_release = Arc::new(Notify::new());
        let origin_addr = spawn_playback_origin(
            Arc::clone(&upstream_hits),
            Arc::clone(&origin_guard),
            Arc::clone(&slow_open_entered),
            Arc::clone(&slow_open_release),
        )
        .await;

        let input = Arc::new(ConfigInput {
            id: 1,
            name: "provider_1".intern(),
            input_type: InputType::Xtream,
            headers: HashMap::default(),
            url: format!("http://{origin_addr}"),
            username: Some("user1".to_string()),
            password: Some("pass1".to_string()),
            enabled: true,
            priority: 0,
            max_connections: provider_max_connections,
            ..ConfigInput::default()
        });

        let mut target = create_test_shared_target();
        target.use_memory_cache = true;
        let opts = target.options.as_mut().expect("shared options");
        opts.share_live_streams.mpeg_ts = share_ts;
        if force_redirect_live {
            opts.force_redirect = Some(shared::model::ClusterFlags::Live);
        }
        target.output = vec![
            tuliprox_core::model::TargetOutput::Xtream(XtreamTargetOutputDto::default().into()),
            tuliprox_core::model::TargetOutput::M3u(M3uTargetOutputDto::default().into()),
        ];
        let target = Arc::new(target);

        let web_auth = WebAuthConfigDto {
            enabled: true,
            issuer: "test".to_string(),
            secret: "test-secret".to_string(),
            ..Default::default()
        };
        let web_ui = WebUiConfigDto {
            enabled: true,
            user_ui_enabled: true,
            player_api_enabled: Some(true),
            auth: Some(web_auth),
            ..Default::default()
        };
        let storage_dir = dir.path().to_string_lossy().into_owned();
        let main_cfg = Config {
            storage_dir: storage_dir.clone(),
            user_config_dir: Some(dir.path().join("user_config").to_string_lossy().into_owned()),
            web_ui: Some((&web_ui).into()),
            user_access_control: true,
            reverse_proxy: Some(crate::model::ReverseProxyConfig {
                resource_rewrite_disabled: false,
                rewrite_secret: [0; 16],
                resource_retry: crate::model::ResourceRetryConfig::default(),
                disabled_header: None,
                stream: Some(crate::model::StreamConfig {
                    shared_burst_secs: 0,
                    admission_strategies: Some(Vec::new()),
                    ..Default::default()
                }),
                cache: None,
                rate_limit: None,
                geoip: None,
                stream_history: None,
                qos_aggregation: None,
                hls_cache: None,
                hls_origin_telemetry: None,
            }),
            ..Config::default()
        };

        let mut app_cfg = create_test_provider_app_config();
        app_cfg.config = Arc::new(ArcSwap::from_pointee(main_cfg));
        app_cfg.paths = Arc::new(ArcSwap::from_pointee(ConfigPaths {
            home_path: storage_dir.clone(),
            config_path: storage_dir.clone(),
            storage_path: storage_dir,
            ..(*app_cfg.paths.load_full()).clone()
        }));
        app_cfg.sources = Arc::new(ArcSwap::from_pointee(SourcesConfig {
            inputs: vec![Arc::clone(&input)],
            sources: vec![ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![Arc::clone(&target)] }],
            ..SourcesConfig::default()
        }));

        let api_proxy_dto = ApiProxyConfigDto {
            server: vec![ApiProxyServerInfoDto {
                name: "default".to_string(),
                protocol: "http".to_string(),
                host: "127.0.0.1".to_string(),
                port: Some("8901".to_string()),
                timezone: "UTC".to_string(),
                message: "test".to_string(),
                path: None,
            }],
            user: vec![TargetUserDto {
                target: target.name.clone(),
                credentials: vec![
                    ProxyUserCredentialsDto {
                        username: "alice".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        max_connections: user_max_connections,
                        proxy: ProxyType::Reverse(None),
                        ..Default::default()
                    },
                    ProxyUserCredentialsDto {
                        username: "bob".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        max_connections: user_max_connections,
                        proxy: ProxyType::Reverse(None),
                        ..Default::default()
                    },
                    ProxyUserCredentialsDto {
                        username: "carol".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: false,
                        max_connections: user_max_connections,
                        proxy: ProxyType::Reverse(None),
                        player_features: Some(PlayerFeaturesDto { api_enabled: Some(false), ..Default::default() }),
                        ..Default::default()
                    },
                ],
            }],
            ..Default::default()
        };
        let mut api_proxy = ApiProxyConfig::from(&api_proxy_dto);
        for target_user in &mut api_proxy.user {
            for cred in &mut target_user.credentials {
                Arc::make_mut(cred).resolve_plan(&HashMap::new());
            }
        }
        app_cfg.set_api_proxy(api_proxy).expect("set api proxy");

        let app_state = create_test_app_state_for_config(Arc::new(app_cfg));

        let ts_url = format!("http://{origin_addr}/live/user1/pass1/101.ts");
        let hls_url = format!("http://{origin_addr}/live/user1/pass1/102.m3u8");
        let slow_ts_url = format!("http://{origin_addr}/live/user1/pass1/103.ts");
        let vod_url = format!("http://{origin_addr}/movie/user1/pass1/104.mp4");
        let ts_header = PlaylistItemHeader {
            id: "101".intern(),
            input_stream_id: "101".intern(),
            virtual_id: shared::model::VirtualId::new(101),
            name: "Live TS".intern(),
            title: "Live TS".intern(),
            url: ts_url.intern(),
            group: "Live".intern(),
            input_name: Arc::clone(&input.name),
            item_type: PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            ..PlaylistItemHeader::default()
        };
        let hls_header = PlaylistItemHeader {
            id: "102".intern(),
            input_stream_id: "102".intern(),
            virtual_id: shared::model::VirtualId::new(102),
            name: "Live HLS".intern(),
            title: "Live HLS".intern(),
            url: hls_url.intern(),
            group: "Live".intern(),
            input_name: Arc::clone(&input.name),
            item_type: PlaylistItemType::LiveHls,
            xtream_cluster: XtreamCluster::Live,
            ..PlaylistItemHeader::default()
        };
        let mut slow_ts_header = PlaylistItemHeader {
            id: "103".intern(),
            input_stream_id: "103".intern(),
            virtual_id: shared::model::VirtualId::new(103),
            name: "Slow Live TS".intern(),
            title: "Slow Live TS".intern(),
            url: slow_ts_url.intern(),
            group: "Live".intern(),
            input_name: Arc::clone(&input.name),
            item_type: PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            ..PlaylistItemHeader::default()
        };
        let mut vod_header = PlaylistItemHeader {
            id: "104".intern(),
            input_stream_id: "104".intern(),
            virtual_id: shared::model::VirtualId::new(104),
            name: "VOD MP4".intern(),
            title: "VOD MP4".intern(),
            url: vod_url.intern(),
            group: "Movies".intern(),
            input_name: Arc::clone(&input.name),
            item_type: PlaylistItemType::Video,
            xtream_cluster: XtreamCluster::Video,
            ..PlaylistItemHeader::default()
        };
        let mut ts_header = ts_header;
        ts_header.gen_uuid();
        let mut hls_header = hls_header;
        hls_header.gen_uuid();
        slow_ts_header.gen_uuid();
        vod_header.gen_uuid();
        let mut playlist = vec![
            PlaylistGroup {
                id: 1,
                title: "Live".intern(),
                channels: vec![
                    PlaylistItem { header: ts_header },
                    PlaylistItem { header: hls_header },
                    PlaylistItem { header: slow_ts_header },
                ],
                xtream_cluster: XtreamCluster::Live,
            },
            PlaylistGroup {
                id: 2,
                title: "Movies".intern(),
                channels: vec![PlaylistItem { header: vod_header }],
                xtream_cluster: XtreamCluster::Video,
            },
        ];
        persist_playlist(
            &app_state.app_config,
            &mut playlist,
            None,
            &target,
            Some(&app_state.playlists),
            TargetPlaylistPersistOptions::default(),
        )
        .await
        .expect("persist playlist");

        let live_ts_vid = playlist[0].channels[0].header.virtual_id.get();
        let live_hls_vid = playlist[0].channels[1].header.virtual_id.get();
        let slow_ts_vid = playlist[0].channels[2].header.virtual_id.get();
        let vod_vid = playlist[1].channels[0].header.virtual_id.get();

        let router = xtream_api_register()
            .merge(m3u_api_register())
            .merge(hls_api_register())
            .with_state(Arc::clone(&app_state));

        Self {
            _dir: dir,
            app_state,
            router,
            live_ts_vid,
            live_hls_vid,
            slow_ts_vid,
            vod_vid,
            upstream_hits,
            slow_open_entered,
            slow_open_release,
            _origin_guard: origin_guard,
        }
    }

    async fn send(&self, uri: &str, port: u16, headers: &[(&str, &str)]) -> Response<Body> {
        let mut builder = Request::builder().uri(uri);
        for (name, value) in headers {
            builder = builder.header(
                HeaderName::from_bytes(name.as_bytes()).expect("valid header name"),
                HeaderValue::from_str(value).expect("valid header value"),
            );
        }
        let mut req = builder.body(Body::empty()).expect("request");
        req.extensions_mut().insert(axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], port))));
        self.router.clone().oneshot(req).await.expect("router response")
    }
}

async fn read_first_chunk(body: Body) -> (Vec<u8>, http_body_util::BodyDataStream<Body>) {
    let mut stream = http_body_util::BodyDataStream::new(body);
    let chunk = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("first chunk timeout")
        .expect("stream yielded")
        .expect("chunk ok");
    (chunk.to_vec(), stream)
}

fn first_playlist_uri(playlist: &str) -> &str {
    playlist
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .expect("rewritten playlist uri present")
}

#[tokio::test]
async fn playback_id_replaces_same_user_ts_stream_at_max_connections_1_with_provider_limit_1() {
    let env = PlaybackIdTestEnv::new(1, 1, false).await;
    let uri = format!("/live/alice/pass/{}.ts", env.live_ts_vid);
    let mut close_events = env.app_state.connection_manager.get_close_connection_channel();

    let resp1 = env.send(&uri, 41001, &[("X-Tuliprox-Playback-Id", "player-session-1")]).await;
    assert_eq!(
        resp1.status(),
        StatusCode::OK,
        "resp1 failed: vid={}, hits={}, headers={:?}",
        env.live_ts_vid,
        env.upstream_hits.load(Ordering::Acquire),
        resp1.headers()
    );
    let (chunk1, stream1) = read_first_chunk(resp1.into_body()).await;
    assert_eq!(chunk1, vec![0x47_u8; 188], "first TS stream must deliver the origin payload");
    assert_eq!(env.app_state.active_users.user_connections("alice").await, 1);
    assert_eq!(env.app_state.active_provider.get_provider_connections_count(), 1);

    let resp2 = env.send(&uri, 41002, &[("X-Tuliprox-Playback-Id", "player-session-1")]).await;
    assert_eq!(
        resp2.status(),
        StatusCode::OK,
        "second request with same playback id must replace first at max_connections=1 and provider max_connections=1"
    );

    // The replaced connection's socket is closed by the manager; the stream task holding it then
    // tears the provider stream down, which is what frees the single provider slot.
    let close_signal = tokio::time::timeout(Duration::from_secs(2), close_events.recv())
        .await
        .expect("replaced TS stream must emit close signal")
        .expect("close channel open");
    assert_eq!(
        close_signal,
        tuliprox_session::CloseConnectionSignal::WithReason(
            SocketAddr::from(([127, 0, 0, 1], 41001)),
            shared::model::DisconnectReason::ClientKicked,
        )
    );
    drop(stream1);

    let (chunk2, _stream2) = read_first_chunk(resp2.into_body()).await;
    assert_eq!(chunk2, vec![0x47_u8; 188], "replacement TS stream must deliver the origin payload");

    assert_eq!(env.app_state.active_users.user_connections("alice").await, 1);
    assert_eq!(env.app_state.active_provider.get_provider_connections_count(), 1);
}

#[tokio::test]
async fn playback_id_for_different_user_does_not_affect_existing_user() {
    let env = PlaybackIdTestEnv::new(4, 1, false).await;
    let uri_alice = format!("/live/alice/pass/{}.ts", env.live_ts_vid);
    let uri_bob = format!("/live/bob/pass/{}.ts", env.live_ts_vid);

    let resp_alice = env.send(&uri_alice, 41101, &[("X-Tuliprox-Playback-Id", "shared-id-across-users")]).await;
    assert_eq!(resp_alice.status(), StatusCode::OK);
    let (_chunk_a, _stream_a) = read_first_chunk(resp_alice.into_body()).await;

    let resp_bob = env.send(&uri_bob, 41102, &[("X-Tuliprox-Playback-Id", "shared-id-across-users")]).await;
    assert_eq!(resp_bob.status(), StatusCode::OK);
    let (_chunk_b, _stream_b) = read_first_chunk(resp_bob.into_body()).await;

    assert_eq!(env.app_state.active_users.user_connections("alice").await, 1);
    assert_eq!(env.app_state.active_users.user_connections("bob").await, 1);
    assert_eq!(env.app_state.active_provider.get_provider_connections_count(), 2);

    // Replacing alice's playback id must leave bob's connection untouched.
    let resp_alice2 = env.send(&uri_alice, 41103, &[("X-Tuliprox-Playback-Id", "shared-id-across-users")]).await;
    assert_eq!(resp_alice2.status(), StatusCode::OK);
    let (_chunk_a2, _stream_a2) = read_first_chunk(resp_alice2.into_body()).await;

    assert_eq!(env.app_state.active_users.user_connections("alice").await, 1);
    assert_eq!(env.app_state.active_users.user_connections("bob").await, 1);
    assert_eq!(env.app_state.active_provider.get_provider_connections_count(), 2);
}

#[tokio::test]
async fn invalid_or_absent_playback_id_and_non_player_api_user_enforce_connection_limit() {
    let env = PlaybackIdTestEnv::new(4, 1, false).await;
    let uri_alice = format!("/live/alice/pass/{}.ts", env.live_ts_vid);

    let resp1 = env.send(&uri_alice, 41201, &[("X-Tuliprox-Playback-Id", "valid-base")]).await;
    assert_eq!(resp1.status(), StatusCode::OK);
    let (_chunk1, _stream1) = read_first_chunk(resp1.into_body()).await;

    // Missing header -> rejected.
    let resp_none = env.send(&uri_alice, 41202, &[]).await;
    assert_ne!(resp_none.status(), StatusCode::OK, "request without playback id must be rejected at limit");

    // Empty, space-containing, and >128 chars -> ignored -> rejected.
    let too_long = "x".repeat(129);
    for (idx, bad) in ["", "has space", " leading", too_long.as_str()].iter().enumerate() {
        let port = 41210 + u16::try_from(idx).expect("small port offset");
        let resp_bad = env.send(&uri_alice, port, &[("X-Tuliprox-Playback-Id", bad)]).await;
        assert_ne!(resp_bad.status(), StatusCode::OK, "invalid playback id {bad:?} must be ignored and limit enforced");
    }

    // Non-Player-API user (carol): valid header must be ignored -> second stream rejected.
    let uri_carol = format!("/live/carol/pass/{}.ts", env.live_ts_vid);
    let resp_carol1 = env.send(&uri_carol, 41220, &[("X-Tuliprox-Playback-Id", "carol-id")]).await;
    assert_eq!(resp_carol1.status(), StatusCode::OK);
    let (_chunk_c1, _stream_c1) = read_first_chunk(resp_carol1.into_body()).await;

    let resp_carol2 = env.send(&uri_carol, 41221, &[("X-Tuliprox-Playback-Id", "carol-id")]).await;
    assert_ne!(resp_carol2.status(), StatusCode::OK, "user without effective Player API must have playback id ignored");
}

#[tokio::test]
async fn apple_fallback_and_case_insensitive_header_and_boundary_lengths_work() {
    let env = PlaybackIdTestEnv::new(4, 1, false).await;
    let uri = format!("/live/alice/pass/{}.ts", env.live_ts_vid);

    let max_len = "a".repeat(128);
    let r1 = env.send(&uri, 41301, &[("x-playback-session-id", max_len.as_str())]).await;
    assert_eq!(r1.status(), StatusCode::OK);
    let (_c1, _s1) = read_first_chunk(r1.into_body()).await;

    let r2 = env.send(&uri, 41302, &[("X-PLAYBACK-SESSION-ID", max_len.as_str())]).await;
    assert_eq!(r2.status(), StatusCode::OK, "Apple fallback case-insensitive 128-char id must replace");
    let (_c2, s2) = read_first_chunk(r2.into_body()).await;
    assert_eq!(env.app_state.active_users.user_connections("alice").await, 1);

    // Boundary length 1 on primary lowercase header.
    let r3 = env.send(&uri, 41303, &[("x-tuliprox-playback-id", "Z")]).await;
    assert_ne!(r3.status(), StatusCode::OK, "different 1-char id must hit max_connections=1");
    drop(s2);
    // Re-test 1-char replacement cleanly.
    let r4 = env.send(&uri, 41304, &[("x-tuliprox-playback-id", "Z")]).await;
    assert_eq!(r4.status(), StatusCode::OK);
    let (_c4, _s4) = read_first_chunk(r4.into_body()).await;
    let r5 = env.send(&uri, 41305, &[("X-Tuliprox-Playback-Id", "Z")]).await;
    assert_eq!(r5.status(), StatusCode::OK, "1-char id case-insensitive must replace");
}

#[tokio::test]
async fn shared_stream_subscriber_replacement_preserves_producer_and_other_subscriber() {
    let env = PlaybackIdTestEnv::new(1, 1, true).await;
    let uri_alice = format!("/live/alice/pass/{}.ts", env.live_ts_vid);
    let uri_bob = format!("/live/bob/pass/{}.ts", env.live_ts_vid);

    let r_alice1 = env.send(&uri_alice, 41401, &[("X-Tuliprox-Playback-Id", "shared-sub-1")]).await;
    assert_eq!(r_alice1.status(), StatusCode::OK);
    let (_ca1, mut stream_alice1) = read_first_chunk(r_alice1.into_body()).await;

    let r_bob = env.send(&uri_bob, 41402, &[("X-Tuliprox-Playback-Id", "bob-sub-1")]).await;
    assert_eq!(r_bob.status(), StatusCode::OK);
    let (_cb, mut stream_bob) = read_first_chunk(r_bob.into_body()).await;

    assert_eq!(env.upstream_hits.load(Ordering::Acquire), 1, "shared stream must open only 1 upstream");
    assert_eq!(env.app_state.active_provider.get_provider_connections_count(), 1);

    let r_alice2 = env.send(&uri_alice, 41403, &[("X-Tuliprox-Playback-Id", "shared-sub-1")]).await;
    assert_eq!(r_alice2.status(), StatusCode::OK, "replacing shared subscriber at max_connections=1 must succeed");
    let (_ca2, _stream_alice2) = read_first_chunk(r_alice2.into_body()).await;

    assert_eq!(env.upstream_hits.load(Ordering::Acquire), 1, "shared producer must stay alive without re-opening");
    assert_eq!(env.app_state.active_provider.get_provider_connections_count(), 1);
    assert_eq!(env.app_state.active_users.user_connections("alice").await, 1);
    assert_eq!(env.app_state.active_users.user_connections("bob").await, 1);

    // Replaced alice subscriber terminates; bob stays open (still waiting on pending burst/next chunk, not closed).
    let a1_end = tokio::time::timeout(Duration::from_secs(2), stream_alice1.next()).await;
    assert!(matches!(a1_end, Ok(None | Some(Err(_)))), "old subscriber must end: {a1_end:?}");
    let bob_poll = tokio::time::timeout(Duration::from_millis(50), stream_bob.next()).await;
    assert!(bob_poll.is_err(), "bob's shared subscriber must remain open and healthy: {bob_poll:?}");
}

#[tokio::test]
async fn hls_followup_preserves_session_while_new_entry_replaces_with_same_playback_id() {
    let env = PlaybackIdTestEnv::new(4, 1, false).await;
    let uri = format!("/live/alice/pass/{}.m3u8", env.live_hls_vid);

    let entry1 = env.send(&uri, 41501, &[("X-Playback-Session-Id", "apple-tv-1")]).await;
    assert_eq!(entry1.status(), StatusCode::OK);
    let body1 = http_body_util::BodyExt::collect(entry1.into_body()).await.expect("body1").to_bytes();
    let body1_text = String::from_utf8_lossy(&body1).into_owned();
    let variant_uri = first_playlist_uri(&body1_text);

    // Follow-up variant request carrying the same Apple X-Playback-Session-Id MUST succeed and reuse the session.
    let variant = env.send(variant_uri, 41502, &[("X-Playback-Session-Id", "apple-tv-1")]).await;
    assert_eq!(variant.status(), StatusCode::OK, "HLS variant follow-up must not terminate its own session");
    let variant_body = http_body_util::BodyExt::collect(variant.into_body()).await.expect("variant body").to_bytes();
    let variant_body_text = String::from_utf8_lossy(&variant_body).into_owned();
    let seg_uri = first_playlist_uri(&variant_body_text);

    let seg_resp = env.send(seg_uri, 41503, &[("X-Playback-Session-Id", "apple-tv-1")]).await;
    assert_eq!(seg_resp.status(), StatusCode::OK, "HLS segment follow-up must not terminate its own session");
    let seg_bytes = http_body_util::BodyExt::collect(seg_resp.into_body()).await.expect("seg body").to_bytes();
    assert_eq!(seg_bytes.as_ref(), vec![0x47_u8; 188].as_slice());

    // A new HLS entry request with the same playback id replaces the old HLS session at max_connections=1.
    let entry2 = env.send(&uri, 41504, &[("X-Playback-Session-Id", "apple-tv-1")]).await;
    assert_eq!(entry2.status(), StatusCode::OK, "second HLS entry with same playback id must replace first");

    // The old segment URL belongs to the ended first session and must no longer stream.
    let stale_seg = env.send(seg_uri, 41505, &[("X-Playback-Session-Id", "apple-tv-1")]).await;
    assert_ne!(stale_seg.status(), StatusCode::OK, "replaced HLS session's segment url must be rejected");
}

#[tokio::test]
async fn playback_id_header_validation_and_player_api_gating() {
    use crate::api::api_utils::session_identity::{effective_playback_id_for_user, extract_valid_playback_id};
    use axum::http::HeaderMap;

    let mut headers = HeaderMap::new();
    headers.insert("x-tuliprox-playback-id", HeaderValue::from_static("primary-id"));
    headers.insert("x-playback-session-id", HeaderValue::from_static("fallback-id"));
    assert_eq!(extract_valid_playback_id(&headers), Some("primary-id"));

    let mut fallback_only = HeaderMap::new();
    fallback_only.insert("X-PLAYBACK-SESSION-ID", HeaderValue::from_static("apple-session"));
    assert_eq!(extract_valid_playback_id(&fallback_only), Some("apple-session"));

    // Invalid primary falls back cleanly; standalone invalid values are ignored.
    let mut invalid_then_fallback = HeaderMap::new();
    invalid_then_fallback.insert("x-tuliprox-playback-id", HeaderValue::from_static("bad space"));
    invalid_then_fallback.insert("x-playback-session-id", HeaderValue::from_static("ok-fallback"));
    assert_eq!(extract_valid_playback_id(&invalid_then_fallback), Some("ok-fallback"));

    for bad in ["", " ", "has space", "a".repeat(129).as_str()] {
        let mut h = HeaderMap::new();
        h.insert("X-Tuliprox-Playback-Id", HeaderValue::from_str(bad).expect("header value"));
        assert_eq!(extract_valid_playback_id(&h), None);
    }

    let min_val = "x";
    let max_val = "y".repeat(128);
    let mut h_min = HeaderMap::new();
    h_min.insert("X-Tuliprox-Playback-Id", HeaderValue::from_static(min_val));
    assert_eq!(extract_valid_playback_id(&h_min), Some(min_val));
    let mut h_max = HeaderMap::new();
    h_max.insert("X-Tuliprox-Playback-Id", HeaderValue::from_str(&max_val).expect("header value"));
    assert_eq!(extract_valid_playback_id(&h_max), Some(max_val.as_str()));

    let cfg = Config {
        web_ui: Some(
            (&WebUiConfigDto {
                enabled: true,
                user_ui_enabled: true,
                player_api_enabled: Some(true),
                auth: Some(WebAuthConfigDto {
                    enabled: true,
                    issuer: "test".to_string(),
                    secret: "test-secret".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            })
                .into(),
        ),
        ..Config::default()
    };
    let mut allowed_user = crate::model::ProxyUserCredentials::default();
    allowed_user.username = "alice".to_string();
    allowed_user.ui_enabled = true;
    assert_eq!(effective_playback_id_for_user(&cfg, &allowed_user, &h_min), Some(min_val));

    let mut disabled_user = allowed_user.clone();
    disabled_user.ui_enabled = false;
    disabled_user.player_features.api_enabled = Some(false);
    assert_eq!(effective_playback_id_for_user(&cfg, &disabled_user, &h_min), None);

    let test_app_state = super::create_test_app_state();
    let mut recording_user = crate::api::api_utils::create_recording_proxy_user(&test_app_state);
    recording_user.ui_enabled = true;
    assert_eq!(effective_playback_id_for_user(&cfg, &recording_user, &h_min), None);
}

async fn assert_concurrent_replacement_during_open_blocks_stale_stream(user_max_connections: u32) {
    let env = Arc::new(PlaybackIdTestEnv::new(4, user_max_connections, false).await);
    let fast_uri = format!("/live/alice/pass/{}.ts", env.live_ts_vid);
    let (user, target) = env.app_state.app_config.get_target_for_username("alice").expect("alice target");
    let input = env.app_state.app_config.get_input_by_name(&"provider_1".intern()).expect("provider_1 input");
    let channel = tuliprox_repository::load_target_playlist_item(
        env.slow_ts_vid,
        &env.app_state.app_config,
        &env.app_state.playlists,
        &target,
        Some(XtreamCluster::Live),
    )
    .await
    .map(|item| match item {
        tuliprox_repository::LoadedTargetPlaylistItem::Xtream(pli) => {
            shared::model::create_stream_channel_with_type(target.id, &pli, PlaylistItemType::Live)
        }
        tuliprox_repository::LoadedTargetPlaylistItem::M3u(pli) => pli.to_stream_channel(target.id),
    })
    .expect("slow channel");

    let addr1: SocketAddr = "127.0.0.1:41601".parse().expect("addr1");
    let fp1 = crate::auth::Fingerprint::new("fp-slow".to_string(), addr1.ip().to_string(), addr1);
    let tok1 = crate::api::api_utils::create_session_fingerprint(&fp1, &user.username, env.slow_ts_vid, true);
    let mut headers1 = axum::http::HeaderMap::new();
    headers1.insert("X-Tuliprox-Playback-Id", HeaderValue::from_static("race-id"));

    let env1 = Arc::clone(&env);
    let slow_url = channel.url.clone();
    let slow_handle = tokio::spawn(async move {
        use axum::response::IntoResponse;
        crate::api::api_utils::stream_response(
            &fp1,
            &env1.app_state,
            &tok1,
            None,
            channel,
            crate::api::api_utils::PlaybackSource::Single { input: &input, stream_url: slow_url.as_ref() },
            None,
            &headers1,
            &target,
            &user,
            shared::model::UserConnectionPermission::Allowed,
            crate::api::model::ConnectionKind::Normal,
            false,
            None,
            false,
            false,
        )
        .await
        .into_response()
    });

    tokio::time::timeout(Duration::from_secs(2), env.slow_open_entered.notified())
        .await
        .expect("request 1 must reach upstream open");

    let resp2 = tokio::time::timeout(
        Duration::from_secs(2),
        env.send(&fast_uri, 41602, &[("X-Tuliprox-Playback-Id", "race-id")]),
    )
    .await
    .expect("replacement request 2 must not block");
    assert_eq!(resp2.status(), StatusCode::OK, "replacement request 2 must succeed");
    let (_chunk2, _stream2) = read_first_chunk(resp2.into_body()).await;
    assert_eq!(env.app_state.active_users.user_connections("alice").await, 1);

    env.slow_open_release.notify_one();
    let resp1 = tokio::time::timeout(Duration::from_secs(2), slow_handle)
        .await
        .expect("request 1 completes")
        .expect("request 1 join");
    assert_ne!(
        resp1.status(),
        StatusCode::OK,
        "replaced request 1 (max_connections={user_max_connections}) must not register stream after replacement"
    );
    assert_eq!(
        env.app_state.active_users.user_connections("alice").await,
        1,
        "only request 2 may remain connected (max_connections={user_max_connections})"
    );
    assert_eq!(
        env.app_state.active_users.active_streams().await.len(),
        1,
        "only request 2's stream row may be active (max_connections={user_max_connections})"
    );
}

#[tokio::test]
async fn concurrent_playback_id_replacement_during_upstream_open_blocks_stale_stream_at_limit() {
    assert_concurrent_replacement_during_open_blocks_stale_stream(1).await;
}

#[tokio::test]
async fn concurrent_playback_id_replacement_during_upstream_open_blocks_stale_stream_without_limits() {
    assert_concurrent_replacement_during_open_blocks_stale_stream(0).await;
}

#[tokio::test]
async fn redirect_proxy_and_dash_redirect_do_not_replace_active_playback_id_connection() {
    let env = PlaybackIdTestEnv::new_with_force_redirect_live(4, 2, false, true).await;
    let vod_uri = format!("/movie/alice/pass/{}.mp4", env.vod_vid);
    let live_redirect_uri = format!("/live/alice/pass/{}.ts", env.live_ts_vid);
    let dash_redirect_uri = format!("/live/alice/pass/{}.mpd", env.live_ts_vid);
    let m3u_redirect_uri = format!("/m3u-stream/live/alice/pass/{}.ts", env.live_ts_vid);

    let vod_resp = env.send(&vod_uri, 41701, &[("X-Tuliprox-Playback-Id", "dev1")]).await;
    assert_eq!(vod_resp.status(), StatusCode::OK);
    let (_chunk, _vod_stream) = read_first_chunk(vod_resp.into_body()).await;
    assert_eq!(env.app_state.active_users.user_connections("alice").await, 1);

    for redirect_uri in [&live_redirect_uri, &dash_redirect_uri, &m3u_redirect_uri] {
        let resp = env.send(redirect_uri, 41702, &[("X-Tuliprox-Playback-Id", "dev1")]).await;
        assert_eq!(resp.status(), StatusCode::FOUND, "redirect route {redirect_uri} must return 302");
        assert_eq!(
            env.app_state.active_users.user_connections("alice").await,
            1,
            "redirect route {redirect_uri} must not terminate active proxied connection"
        );
        assert_eq!(
            env.app_state.active_users.active_streams().await.len(),
            1,
            "redirect route {redirect_uri} must leave active proxied stream intact"
        );
    }
}

#[tokio::test]
async fn local_file_stream_registers_playback_id_without_limits_and_replaces_cleanly() {
    let env = PlaybackIdTestEnv::new(1, 0, false).await;
    let user = env.app_state.app_config.get_user_credentials("alice").expect("alice");
    let input = Arc::clone(&env.app_state.app_config.sources.load().inputs[0]);
    let channel = shared::model::StreamChannel {
        virtual_id: env.vod_vid,
        provider_id: 104,
        item_type: PlaylistItemType::LocalVideo,
        url: Arc::from("file:///tmp/local-vod.mp4"),
        title: "Local VOD".intern(),
        ..Default::default()
    };

    let open_local = |port: u16, token: &'static str, pid: &'static str| {
        let app_state = Arc::clone(&env.app_state);
        let input = Arc::clone(&input);
        let user = user.clone();
        let channel = channel.clone();
        async move {
            let addr: SocketAddr = format!("127.0.0.1:{port}").parse().expect("addr");
            let fp = crate::auth::Fingerprint::new(format!("fp-{port}"), addr.ip().to_string(), addr);
            let mut headers = axum::http::HeaderMap::new();
            headers.insert("x-tuliprox-playback-id", HeaderValue::from_static(pid));
            let body = futures::stream::iter([
                Ok(bytes::Bytes::from_static(b"LOCAL")),
                Ok(bytes::Bytes::from_static(b"DATA")),
            ]);
            let pending = futures::stream::pending::<Result<bytes::Bytes, crate::api::model::StreamError>>();
            let boxed: crate::api::model::BoxedProviderStream = Box::pin(futures::StreamExt::chain(body, pending));
            crate::api::api_utils::local_files::activate_local_file_stream(
                &app_state,
                &fp,
                &channel,
                &headers,
                &input,
                &user,
                shared::model::UserConnectionPermission::Allowed,
                crate::api::model::ConnectionKind::Normal,
                Some(token),
                None,
                boxed,
            )
            .await
        }
    };

    let stream1 = open_local(41801, "tok-local-1", "local-dev-1").await.expect("first local stream admitted");
    let _drop_guard1 = stream1;
    assert_eq!(env.app_state.active_users.user_connections("alice").await, 1);

    let stream2 = open_local(41802, "tok-local-2", "local-dev-1").await.expect("replacement local stream admitted");
    let _drop_guard2 = stream2;
    assert_eq!(
        env.app_state.active_users.user_connections("alice").await,
        1,
        "local file stream with max_connections=0 must tag playback_id atomically so same playback_id replaces it"
    );
    let active = env.app_state.active_users.active_streams().await;
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].session_token.as_deref(), Some("tok-local-2"));
}
