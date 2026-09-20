use axum::{
    body::Body,
    extract::{Extension, Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use bytes::BytesMut;
use clap::{Parser, Subcommand};
use futures::StreamExt;
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt::Write,
    net::SocketAddr,
    path::PathBuf,
    process::ExitCode,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    net::TcpListener,
    sync::{oneshot, watch, Mutex},
};
use tuliprox_testkit::{
    bootstrap::{fixture_password, IsolatedFixture},
    config::{ExpectedPlayback, PostReadAction, RuntimeAssertions, Scenario},
    control::{PlaybackRegistry, StartDisposition},
    custom_video::WireMarkerScanner,
    discovery::VirtualIdMap,
    faults::{FaultSchedule, OriginFault},
    frame::{Frame, FrameDecoder, FrameValidator},
    hls::read_segments,
    observation::TuliproxObserver,
    oracle::{AdmissionOracle, Decision as AdmissionDecision, Request as AdmissionRequest},
    origin_events::{BodyCloseReason, OriginEvent, OriginEventKind, OriginStats},
    origin_transport::{
        serve_tracked_media_listener, OriginConnectionMeta, OriginLimitMode, OriginPolicy, OriginTracker,
    },
    protocol::{
        AgentId, AgentMessage, Command as ControlCommand, CommandId, Envelope, OriginStreamId, PlaybackEvent,
        PlaybackId, PlaybackOutcome, RejectionReason, RunId, RunLease,
    },
    report::{ReportEvent, RunReport},
    secret::environment,
    transport::{controller_websocket, AgentControlConnection, ControllerState, SharedControllerState},
    RunExit, TestkitError,
};

fn is_rejection_http_status(status: u16, headers: &reqwest::header::HeaderMap) -> bool {
    matches!(status, 403 | 429) || ((status == 502 || status == 503) && headers.get("x-tuliprox-rejection").is_some())
}

/// Tuliprox answers a suppressed reentry retry with `204 No Content`: a success status
/// and an empty body. Classify it as an admission rejection so a scenario can assert the
/// quiet termination instead of observing a bare end-of-body.
fn quiet_suppression_outcome(status: u16) -> Option<PlaybackOutcome> {
    (status == 204).then_some(PlaybackOutcome::AdmissionRejected { reason: RejectionReason::HttpStatus(status) })
}

/// Build the final playback URL for a live channel, dispatching between M3U-discovered URLs and
/// Xtream live endpoint URLs constructed from the virtual ID embedded in the M3U playlist entry.
fn resolve_playback_url(
    endpoint: tuliprox_testkit::config::PlaybackEndpoint,
    channel_protocol: &str,
    base_url: &str,
    username: &str,
    password: &str,
    vids: &VirtualIdMap,
    marker: u32,
) -> Result<String, TestkitError> {
    use tuliprox_testkit::config::PlaybackEndpoint;
    match endpoint {
        PlaybackEndpoint::M3u => Ok(vids.playback_url(marker)?.to_owned()),
        PlaybackEndpoint::Xtream => {
            if channel_protocol != "xtream_ts" {
                return Err(TestkitError::Configuration(format!(
                    "playback_endpoint: xtream requires protocol xtream_ts, got {channel_protocol}"
                )));
            }
            let virtual_id = vids.virtual_id_from_marker(marker)?;
            let username_enc = url::form_urlencoded::byte_serialize(username.as_bytes()).collect::<String>();
            let password_enc = url::form_urlencoded::byte_serialize(password.as_bytes()).collect::<String>();
            Ok(format!("{base_url}/live/{username_enc}/{password_enc}/{virtual_id}.ts"))
        }
    }
}

const PLAYBACK_IDLE_TIMEOUT: Duration = Duration::from_secs(15);
const PLAYLIST_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Parser)]
#[command(name = "tuliprox-testkit", version, about = "Tuliprox streaming admission test kit")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve a controllable synthetic live-stream origin.
    Origin {
        #[arg(long, default_value = "127.0.0.1:9910")]
        listen: SocketAddr,
        #[arg(long)]
        control_listen: Option<SocketAddr>,
        #[arg(long, default_value = "local-run")]
        run_id: String,
        #[arg(long, default_value_t = 64_000)]
        bitrate: u32,
        #[arg(long, value_delimiter = ',', default_value = "17")]
        markers: Vec<u32>,
        #[arg(long)]
        account_limit: Option<usize>,
        #[arg(long, default_value = "observe_only")]
        limit_mode: String,
    },
    /// Execute a local, sequential scenario against Tuliprox URLs.
    Controller {
        #[arg(long)]
        scenario: PathBuf,
        #[arg(long, default_value = "testkit-report")]
        report_directory: PathBuf,
        #[arg(long, default_value = "127.0.0.1:9900")]
        agent_listen: SocketAddr,
        #[arg(long)]
        run_id: Option<String>,
    },
    /// Read and validate one framed playback URL; useful on a remote listener host.
    Agent {
        #[arg(long)]
        url: String,
        #[arg(long)]
        expected_marker: u32,
        #[arg(long, default_value_t = 5)]
        frames: u64,
        #[arg(long)]
        hls: bool,
        #[arg(long)]
        controller: Option<String>,
        #[arg(long, default_value = "local-agent")]
        agent_id: String,
        #[arg(long, default_value = "local-run")]
        run_id: String,
    },
}

#[derive(Clone)]
struct OriginState {
    run_id: RunId,
    bitrate: u32,
    markers: Arc<Vec<u32>>,
    stream_counter: Arc<Mutex<u64>>,
    observations: Arc<Mutex<Vec<OriginObservation>>>,
    faults: Arc<Mutex<FaultSchedule>>,
    hls_sequences: Arc<Mutex<HashMap<u32, u64>>>,
    tracker: OriginTracker,
}

#[derive(Clone, Serialize)]
struct OriginObservation {
    origin_stream_id: String,
    channel_marker: u32,
    frames_emitted: u64,
    bytes_emitted: u64,
}

#[derive(Serialize)]
struct OriginInstance {
    run_id: String,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(exit) => exit.into(),
        Err(error) => {
            eprintln!("{error}");
            match error {
                TestkitError::Configuration(_) => RunExit::Invalid.into(),
                TestkitError::Protocol(_) | TestkitError::Io(_) | TestkitError::Http(_) => RunExit::Inconclusive.into(),
            }
        }
    }
}

async fn run(cli: Cli) -> Result<RunExit, TestkitError> {
    match cli.command {
        Command::Origin { listen, control_listen, run_id, bitrate, mut markers, account_limit, limit_mode } => {
            markers.sort_unstable();
            markers.dedup();
            if markers.is_empty() {
                return Err(TestkitError::Configuration("origin requires at least one channel marker".to_owned()));
            }
            let mode = match limit_mode.as_str() {
                "reject_new" => OriginLimitMode::RejectNew,
                "evict_oldest" => OriginLimitMode::EvictOldest,
                _ => OriginLimitMode::ObserveOnly,
            };
            let policy = OriginPolicy { account_limit, limit_mode: mode };
            let tracker = OriginTracker::new(&run_id, policy);
            serve_origin(
                listen,
                control_listen,
                OriginState {
                    run_id: RunId::new(run_id),
                    bitrate,
                    markers: Arc::new(markers),
                    stream_counter: Arc::new(Mutex::new(0)),
                    observations: Arc::new(Mutex::new(Vec::new())),
                    faults: Arc::new(Mutex::new(FaultSchedule::default())),
                    hls_sequences: Arc::new(Mutex::new(HashMap::new())),
                    tracker,
                },
            )
            .await
        }
        Command::Agent { url, expected_marker, frames, hls, controller, agent_id, run_id } => {
            if let Some(controller) = controller {
                return run_controlled_agent(&controller, AgentId::new(agent_id), RunId::new(run_id)).await;
            }
            if hls {
                match read_segments(&url, &RunId::new(run_id), expected_marker, frames, &BTreeMap::new()).await {
                    Ok(received) => {
                        println!("received {received} valid HLS frames for marker {expected_marker}");
                        Ok(RunExit::Passed)
                    }
                    Err(error) => {
                        eprintln!("{error}");
                        Ok(RunExit::Failed)
                    }
                }
            } else {
                let outcome = run_agent(&url, &RunId::new(run_id), expected_marker, frames, &BTreeMap::new()).await?;
                Ok(if outcome.is_streaming() { RunExit::Passed } else { RunExit::Failed })
            }
        }
        Command::Controller { scenario, report_directory, agent_listen, run_id } => {
            run_controller(&scenario, &report_directory, agent_listen, run_id.as_deref()).await
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn run_controlled_agent(controller: &str, agent_id: AgentId, run_id: RunId) -> Result<RunExit, TestkitError> {
    let hello = Envelope {
        schema_version: 1,
        run_id: run_id.clone(),
        run_generation: 1,
        message_id: "agent-hello".to_owned(),
        agent_id: agent_id.clone(),
        agent_boot_id: "agent-boot".to_owned(),
        source_sequence: 1,
        caused_by_command_id: None,
        local_elapsed_nanos: 0,
        payload: AgentMessage::Hello {
            hostname: "remote-agent".to_owned(),
            supported_protocols: vec!["xtream_ts".to_owned(), "hls".to_owned()],
            maximum_listeners: 1,
        },
    };
    let mut connection = AgentControlConnection::connect(controller, hello).await?;
    connection
        .send_event(Envelope {
            schema_version: 1,
            run_id: run_id.clone(),
            run_generation: 1,
            message_id: "agent-ready".to_owned(),
            agent_id: agent_id.clone(),
            agent_boot_id: "agent-boot".to_owned(),
            source_sequence: 2,
            caused_by_command_id: None,
            local_elapsed_nanos: 0,
            payload: AgentMessage::Ready,
        })
        .await?;
    let mut sequence = 3;
    let mut lease = RunLease::new(1, Duration::from_secs(15));
    let mut active: HashMap<String, oneshot::Sender<()>> = HashMap::new();
    let (term_tx, mut term_rx) = tokio::sync::mpsc::channel::<(PlaybackId, PlaybackOutcome)>(64);
    let mut pending_stops: HashMap<String, Option<tuliprox_testkit::protocol::CommandId>> = HashMap::new();
    let mut playback_registry = PlaybackRegistry::default();
    let mut heartbeats = tokio::time::interval(Duration::from_secs(5));
    heartbeats.tick().await;
    loop {
        let command = tokio::select! {
            Some((playback_id, outcome)) = term_rx.recv() => {
                active.remove(&playback_id.0);
                let caused_by = pending_stops.remove(&playback_id.0).flatten();
                send_agent_event(
                    &connection,
                    &run_id,
                    &agent_id,
                    &mut sequence,
                    caused_by,
                    PlaybackEvent::Terminal {
                        playback_id,
                        outcome: outcome.to_string(),
                        typed_outcome: Some(outcome),
                    },
                )
                .await?;
                continue;
            }
            command = connection.next_command() => command?,
            _ = heartbeats.tick() => {
                connection.send_event(Envelope {
                    schema_version: 1,
                    run_id: run_id.clone(),
                    run_generation: 1,
                    message_id: format!("heartbeat-{sequence}"),
                    agent_id: agent_id.clone(),
                    agent_boot_id: "agent-boot".to_owned(),
                    source_sequence: sequence,
                    caused_by_command_id: None,
                    local_elapsed_nanos: 0,
                    payload: AgentMessage::Heartbeat,
                }).await?;
                sequence = sequence.saturating_add(1);
                continue;
            }
            () = tokio::time::sleep_until(lease.deadline()) => {
                release_active_playbacks(&mut active);
                send_agent_event(
                    &connection,
                    &run_id,
                    &agent_id,
                    &mut sequence,
                    None,
                    PlaybackEvent::LeaseExpired,
                ).await?;
                return Ok(RunExit::Inconclusive);
            }
        };
        let Some(command) = command else {
            release_active_playbacks(&mut active);
            return Ok(RunExit::Inconclusive);
        };
        if lease.expired() {
            release_active_playbacks(&mut active);
            return Ok(RunExit::Inconclusive);
        }
        if command.run_id != run_id || command.run_generation != 1 {
            return Err(TestkitError::Protocol("controller sent a stale or foreign command".to_owned()));
        }
        let caused_by = command.caused_by_command_id.clone();
        let command_id = caused_by.clone().unwrap_or_else(|| CommandId::new(command.message_id.clone()));
        match command.payload {
            ControlCommand::StartPlayback {
                playback_id,
                url,
                headers,
                expected_run_id,
                expected_marker,
                required_frames,
            } => {
                let fingerprint = playback_start_fingerprint(&url, &headers);
                match playback_registry.register_start(run_id.clone(), 1, playback_id.clone(), fingerprint)? {
                    StartDisposition::Replay => {
                        send_agent_event(
                            &connection,
                            &run_id,
                            &agent_id,
                            &mut sequence,
                            caused_by,
                            PlaybackEvent::CommandAccepted { command_id },
                        )
                        .await?;
                        continue;
                    }
                    StartDisposition::New => {}
                }
                let target_run_id = expected_run_id.unwrap_or_else(|| run_id.clone());
                let marker = expected_marker.map_or_else(|| marker_from_url(&url), Ok)?;
                let frames = required_frames.unwrap_or(5);
                send_agent_event(
                    &connection,
                    &run_id,
                    &agent_id,
                    &mut sequence,
                    caused_by.clone(),
                    PlaybackEvent::CommandAccepted { command_id },
                )
                .await?;
                match start_held_playback(playback_id.0.clone(), &target_run_id, &url, marker, frames, &headers).await?
                {
                    Ok(held) => {
                        let term_tx_clone = term_tx.clone();
                        let p_id = playback_id.clone();
                        tokio::spawn(async move {
                            let outcome = match held.task.await {
                                Ok(Ok(outcome)) => outcome,
                                Ok(Err(err)) => PlaybackOutcome::InfrastructureError { message: err.to_string() },
                                Err(join_err) => PlaybackOutcome::InfrastructureError { message: join_err.to_string() },
                            };
                            let _ = term_tx_clone.send((p_id, outcome)).await;
                        });
                        active.insert(playback_id.0.clone(), held.release);
                        send_agent_event(
                            &connection,
                            &run_id,
                            &agent_id,
                            &mut sequence,
                            caused_by,
                            PlaybackEvent::FirstValidFrame { playback_id, sequence: 0 },
                        )
                        .await?;
                    }
                    Err(outcome) => {
                        send_agent_event(
                            &connection,
                            &run_id,
                            &agent_id,
                            &mut sequence,
                            caused_by,
                            PlaybackEvent::Terminal {
                                playback_id,
                                outcome: outcome.to_string(),
                                typed_outcome: Some(outcome),
                            },
                        )
                        .await?;
                    }
                }
            }
            ControlCommand::StopAll => {
                release_active_playbacks(&mut active);
                return Ok(RunExit::Passed);
            }
            ControlCommand::StopPlayback { playback_id } => {
                if !playback_registry.stop(&run_id, 1, &playback_id) {
                    send_agent_event(
                        &connection,
                        &run_id,
                        &agent_id,
                        &mut sequence,
                        caused_by,
                        PlaybackEvent::Terminal {
                            playback_id,
                            outcome: "protocol_rejected".to_owned(),
                            typed_outcome: Some(PlaybackOutcome::InfrastructureError {
                                message: "protocol_rejected".to_owned(),
                            }),
                        },
                    )
                    .await?;
                    continue;
                }
                if let Some(release) = active.remove(&playback_id.0) {
                    pending_stops.insert(playback_id.0.clone(), caused_by);
                    let _ = release.send(());
                } else {
                    send_agent_event(
                        &connection,
                        &run_id,
                        &agent_id,
                        &mut sequence,
                        caused_by,
                        PlaybackEvent::Terminal {
                            playback_id,
                            outcome: "cancelled".to_owned(),
                            typed_outcome: Some(PlaybackOutcome::ExplicitStop),
                        },
                    )
                    .await?;
                }
            }
            ControlCommand::ConfigureRun { lease_millis } | ControlCommand::RenewRunLease { lease_millis } => {
                lease.renew(1, Duration::from_millis(lease_millis.max(1)))?;
            }
        }
    }
}

fn playback_start_fingerprint(url: &str, headers: &BTreeMap<String, String>) -> String {
    let mut source = String::with_capacity(url.len().saturating_add(headers.len().saturating_mul(32)));
    source.push_str(url);
    for (name, value) in headers {
        source.push('\n');
        source.push_str(name);
        source.push(':');
        source.push_str(value);
    }
    blake3::hash(source.as_bytes()).to_hex().to_string()
}

fn release_active_playbacks(active: &mut HashMap<String, oneshot::Sender<()>>) {
    for (_, release) in active.drain() {
        let _ = release.send(());
    }
}

async fn send_agent_event(
    connection: &AgentControlConnection,
    run_id: &RunId,
    agent_id: &AgentId,
    sequence: &mut u64,
    caused_by_command_id: Option<tuliprox_testkit::protocol::CommandId>,
    event: PlaybackEvent,
) -> Result<(), TestkitError> {
    connection
        .send_event(Envelope {
            schema_version: 1,
            run_id: run_id.clone(),
            run_generation: 1,
            message_id: format!("event-{sequence}"),
            agent_id: agent_id.clone(),
            agent_boot_id: "agent-boot".to_owned(),
            source_sequence: *sequence,
            caused_by_command_id,
            local_elapsed_nanos: 0,
            payload: AgentMessage::Event { event },
        })
        .await?;
    *sequence = sequence.saturating_add(1);
    Ok(())
}

struct BodyDropGuard {
    conn_id: u64,
    req_id: u64,
    tracker: OriginTracker,
    bytes_emitted: Arc<AtomicU64>,
    start_time: Instant,
    closed: Arc<AtomicBool>,
    evicted: Arc<AtomicBool>,
}

impl Drop for BodyDropGuard {
    fn drop(&mut self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            let conn_id = self.conn_id;
            let req_id = self.req_id;
            let tracker = self.tracker.clone();
            let bytes = self.bytes_emitted.load(Ordering::Relaxed);
            let duration_ms = u64::try_from(self.start_time.elapsed().as_millis()).unwrap_or(u64::MAX);
            let reason = if self.evicted.load(Ordering::Relaxed) {
                BodyCloseReason::Evicted
            } else {
                BodyCloseReason::ClientDisconnected
            };
            tokio::spawn(async move {
                tracker.on_body_closed(conn_id, req_id, bytes, duration_ms, reason).await;
            });
        }
    }
}

async fn serve_origin(
    media_listen: SocketAddr,
    control_listen: Option<SocketAddr>,
    state: OriginState,
) -> Result<RunExit, TestkitError> {
    let media_listener = TcpListener::bind(media_listen).await?;
    let media_app = Router::new()
        .route("/catalog/input.m3u", get(catalog))
        .route("/live/{*stream}", get(live))
        .route("/hls/{*resource}", get(hls))
        .route("/vod/{*object}", get(vod).head(vod_head))
        .with_state(state.clone());

    let control_app = Router::new()
        .route("/health", get(|| async { StatusCode::NO_CONTENT }))
        .route("/v1/instance", get(origin_instance))
        .route("/v1/runs/{run_id}/reset", post(reset_origin_run))
        .route("/v1/runs/{run_id}/connections", get(origin_connections))
        .route("/v1/runs/{run_id}/events", get(origin_events))
        .route("/v1/runs/{run_id}/stats", get(origin_stats))
        .route("/v1/runs/{run_id}/faults/{fault_id}", put(set_fault).delete(clear_fault))
        .with_state(state.clone());

    let control_addr = control_listen.unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 0)));
    let control_listener = TcpListener::bind(control_addr).await?;
    let control_task = tokio::spawn(async move {
        let _ = axum::serve(control_listener, control_app).await;
    });

    let tracker = state.tracker.clone();
    let media_res = serve_tracked_media_listener(media_listener, media_app, tracker).await;

    control_task.abort();
    media_res.map_err(TestkitError::Io)?;
    Ok(RunExit::Passed)
}

fn catalog_m3u(state: &OriginState, host: &str, account: Option<&str>) -> String {
    let account_query = account.map_or_else(String::new, |account| format!("&token={account}"));
    let mut catalog = String::from("#EXTM3U\n");
    for marker in state.markers.iter() {
        let _ = writeln!(catalog, "#EXTINF:-1 tvg-id=\"test-{marker}\",Test channel {marker}");
        let _ = writeln!(catalog, "http://{host}/live/{marker}.ts?run={}{}", state.run_id.0, account_query);
    }
    let _ = writeln!(catalog, "#EXTINF:-1 tvg-id=\"test-vod-movie.mkv\" tvg-type=\"movie\",Test Movie");
    let _ = writeln!(catalog, "http://{host}/vod/movie.mkv?run={}{}", state.run_id.0, account_query);
    catalog
}

fn origin_account(query: &HashMap<String, String>) -> Option<&str> {
    query.get("account").or_else(|| query.get("token")).map(String::as_str)
}

async fn catalog(
    State(state): State<OriginState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let host = headers.get(header::HOST).and_then(|value| value.to_str().ok()).unwrap_or("127.0.0.1");
    ([(header::CONNECTION, "close")], catalog_m3u(&state, host, query.get("account").map(String::as_str)))
}

struct LiveStreamState {
    ticker: tokio::time::Interval,
    evict_rx: oneshot::Receiver<()>,
    evicted_flag: Arc<AtomicBool>,
    run_id: RunId,
    stream_id: OriginStreamId,
    marker: u32,
    index: u64,
    observations: Arc<Mutex<Vec<OriginObservation>>>,
    observed_stream_id: String,
    bytes_emitted: Arc<AtomicU64>,
    _guard: Arc<BodyDropGuard>,
}

#[allow(clippy::too_many_lines)]
async fn live(
    Path(stream): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    State(state): State<OriginState>,
    meta: Option<Extension<OriginConnectionMeta>>,
    headers: HeaderMap,
) -> Response {
    if query.get("run") != Some(&state.run_id.0) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(marker) = stream.strip_suffix(".ts").unwrap_or(&stream).parse::<u32>() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !state.markers.contains(&marker) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if matches!(state.faults.lock().await.take("close-before-first-byte"), Some(OriginFault::CloseBeforeFirstByte)) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }

    let conn_id = meta.as_ref().map_or(0, |m| m.conn_id);
    let range_str = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
    let req_res = state
        .tracker
        .on_request_started(conn_id, "GET", &format!("/live/{stream}"), range_str, ua, origin_account(&query))
        .await;

    let req_id = match req_res {
        Ok(id) => id,
        Err((_id, status)) => {
            return StatusCode::from_u16(status).unwrap_or(StatusCode::TOO_MANY_REQUESTS).into_response()
        }
    };

    let mut counter = state.stream_counter.lock().await;
    *counter += 1;
    let stream_id = OriginStreamId::new(format!("origin-{}", *counter));
    drop(counter);
    state.observations.lock().await.push(OriginObservation {
        origin_stream_id: stream_id.0.clone(),
        channel_marker: marker,
        frames_emitted: 0,
        bytes_emitted: 0,
    });

    let evict_rx = state.tracker.on_body_started(conn_id, req_id, None).await;

    let run_id = state.run_id.clone();
    let bitrate = state.bitrate;
    let mut sample = BytesMut::new();
    if Frame::synthetic(&run_id, &stream_id, marker, 0).encode(&mut sample).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let bits = u128::try_from(sample.len()).unwrap_or(u128::MAX).saturating_mul(8);
    let nanos = bits.saturating_mul(1_000_000_000).checked_div(u128::from(bitrate.max(1))).unwrap_or(1);
    let interval = Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX).max(1));
    let observations = state.observations.clone();
    let observed_stream_id = stream_id.0.clone();

    let bytes_emitted = Arc::new(AtomicU64::new(0));
    let closed = Arc::new(AtomicBool::new(false));
    let evicted = Arc::new(AtomicBool::new(false));
    let guard = Arc::new(BodyDropGuard {
        conn_id,
        req_id,
        tracker: state.tracker.clone(),
        bytes_emitted: bytes_emitted.clone(),
        start_time: Instant::now(),
        closed: closed.clone(),
        evicted: evicted.clone(),
    });

    let evicted_flag = evicted.clone();
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let stream_state = LiveStreamState {
        ticker,
        evict_rx,
        evicted_flag,
        run_id,
        stream_id,
        marker,
        index: 0,
        observations,
        observed_stream_id,
        bytes_emitted,
        _guard: guard,
    };

    let body = futures::stream::unfold(stream_state, |mut state| async move {
        tokio::select! {
            biased;
            res = &mut state.evict_rx => {
                if res.is_ok() {
                    state.evicted_flag.store(true, Ordering::Release);
                }
                None
            }
            _ = state.ticker.tick() => {
                let frame = Frame::synthetic(&state.run_id, &state.stream_id, state.marker, state.index);
                state.index = state.index.saturating_add(1);
                let mut encoded = BytesMut::new();
                if frame.encode(&mut encoded).is_err() {
                    return None;
                }
                let bytes = encoded.len() as u64;
                state.bytes_emitted.fetch_add(bytes, Ordering::Relaxed);
                if let Some(observation) = state.observations
                    .lock()
                    .await
                    .iter_mut()
                    .find(|observation| observation.origin_stream_id == state.observed_stream_id)
                {
                    observation.frames_emitted = observation.frames_emitted.saturating_add(1);
                    observation.bytes_emitted = observation.bytes_emitted.saturating_add(bytes);
                }
                Some((Ok::<_, std::convert::Infallible>(encoded.freeze()), state))
            }
        }
    });
    let mut response = Response::new(Body::from_stream(body));
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("video/mp2t"));
    if let Ok(value) = HeaderValue::from_str(&bitrate.to_string()) {
        response.headers_mut().insert("x-tpx-test-bitrate", value);
    }
    response
}

async fn origin_connections(Path(run_id): Path<String>, State(state): State<OriginState>) -> Response {
    if run_id != state.run_id.0 {
        return StatusCode::NOT_FOUND.into_response();
    }
    Json(state.observations.lock().await.clone()).into_response()
}

async fn origin_events(Path(run_id): Path<String>, State(state): State<OriginState>) -> Response {
    if run_id != state.run_id.0 {
        return StatusCode::NOT_FOUND.into_response();
    }
    Json(state.tracker.events().await).into_response()
}

async fn origin_stats(Path(run_id): Path<String>, State(state): State<OriginState>) -> Response {
    if run_id != state.run_id.0 {
        return StatusCode::NOT_FOUND.into_response();
    }
    Json(state.tracker.stats().await).into_response()
}

async fn origin_instance(State(state): State<OriginState>) -> Json<OriginInstance> {
    Json(OriginInstance { run_id: state.run_id.0.clone() })
}

async fn reset_origin_run(Path(run_id): Path<String>, State(state): State<OriginState>) -> StatusCode {
    if run_id != state.run_id.0 {
        return StatusCode::NOT_FOUND;
    }
    *state.stream_counter.lock().await = 0;
    state.hls_sequences.lock().await.clear();
    state.observations.lock().await.clear();
    state.tracker.reset().await;
    StatusCode::NO_CONTENT
}

async fn set_fault(Path((run_id, fault_id)): Path<(String, String)>, State(state): State<OriginState>) -> StatusCode {
    if run_id != state.run_id.0 {
        return StatusCode::NOT_FOUND;
    }
    let fault = match fault_id.as_str() {
        "close-before-first-byte" => OriginFault::CloseBeforeFirstByte,
        _ => return StatusCode::BAD_REQUEST,
    };
    match state.faults.lock().await.insert(fault_id, fault) {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::CONFLICT,
    }
}

async fn clear_fault(Path((run_id, fault_id)): Path<(String, String)>, State(state): State<OriginState>) -> StatusCode {
    if run_id != state.run_id.0 {
        return StatusCode::NOT_FOUND;
    }
    match state.faults.lock().await.clear(&fault_id) {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::NOT_FOUND,
    }
}

async fn hls(
    Path(resource): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    State(state): State<OriginState>,
) -> Response {
    if query.get("run") != Some(&state.run_id.0) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some((marker, resource)) = resource.split_once('/') else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(marker) = marker.parse::<u32>() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !state.markers.contains(&marker) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if resource == "index.m3u8" {
        let mut sequences = state.hls_sequences.lock().await;
        let sequence = sequences.entry(marker).or_insert(0);
        *sequence += 1;
        let first = sequence.saturating_sub(1);
        let playlist = format!(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:{first}\n#EXTINF:1.0,\n{first}.ts?run={}\n#EXTINF:1.0,\n{}.ts?run={}\n",
            state.run_id.0, *sequence, state.run_id.0
        );
        return ([(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")], playlist).into_response();
    }
    let segment = resource.strip_suffix(".ts").and_then(|part| part.parse::<u64>().ok());
    let Some(sequence) = segment else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let frame =
        Frame::synthetic(&state.run_id, &OriginStreamId::new(format!("hls-presentation-{marker}")), marker, sequence);
    let mut encoded = BytesMut::new();
    if frame.encode(&mut encoded).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    ([(header::CONTENT_TYPE, "video/mp2t")], encoded.freeze()).into_response()
}

async fn vod(
    Path(object): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    State(state): State<OriginState>,
    meta: Option<Extension<OriginConnectionMeta>>,
    headers: HeaderMap,
) -> Response {
    if query.get("run") != Some(&state.run_id.0) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let conn_id = meta.as_ref().map_or(0, |m| m.conn_id);
    let total_size =
        query.get("size").and_then(|s| s.parse::<u64>().ok()).unwrap_or(tuliprox_testkit::vod::DEFAULT_VOD_SIZE);

    let range_header = headers.get(header::RANGE).and_then(|value| value.to_str().ok());
    let ua = headers.get(header::USER_AGENT).and_then(|value| value.to_str().ok());
    let req_res = state
        .tracker
        .on_request_started(conn_id, "GET", &format!("/vod/{object}"), range_header, ua, origin_account(&query))
        .await;

    let req_id = match req_res {
        Ok(id) => id,
        Err((_id, status)) => {
            return StatusCode::from_u16(status).unwrap_or(StatusCode::TOO_MANY_REQUESTS).into_response()
        }
    };

    let (start, end, status) = match range_header {
        None => (0, total_size, StatusCode::OK),
        Some(range_str) => {
            if let Some((start, end)) = tuliprox_testkit::vod::parse_range_header(range_str, total_size) {
                (start, end, StatusCode::PARTIAL_CONTENT)
            } else {
                let mut response = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
                if let Ok(value) = HeaderValue::from_str(&format!("bytes */{total_size}")) {
                    response.headers_mut().insert(header::CONTENT_RANGE, value);
                }
                apply_vod_validators(&mut response, &vod_payload(&object));
                return response;
            }
        }
    };

    let length = end.saturating_sub(start);
    let evict_rx = state.tracker.on_body_started(conn_id, req_id, Some(length)).await;
    let stall_ms = query.get("stall_ms").and_then(|s| s.parse::<u64>().ok());
    let abort_after_bytes = query.get("abort_after_bytes").and_then(|s| s.parse::<u64>().ok());

    let options = tuliprox_testkit::vod::VodStreamOptions {
        object: object.clone(),
        start_offset: start,
        length,
        bitrate: state.bitrate,
        chunk_size: 64 * 1024,
        stall_ms,
        abort_after_bytes,
    };

    let bytes_emitted = Arc::new(AtomicU64::new(0));
    let closed = Arc::new(AtomicBool::new(false));
    let evicted = Arc::new(AtomicBool::new(false));
    let guard = Arc::new(BodyDropGuard {
        conn_id,
        req_id,
        tracker: state.tracker.clone(),
        bytes_emitted: bytes_emitted.clone(),
        start_time: Instant::now(),
        closed: closed.clone(),
        evicted: evicted.clone(),
    });
    let body_stream = tuliprox_testkit::vod::create_vod_stream_with_evicted(options, evict_rx, Some(evicted));

    let bytes_tracker = bytes_emitted.clone();
    let tracked_stream = body_stream.map(move |item| {
        let _keep_guard = &guard;
        if let Ok(chunk) = &item {
            bytes_tracker.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        }
        item
    });

    let mut response = Response::new(Body::from_stream(tracked_stream));
    *response.status_mut() = status;
    response.headers_mut().insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if status == StatusCode::PARTIAL_CONTENT {
        let value = format!("bytes {}-{}/{}", start, end.saturating_sub(1), total_size);
        if let Ok(value) = HeaderValue::from_str(&value) {
            response.headers_mut().insert(header::CONTENT_RANGE, value);
        }
    }
    if let Ok(value) = HeaderValue::from_str(&length.to_string()) {
        response.headers_mut().insert(header::CONTENT_LENGTH, value);
    }
    apply_vod_validators(&mut response, &vod_payload(&object));
    response
}

async fn vod_head(
    Path(object): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    State(state): State<OriginState>,
) -> Response {
    if query.get("run") != Some(&state.run_id.0) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let total_size =
        query.get("size").and_then(|s| s.parse::<u64>().ok()).unwrap_or(tuliprox_testkit::vod::DEFAULT_VOD_SIZE);
    let mut response = StatusCode::OK.into_response();
    response.headers_mut().insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Ok(value) = HeaderValue::from_str(&total_size.to_string()) {
        response.headers_mut().insert(header::CONTENT_LENGTH, value);
    }
    apply_vod_validators(&mut response, &vod_payload(&object));
    response
}

fn vod_payload(object: &str) -> Vec<u8> {
    let digest = blake3::hash(object.as_bytes());
    digest.as_bytes().iter().copied().cycle().take(16 * 1024).collect()
}

fn apply_vod_validators(response: &mut Response, payload: &[u8]) {
    let etag = format!("\"{}\"", blake3::hash(payload).to_hex());
    if let Ok(etag) = HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, etag);
    }
    response.headers_mut().insert(header::LAST_MODIFIED, HeaderValue::from_static("Thu, 01 Jan 1970 00:00:00 GMT"));
}

async fn run_agent(
    url: &str,
    expected_run_id: &RunId,
    expected_marker: u32,
    required_frames: u64,
    headers: &BTreeMap<String, String>,
) -> Result<PlaybackOutcome, TestkitError> {
    let client = reqwest::Client::builder().http1_only().pool_max_idle_per_host(0).build()?;
    let mut request = client.get(url);
    for (name, value) in headers {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
            TestkitError::Configuration(format!("invalid playback request header name {name}: {error}"))
        })?;
        let value = reqwest::header::HeaderValue::from_str(value).map_err(|error| {
            TestkitError::Configuration(format!("invalid playback request header value for {name}: {error}"))
        })?;
        request = request.header(name, value);
    }
    let response = match request.send().await {
        Ok(res) => res,
        Err(err) => return Ok(PlaybackOutcome::TransportError { message: err.to_string() }),
    };
    let status = response.status().as_u16();
    if !response.status().is_success() {
        eprintln!("playback returned {status}");
        return Ok(if is_rejection_http_status(status, response.headers()) {
            PlaybackOutcome::AdmissionRejected { reason: RejectionReason::HttpStatus(status) }
        } else {
            PlaybackOutcome::HttpError { status }
        });
    }
    if let Some(outcome) = quiet_suppression_outcome(status) {
        return Ok(outcome);
    }
    let mut body = response.bytes_stream();
    let mut decoder = FrameDecoder::default();
    let mut validator = FrameValidator::new(expected_run_id, expected_marker);
    let mut custom_video = WireMarkerScanner::default();
    let mut received_frames = 0;
    let mut received_bytes = 0;
    loop {
        let chunk = match tokio::time::timeout(PLAYBACK_IDLE_TIMEOUT, body.next()).await {
            Ok(Some(Ok(chunk))) => chunk,
            Ok(Some(Err(err))) => return Ok(PlaybackOutcome::TransportError { message: err.to_string() }),
            Ok(None) => break,
            Err(_) => {
                eprintln!("stream did not produce bytes before the idle deadline");
                return Ok(PlaybackOutcome::IdleTimeout);
            }
        };
        received_bytes += chunk.len() as u64;
        if let Some(kind) = custom_video.push(&chunk)? {
            eprintln!("received custom-video fallback: {kind:?}");
            if matches!(
                kind,
                tuliprox_testkit::custom_video::CustomVideoKind::UserConnectionsExhausted
                    | tuliprox_testkit::custom_video::CustomVideoKind::ProviderConnectionsExhausted
                    | tuliprox_testkit::custom_video::CustomVideoKind::LowPriorityPreempted
                    | tuliprox_testkit::custom_video::CustomVideoKind::UserAccountExpired
            ) {
                return Ok(PlaybackOutcome::AdmissionRejected { reason: RejectionReason::CustomVideo(kind) });
            }
            return Ok(PlaybackOutcome::InvalidData {
                message: format!("unexpected non-admission custom video: {kind:?}"),
            });
        }
        for frame in decoder.push(&chunk)? {
            if let Err(error) = validator.validate(&frame) {
                eprintln!("{error}");
                return Ok(PlaybackOutcome::InvalidData { message: error.to_string() });
            }
            received_frames += 1;
            if received_frames >= required_frames {
                println!("received {received_frames} valid frames for marker {expected_marker}");
                return Ok(PlaybackOutcome::Streaming { frames: received_frames, bytes: received_bytes });
            }
        }
    }
    eprintln!("stream ended after {received_frames} valid frames");
    Ok(PlaybackOutcome::UnexpectedEof { frames: received_frames, bytes: received_bytes })
}

/// Keep the HTTP body consumed after proving its frame identity.  Retaining the
/// request alone is not sufficient: a bounded proxy buffer could otherwise make
/// the upstream session disappear before the following admission request.
async fn run_agent_until_released(
    url: String,
    expected_run_id: RunId,
    expected_marker: u32,
    required_frames: u64,
    headers: BTreeMap<String, String>,
    ready: oneshot::Sender<()>,
    mut release: oneshot::Receiver<()>,
) -> Result<PlaybackOutcome, TestkitError> {
    let client = reqwest::Client::builder().http1_only().pool_max_idle_per_host(0).build()?;
    let mut request = client.get(url);
    for (name, value) in &headers {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
            TestkitError::Configuration(format!("invalid playback request header name {name}: {error}"))
        })?;
        let value = reqwest::header::HeaderValue::from_str(value).map_err(|error| {
            TestkitError::Configuration(format!("invalid playback request header value for {name}: {error}"))
        })?;
        request = request.header(name, value);
    }
    let response = match request.send().await {
        Ok(res) => res,
        Err(err) => return Ok(PlaybackOutcome::TransportError { message: err.to_string() }),
    };
    let status = response.status().as_u16();
    if !response.status().is_success() {
        return Ok(if is_rejection_http_status(status, response.headers()) {
            PlaybackOutcome::AdmissionRejected { reason: RejectionReason::HttpStatus(status) }
        } else {
            PlaybackOutcome::HttpError { status }
        });
    }
    if let Some(outcome) = quiet_suppression_outcome(status) {
        return Ok(outcome);
    }
    let mut body = response.bytes_stream();
    let mut decoder = FrameDecoder::default();
    let mut validator = FrameValidator::new(&expected_run_id, expected_marker);
    let mut custom_video = WireMarkerScanner::default();
    let mut received_frames = 0;
    let mut received_bytes = 0;
    let mut ready = Some(ready);
    loop {
        tokio::select! {
            _ = &mut release => {
                return Ok(if ready.is_none() {
                    PlaybackOutcome::ExplicitStop
                } else {
                    PlaybackOutcome::UnexpectedEof { frames: received_frames, bytes: received_bytes }
                });
            }
            chunk = tokio::time::timeout(PLAYBACK_IDLE_TIMEOUT, body.next()) => {
                let chunk = match chunk {
                    Ok(Some(Ok(chunk))) => chunk,
                    Ok(Some(Err(err))) => return Ok(PlaybackOutcome::TransportError { message: err.to_string() }),
                    Ok(None) => return Ok(PlaybackOutcome::UnexpectedEof { frames: received_frames, bytes: received_bytes }),
                    Err(_) => return Ok(PlaybackOutcome::IdleTimeout),
                };
                received_bytes += chunk.len() as u64;
                if let Some(kind) = custom_video.push(&chunk)? {
                    if matches!(
                        kind,
                        tuliprox_testkit::custom_video::CustomVideoKind::UserConnectionsExhausted
                            | tuliprox_testkit::custom_video::CustomVideoKind::ProviderConnectionsExhausted
                            | tuliprox_testkit::custom_video::CustomVideoKind::LowPriorityPreempted
                            | tuliprox_testkit::custom_video::CustomVideoKind::UserAccountExpired
                    ) {
                        return Ok(PlaybackOutcome::AdmissionRejected {
                            reason: RejectionReason::CustomVideo(kind),
                        });
                    }
                    return Ok(PlaybackOutcome::InvalidData {
                        message: format!("unexpected non-admission custom video: {kind:?}"),
                    });
                }
                for frame in decoder.push(&chunk)? {
                    if let Err(error) = validator.validate(&frame) {
                        return Ok(PlaybackOutcome::InvalidData { message: error.to_string() });
                    }
                    received_frames += 1;
                    if received_frames >= required_frames {
                        if let Some(ready) = ready.take() {
                            let _ = ready.send(());
                        }
                    }
                }
            }
        }
    }
}

async fn run_hls_until_released(
    url: String,
    expected_run_id: RunId,
    expected_marker: u32,
    required_frames: u64,
    headers: BTreeMap<String, String>,
    ready: oneshot::Sender<()>,
    mut release: oneshot::Receiver<()>,
) -> Result<PlaybackOutcome, TestkitError> {
    let mut ready = Some(ready);
    loop {
        tokio::select! {
            _ = &mut release => {
                return Ok(if ready.is_none() {
                    PlaybackOutcome::ExplicitStop
                } else {
                    PlaybackOutcome::UnexpectedEof { frames: 0, bytes: 0 }
                });
            }
            result = read_segments(&url, &expected_run_id, expected_marker, required_frames, &headers) => {
                if let Err(err) = result {
                    return Ok(PlaybackOutcome::TransportError { message: err.to_string() });
                }
                if let Some(ready) = ready.take() {
                    let _ = ready.send(());
                }
            }
        }
        tokio::select! {
            _ = &mut release => return Ok(PlaybackOutcome::ExplicitStop),
            () = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }
}

fn is_hls_manifest_url(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|parsed| {
        std::path::Path::new(parsed.path()).extension().is_some_and(|extension| extension.eq_ignore_ascii_case("m3u8"))
    })
}

struct HeldPlayback {
    playback_id: String,
    release: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<PlaybackOutcome, TestkitError>>,
}

#[derive(Clone)]
pub struct OriginObserver {
    control_base_url: String,
    client: reqwest::Client,
}

impl OriginObserver {
    #[must_use]
    pub fn new(control_base_url: &str) -> Self {
        Self { control_base_url: control_base_url.trim_end_matches('/').to_owned(), client: reqwest::Client::new() }
    }

    pub async fn stats(&self, run_id: &str) -> Result<OriginStats, TestkitError> {
        let url = format!("{}/v1/runs/{run_id}/stats", self.control_base_url);
        let resp = self.client.get(&url).send().await?;
        if !resp.status().is_success() {
            return Err(TestkitError::Protocol(format!("origin stats error: {}", resp.status())));
        }
        let stats = resp.json::<OriginStats>().await?;
        Ok(stats)
    }

    pub async fn events(&self, run_id: &str) -> Result<Vec<OriginEvent>, TestkitError> {
        let url = format!("{}/v1/runs/{run_id}/events", self.control_base_url);
        let resp = self.client.get(&url).send().await?;
        if !resp.status().is_success() {
            return Err(TestkitError::Protocol(format!("origin events error: {}", resp.status())));
        }
        let events = resp.json::<Vec<OriginEvent>>().await?;
        Ok(events)
    }

    pub async fn reset(&self, run_id: &str) -> Result<(), TestkitError> {
        let url = format!("{}/v1/runs/{run_id}/reset", self.control_base_url);
        let resp = self.client.post(&url).send().await?;
        if !resp.status().is_success() {
            return Err(TestkitError::Protocol(format!("origin reset error: {}", resp.status())));
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_vod_until_released(
    url: String,
    vod_object: Option<String>,
    method: Option<String>,
    range: Option<String>,
    expected_status: Option<u16>,
    expected_content_range: Option<String>,
    read_limit_bytes: Option<u64>,
    post_read_action: PostReadAction,
    headers: BTreeMap<String, String>,
    ready: oneshot::Sender<()>,
    mut release: oneshot::Receiver<()>,
) -> Result<PlaybackOutcome, TestkitError> {
    let client = reqwest::Client::builder().http1_only().pool_max_idle_per_host(0).build()?;
    let method = match method.as_deref() {
        Some("HEAD") => reqwest::Method::HEAD,
        _ => reqwest::Method::GET,
    };
    let mut request = client.request(method.clone(), &url);
    for (name, value) in &headers {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
            TestkitError::Configuration(format!("invalid playback request header name {name}: {error}"))
        })?;
        let value = reqwest::header::HeaderValue::from_str(value).map_err(|error| {
            TestkitError::Configuration(format!("invalid playback request header value for {name}: {error}"))
        })?;
        request = request.header(name, value);
    }
    if let Some(ref range_str) = range {
        let name = reqwest::header::RANGE;
        let value = reqwest::header::HeaderValue::from_str(range_str)
            .map_err(|error| TestkitError::Configuration(format!("invalid range header value {range_str}: {error}")))?;
        request = request.header(name, value);
    }
    let response = match request.send().await {
        Ok(res) => res,
        Err(err) => {
            return Ok(PlaybackOutcome::TransportError { message: err.to_string() });
        }
    };
    let status = response.status().as_u16();
    if is_rejection_http_status(status, response.headers()) {
        return Ok(PlaybackOutcome::AdmissionRejected { reason: RejectionReason::HttpStatus(status) });
    }
    let expected = expected_status.unwrap_or(if range.is_some() { 206 } else { 200 });
    if status != expected {
        return Ok(PlaybackOutcome::HttpError { status });
    }
    let start_offset = if range.is_some() || status == 206 {
        let Some(cr_header) = response.headers().get(reqwest::header::CONTENT_RANGE).and_then(|v| v.to_str().ok())
        else {
            return Ok(PlaybackOutcome::InvalidData { message: "range response omits Content-Range".to_owned() });
        };
        let Some((cr_start, cr_end, cr_total)) = tuliprox_testkit::vod::parse_content_range(cr_header) else {
            return Ok(PlaybackOutcome::InvalidData { message: format!("invalid Content-Range header: {cr_header}") });
        };
        if cr_start > cr_end {
            return Ok(PlaybackOutcome::InvalidData {
                message: format!("Content-Range start exceeds end: {cr_header}"),
            });
        }
        if let Some(ref exp_cr) = expected_content_range {
            if cr_header != exp_cr {
                return Ok(PlaybackOutcome::InvalidData {
                    message: format!("unexpected content-range: {cr_header:?}, expected: {exp_cr}"),
                });
            }
        }
        if let Some(ref r) = range {
            let total_for_range = cr_total.unwrap_or(tuliprox_testkit::vod::DEFAULT_VOD_SIZE);
            let Some((req_start, req_end_exclusive)) = tuliprox_testkit::vod::parse_range_header(r, total_for_range)
            else {
                return Ok(PlaybackOutcome::InvalidData {
                    message: format!("failed to parse requested range header against size {total_for_range}: {r}"),
                });
            };
            let req_end_inclusive = req_end_exclusive.saturating_sub(1);
            if cr_start != req_start || cr_end != req_end_inclusive {
                return Ok(PlaybackOutcome::InvalidData {
                    message: format!(
                        "Content-Range {cr_header} does not match requested range {req_start}-{req_end_inclusive}"
                    ),
                });
            }
            if let Some(total) = cr_total {
                if cr_end >= total {
                    return Ok(PlaybackOutcome::InvalidData {
                        message: format!("Content-Range end {cr_end} exceeds or equals total size {total}"),
                    });
                }
            }
        }
        let expected_len = cr_end.saturating_sub(cr_start).saturating_add(1);
        if let Some(cl) = response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
        {
            if cl != expected_len {
                return Ok(PlaybackOutcome::InvalidData {
                    message: format!("Content-Length {cl} does not match Content-Range length {expected_len}"),
                });
            }
        }
        cr_start
    } else {
        0
    };
    if method == reqwest::Method::HEAD {
        let _ = ready.send(());
        let _ = release.await;
        return Ok(PlaybackOutcome::ExplicitStop);
    }

    let object_name = vod_object.as_deref().or_else(|| url.rsplit('/').next()).unwrap_or("movie.mkv");

    let mut body = response.bytes_stream();
    let mut received_bytes: u64 = 0;
    let limit = read_limit_bytes.unwrap_or(u64::MAX);
    let mut ready = Some(ready);

    while received_bytes < limit {
        tokio::select! {
            _ = &mut release => {
                return Ok(if ready.is_none() {
                    PlaybackOutcome::ExplicitStop
                } else {
                    PlaybackOutcome::UnexpectedEof { frames: 0, bytes: received_bytes }
                });
            }
            chunk = tokio::time::timeout(PLAYBACK_IDLE_TIMEOUT, body.next()) => {
                match chunk {
                    Ok(Some(Ok(bytes))) => {
                        let to_take = usize::try_from(limit.saturating_sub(received_bytes))
                            .unwrap_or(usize::MAX)
                            .min(bytes.len());
                        let chunk_offset = start_offset.saturating_add(received_bytes);
                        if !tuliprox_testkit::vod::verify_deterministic_slice(object_name, chunk_offset, &bytes[..to_take]) {
                            return Ok(PlaybackOutcome::InvalidData {
                                message: format!("deterministic slice verification failed for {object_name} at offset {chunk_offset}"),
                            });
                        }
                        received_bytes += to_take as u64;
                        if received_bytes >= limit || (read_limit_bytes.is_none() && to_take == bytes.len()) {
                            if let Some(r) = ready.take() {
                                let _ = r.send(());
                            }
                        }
                    }
                    Ok(Some(Err(err))) => return Ok(PlaybackOutcome::TransportError { message: err.to_string() }),
                    Ok(None) => {
                        if let Some(r) = ready.take() {
                            let _ = r.send(());
                        }
                        return Ok(PlaybackOutcome::UnexpectedEof { frames: 0, bytes: received_bytes });
                    }
                    Err(_) => return Ok(PlaybackOutcome::IdleTimeout),
                }
            }
        }
    }

    if let Some(r) = ready.take() {
        let _ = r.send(());
    }

    match post_read_action {
        PostReadAction::Close => {
            drop(body);
            Ok(PlaybackOutcome::Streaming { frames: 0, bytes: received_bytes })
        }
        PostReadAction::Pause => {
            let _ = release.await;
            Ok(PlaybackOutcome::ExplicitStop)
        }
        PostReadAction::KeepOpen => loop {
            tokio::select! {
                _ = &mut release => return Ok(PlaybackOutcome::ExplicitStop),
                chunk = tokio::time::timeout(PLAYBACK_IDLE_TIMEOUT, body.next()) => {
                    match chunk {
                        Ok(Some(Ok(bytes))) => {
                            let _ = bytes;
                        }
                        Ok(Some(Err(err))) => return Ok(PlaybackOutcome::TransportError { message: err.to_string() }),
                        Ok(None) => return Ok(PlaybackOutcome::ExplicitStop),
                        Err(_) => return Ok(PlaybackOutcome::IdleTimeout),
                    }
                }
            }
        },
    }
}

#[allow(clippy::too_many_arguments)]
async fn start_held_vod_playback(
    playback_id: String,
    url: &str,
    vod_object: Option<String>,
    method: Option<String>,
    range: Option<String>,
    expected_status: Option<u16>,
    expected_content_range: Option<String>,
    read_limit_bytes: Option<u64>,
    post_read_action: PostReadAction,
    headers: &BTreeMap<String, String>,
) -> Result<Result<HeldPlayback, PlaybackOutcome>, TestkitError> {
    let (ready_tx, ready_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let is_close = post_read_action == PostReadAction::Close;
    let task = tokio::spawn(run_vod_until_released(
        url.to_owned(),
        vod_object,
        method,
        range,
        expected_status,
        expected_content_range,
        read_limit_bytes,
        post_read_action,
        headers.clone(),
        ready_tx,
        release_rx,
    ));

    match tokio::time::timeout(Duration::from_secs(10), ready_rx).await {
        Ok(Ok(())) => {
            if is_close {
                match task.await {
                    Ok(Ok(outcome)) => Ok(Err(outcome)),
                    Ok(Err(err)) => Err(err),
                    Err(err) => Err(TestkitError::Protocol(format!("task error: {err}"))),
                }
            } else {
                Ok(Ok(HeldPlayback { playback_id, release: release_tx, task }))
            }
        }
        Ok(Err(_)) => match task.await {
            Ok(Ok(outcome)) => Ok(Err(outcome)),
            Ok(Err(error)) => Err(error),
            Err(error) => Err(TestkitError::Protocol(format!("playback task failed: {error}"))),
        },
        Err(_) => {
            task.abort();
            let _ = task.await;
            Ok(Err(PlaybackOutcome::IdleTimeout))
        }
    }
}

async fn start_held_playback(
    playback_id: String,
    expected_run_id: &RunId,
    url: &str,
    marker: u32,
    frames: u64,
    headers: &BTreeMap<String, String>,
) -> Result<Result<HeldPlayback, PlaybackOutcome>, TestkitError> {
    let (ready_tx, ready_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let task = if is_hls_manifest_url(url) {
        tokio::spawn(run_hls_until_released(
            url.to_owned(),
            expected_run_id.clone(),
            marker,
            frames,
            headers.clone(),
            ready_tx,
            release_rx,
        ))
    } else {
        tokio::spawn(run_agent_until_released(
            url.to_owned(),
            expected_run_id.clone(),
            marker,
            frames,
            headers.clone(),
            ready_tx,
            release_rx,
        ))
    };
    match tokio::time::timeout(Duration::from_secs(10), ready_rx).await {
        Ok(Ok(())) => Ok(Ok(HeldPlayback { playback_id, release: release_tx, task })),
        Ok(Err(_)) => match task.await {
            Ok(Ok(outcome)) => Ok(Err(outcome)),
            Ok(Err(error)) => Err(error),
            Err(error) => Err(TestkitError::Protocol(format!("playback task failed: {error}"))),
        },
        Err(_) => {
            task.abort();
            let _ = task.await;
            Ok(Err(PlaybackOutcome::IdleTimeout))
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn run_controller(
    path: &std::path::Path,
    report_directory: &std::path::Path,
    agent_listen: SocketAddr,
    run_id: Option<&str>,
) -> Result<RunExit, TestkitError> {
    let scenario_name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown").to_owned();
    let mut scenario = match Scenario::from_path(path) {
        Ok(scenario) => scenario,
        Err(err) => {
            let _ = write_raw_report(
                &scenario_name,
                report_directory,
                RunExit::Failed,
                Vec::new(),
                "pre-start",
                run_id,
                Some("configuration_error"),
                Some(&err.to_string()),
                Some(4),
            );
            return Err(err);
        }
    };
    let working_directory = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let expanded_steps = match scenario.expanded_steps() {
        Ok(steps) => steps,
        Err(err) => {
            let _ = write_report(
                &scenario,
                report_directory,
                RunExit::Failed,
                Vec::new(),
                "pre-start",
                run_id,
                Some("configuration_error"),
                Some(&err.to_string()),
                Some(4),
            );
            return Err(err);
        }
    };
    let fixture = match scenario.tuliprox.execution_mode {
        tuliprox_testkit::config::ExecutionMode::IsolatedFixture => {
            let Some(bootstrap) = scenario.tuliprox.bootstrap.as_ref() else {
                let err = TestkitError::Configuration("isolated_fixture requires a bootstrap definition".to_owned());
                let _ = write_report(
                    &scenario,
                    report_directory,
                    RunExit::Failed,
                    Vec::new(),
                    "pre-start",
                    run_id,
                    Some("configuration_error"),
                    Some(&err.to_string()),
                    Some(4),
                );
                return Err(err);
            };
            let testkit_binary = match std::env::current_exe() {
                Ok(b) => b,
                Err(err) => {
                    let err = TestkitError::Io(err);
                    let _ = write_report(
                        &scenario,
                        report_directory,
                        RunExit::Failed,
                        Vec::new(),
                        "pre-start",
                        run_id,
                        Some("bootstrap_error"),
                        Some(&err.to_string()),
                        Some(4),
                    );
                    return Err(err);
                }
            };
            let fixture = match IsolatedFixture::start(
                bootstrap,
                working_directory,
                &testkit_binary,
                &scenario.name,
                scenario.origin.as_ref(),
                scenario.policy_contract.as_ref(),
                &scenario.channels,
                &scenario.tuliprox.fixture_stream,
                scenario.tuliprox.playback_endpoint == tuliprox_testkit::config::PlaybackEndpoint::Xtream,
            )
            .await
            {
                Ok(f) => f,
                Err(err) => {
                    let _ = write_report(
                        &scenario,
                        report_directory,
                        RunExit::Failed,
                        Vec::new(),
                        "pre-start",
                        run_id,
                        Some("bootstrap_error"),
                        Some(&err.to_string()),
                        Some(4),
                    );
                    return Err(err);
                }
            };
            scenario.tuliprox.base_url.clone_from(&fixture.base_url);
            scenario.tuliprox.api_base_url = Some(format!("{}/api/v1", fixture.base_url));
            let has_vod =
                expanded_steps.iter().any(|s| s.start.as_ref().and_then(|st| st.vod_object.as_ref()).is_some());
            if !scenario.channels.is_empty() || has_vod {
                let Some(username) = scenario.actors.iter().find_map(|actor| actor.username.as_deref()) else {
                    let err =
                        TestkitError::Configuration("channel or vod scenarios require an actor username".to_owned());
                    let _ = write_report(
                        &scenario,
                        report_directory,
                        RunExit::Failed,
                        Vec::new(),
                        "pre-start",
                        run_id,
                        Some("configuration_error"),
                        Some(&err.to_string()),
                        Some(4),
                    );
                    let _ = fixture.stop().await;
                    return Err(err);
                };
                scenario.tuliprox.playlist_url = Some(format!(
                    "{}/m3u?username={}&password={}",
                    fixture.base_url,
                    url::form_urlencoded::byte_serialize(username.as_bytes()).collect::<String>(),
                    fixture_password(),
                ));
            }
            Some(fixture)
        }
        tuliprox_testkit::config::ExecutionMode::ExistingInstance => None,
    };
    let origin_control_url = fixture.as_ref().map(|f| f.origin_control_url.clone());
    let scenario_name_for_err = scenario.name.clone();
    let result =
        run_controller_scenario(scenario, report_directory, agent_listen, origin_control_url.as_deref(), run_id).await;
    let cleanup = match fixture {
        Some(fixture) => fixture.stop().await,
        None => Ok(()),
    };
    match (result, cleanup) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Ok(_), Err(error)) => {
            let _ = write_raw_report(
                &scenario_name_for_err,
                report_directory,
                RunExit::Failed,
                Vec::new(),
                "cleanup",
                run_id,
                Some("cleanup_error"),
                Some(&error.to_string()),
                Some(4),
            );
            Err(error)
        }
        (Err(error), _) => {
            let _ = write_raw_report(
                &scenario_name_for_err,
                report_directory,
                RunExit::Failed,
                Vec::new(),
                "execution_error",
                run_id,
                Some("execution_error"),
                Some(&error.to_string()),
                Some(4),
            );
            Err(error)
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn run_controller_scenario(
    scenario: Scenario,
    report_directory: &std::path::Path,
    agent_listen: SocketAddr,
    origin_control_url: Option<&str>,
    run_id: Option<&str>,
) -> Result<RunExit, TestkitError> {
    if scenario.tuliprox.execution_mode == tuliprox_testkit::config::ExecutionMode::ExistingInstance {
        eprintln!("existing-instance mode is intentionally inconclusive until loaded-policy evidence is available");
        write_report(
            &scenario,
            report_directory,
            RunExit::Inconclusive,
            Vec::new(),
            "unproven-existing-instance-policy",
            run_id,
            Some("execution_mode_unsupported"),
            Some("existing-instance mode is intentionally inconclusive until loaded-policy evidence is available"),
            Some(3),
        )?;
        return Ok(RunExit::Inconclusive);
    }
    let api_base_url = scenario
        .tuliprox
        .api_base_url
        .clone()
        .unwrap_or_else(|| format!("{}/api/v1", scenario.tuliprox.base_url.trim_end_matches('/')));
    let credentials = match (&scenario.tuliprox.admin_username, &scenario.tuliprox.admin_password_env) {
        (Some(username), Some(password_env)) => Some((username.clone(), environment(password_env)?)),
        (None, None) => None,
        _ => {
            return Err(TestkitError::Configuration(
                "admin_username and admin_password_env must be specified together".to_owned(),
            ))
        }
    };
    let observer = TuliproxObserver::new(api_base_url, credentials);
    let policy_hash = match observer.config().await {
        Ok(config) => {
            if let Some(contract) = &scenario.policy_contract {
                match tuliprox_testkit::policy::validate_fixture_policy(contract, &config) {
                    Ok(()) => {}
                    Err(TestkitError::Configuration(error)) => {
                        eprintln!("fixture policy contract rejected: {error}");
                        write_report(
                            &scenario,
                            report_directory,
                            RunExit::Invalid,
                            Vec::new(),
                            "policy-mismatch",
                            run_id,
                            Some("policy_mismatch"),
                            Some(&error),
                            Some(2),
                        )?;
                        return Ok(RunExit::Invalid);
                    }
                    Err(error) => {
                        eprintln!("fixture policy evidence unavailable: {error}");
                        write_report(
                            &scenario,
                            report_directory,
                            RunExit::Inconclusive,
                            Vec::new(),
                            "policy-unreadable",
                            run_id,
                            Some("policy_unreadable"),
                            Some(&error.to_string()),
                            Some(3),
                        )?;
                        return Ok(RunExit::Inconclusive);
                    }
                }
            }
            blake3::hash(config.to_string().as_bytes()).to_hex().to_string()
        }
        Err(error) => {
            eprintln!("configuration observation unavailable: {error}");
            write_report(
                &scenario,
                report_directory,
                RunExit::Inconclusive,
                Vec::new(),
                "unavailable",
                run_id,
                Some("configuration_unavailable"),
                Some(&error.to_string()),
                Some(3),
            )?;
            return Ok(RunExit::Inconclusive);
        }
    };
    let expanded_steps = scenario.expanded_steps()?;
    let requires_discovery = expanded_steps.iter().any(|step| {
        step.start.as_ref().and_then(|start| start.channel.as_ref().or(start.vod_object.as_ref())).is_some()
    });
    let virtual_ids_by_user = if requires_discovery {
        let required_markers = channel_markers(&scenario, &expanded_steps)?;
        let required_vod = vod_objects(&expanded_steps);
        let mut map = HashMap::new();
        let usernames: std::collections::BTreeSet<String> =
            scenario.actors.iter().filter_map(|actor| actor.username.clone()).collect();
        if usernames.is_empty() {
            let playlist_url = scenario.tuliprox.playlist_url.as_deref().ok_or_else(|| {
                TestkitError::Configuration(
                    "channel-based or vod-based starts require tuliprox.playlist_url".to_owned(),
                )
            })?;
            let vids = discover_virtual_ids(playlist_url, &required_markers, &required_vod).await?;
            map.insert(String::new(), vids);
        } else {
            for user in usernames {
                let playlist_url = format!(
                    "{}/m3u?username={}&password={}",
                    scenario.tuliprox.base_url.trim_end_matches('/'),
                    url::form_urlencoded::byte_serialize(user.as_bytes()).collect::<String>(),
                    fixture_password(),
                );
                let vids = discover_virtual_ids(&playlist_url, &required_markers, &required_vod).await?;
                map.insert(user, vids);
            }
        }
        map
    } else {
        HashMap::new()
    };
    let origin_observer = origin_control_url.map(OriginObserver::new);
    let controller_state = start_controller_transport(agent_listen, &scenario.name).await?;
    wait_for_required_agents(&controller_state, &scenario.agents.required).await?;
    let (stop_lease_renewals, lease_stop) = watch::channel(false);
    let lease_renewal_task =
        start_agent_lease_renewals(controller_state.clone(), &scenario.agents.required, lease_stop);
    let execution_result = execute_scenario_steps(
        &scenario,
        &expanded_steps,
        &observer,
        origin_observer.as_ref(),
        &controller_state,
        &virtual_ids_by_user,
    )
    .await;
    let _ = stop_lease_renewals.send(true);
    let lease_result = lease_renewal_task
        .await
        .map_err(|error| TestkitError::Protocol(format!("agent lease renewal task failed: {error}")))?;
    let (any_failed, observations_incomplete, events) = execution_result?;
    lease_result?;
    let (outcome, exit_code, error_kind, error_message) = if any_failed {
        (RunExit::Failed, Some(1), Some("assertion_failed"), Some("one or more scenario steps failed"))
    } else if observations_incomplete {
        (RunExit::Inconclusive, Some(3), Some("incomplete_observations"), Some("scenario observations were incomplete"))
    } else {
        (RunExit::Passed, Some(0), None, None)
    };
    write_report(
        &scenario,
        report_directory,
        outcome,
        events,
        &policy_hash,
        run_id,
        error_kind,
        error_message,
        exit_code,
    )?;
    Ok(outcome)
}

fn channel_markers(scenario: &Scenario, steps: &[tuliprox_testkit::config::Step]) -> Result<Vec<u32>, TestkitError> {
    let mut markers =
        steps
            .iter()
            .filter_map(|step| step.start.as_ref())
            .filter_map(|start| start.channel.as_deref())
            .map(|channel| {
                scenario.channels.get(channel).map(|definition| definition.origin_marker).ok_or_else(|| {
                    TestkitError::Configuration(format!("channel {channel} is not defined by the scenario"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
    markers.sort_unstable();
    markers.dedup();
    Ok(markers)
}

fn vod_objects(steps: &[tuliprox_testkit::config::Step]) -> Vec<String> {
    let mut vods = steps
        .iter()
        .filter_map(|step| step.start.as_ref())
        .filter_map(|start| start.vod_object.as_deref())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    vods.sort_unstable();
    vods.dedup();
    vods
}

async fn discover_virtual_ids(
    playlist_url: &str,
    required_markers: &[u32],
    required_vod_objects: &[String],
) -> Result<VirtualIdMap, TestkitError> {
    let deadline = tokio::time::Instant::now() + PLAYLIST_DISCOVERY_TIMEOUT;
    let client = reqwest::Client::new();
    loop {
        let last_reason = match client.get(playlist_url).send().await {
            Ok(response) => match response.error_for_status() {
                Ok(response) => match response.text().await {
                    Ok(playlist) => match VirtualIdMap::from_m3u(&playlist) {
                        Ok(virtual_ids) => {
                            let missing = required_markers
                                .iter()
                                .copied()
                                .filter(|marker| virtual_ids.playback_url(*marker).is_err())
                                .collect::<Vec<_>>();
                            let missing_vod = required_vod_objects
                                .iter()
                                .filter(|name| virtual_ids.named_playback_url(name).is_err())
                                .collect::<Vec<_>>();
                            if missing.is_empty() && missing_vod.is_empty() {
                                return Ok(virtual_ids);
                            }
                            format!("playlist has not published markers {missing:?} or vod {missing_vod:?}")
                        }
                        Err(error) => error.to_string(),
                    },
                    Err(error) => error.to_string(),
                },
                Err(error) => error.to_string(),
            },
            Err(error) => error.to_string(),
        };
        if tokio::time::Instant::now() >= deadline {
            return Err(TestkitError::Protocol(format!(
                "Tuliprox did not publish the required virtual IDs within {} seconds: {last_reason}",
                PLAYLIST_DISCOVERY_TIMEOUT.as_secs()
            )));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn start_agent_lease_renewals(
    controller_state: ControllerState,
    agent_ids: &[String],
    mut stop: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<Result<(), TestkitError>> {
    const LEASE_DURATION: Duration = Duration::from_secs(15);
    let agents = agent_ids.iter().cloned().map(AgentId::new).collect::<Vec<_>>();
    tokio::spawn(async move {
        for agent in &agents {
            dispatch_run_lease(&controller_state, agent, LEASE_DURATION, true).await?;
        }
        let mut interval = tokio::time::interval(LEASE_DURATION / 3);
        interval.tick().await;
        loop {
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        return Ok(());
                    }
                }
                _ = interval.tick() => {
                    for agent in &agents {
                        dispatch_run_lease(&controller_state, agent, LEASE_DURATION, false).await?;
                    }
                }
            }
        }
    })
}

async fn dispatch_run_lease(
    controller_state: &ControllerState,
    agent: &AgentId,
    lease_duration: Duration,
    initial: bool,
) -> Result<(), TestkitError> {
    let sequence = controller_state.next_command_sequence();
    let command_id = format!("lease-{}-{sequence}", agent.0);
    let lease_millis = u64::try_from(lease_duration.as_millis()).unwrap_or(u64::MAX);
    controller_state
        .dispatch(
            agent,
            Envelope {
                schema_version: 1,
                run_id: controller_state.run_id(),
                run_generation: 1,
                message_id: command_id.clone(),
                agent_id: AgentId::new("controller"),
                agent_boot_id: "controller-boot".to_owned(),
                source_sequence: sequence,
                caused_by_command_id: Some(CommandId::new(command_id)),
                local_elapsed_nanos: 0,
                payload: if initial {
                    ControlCommand::ConfigureRun { lease_millis }
                } else {
                    ControlCommand::RenewRunLease { lease_millis }
                },
            },
        )
        .await
}

#[allow(clippy::too_many_arguments)]
async fn check_origin_assertions<'a>(
    step_id: &'a str,
    assert_origin: &tuliprox_testkit::config::AssertOrigin,
    obs: &OriginObserver,
    origin_run_id: &str,
    events: &mut Vec<(&'a str, &'static str)>,
    any_failed: &mut bool,
    observations_incomplete: &mut bool,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(stats) = obs.stats(origin_run_id).await {
            let body_matches =
                assert_origin.active_body.is_none_or(|expected| stats.active_body_connections == expected);
            if body_matches || tokio::time::Instant::now() >= deadline {
                if let Some(max_tcp) = assert_origin.max_active_tcp {
                    if stats.max_active_tcp_connections > max_tcp {
                        *any_failed = true;
                        events.push((step_id, "origin_peak_tcp_exceeded"));
                    }
                }
                if let Some(max_body) = assert_origin.max_active_body {
                    if stats.max_active_body_connections > max_body {
                        *any_failed = true;
                        events.push((step_id, "origin_peak_body_exceeded"));
                    }
                }
                if let Some(act_body) = assert_origin.active_body {
                    if stats.active_body_connections != act_body {
                        *any_failed = true;
                        events.push((step_id, "origin_active_body_mismatch"));
                    }
                }
                let check_evictions = assert_origin.no_evictions.unwrap_or(false);
                let check_rejections = assert_origin.no_limit_rejections.unwrap_or(false);
                let check_account = assert_origin.latest_request_account.as_deref();
                if check_evictions || check_rejections || check_account.is_some() {
                    if let Ok(evts) = obs.events(origin_run_id).await {
                        if check_evictions
                            && evts.iter().any(|e| matches!(e.kind, OriginEventKind::EvictionTriggered { .. }))
                        {
                            *any_failed = true;
                            events.push((step_id, "origin_eviction_observed"));
                        }
                        if check_rejections
                            && evts.iter().any(|e| matches!(e.kind, OriginEventKind::LimitRejected { .. }))
                        {
                            *any_failed = true;
                            events.push((step_id, "origin_rejection_observed"));
                        }
                        if let Some(expected_account) = check_account {
                            let latest_account = evts.iter().rev().find_map(|event| match &event.kind {
                                OriginEventKind::RequestStarted { path, account, .. }
                                    if path.starts_with("/live/") || path.starts_with("/vod/") =>
                                {
                                    Some(account.as_deref())
                                }
                                _ => None,
                            });
                            if latest_account != Some(Some(expected_account)) {
                                *any_failed = true;
                                events.push((step_id, "origin_request_account_mismatch"));
                            }
                        }
                    } else {
                        *observations_incomplete = true;
                        events.push((step_id, "origin_events_unavailable"));
                    }
                }
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        } else {
            if tokio::time::Instant::now() >= deadline {
                *observations_incomplete = true;
                events.push((step_id, "origin_stats_unavailable"));
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

/// Verifies the SUT's user-side accounting (`active_users`, `active_user_connections`)
/// after a step, polling briefly so an in-flight cleanup does not fail the check.
async fn check_runtime_assertions<'a>(
    step_id: &'a str,
    assertions: &RuntimeAssertions,
    observer: &TuliproxObserver,
    events: &mut Vec<(&'a str, &'static str)>,
    any_failed: &mut bool,
    observations_incomplete: &mut bool,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(status) = observer.status().await {
            let users = status.get("active_users").and_then(serde_json::Value::as_u64);
            let connections = status.get("active_user_connections").and_then(serde_json::Value::as_u64);
            let (Some(users), Some(connections)) = (users, connections) else {
                *observations_incomplete = true;
                events.push((step_id, "runtime_user_counts_unavailable"));
                return;
            };
            let users_ok = assertions.active_users.is_none_or(|expected| users == expected as u64);
            let connections_ok =
                assertions.active_user_connections.is_none_or(|expected| connections == expected as u64);
            if (users_ok && connections_ok) || tokio::time::Instant::now() >= deadline {
                if !users_ok {
                    *any_failed = true;
                    events.push((step_id, "runtime_active_users_mismatch"));
                }
                if !connections_ok {
                    *any_failed = true;
                    events.push((step_id, "runtime_active_user_connections_mismatch"));
                }
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        } else if tokio::time::Instant::now() >= deadline {
            *observations_incomplete = true;
            events.push((step_id, "runtime_status_unavailable"));
            return;
        } else {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn execute_scenario_steps<'a>(
    scenario: &'a Scenario,
    steps: &'a [tuliprox_testkit::config::Step],
    observer: &TuliproxObserver,
    origin_observer: Option<&OriginObserver>,
    controller_state: &ControllerState,
    virtual_ids_by_user: &HashMap<String, VirtualIdMap>,
) -> Result<(bool, bool, Vec<(&'a str, &'static str)>), TestkitError> {
    let mut any_failed = false;
    let mut observations_incomplete = false;
    let mut events = Vec::new();
    let mut held_playbacks: HashMap<String, HeldPlayback> = HashMap::new();
    let mut remote_playbacks: HashMap<String, String> = HashMap::new();
    let mut expected_terminations = HashSet::new();
    let mut playback_users = HashMap::new();
    let mut admission_oracles = scenario.policy_contract.as_ref().map(|contract| {
        contract
            .users
            .iter()
            .map(|(username, policy)| {
                (
                    username.clone(),
                    AdmissionOracle::with_policy(
                        policy.max_connections,
                        policy.soft_connections,
                        contract.effective_admission_strategies(),
                        contract.grace.as_ref().map(|grace| grace.mode),
                    ),
                )
            })
            .collect::<HashMap<_, _>>()
    });
    let mut admission_order = 0_u64;
    if let Some(obs) = origin_observer {
        let origin_run_id =
            scenario.origin.as_ref().map_or_else(|| RunId::new(&scenario.name), |origin| RunId::new(&origin.run_id));
        let _ = obs.reset(&origin_run_id.0).await;
    }
    let execution_result = async {
        for step in steps {
            if let Some(stop) = &step.stop {
                if let Some(oracles) = admission_oracles.as_mut() {
                    if let Some(username) = playback_users.remove(&stop.playback_id) {
                        oracles
                            .get_mut(&username)
                            .ok_or_else(|| TestkitError::Protocol(format!("missing admission oracle for {username}")))?
                            .release(&stop.playback_id)?;
                    }
                }
                if let Some(held) = held_playbacks.remove(&stop.playback_id) {
                    let _ = held.release.send(());
                    match held.task.await {
                        Ok(Ok(PlaybackOutcome::ExplicitStop | PlaybackOutcome::Streaming { .. })) => {
                            events.push((stop.playback_id.as_str(), "stopped"));
                        }
                        Ok(Ok(_) | Err(_)) | Err(_) => any_failed = true,
                    }
                } else if let Some(agent) = remote_playbacks.remove(&stop.playback_id) {
                    let outcome = dispatch_remote_stop(
                        controller_state,
                        &agent,
                        &scenario.name,
                        &step.command_id,
                        &stop.playback_id,
                    )
                    .await?;
                    match outcome {
                        PlaybackOutcome::ExplicitStop | PlaybackOutcome::Streaming { .. } => {
                            events.push((stop.playback_id.as_str(), "stopped"));
                        }
                        _ => any_failed = true,
                    }
                } else {
                    return Err(TestkitError::Protocol(format!(
                        "playback {} was not active at stop",
                        stop.playback_id
                    )));
                }

                if let Some(assert_origin) = &step.assert_origin {
                    if let Some(obs) = origin_observer {
                        let origin_run_id = scenario
                            .origin
                            .as_ref()
                            .map_or_else(|| RunId::new(&scenario.name), |origin| RunId::new(&origin.run_id));
                        check_origin_assertions(
                            stop.playback_id.as_str(),
                            assert_origin,
                            obs,
                            &origin_run_id.0,
                            &mut events,
                            &mut any_failed,
                            &mut observations_incomplete,
                        )
                        .await;
                    } else {
                        observations_incomplete = true;
                        events.push((stop.playback_id.as_str(), "origin_observer_missing"));
                    }
                }

                if let Some(runtime) = &step.assert_runtime {
                    check_runtime_assertions(
                        stop.playback_id.as_str(),
                        runtime,
                        observer,
                        &mut events,
                        &mut any_failed,
                        &mut observations_incomplete,
                    )
                    .await;
                }

                continue;
            }
            let start =
                step.start.as_ref().ok_or_else(|| TestkitError::Configuration("step has no start".to_owned()))?;
            let actor = scenario
                .actors
                .iter()
                .find(|actor| actor.id == start.actor)
                .ok_or_else(|| TestkitError::Configuration(format!("unknown actor {}", start.actor)))?;
            let is_vod = start.vod_object.is_some()
                || start.range.is_some()
                || start.read_limit_bytes.is_some()
                || start.method.is_some();
            let frames = step
                .await_frames
                .or_else(|| step.await_condition.as_ref().and_then(|condition| condition.valid_frames))
                .unwrap_or(1);
            let url_owned: String;
            let url = if let Some(url) = start.url.as_deref() {
                url
            } else {
                let vids = if let Some(username) = actor.username.as_deref() {
                    virtual_ids_by_user.get(username).or_else(|| virtual_ids_by_user.get(""))
                } else {
                    virtual_ids_by_user.get("")
                }
                .ok_or_else(|| {
                    TestkitError::Configuration(format!(
                        "virtual-ID discovery was not initialized for actor {}",
                        actor.id
                    ))
                })?;
                if let Some(vod) = start.vod_object.as_deref() {
                    vids.named_playback_url(vod)?
                } else {
                    let channel = start.channel.as_deref().ok_or_else(|| {
                        TestkitError::Configuration("start has no URL, vod_object, or channel".to_owned())
                    })?;
                    let channel_def = scenario
                        .channels
                        .get(channel)
                        .ok_or_else(|| TestkitError::Configuration(format!("unknown channel {channel}")))?;
                    let marker = channel_def.origin_marker;
                    url_owned = resolve_playback_url(
                        scenario.tuliprox.playback_endpoint,
                        &channel_def.protocol,
                        &scenario.tuliprox.base_url,
                        actor.username.as_deref().unwrap_or(""),
                        fixture_password(),
                        vids,
                        marker,
                    )?;
                    &url_owned
                }
            };
            let marker = if is_vod {
                0
            } else {
                start
                    .channel
                    .as_deref()
                    .and_then(|channel| scenario.channels.get(channel))
                    .map_or_else(|| marker_from_url(url), |channel| Ok(channel.origin_marker))?
            };
            admission_order = admission_order.saturating_add(1);
            let mut evicted_victim = None;
            let mut evicted_victim_id = None;
            if let Some(oracles) = admission_oracles.as_mut() {
                let username = actor.username.as_deref().ok_or_else(|| {
                    TestkitError::Configuration(format!(
                        "actor {} requires username because this scenario has a user admission policy",
                        actor.id
                    ))
                })?;
                let oracle = oracles.get_mut(username).ok_or_else(|| {
                    TestkitError::Configuration(format!(
                        "actor {} references user {username} absent from policy_contract",
                        actor.id
                    ))
                })?;
                let client_ip =
                    actor.client_ip.as_ref().and_then(|ip| ip.value.as_deref()).unwrap_or("peer-address").to_owned();
                let (decision, evicted) = oracle.admit_with_evicted(AdmissionRequest {
                    playback_id: start.playback_id.clone(),
                    client_ip,
                    started_order: admission_order,
                })?;
                evicted_victim = evicted;
                let oracle_expected = match &decision {
                    AdmissionDecision::Admit
                    | AdmissionDecision::AdmitSoft
                    | AdmissionDecision::Evict { .. }
                    | AdmissionDecision::Grace { .. } => ExpectedPlayback::Streaming,
                    AdmissionDecision::Deny => ExpectedPlayback::Rejected,
                };
                let oracle_tracks_playback = matches!(
                    &decision,
                    AdmissionDecision::Admit | AdmissionDecision::AdmitSoft | AdmissionDecision::Evict { .. }
                );
                if let AdmissionDecision::Evict { playback_id } = &decision {
                    expected_terminations.insert(playback_id.clone());
                    evicted_victim_id = Some(playback_id.clone());
                }
                let provider_limited_rejection = scenario
                    .policy_contract
                    .as_ref()
                    .and_then(tuliprox_testkit::config::PolicyContract::provider_capacity)
                    .is_some()
                    && step.expect.is_rejected()
                    && oracle_expected == ExpectedPlayback::Streaming;
                // The static oracle does not model the time-based reentry guard, so a
                // suppressed retry intentionally diverges from its eviction prediction.
                // The concrete SUT outcome is asserted through `step.expect`.
                let reentry_suppression = matches!(step.expect, ExpectedPlayback::Suppressed);
                if step.expect != oracle_expected && !provider_limited_rejection && !reentry_suppression {
                    return Err(TestkitError::Configuration(format!(
                        "step {} expects {:?}, but policy oracle expects {:?}",
                        step.command_id, step.expect, oracle_expected
                    )));
                }
                if oracle_tracks_playback {
                    playback_users.insert(start.playback_id.clone(), username.to_owned());
                }
            }
            let mut headers = actor_request_headers(actor, actor.agent == "local")?;
            if let Some(ua) = &start.user_agent {
                headers.insert("user-agent".to_owned(), ua.clone());
            }
            let origin_run_id = scenario
                .origin
                .as_ref()
                .map_or_else(|| RunId::new(&scenario.name), |origin| RunId::new(&origin.run_id));
            let local_outcome = if actor.agent == "local" {
                if is_vod {
                    match start_held_vod_playback(
                        start.playback_id.clone(),
                        url,
                        start.vod_object.clone(),
                        start.method.clone(),
                        start.range.clone(),
                        start.expected_status,
                        start.expected_content_range.clone(),
                        start.read_limit_bytes,
                        start.post_read_action.unwrap_or_default(),
                        &headers,
                    )
                    .await?
                    {
                        Ok(held) => {
                            held_playbacks.insert(start.playback_id.clone(), held);
                            Some(PlaybackOutcome::Streaming { frames: 0, bytes: 0 })
                        }
                        Err(outcome) => Some(outcome),
                    }
                } else {
                    match start_held_playback(start.playback_id.clone(), &origin_run_id, url, marker, frames, &headers)
                        .await?
                    {
                        Ok(held) => {
                            held_playbacks.insert(start.playback_id.clone(), held);
                            Some(PlaybackOutcome::Streaming { frames, bytes: 0 })
                        }
                        Err(outcome) => Some(outcome),
                    }
                }
            } else {
                None
            };
            let remote_outcome = if actor.agent == "local" {
                None
            } else {
                dispatch_remote_playback(
                    controller_state,
                    &actor.agent,
                    &scenario.name,
                    &step.command_id,
                    &start.playback_id,
                    url,
                    headers.clone(),
                    Some(origin_run_id),
                    Some(marker),
                    Some(frames),
                )
                .await?
            };
            let (step_passed, actual_streaming) = match (&local_outcome, &remote_outcome) {
                (Some(outcome), _) | (None, Some(outcome)) => {
                    (step.expect.matches_outcome(outcome), outcome.is_streaming())
                }
                (None, None) => (false, false),
            };
            if step_passed {
                events.push((
                    start.playback_id.as_str(),
                    if step.expect.is_streaming() { "received_valid_frames" } else { "rejected_as_expected" },
                ));
                if remote_outcome.as_ref().is_some_and(PlaybackOutcome::is_streaming) {
                    remote_playbacks.insert(start.playback_id.clone(), actor.agent.clone());
                }
            } else {
                eprintln!(
                    "Step {} ({}) failed: expect={:?}, local_outcome={:?}, remote_outcome={:?}",
                    step.command_id, start.playback_id, step.expect, local_outcome, remote_outcome
                );
                any_failed = true;
                events.push((start.playback_id.as_str(), "failed"));
            }
            if !actual_streaming {
                if let Some(oracles) = admission_oracles.as_mut() {
                    if let Some(username) = actor.username.as_deref() {
                        if let Some(oracle) = oracles.get_mut(username) {
                            oracle.rollback(&start.playback_id, evicted_victim);
                        }
                    }
                    playback_users.remove(&start.playback_id);
                    if let Some(victim_id) = &evicted_victim_id {
                        expected_terminations.remove(victim_id);
                    }
                }
            }
            if let Some(assert_origin) = &step.assert_origin {
                if let Some(obs) = origin_observer {
                    let origin_run_id = scenario
                        .origin
                        .as_ref()
                        .map_or_else(|| RunId::new(&scenario.name), |origin| RunId::new(&origin.run_id));
                    check_origin_assertions(
                        start.playback_id.as_str(),
                        assert_origin,
                        obs,
                        &origin_run_id.0,
                        &mut events,
                        &mut any_failed,
                        &mut observations_incomplete,
                    )
                    .await;
                } else {
                    observations_incomplete = true;
                    events.push((start.playback_id.as_str(), "origin_observer_missing"));
                }
            }
            if let Ok(snapshot) = observer.runtime_snapshot().await {
                events.push((start.playback_id.as_str(), "runtime_observed"));
                if let Some(policy) = &scenario.policy_contract {
                    if let Ok(slots) = provider_slot_count(&snapshot.status) {
                        if policy.provider_capacity().is_some_and(|limit| slots > limit) {
                            any_failed = true;
                            events.push((start.playback_id.as_str(), "provider_limit_exceeded"));
                        }
                        if policy.expected_provider_slots.is_some_and(|expected| slots != expected) {
                            any_failed = true;
                            events.push((start.playback_id.as_str(), "provider_slot_count_mismatch"));
                        }
                    } else {
                        observations_incomplete = true;
                        events.push((start.playback_id.as_str(), "provider_observation_invalid"));
                    }
                }
            } else {
                observations_incomplete = true;
                events.push((start.playback_id.as_str(), "runtime_observation_unavailable"));
            }
            if let Some(runtime) = &step.assert_runtime {
                check_runtime_assertions(
                    start.playback_id.as_str(),
                    runtime,
                    observer,
                    &mut events,
                    &mut any_failed,
                    &mut observations_incomplete,
                )
                .await;
            }
        }
        Ok::<(), TestkitError>(())
    }
    .await;
    for (_, mut held) in held_playbacks {
        let expected_termination = expected_terminations.contains(&held.playback_id);
        let event_playback_id = steps
            .iter()
            .filter_map(|step| step.start.as_ref())
            .find(|start| start.playback_id == held.playback_id)
            .map(|start| start.playback_id.as_str())
            .ok_or_else(|| TestkitError::Protocol(format!("missing scenario playback {}", held.playback_id)))?;
        if expected_termination {
            match tokio::time::timeout(PLAYBACK_IDLE_TIMEOUT, &mut held.task).await {
                Ok(Ok(Ok(PlaybackOutcome::Streaming { .. } | PlaybackOutcome::ExplicitStop))) => {
                    events.push((event_playback_id, "unexpected_clean_terminal"));
                    any_failed = true;
                }
                Ok(Ok(Ok(_))) => events.push((event_playback_id, "evicted")),
                Ok(Ok(Err(_)) | Err(_)) => {
                    events.push((event_playback_id, "unexpected_eviction_terminal"));
                    any_failed = true;
                }
                Err(_) => {
                    held.task.abort();
                    let _ = held.task.await;
                    any_failed = true;
                    events.push((event_playback_id, "expected_eviction_not_observed"));
                }
            }
            let _ = held.release.is_closed();
        } else {
            let _ = held.release.send(());
            match held.task.await {
                Ok(Ok(PlaybackOutcome::ExplicitStop | PlaybackOutcome::Streaming { .. })) => {}
                Ok(Ok(_) | Err(_)) | Err(_) => any_failed = true,
            }
        }
    }
    for (playback_id, agent) in remote_playbacks {
        if let Ok(outcome) = dispatch_remote_stop(
            controller_state,
            &agent,
            &scenario.name,
            &format!("cleanup-{playback_id}"),
            &playback_id,
        )
        .await
        {
            match outcome {
                PlaybackOutcome::ExplicitStop | PlaybackOutcome::Streaming { .. } => {}
                _ => any_failed = true,
            }
        } else {
            any_failed = true;
        }
    }
    execution_result?;
    if scenario.policy_contract.as_ref().and_then(tuliprox_testkit::config::PolicyContract::provider_capacity).is_some()
    {
        if wait_for_provider_slots(observer, 0, Duration::from_secs(5)).await.is_ok() {
            events.push((scenario.name.as_str(), "provider_slots_released"));
        } else {
            any_failed = true;
            events.push((scenario.name.as_str(), "provider_slots_not_released"));
        }
    }
    Ok((any_failed, observations_incomplete, events))
}

async fn wait_for_provider_slots(
    observer: &TuliproxObserver,
    expected: usize,
    timeout: Duration,
) -> Result<(), TestkitError> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if provider_slot_count(&observer.status().await?)? == expected {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(TestkitError::Protocol(format!("provider slots did not converge to {expected}")));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn provider_slot_count(status: &serde_json::Value) -> Result<usize, TestkitError> {
    let providers = status.get("active_provider_connections").ok_or_else(|| {
        TestkitError::Protocol("status response missing active_provider_connections field".to_owned())
    })?;
    if providers.is_null() {
        return Ok(0);
    }
    let Some(providers) = providers.as_object() else {
        return Err(TestkitError::Protocol("status active_provider_connections is not an object".to_owned()));
    };
    providers.values().try_fold(0_usize, |total, value| {
        let count = value
            .as_u64()
            .ok_or_else(|| TestkitError::Protocol("provider connection count is not an unsigned integer".to_owned()))?;
        let count = usize::try_from(count)
            .map_err(|_| TestkitError::Protocol("provider connection count exceeds platform capacity".to_owned()))?;
        total.checked_add(count).ok_or_else(|| TestkitError::Protocol("provider connection count overflow".to_owned()))
    })
}

fn actor_request_headers(
    actor: &tuliprox_testkit::config::Actor,
    allow_controller_secrets: bool,
) -> Result<BTreeMap<String, String>, TestkitError> {
    let mut headers = BTreeMap::new();
    if let Some(user_agent) = &actor.user_agent {
        headers.insert("user-agent".to_owned(), user_agent.clone());
    }
    if let Some(client_ip) = &actor.client_ip {
        let value = client_ip.value.as_deref().filter(|value| !value.trim().is_empty()).ok_or_else(|| {
            TestkitError::Configuration(format!("actor {} client_ip requires a non-empty value", actor.id))
        })?;
        match client_ip.mode.as_str() {
            "x-real-ip" => {
                headers.insert("x-real-ip".to_owned(), value.to_owned());
            }
            "forwarded" | "x-forwarded-for" => {
                headers.insert("x-forwarded-for".to_owned(), value.to_owned());
            }
            unsupported => {
                return Err(TestkitError::Configuration(format!(
                    "actor {} has unsupported client_ip mode {unsupported}",
                    actor.id
                )));
            }
        }
    }
    match (&actor.username, &actor.password_env) {
        (Some(username), Some(password_env)) if allow_controller_secrets => {
            let credentials = format!("{username}:{}", environment(password_env)?);
            headers.insert("authorization".to_owned(), format!("Basic {}", BASE64.encode(credentials)));
        }
        (Some(_), Some(_)) => {
            return Err(TestkitError::Configuration(format!(
                "remote actor {} credentials must be supplied by an agent-local secret provider",
                actor.id
            )));
        }
        (Some(_) | None, None) => {}
        _ => {
            return Err(TestkitError::Configuration(format!(
                "actor {} must specify username and password_env together",
                actor.id
            )));
        }
    }
    Ok(headers)
}

async fn start_controller_transport(agent_listen: SocketAddr, run_name: &str) -> Result<ControllerState, TestkitError> {
    let listener = TcpListener::bind(agent_listen).await?;
    let state: ControllerState = Arc::new(SharedControllerState::new(RunId::new(run_name), 1));
    let app = Router::new().route("/agent", get(controller_websocket)).with_state(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(state)
}

async fn wait_for_required_agents(controller_state: &ControllerState, required: &[String]) -> Result<(), TestkitError> {
    if required.is_empty() {
        return Ok(());
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let connected = controller_state
            .ready_agents()
            .await
            .into_iter()
            .map(|agent| agent.0)
            .collect::<std::collections::HashSet<_>>();
        if required.iter().all(|agent| connected.contains(agent)) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(TestkitError::Protocol("required test agents did not become ready before deadline".to_owned()));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_remote_playback(
    controller_state: &ControllerState,
    agent: &str,
    run_name: &str,
    command_id: &str,
    playback_id: &str,
    url: &str,
    headers: BTreeMap<String, String>,
    expected_run_id: Option<RunId>,
    expected_marker: Option<u32>,
    required_frames: Option<u64>,
) -> Result<Option<PlaybackOutcome>, TestkitError> {
    let command = Envelope {
        schema_version: 1,
        run_id: RunId::new(run_name),
        run_generation: 1,
        message_id: command_id.to_owned(),
        agent_id: AgentId::new("controller"),
        agent_boot_id: "controller-boot".to_owned(),
        source_sequence: controller_state.next_command_sequence(),
        caused_by_command_id: Some(tuliprox_testkit::protocol::CommandId::new(command_id)),
        local_elapsed_nanos: 0,
        payload: ControlCommand::StartPlayback {
            playback_id: tuliprox_testkit::protocol::PlaybackId::new(playback_id),
            url: url.to_owned(),
            headers,
            expected_run_id,
            expected_marker,
            required_frames,
        },
    };
    controller_state.dispatch(&AgentId::new(agent), command).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(event) = controller_state.take_playback_event(playback_id).await {
            match event {
                PlaybackEvent::FirstValidFrame { .. } => {
                    return Ok(Some(PlaybackOutcome::Streaming { frames: required_frames.unwrap_or(5), bytes: 0 }));
                }
                PlaybackEvent::Terminal { typed_outcome, outcome, .. } => {
                    return Ok(Some(typed_outcome.unwrap_or_else(|| match outcome.as_str() {
                        "passed" => PlaybackOutcome::Streaming { frames: required_frames.unwrap_or(5), bytes: 0 },
                        "rejected" | "rejected_as_expected" => {
                            PlaybackOutcome::AdmissionRejected { reason: RejectionReason::Other("rejected".to_owned()) }
                        }
                        _ => PlaybackOutcome::InfrastructureError { message: outcome },
                    })));
                }
                PlaybackEvent::CommandAccepted { .. }
                | PlaybackEvent::HeadersReceived { .. }
                | PlaybackEvent::Progress { .. }
                | PlaybackEvent::LeaseExpired => {}
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(TestkitError::Protocol(format!(
                "remote agent {agent} did not report terminal playback {playback_id}"
            )));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn dispatch_remote_stop(
    controller_state: &ControllerState,
    agent: &str,
    run_name: &str,
    command_id: &str,
    playback_id: &str,
) -> Result<PlaybackOutcome, TestkitError> {
    controller_state
        .dispatch(
            &AgentId::new(agent),
            Envelope {
                schema_version: 1,
                run_id: RunId::new(run_name),
                run_generation: 1,
                message_id: command_id.to_owned(),
                agent_id: AgentId::new("controller"),
                agent_boot_id: "controller-boot".to_owned(),
                source_sequence: controller_state.next_command_sequence(),
                caused_by_command_id: Some(tuliprox_testkit::protocol::CommandId::new(command_id)),
                local_elapsed_nanos: 0,
                payload: ControlCommand::StopPlayback {
                    playback_id: tuliprox_testkit::protocol::PlaybackId::new(playback_id),
                },
            },
        )
        .await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(PlaybackEvent::Terminal { typed_outcome, outcome, .. }) =
            controller_state.take_playback_event(playback_id).await
        {
            return Ok(typed_outcome.unwrap_or(match outcome.as_str() {
                "passed" | "cancelled" => PlaybackOutcome::ExplicitStop,
                _ => PlaybackOutcome::InfrastructureError { message: outcome },
            }));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(TestkitError::Protocol(format!(
                "remote agent {agent} did not confirm stop for playback {playback_id}"
            )));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[allow(clippy::too_many_arguments)]
fn write_raw_report(
    scenario_name: &str,
    directory: &std::path::Path,
    outcome: RunExit,
    events: Vec<(&str, &str)>,
    policy_hash: &str,
    run_id: Option<&str>,
    error_kind: Option<&str>,
    error_message: Option<&str>,
    exit_code: Option<i32>,
) -> Result<(), TestkitError> {
    let outcome_text = match outcome {
        RunExit::Passed => "passed",
        RunExit::Failed => "failed",
        RunExit::Invalid => "invalid",
        RunExit::Inconclusive => "inconclusive",
    };
    let report = RunReport {
        schema_version: 1,
        scenario: scenario_name,
        outcome: outcome_text,
        policy_hash,
        run_id,
        error_kind,
        error_message,
        exit_code,
        events: events
            .into_iter()
            .map(|(playback_id, event)| ReportEvent { source: "agent", playback_id, event, detail: "redacted" })
            .collect(),
    };
    report.write_to(directory)
}

#[allow(clippy::too_many_arguments)]
fn write_report(
    scenario: &Scenario,
    directory: &std::path::Path,
    outcome: RunExit,
    events: Vec<(&str, &str)>,
    policy_hash: &str,
    run_id: Option<&str>,
    error_kind: Option<&str>,
    error_message: Option<&str>,
    exit_code: Option<i32>,
) -> Result<(), TestkitError> {
    write_raw_report(
        &scenario.name,
        directory,
        outcome,
        events,
        policy_hash,
        run_id,
        error_kind,
        error_message,
        exit_code,
    )
}

fn marker_from_url(url: &str) -> Result<u32, TestkitError> {
    let parsed =
        url::Url::parse(url).map_err(|error| TestkitError::Configuration(format!("invalid stream URL: {error}")))?;
    let path = parsed
        .path_segments()
        .ok_or_else(|| TestkitError::Configuration("stream URL has no path segments".to_owned()))?
        .collect::<Vec<_>>();
    path.iter()
        .rev()
        .find_map(|segment| segment.split('.').next().and_then(|value| value.parse::<u32>().ok()))
        .ok_or_else(|| TestkitError::Configuration("stream URL has no numeric marker path segment".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_origin_state(run_id: &str) -> OriginState {
        OriginState {
            run_id: RunId::new(run_id),
            bitrate: 64_000,
            markers: Arc::new(vec![17]),
            stream_counter: Arc::new(Mutex::new(4)),
            observations: Arc::new(Mutex::new(Vec::new())),
            faults: Arc::new(Mutex::new(FaultSchedule::default())),
            hls_sequences: Arc::new(Mutex::new(HashMap::from([(17, 9)]))),
            tracker: OriginTracker::new(run_id, OriginPolicy::default()),
        }
    }

    #[test]
    fn extracts_marker_from_ts_url() {
        assert!(matches!(marker_from_url("http://origin/live/17.ts"), Ok(17)));
    }

    #[test]
    fn extracts_marker_from_hls_manifest_url() {
        assert!(matches!(marker_from_url("http://origin/hls/17/index.m3u8"), Ok(17)));
    }

    #[test]
    fn recognizes_hls_manifest_urls() {
        assert!(is_hls_manifest_url("http://origin/hls/17/index.m3u8"));
        assert!(!is_hls_manifest_url("http://origin/live/17.ts"));
    }

    #[test]
    fn parses_open_and_closed_byte_ranges() {
        assert_eq!(tuliprox_testkit::vod::parse_range_header("bytes=2-4", 10), Some((2, 5)));
        assert_eq!(tuliprox_testkit::vod::parse_range_header("bytes=8-", 10), Some((8, 10)));
        assert_eq!(tuliprox_testkit::vod::parse_range_header("bytes=10-", 10), None);
    }

    #[test]
    fn vod_validators_are_stable_for_identical_payloads() {
        let payload = vod_payload("movie.ts");
        let mut first = StatusCode::OK.into_response();
        let mut second = StatusCode::OK.into_response();
        apply_vod_validators(&mut first, &payload);
        apply_vod_validators(&mut second, &payload);
        assert_eq!(first.headers().get(header::ETAG), second.headers().get(header::ETAG));
        assert_eq!(
            first.headers().get(header::LAST_MODIFIED),
            Some(&HeaderValue::from_static("Thu, 01 Jan 1970 00:00:00 GMT"))
        );
    }

    #[tokio::test]
    async fn origin_reset_is_scoped_to_its_run() {
        let state = test_origin_state("run-a");
        state.observations.lock().await.push(OriginObservation {
            origin_stream_id: "origin-4".to_owned(),
            channel_marker: 17,
            frames_emitted: 0,
            bytes_emitted: 0,
        });
        assert_eq!(reset_origin_run(Path("other".to_owned()), State(state.clone())).await, StatusCode::NOT_FOUND);
        assert_eq!(reset_origin_run(Path("run-a".to_owned()), State(state.clone())).await, StatusCode::NO_CONTENT);
        assert!(state.observations.lock().await.is_empty());
        assert_eq!(*state.stream_counter.lock().await, 0);
        assert!(state.hls_sequences.lock().await.is_empty());
    }

    #[tokio::test]
    async fn catalog_exposes_each_configured_marker() {
        let mut state = test_origin_state("catalog-run");
        state.markers = Arc::new(vec![17, 19]);
        let document = catalog_m3u(&state, "127.0.0.1", None);
        assert!(document.contains("tvg-id=\"test-17\""));
        assert!(document.contains("tvg-id=\"test-19\""));
        assert!(document.contains("run=catalog-run"));
    }

    #[tokio::test]
    async fn hls_rejects_a_marker_absent_from_the_origin_catalog() {
        let state = test_origin_state("hls-run");
        assert_eq!(
            hls(
                Path("19/index.m3u8".to_owned()),
                Query(HashMap::from([("run".to_owned(), "hls-run".to_owned())])),
                State(state),
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn live_rejects_a_missing_or_foreign_run_correlation() {
        let state = test_origin_state("live-run");
        assert_eq!(
            live(Path("17.ts".to_owned()), Query(HashMap::new()), State(state.clone()), None, HeaderMap::new())
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            live(
                Path("17.ts".to_owned()),
                Query(HashMap::from([("run".to_owned(), "other".to_owned())])),
                State(state),
                None,
                HeaderMap::new(),
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn vod_head_rejects_a_missing_run_correlation() {
        let state = test_origin_state("vod-run");
        assert_eq!(
            vod_head(Path("movie.ts".to_owned()), Query(HashMap::new()), State(state)).await.status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn faults_cannot_be_mutated_by_a_foreign_run() {
        let state = test_origin_state("fault-run");
        assert_eq!(
            set_fault(Path(("other".to_owned(), "close-before-first-byte".to_owned())), State(state.clone()),).await,
            StatusCode::NOT_FOUND
        );
        assert!(state.faults.lock().await.take("close-before-first-byte").is_none());
    }

    #[test]
    fn actor_identity_uses_configured_forwarded_ip_and_user_agent() {
        let actor = tuliprox_testkit::config::Actor {
            id: "viewer".to_owned(),
            agent: "local".to_owned(),
            username: None,
            password_env: None,
            user_agent: Some("test-receiver/1".to_owned()),
            client_ip: Some(tuliprox_testkit::config::ClientIp {
                mode: "x-real-ip".to_owned(),
                value: Some("198.51.100.42".to_owned()),
            }),
        };
        let headers = actor_request_headers(&actor, true).unwrap();
        assert_eq!(headers.get("x-real-ip"), Some(&"198.51.100.42".to_owned()));
        assert_eq!(headers.get("user-agent"), Some(&"test-receiver/1".to_owned()));
    }

    #[test]
    fn remote_actor_cannot_receive_controller_credentials() {
        let actor = tuliprox_testkit::config::Actor {
            id: "viewer".to_owned(),
            agent: "agent-b".to_owned(),
            username: Some("alice".to_owned()),
            password_env: Some("TESTKIT_PASSWORD".to_owned()),
            user_agent: None,
            client_ip: None,
        };
        assert!(matches!(actor_request_headers(&actor, false), Err(TestkitError::Configuration(_))));
    }

    #[tokio::test]
    async fn vod_reader_rejects_206_missing_content_range() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
            let response = "HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nConnection: close\r\n\r\ntest";
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
        });

        let (ready_tx, _ready_rx) = tokio::sync::oneshot::channel();
        let (_release_tx, release_rx) = tokio::sync::oneshot::channel();
        let outcome = run_vod_until_released(
            format!("http://127.0.0.1:{port}/movie.mkv"),
            None,
            None,
            Some("bytes=0-3".to_owned()),
            None,
            None,
            None,
            PostReadAction::KeepOpen,
            BTreeMap::new(),
            ready_tx,
            release_rx,
        )
        .await
        .unwrap();

        assert!(matches!(outcome, PlaybackOutcome::InvalidData { message } if message.contains("omits Content-Range")));
    }

    #[tokio::test]
    async fn check_origin_assertions_events_http_500_marks_observations_incomplete() {
        use axum::routing::get;
        let app = Router::new()
            .route(
                "/v1/runs/test-run/stats",
                get(|| async {
                    Json(OriginStats {
                        active_tcp_connections: 0,
                        max_active_tcp_connections: 0,
                        active_body_connections: 0,
                        max_active_body_connections: 0,
                        total_tcp_connections: 0,
                        total_requests: 0,
                        total_bytes_emitted: 0,
                    })
                }),
            )
            .route("/v1/runs/test-run/events", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let obs = OriginObserver::new(&format!("http://127.0.0.1:{port}"));
        let mut events = Vec::new();
        let mut any_failed = false;
        let mut observations_incomplete = false;
        let assert_origin = tuliprox_testkit::config::AssertOrigin { no_evictions: Some(true), ..Default::default() };

        check_origin_assertions(
            "step-1",
            &assert_origin,
            &obs,
            "test-run",
            &mut events,
            &mut any_failed,
            &mut observations_incomplete,
        )
        .await;

        assert!(!any_failed);
        assert!(observations_incomplete);
        assert!(events.iter().any(|(s, e)| *s == "step-1" && *e == "origin_events_unavailable"));
    }

    #[tokio::test]
    async fn check_origin_assertions_eviction_observed_marks_failed() {
        use axum::routing::get;
        let app = Router::new()
            .route(
                "/v1/runs/test-run/stats",
                get(|| async {
                    Json(OriginStats {
                        active_tcp_connections: 0,
                        max_active_tcp_connections: 0,
                        active_body_connections: 0,
                        max_active_body_connections: 0,
                        total_tcp_connections: 0,
                        total_requests: 0,
                        total_bytes_emitted: 0,
                    })
                }),
            )
            .route(
                "/v1/runs/test-run/events",
                get(|| async {
                    Json(vec![OriginEvent::new(
                        1,
                        "test-run",
                        OriginEventKind::EvictionTriggered {
                            evicted_conn_id: 1,
                            evicted_request_id: 1,
                            triggering_conn_id: 2,
                        },
                    )])
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let obs = OriginObserver::new(&format!("http://127.0.0.1:{port}"));
        let mut events = Vec::new();
        let mut any_failed = false;
        let mut observations_incomplete = false;
        let assert_origin = tuliprox_testkit::config::AssertOrigin { no_evictions: Some(true), ..Default::default() };

        check_origin_assertions(
            "step-1",
            &assert_origin,
            &obs,
            "test-run",
            &mut events,
            &mut any_failed,
            &mut observations_incomplete,
        )
        .await;

        assert!(any_failed);
        assert!(!observations_incomplete);
        assert!(events.iter().any(|(s, e)| *s == "step-1" && *e == "origin_eviction_observed"));
    }
}
