use super::support::FakeOriginMode;
use crate::{
    api::{
        api_utils::failover_group::{member_key, CircuitView},
        endpoints::{
            hdhomerun_api::hdhr_api_register, m3u_api::m3u_api_register, xmltv_api::xmltv_api_register,
            xtream_api::xtream_api_register,
        },
        model::{AppState, HdHomerunAppState},
    },
    model::{
        Config, ConfigInput, ConfigProvider, ConfigTarget, Epg, HdHomeRunDeviceConfig, HdHomeRunTargetOutput,
        M3uTargetOutput, ProxyUserCredentials, SourcesConfig, StreamConfig, TargetOutput, XtreamTargetOutput,
    },
    repository::{
        get_target_storage_path, persist_playlist_views_with_groups, FailoverGroupBatch, FailoverGroupOrigin,
        FailoverGroupRecord, FailoverGroupStore, PendingFailoverGroup, PlaylistStorageState,
        TargetPlaylistPersistOptions,
    },
};
use arc_swap::ArcSwap;
use axum::{
    body::Body,
    http::{HeaderMap, Request},
    response::Response,
};
use shared::{
    foundation::Filter,
    model::{
        AdmissionStrategy, ClusterFlags, ConfigPaths, ConfigTargetOptions, FailoverGroupPolicy, FailoverGroupsConfig,
        FailoverGroupsTargetConfig, HdHomeRunTargetOutputDto, InputType, LiveStreamProperties, M3uTargetOutputDto,
        PlaylistItem, PlaylistItemHeader, PlaylistItemType, ProcessingOrder, ProxyUserStatus, StreamProperties,
        TargetType, UUIDType, VirtualId, XtreamCluster, XtreamTargetOutputDto,
    },
    utils::Internable,
};
use std::{
    collections::HashMap,
    net::SocketAddr,
    str::FromStr,
    sync::{atomic::AtomicI8, Arc},
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tower::ServiceExt;

pub(super) fn ts_fixture() -> Vec<u8> {
    let mut data = vec![0u8; 188 * 4];
    for i in 0..4 {
        data[i * 188] = 0x47;
        data[i * 188 + 1] = 0x1f;
        data[i * 188 + 2] = 0xff;
        data[i * 188 + 3] = 0x10;
    }
    data
}

/// A short MPEG-TS program: PAT, PMT with one H.264 stream on `video_pid`, a keyframe PES start,
/// then `continuation` payload packets on `video_pid`. The video PID tells members apart.
pub(crate) fn member_ts(video_pid: u16, continuation: usize) -> Vec<u8> {
    fn packet(pid: u16, pusi: bool, adaptation: Option<u8>, payload: &[u8]) -> Vec<u8> {
        let pid_bytes = pid.to_be_bytes();
        let mut packet = vec![0x47, (if pusi { 0x40 } else { 0 }) | (pid_bytes[0] & 0x1F), pid_bytes[1]];
        if let Some(flags) = adaptation {
            let adaptation_len = 183 - payload.len();
            packet.extend([0x30, u8::try_from(adaptation_len).expect("adaptation"), flags]);
            packet.extend(std::iter::repeat_n(0xFF, adaptation_len - 1));
        } else {
            packet.push(0x10);
        }
        packet.extend_from_slice(payload);
        packet.resize(188, 0xFF);
        packet
    }
    fn section(mut body: Vec<u8>) -> Vec<u8> {
        let length = u8::try_from(body.len() - 3 + 4).expect("section length");
        body[1] = 0xB0;
        body[2] = length;
        let crc = tuliprox_mpegts::packet::mpeg2_crc32(&body);
        body.extend_from_slice(&crc.to_be_bytes());
        let mut payload = vec![0x00];
        payload.extend(body);
        payload
    }
    let pid = video_pid.to_be_bytes();
    let pat = section(vec![0x00, 0, 0, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xF0, 0x00]);
    let pmt = section(vec![
        0x02,
        0,
        0,
        0x00,
        0x01,
        0xC1,
        0x00,
        0x00,
        0xE0 | pid[0],
        pid[1],
        0xF0,
        0x00,
        0x1B,
        0xE0 | pid[0],
        pid[1],
        0xF0,
        0x00,
    ]);
    let keyframe = [
        0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x80, 0x05, 0x21, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
        0x65,
    ];
    let mut ts = packet(0, true, None, &pat);
    ts.extend(packet(0x1000, true, None, &pmt));
    ts.extend(packet(video_pid, true, Some(0x40), &keyframe));
    for _ in 0..continuation {
        ts.extend(packet(video_pid, false, None, &[0x11; 184]));
    }
    ts
}

/// Whether `bytes` holds a TS packet on `pid` at a packet boundary.
pub(crate) fn has_pid(bytes: &[u8], pid: u16) -> bool {
    bytes
        .as_chunks::<188>()
        .0
        .iter()
        .any(|packet| packet[0] == 0x47 && ((u16::from(packet[1] & 0x1F) << 8) | u16::from(packet[2])) == pid)
}

/// Reads the response body until `done` holds for what was read, the body ends, or `timeout`
/// passes. Returns the bytes read and whether the body ended.
pub(crate) async fn read_body_until(
    response: Response,
    timeout: std::time::Duration,
    done: impl Fn(&[u8]) -> bool,
) -> (Vec<u8>, bool) {
    use futures::StreamExt;
    let mut body = response.into_body().into_data_stream();
    let mut read = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if done(&read) {
            return (read, false);
        }
        match tokio::time::timeout_at(deadline, body.next()).await {
            Ok(Some(Ok(chunk))) => read.extend_from_slice(&chunk),
            Ok(Some(Err(_)) | None) => return (read, true),
            Err(_) => return (read, false),
        }
    }
}

pub(crate) struct TrackedOrigin {
    pub addr: SocketAddr,
    pub requests: Arc<std::sync::Mutex<Vec<String>>>,
    pub _task: tokio::task::JoinHandle<()>,
}

#[allow(clippy::too_many_lines)]
async fn spawn_tracked_origin(mode: FakeOriginMode) -> TrackedOrigin {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
    let addr = listener.local_addr().expect("test origin address");
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let requests_clone = Arc::clone(&requests);

    let task = tokio::spawn(async move {
        for _ in 0..20 {
            let Ok((mut socket, _)) = listener.accept().await else { break };
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0_u8; 1024];
                let Ok(read) = socket.read(&mut chunk).await else { break };
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            if request.is_empty() {
                continue;
            }
            let request_str = String::from_utf8_lossy(&request).to_string();
            requests_clone.lock().expect("mutex").push(request_str.clone());

            match &mode {
                FakeOriginMode::EmptyBody => {
                    let head =
                        "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.shutdown().await;
                }
                FakeOriginMode::BlockFirstChunk(notify) => {
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(head.as_bytes()).await;
                    let mut probe = [0_u8; 1];
                    tokio::select! {
                        () = notify.notified() => {},
                        _ = socket.read(&mut probe) => {},
                    }
                    let _ = socket.shutdown().await;
                }
                FakeOriginMode::ErrorBeforeFirstChunk => {
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.shutdown().await;
                }
                FakeOriginMode::ErrorAfterFirstChunk(first_chunk) => {
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(head.as_bytes()).await;
                    let chunk_header = format!("{:x}\r\n", first_chunk.len());
                    let _ = socket.write_all(chunk_header.as_bytes()).await;
                    let _ = socket.write_all(first_chunk).await;
                    let _ = socket.write_all(b"\r\n").await;
                    let _ = socket.shutdown().await;
                }
                FakeOriginMode::FixedBytes(bytes) => {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(bytes).await;
                    let _ = socket.shutdown().await;
                }
                FakeOriginMode::TsThenStall(bytes) => {
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(bytes).await;
                    // Keeps the connection open without data until the client goes away.
                    let mut probe = [0_u8; 1];
                    let _ = socket.read(&mut probe).await;
                }
                FakeOriginMode::TsStream(bytes) => {
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(head.as_bytes()).await;
                    while socket.write_all(bytes).await.is_ok() {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
                FakeOriginMode::Silent => {
                    let mut probe = [0_u8; 1];
                    let _ = socket.read(&mut probe).await;
                }
                FakeOriginMode::Status(status) => {
                    let head = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.shutdown().await;
                }
                FakeOriginMode::Body { content_type, bytes } => {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(bytes).await;
                    let _ = socket.shutdown().await;
                }
                FakeOriginMode::Hls => {
                    if request_str.contains(".ts") {
                        let bytes = ts_fixture();
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            bytes.len()
                        );
                        let _ = socket.write_all(head.as_bytes()).await;
                        let _ = socket.write_all(&bytes).await;
                        let _ = socket.shutdown().await;
                    } else {
                        let body = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:10\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:10.0,\nsegment1.ts\n#EXTINF:10.0,\nsegment2.ts\n";
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = socket.write_all(head.as_bytes()).await;
                        let _ = socket.write_all(body.as_bytes()).await;
                        let _ = socket.shutdown().await;
                    }
                }
            }
        }
    });

    TrackedOrigin { addr, requests, _task: task }
}

#[allow(dead_code)]
pub(crate) fn first_segment_url(manifest: &str) -> String {
    manifest
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .unwrap_or_default()
}

#[allow(dead_code)]
pub(crate) struct GroupPlaybackEnv {
    pub app_state: Arc<AppState>,
    pub target: Arc<ConfigTarget>,
    pub group_vid: u32,
    pub plain_vid: u32,
    pub favourite_vid: u32,
    pub group_record: Arc<FailoverGroupRecord>,
    pub origins: HashMap<String, TrackedOrigin>,
    pub router: axum::Router,
    pub hdhr_router: Option<axum::Router>,
    pub _dir: TempDir,
}

#[allow(dead_code)]
impl GroupPlaybackEnv {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(inputs: &[&str]) -> GroupPlaybackEnvBuilder { Self::builder(inputs) }

    pub fn builder(inputs: &[&str]) -> GroupPlaybackEnvBuilder {
        GroupPlaybackEnvBuilder {
            inputs: inputs.iter().map(|s| (*s).to_string()).collect(),
            origin_modes: HashMap::new(),
            qos: Vec::new(),
            hls: false,
            stalker: None,
            user_limit: None,
            admission: None,
            favourite: false,
            plain_channel: true,
            member_archives: Vec::new(),
            epg: None,
            epg_channel_id: None,
            hdhomerun: false,
            failover_groups: None,
            hls_cache: false,
            share_ts: false,
            sleep_timer_mins: None,
            max_connections: HashMap::new(),
            channel_unavailable_video: false,
        }
    }

    pub async fn get_xtream_live_streams(&self) -> Response {
        let mut req = Request::builder()
            .uri("/player_api.php?username=viewer&password=secret&action=get_live_streams")
            .body(Body::empty())
            .expect("request builder");
        req.extensions_mut().insert(axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))));
        self.router.clone().oneshot(req).await.expect("endpoint response")
    }

    pub async fn get_m3u_playlist(&self) -> Response {
        let mut req = Request::builder()
            .uri("/get.php?username=viewer&password=secret&type=m3u_plus&output=ts")
            .body(Body::empty())
            .expect("request builder");
        req.extensions_mut().insert(axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))));
        self.router.clone().oneshot(req).await.expect("endpoint response")
    }

    pub async fn get_hdhomerun_lineup(&self) -> Response {
        let Some(ref hdhr_router) = self.hdhr_router else {
            panic!("HDHomeRun router not initialized; did you call .with_hdhomerun()?");
        };
        let mut req = Request::builder().uri("/lineup.json").body(Body::empty()).expect("request builder");
        req.extensions_mut().insert(axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))));
        hdhr_router.clone().oneshot(req).await.expect("endpoint response")
    }

    pub async fn get_xmltv_epg(&self) -> Response {
        let mut req = Request::builder()
            .uri("/xmltv.php?username=viewer&password=secret")
            .body(Body::empty())
            .expect("request builder");
        req.extensions_mut().insert(axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))));
        self.router.clone().oneshot(req).await.expect("endpoint response")
    }

    pub async fn response_text(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("read body");
        String::from_utf8(bytes.to_vec()).expect("utf-8 body")
    }

    /// Like `get_xtream_live`, from another client socket of the same user.
    pub async fn get_xtream_live_from(&self, vid: u32, port: u16) -> Response {
        let mut req = Request::builder()
            .uri(format!("/live/viewer/secret/{vid}.ts"))
            .body(Body::empty())
            .expect("request builder");
        req.extensions_mut().insert(axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], port))));
        self.router.clone().oneshot(req).await.expect("endpoint response")
    }

    pub async fn get_xtream_live(&self, vid: u32) -> Response {
        let mut req = Request::builder()
            .uri(format!("/live/viewer/secret/{vid}.ts"))
            .body(Body::empty())
            .expect("request builder");
        req.extensions_mut().insert(axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))));
        self.router.clone().oneshot(req).await.expect("endpoint response")
    }

    pub async fn get_xtream_live_with_range(&self, vid: u32, range: &str) -> Response {
        let mut req = Request::builder()
            .uri(format!("/live/viewer/secret/{vid}.ts"))
            .header("Range", range)
            .body(Body::empty())
            .expect("request builder");
        req.extensions_mut().insert(axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))));
        self.router.clone().oneshot(req).await.expect("endpoint response")
    }

    pub async fn get_m3u_live(&self, vid: u32) -> Response {
        let mut req = Request::builder()
            .uri(format!("/m3u-stream/live/viewer/secret/{vid}.ts"))
            .body(Body::empty())
            .expect("request builder");
        req.extensions_mut().insert(axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))));
        self.router.clone().oneshot(req).await.expect("endpoint response")
    }

    pub fn origin_hits(&self, input: &str) -> usize {
        self.origins.get(input).map_or(0, |o| o.requests.lock().expect("mutex").len())
    }

    pub fn last_request_headers(&self, input: &str) -> HeaderMap {
        let mut map = HeaderMap::new();
        let Some(origin) = self.origins.get(input) else { return map };
        let guard = origin.requests.lock().expect("mutex");
        let Some(last) = guard.last() else { return map };
        for line in last.lines().skip(1) {
            if line.is_empty() {
                break;
            }
            if let Some((name, val)) = line.split_once(':') {
                if let (Ok(n), Ok(v)) =
                    (axum::http::HeaderName::from_str(name.trim()), axum::http::HeaderValue::from_str(val.trim()))
                {
                    map.insert(n, v);
                }
            }
        }
        map
    }

    pub fn circuit(&self, input: &str) -> CircuitView {
        let member =
            self.group_record.members.iter().find(|m| m.input_name.as_ref() == input).expect("member for input");
        let key = member_key(member);
        self.app_state.member_health.circuit_view(&key, std::time::Instant::now())
    }

    pub fn input_circuit(&self, input: &str) -> CircuitView {
        self.app_state.member_health.input_circuit_view(input, std::time::Instant::now())
    }

    pub fn open_circuit(&self, input: &str) {
        let member =
            self.group_record.members.iter().find(|m| m.input_name.as_ref() == input).expect("member for input");
        let key = member_key(member);
        for _ in 0..10 {
            if matches!(self.circuit(input), CircuitView::Open { .. }) {
                break;
            }
            self.app_state.member_health.record_classified_failure(
                &key,
                tuliprox_session::failover_group::FailureClass::Stream,
                std::time::Instant::now(),
            );
        }
    }

    pub fn member_vid(&self, input: &str) -> u32 {
        let member =
            self.group_record.members.iter().find(|m| m.input_name.as_ref() == input).expect("member for input");
        member.member_vid.get()
    }

    pub fn favourite_vid(&self) -> u32 { self.favourite_vid }

    pub fn hold(&self, input: &str, priority: i8) -> Vec<tuliprox_core::model::provider::ProviderHandle> {
        let mut handles = Vec::new();
        while let Some(h) = self.app_state.active_provider.acquire_connection(
            &input.into(),
            &SocketAddr::from(([127, 0, 0, 1], 200)),
            priority,
            tuliprox_session::active_provider_manager::ConnectionKind::Normal,
        ) {
            handles.push(h);
        }
        handles
    }

    pub async fn get_hls_manifest(&self, vid: u32) -> Response {
        let mut req = Request::builder()
            .uri(format!("/live/viewer/secret/{vid}.m3u8"))
            .body(Body::empty())
            .expect("request builder");
        req.extensions_mut().insert(axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))));
        self.router.clone().oneshot(req).await.expect("endpoint response")
    }

    pub async fn get_hls_manifest_text(&self, vid: u32) -> String {
        let response = self.get_hls_manifest(vid).await;
        assert_eq!(response.status(), 200);
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.expect("bytes");
        String::from_utf8(bytes.to_vec()).expect("utf-8 manifest")
    }

    pub async fn refresh_hls_manifest(&self, master_text: &str) -> Response {
        let uri = first_segment_url(master_text);
        self.get(&uri).await
    }

    pub async fn get(&self, uri: &str) -> Response {
        let path = if let Ok(parsed) = url::Url::parse(uri) {
            let mut p = parsed.path().to_string();
            if let Some(stripped) = p.strip_prefix("/iptv") {
                p = stripped.to_string();
            }
            if let Some(q) = parsed.query() {
                p.push('?');
                p.push_str(q);
            }
            p
        } else if let Some(stripped) = uri.strip_prefix("/iptv") {
            stripped.to_string()
        } else {
            uri.to_string()
        };
        let mut req = Request::builder().uri(path).body(Body::empty()).expect("request builder");
        req.extensions_mut().insert(axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))));
        self.router.clone().oneshot(req).await.expect("endpoint response")
    }

    pub async fn bound_member_input(&self, username: &str) -> String {
        self.app_state.active_users.bound_member_input(username).await.map(|s| s.to_string()).unwrap_or_default()
    }

    pub async fn active_session_vids(&self, username: &str) -> Vec<u32> {
        self.app_state.active_users.active_session_vids(username).await
    }

    pub fn slots_in_use(&self) -> usize { self.app_state.active_provider.get_provider_connections_count() }

    pub async fn republish_without_groups(&self) {
        let mut playlist = vec![shared::model::PlaylistGroup {
            id: 1,
            title: "News".intern(),
            channels: vec![PlaylistItem {
                header: PlaylistItemHeader {
                    uuid: UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000002"),
                    id: "plain".intern(),
                    name: "Plain TV".intern(),
                    title: "Plain TV".intern(),
                    group: "News".intern(),
                    url: "http://127.0.0.1/plain".intern(),
                    input_name: self.inputs()[0].as_str().intern(),
                    item_type: PlaylistItemType::Live,
                    xtream_cluster: XtreamCluster::Live,
                    ..PlaylistItemHeader::default()
                },
            }],
            xtream_cluster: XtreamCluster::Live,
        }];
        persist_playlist_views_with_groups(
            &self.app_state.app_config,
            &mut playlist,
            None,
            None,
            &self.target,
            Some(&self.app_state.playlists),
            TargetPlaylistPersistOptions::default(),
            &mut FailoverGroupBatch::default(),
        )
        .await
        .expect("republished without groups");
    }

    fn inputs(&self) -> Vec<String> { self.origins.keys().cloned().collect() }
}

#[allow(clippy::struct_excessive_bools)]
pub(crate) struct GroupPlaybackEnvBuilder {
    inputs: Vec<String>,
    origin_modes: HashMap<String, FakeOriginMode>,
    qos: Vec<(String, u32, u8)>,
    hls: bool,
    stalker: Option<String>,
    user_limit: Option<u32>,
    admission: Option<Vec<AdmissionStrategy>>,
    favourite: bool,
    plain_channel: bool,
    member_archives: Vec<Option<i32>>,
    epg: Option<Epg>,
    epg_channel_id: Option<String>,
    hdhomerun: bool,
    failover_groups: Option<FailoverGroupsConfig>,
    hls_cache: bool,
    share_ts: bool,
    sleep_timer_mins: Option<u32>,
    max_connections: HashMap<String, u16>,
    channel_unavailable_video: bool,
}

#[allow(dead_code)]
impl GroupPlaybackEnvBuilder {
    pub fn hls_cache(mut self) -> Self {
        self.hls_cache = true;
        self.hls = true;
        self
    }

    /// Serves the channel-unavailable custom video (test fixture, PIDs 0x100/0x101).
    pub fn channel_unavailable_video(mut self) -> Self {
        self.channel_unavailable_video = true;
        self
    }

    pub fn sleep_timer_mins(mut self, mins: u32) -> Self {
        self.sleep_timer_mins = Some(mins);
        self
    }

    pub fn max_connections(mut self, input: &str, max: u16) -> Self {
        self.max_connections.insert(input.to_string(), max);
        self
    }

    pub fn share_ts(mut self) -> Self {
        self.share_ts = true;
        self
    }

    /// The `config.yml` failover group settings.
    pub fn with_failover_groups(mut self, failover_groups: FailoverGroupsConfig) -> Self {
        self.failover_groups = Some(failover_groups);
        self
    }

    /// The `config.yml` failover settings.
    pub fn with_failover(self, failover: shared::model::FailoverGroupRuntimeConfig) -> Self {
        let failover_groups = FailoverGroupsConfig { failover, ..self.failover_groups.clone().unwrap_or_default() };
        self.with_failover_groups(failover_groups)
    }

    pub fn origin_mode(mut self, input: &str, mode: FakeOriginMode) -> Self {
        self.origin_modes.insert(input.to_string(), mode);
        self
    }

    pub fn origin_status(mut self, input: &str, status: u16) -> Self {
        self.origin_modes.insert(input.to_string(), FakeOriginMode::Status(status));
        self
    }

    pub fn origin_body(mut self, input: &str, content_type: &str, body: Vec<u8>) -> Self {
        self.origin_modes
            .insert(input.to_string(), FakeOriginMode::Body { content_type: content_type.to_string(), bytes: body });
        self
    }

    pub fn with_qos(mut self, input: &str, provider_id: u32, score: u8) -> Self {
        self.qos.push((input.to_string(), provider_id, score));
        self
    }

    pub fn with_favourite(mut self) -> Self {
        self.favourite = true;
        self
    }

    pub fn hls(mut self) -> Self {
        self.hls = true;
        self
    }

    pub fn stalker(mut self, input: &str) -> Self {
        self.stalker = Some(input.to_string());
        self
    }

    pub fn user_limit(mut self, limit: u32) -> Self {
        self.user_limit = Some(limit);
        self
    }

    pub fn admission(mut self, strategies: Vec<AdmissionStrategy>) -> Self {
        self.admission = Some(strategies);
        self
    }

    pub fn without_plain_channel(mut self) -> Self {
        self.plain_channel = false;
        self
    }

    pub fn with_member_archives(mut self, archives: &[Option<i32>]) -> Self {
        self.member_archives = archives.to_vec();
        self
    }

    pub fn with_epg(mut self, epg: Epg, channel_id: &str) -> Self {
        self.epg = Some(epg);
        self.epg_channel_id = Some(channel_id.to_string());
        self
    }

    pub fn with_hdhomerun(mut self) -> Self {
        self.hdhomerun = true;
        self
    }

    #[allow(clippy::too_many_lines)]
    pub async fn build(self) -> GroupPlaybackEnv {
        let dir = TempDir::new().expect("temp dir");

        let mut origins = HashMap::new();
        for input in &self.inputs {
            let mode = self.origin_modes.get(input).cloned().unwrap_or_else(|| {
                if self.hls {
                    FakeOriginMode::Hls
                } else {
                    FakeOriginMode::FixedBytes(ts_fixture())
                }
            });
            let origin = spawn_tracked_origin(mode).await;
            origins.insert(input.clone(), origin);
        }

        let mut config_inputs = Vec::new();
        for input_name in &self.inputs {
            let origin = &origins[input_name];
            let is_stalker = self.stalker.as_deref() == Some(input_name.as_str());
            let provider = ConfigProvider {
                name: Arc::from(input_name.as_str()),
                urls: vec![Arc::from(format!("http://{}", origin.addr).as_str())],
                provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
                dns: None,
                current_url_index: std::sync::atomic::AtomicUsize::new(0),
                dns_cache: Arc::default(),
            };
            let input = Arc::new(ConfigInput {
                id: u16::try_from(config_inputs.len()).unwrap_or(0) + 1,
                name: Arc::from(input_name.as_str()),
                input_type: if is_stalker { InputType::Stalker } else { InputType::Xtream },
                url: format!("http://{}", origin.addr),
                username: Some("viewer".to_string()),
                password: Some("secret".to_string()),
                priority: 1,
                max_connections: self.max_connections.get(input_name).copied().unwrap_or(10),
                enabled: true,
                provider_configs: Some(vec![Arc::new(provider)]),
                stalker: if is_stalker {
                    Some(crate::model::StalkerInputConfig {
                        device: None,
                        auth_mode: shared::model::StalkerAuthMode::Auto,
                        mag_preset: shared::model::StalkerMagPreset::GenericSafe,
                        endpoint_preference: shared::model::StalkerEndpointPreference::ServerLoad,
                        size_caps: None,
                        catalog_max_pages: None,
                        username: None,
                        password: None,
                    })
                } else {
                    None
                },
                ..ConfigInput::default()
            });
            config_inputs.push(input);
        }

        let mut user = ProxyUserCredentials::default();
        user.username = "viewer".to_string();
        user.password = "secret".to_string();
        user.status = Some(ProxyUserStatus::Active);
        user.output_clusters = ClusterFlags::all();
        user.max_connections = self.user_limit.unwrap_or(2);
        user.proxy = shared::model::ProxyType::Reverse(None);

        let storage_dir = dir.path().to_string_lossy().into_owned();
        let config = Config {
            custom_stream_response_enabled: self.channel_unavailable_video,
            sleep_timer_mins: self.sleep_timer_mins,
            storage_dir: storage_dir.clone(),
            user_access_control: true,
            failover_groups: self.failover_groups.clone().unwrap_or_default(),
            reverse_proxy: Some(crate::model::ReverseProxyConfig {
                resource_rewrite_disabled: false,
                rewrite_secret: [0; 16],
                resource_retry: crate::model::ResourceRetryConfig::default(),
                disabled_header: None,
                stream: Some(StreamConfig { admission_strategies: self.admission, ..StreamConfig::default() }),
                cache: None,
                rate_limit: None,
                geoip: None,
                stream_history: None,
                qos_aggregation: None,
                hls_cache: self
                    .hls_cache
                    .then(|| crate::model::HlsCacheConfig::from(&shared::model::HlsCacheConfigDto::default())),
                hls_origin_telemetry: None,
            }),
            ..Config::default()
        };

        let target_name = "test_target".to_string();
        let api_proxy = crate::model::ApiProxyConfig {
            server: vec![crate::model::ApiProxyServerInfo {
                name: "default".to_string(),
                protocol: "https".to_string(),
                host: "example.test".to_string(),
                port: None,
                timezone: "UTC".to_string(),
                message: String::new(),
                path: Some("iptv".to_string()),
            }],
            user: vec![crate::model::TargetUser { target: target_name.clone(), credentials: vec![Arc::new(user)] }],
            ..Default::default()
        };

        let mut target_outputs = vec![
            TargetOutput::Xtream(XtreamTargetOutput::from(&XtreamTargetOutputDto::default())),
            TargetOutput::M3u(M3uTargetOutput::from(&M3uTargetOutputDto {
                filename: Some("playlist.m3u".to_string()),
                ..M3uTargetOutputDto::default()
            })),
        ];
        if self.hdhomerun {
            target_outputs.push(TargetOutput::HdHomeRun(HdHomeRunTargetOutput::from(&HdHomeRunTargetOutputDto {
                device: "test_hdhr".to_string(),
                username: "viewer".to_string(),
                use_output: Some(TargetType::M3u),
            })));
        }

        let target = Arc::new(ConfigTarget {
            id: 1,
            enabled: true,
            name: "test_target".to_string(),
            options: Some(ConfigTargetOptions {
                failover_groups: Some(FailoverGroupsTargetConfig {
                    enabled: true,
                    ..FailoverGroupsTargetConfig::default()
                }),
                share_live_streams: shared::model::ConfigTargetShareLiveStreams {
                    hls: self.hls_cache,
                    mpeg_ts: self.share_ts,
                },
                ..ConfigTargetOptions::default()
            }),
            output: target_outputs,
            filter: Filter::default().into(),
            sort: None,
            curation: None,
            rename: None,
            mapping_ids: None,
            mapping: Arc::new(arc_swap::ArcSwapOption::default()),
            favourites: None,
            processing_order: ProcessingOrder::default(),
            execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
            watch: None,
            use_memory_cache: false,
        });

        let mut app_cfg = crate::api::api_utils::tests::support::create_test_app_config();
        app_cfg.config = Arc::new(ArcSwap::from_pointee(config));
        app_cfg.api_proxy = Arc::new(arc_swap::ArcSwapOption::from(Some(Arc::new(api_proxy))));
        app_cfg.sources = Arc::new(ArcSwap::from_pointee(SourcesConfig {
            inputs: config_inputs.clone(),
            sources: vec![crate::model::ConfigSource {
                inputs: config_inputs.iter().map(|i| Arc::clone(&i.name)).collect(),
                targets: vec![Arc::clone(&target)],
            }],
            ..SourcesConfig::default()
        }));
        app_cfg.paths = Arc::new(ArcSwap::from_pointee(ConfigPaths {
            home_path: storage_dir.clone(),
            config_path: storage_dir.clone(),
            storage_path: storage_dir,
            config_file_path: String::new(),
            sources_file_path: String::new(),
            mapping_file_path: None,
            mapping_files_used: None,
            template_file_path: None,
            template_files_used: None,
            failover_file_path: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        }));

        let group_uuid = UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000001");
        let representative_input = &self.inputs[0];
        let representative_origin = &origins[representative_input];
        let rep_url = format!("http://{}/live/101", representative_origin.addr);

        let group_epg = self.epg_channel_id.as_deref().map(Internable::intern);
        let max_archive = self.member_archives.iter().flatten().copied().max();
        let group_additional_props = max_archive.map(|dur| {
            StreamProperties::Live(Box::new(LiveStreamProperties {
                name: "ZDF HD".intern(),
                category_id: 1,
                stream_id: 0,
                stream_icon: "".intern(),
                direct_source: "".intern(),
                custom_sid: None,
                added: None,
                stream_type: None,
                epg_channel_id: group_epg.clone(),
                tv_archive: Some(1),
                tv_archive_duration: Some(dur),
                is_adult: 0,
                ..Default::default()
            }))
        });

        let group_channel = PlaylistItem {
            header: PlaylistItemHeader {
                uuid: group_uuid,
                id: "zdf-hd".intern(),
                input_stream_id: "101".intern(),
                name: "ZDF HD".intern(),
                title: "ZDF HD".intern(),
                group: "General".intern(),
                url: rep_url.as_str().intern(),
                input_name: Arc::from(representative_input.as_str()),
                item_type: if self.hls { PlaylistItemType::LiveHls } else { PlaylistItemType::Live },
                xtream_cluster: XtreamCluster::Live,
                failover_group_uuid: Some(group_uuid),
                epg_channel_id: group_epg.clone(),
                additional_properties: group_additional_props,
                ..PlaylistItemHeader::default()
            },
        };

        let plain_uuid = UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000002");
        let plain_channel = PlaylistItem {
            header: PlaylistItemHeader {
                uuid: plain_uuid,
                id: "plain-tv".intern(),
                input_stream_id: "plain".intern(),
                name: "Plain TV".intern(),
                title: "Plain TV".intern(),
                group: "General".intern(),
                url: format!("http://{}/live/plain", representative_origin.addr).as_str().intern(),
                input_name: Arc::from(representative_input.as_str()),
                item_type: PlaylistItemType::Live,
                xtream_cluster: XtreamCluster::Live,
                ..PlaylistItemHeader::default()
            },
        };

        let mut members = Vec::new();
        for (idx, input_name) in self.inputs.iter().enumerate() {
            let origin = &origins[input_name];
            let member_url = format!("http://{}/live/101", origin.addr);
            let member_archive = self.member_archives.get(idx).copied().flatten();
            let member_additional_props = member_archive.map(|dur| {
                StreamProperties::Live(Box::new(LiveStreamProperties {
                    name: format!("ZDF HD ({input_name})").intern(),
                    category_id: 1,
                    stream_id: 0,
                    stream_icon: "".intern(),
                    direct_source: "".intern(),
                    custom_sid: None,
                    added: None,
                    stream_type: None,
                    epg_channel_id: group_epg.clone(),
                    tv_archive: Some(1),
                    tv_archive_duration: Some(dur),
                    is_adult: 0,
                    ..Default::default()
                }))
            });
            let member = PlaylistItem {
                header: PlaylistItemHeader {
                    uuid: UUIDType::from_valid_uuid(&format!("00000000-0000-4000-8000-0000000000{:02x}", idx + 10)),
                    id: (idx + 1).to_string().intern(),
                    input_stream_id: (idx + 1).to_string().intern(),
                    name: format!("ZDF HD ({input_name})").intern(),
                    title: "ZDF HD".intern(),
                    group: "General".intern(),
                    url: member_url.as_str().intern(),
                    input_name: Arc::from(input_name.as_str()),
                    item_type: if self.hls { PlaylistItemType::LiveHls } else { PlaylistItemType::Live },
                    xtream_cluster: XtreamCluster::Live,
                    epg_channel_id: group_epg.clone(),
                    additional_properties: member_additional_props,
                    ..PlaylistItemHeader::default()
                },
            };
            members.push(member);
        }

        let channels =
            if self.plain_channel { vec![group_channel.clone(), plain_channel] } else { vec![group_channel.clone()] };

        let mut playlist = vec![shared::model::PlaylistGroup {
            id: 1,
            title: "General".intern(),
            channels,
            xtream_cluster: XtreamCluster::Live,
        }];

        let mut favourite_vid = 0;
        if self.favourite {
            let fav_uuid = UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000099");
            let fav_channel = PlaylistItem {
                header: PlaylistItemHeader {
                    uuid: fav_uuid,
                    failover_group_uuid: Some(group_uuid),
                    group: "Favourites".intern(),
                    ..group_channel.header.clone()
                },
            };
            playlist.push(shared::model::PlaylistGroup {
                id: 99,
                title: "Favourites".intern(),
                channels: vec![fav_channel],
                xtream_cluster: XtreamCluster::Live,
            });
        }

        let mut pending = FailoverGroupBatch::with_groups(vec![PendingFailoverGroup {
            group_uuid,
            key: Arc::from("zdf hd"),
            origin: FailoverGroupOrigin::Auto,
            policy: FailoverGroupPolicy::Auto,
            members,
            orphaned: Vec::new(),
            epg_split: false,
        }]);

        let app_cfg = Arc::new(app_cfg);
        let playlists = Arc::new(PlaylistStorageState::new());
        persist_playlist_views_with_groups(
            &app_cfg,
            &mut playlist,
            None,
            self.epg.as_ref(),
            &target,
            Some(&playlists),
            TargetPlaylistPersistOptions::default(),
            &mut pending,
        )
        .await
        .expect("persists playlist views");

        let group_vid = playlist[0].channels[0].header.virtual_id.get();
        let plain_vid = if self.plain_channel { playlist[0].channels[1].header.virtual_id.get() } else { 0 };
        if self.favourite {
            favourite_vid = playlist[1].channels[0].header.virtual_id.get();
        }

        let config_snapshot = app_cfg.config.load();
        let path = get_target_storage_path(&config_snapshot, &target.name).expect("path");
        let record =
            FailoverGroupStore::read_group(&path, VirtualId::new(group_vid)).expect("read").expect("record exists");
        let group_record = Arc::new(record);

        let app_state = crate::api::api_utils::tests::support::create_test_app_state_for_config(Arc::clone(&app_cfg));
        if self.channel_unavailable_video {
            let fixture =
                include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"));
            app_state.app_config.custom_stream_response.store(Some(Arc::new(crate::model::CustomStreamResponse {
                channel_unavailable: Some(crate::api::model::TransportStreamBuffer::new(fixture.to_vec())),
                user_connections_exhausted: None,
                provider_connections_exhausted: None,
                low_priority_preempted: None,
                user_account_expired: None,
                panel_api_provisioning: None,
                hls_session_or_lease_expired: None,
                maintenance: None,
                panel_api_provisioning_hls_segments: Vec::new(),
            })));
        }
        app_state
            .member_health
            .set_event_sink(Arc::clone(&app_state.event_manager) as Arc<dyn shared::model::EventSink>);

        for (input, provider_id, score) in self.qos {
            app_state.member_health.replace_qos([(
                tuliprox_session::failover_group::QosKey::Provider { input: Arc::from(input.as_str()), provider_id },
                tuliprox_session::failover_group::QosPart {
                    score_24h: score,
                    score_7d: score,
                    confidence: 100,
                    failure_streak: 0,
                },
            )]);
        }

        let router = xtream_api_register()
            .merge(m3u_api_register())
            .merge(crate::api::endpoints::hls_api::hls_api_register())
            .merge(xmltv_api_register())
            .with_state(Arc::clone(&app_state));

        let hdhr_router = if self.hdhomerun {
            let hdhr_state = Arc::new(HdHomerunAppState {
                app_state: Arc::clone(&app_state),
                device: Arc::new(HdHomeRunDeviceConfig {
                    friendly_name: "Tuliprox HDHR".to_string(),
                    manufacturer: "Tuliprox".to_string(),
                    model_name: "HDHR-TEST".to_string(),
                    model_number: "HDHR4-2US".to_string(),
                    firmware_name: "hdhomerun4_atsc".to_string(),
                    firmware_version: "20260101".to_string(),
                    device_id: "12345678".to_string(),
                    device_type: "tuner".to_string(),
                    device_udn: "uuid:12345678-0000-0000-0000-000000000000".to_string(),
                    name: "test_hdhr".to_string(),
                    port: 5004,
                    tuner_count: 2,
                    t_username: "viewer".to_string(),
                    t_enabled: true,
                }),
                hd_scan_state: Arc::new(AtomicI8::new(-1)),
            });
            Some(hdhr_api_register(false).with_state(hdhr_state))
        } else {
            None
        };

        GroupPlaybackEnv {
            app_state,
            target,
            group_vid,
            plain_vid,
            favourite_vid,
            group_record,
            origins,
            router,
            hdhr_router,
            _dir: dir,
        }
    }
}
