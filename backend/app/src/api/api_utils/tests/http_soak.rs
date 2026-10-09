use super::{
    create_test_app_state_for_config, create_test_fingerprint, create_test_provider_app_config,
    force_provider_stream_response, load_test_channel, load_test_rss_kib, load_test_session, load_test_user,
    spawn_load_test_origin, validate_load_response, ForceStreamRequestContext,
};
use crate::model::{ConfigInput, SourcesConfig};
use arc_swap::ArcSwap;
use axum::{
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use http_body_util::BodyExt;
use shared::{model::InputType, utils::Internable};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};

#[derive(Debug, Clone)]
pub(in crate::api::api_utils::tests) struct SoakResourceSample {
    pub(in crate::api::api_utils::tests) elapsed_secs: f64,
    pub(in crate::api::api_utils::tests) rss_kib: Option<u64>,
    pub(in crate::api::api_utils::tests) provider_connections: usize,
    pub(in crate::api::api_utils::tests) lease_starting: usize,
    pub(in crate::api::api_utils::tests) lease_active: usize,
    pub(in crate::api::api_utils::tests) lease_idle: usize,
    pub(in crate::api::api_utils::tests) user_requests: usize,
    pub(in crate::api::api_utils::tests) user_sessions: usize,
    pub(in crate::api::api_utils::tests) shared_origins: usize,
    pub(in crate::api::api_utils::tests) shared_subscribers: usize,
    pub(in crate::api::api_utils::tests) shared_meters: usize,
    pub(in crate::api::api_utils::tests) running_tasks: usize,
    pub(in crate::api::api_utils::tests) completed: u64,
    pub(in crate::api::api_utils::tests) planned_aborted: u64,
    pub(in crate::api::api_utils::tests) unexpected_failed: u64,
}

pub(in crate::api::api_utils::tests) fn median_u64(values: &mut [u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[values.len() / 2])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "30-minute soak: TULIPROX_SOAK_SECS=60 cargo +stable test -p tuliprox --release --bin tuliprox -- --ignored --nocapture http_provider_lease_soak_test"]
#[allow(clippy::too_many_lines)]
async fn http_provider_lease_soak_test() {
    const SOAK_PACKET: [u8; 188] = [0x47; 188];
    let duration_secs: u64 = std::env::var("TULIPROX_SOAK_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(1800);
    let origin_addr = spawn_load_test_origin(&SOAK_PACKET).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 0,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app = create_test_app_state_for_config(Arc::new(config));

    let baseline_rss_kib = load_test_rss_kib();
    let completed = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let aborted = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let unexpected_failed = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let peak_connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let running_tasks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let resource_samples = Arc::new(std::sync::Mutex::new(Vec::<SoakResourceSample>::new()));

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(duration_secs);
    let sampler_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler_stop_task = Arc::clone(&sampler_stop);
    let app_sampler = Arc::clone(&app);
    let sampler_input_name = Arc::clone(&input.name);
    let sampler_completed = Arc::clone(&completed);
    let sampler_aborted = Arc::clone(&aborted);
    let sampler_failed = Arc::clone(&unexpected_failed);
    let sampler_running = Arc::clone(&running_tasks);
    let sampler_samples = Arc::clone(&resource_samples);
    let sampler_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        let start_time = std::time::Instant::now();
        while !sampler_stop_task.load(std::sync::atomic::Ordering::Relaxed) {
            interval.tick().await;
            let rss = load_test_rss_kib().map_or_else(|| "unsupported".to_string(), |rss| rss.to_string());
            let conns = app_sampler.active_provider.get_provider_connections_count();
            let leases = app_sampler.active_provider.provider_lease_usage(&sampler_input_name);
            let (user_requests, user_sessions) = app_sampler.active_users.playback_resource_counts().await;
            let (shared_origins, shared_subscribers) = app_sampler.shared_stream_manager.resource_counts().await;
            let shared_meters = app_sampler.shared_stream_manager.meter_count();
            let sample = SoakResourceSample {
                elapsed_secs: start_time.elapsed().as_secs_f64(),
                rss_kib: load_test_rss_kib(),
                provider_connections: conns,
                lease_starting: leases.starting,
                lease_active: leases.active,
                lease_idle: leases.idle,
                user_requests,
                user_sessions,
                shared_origins,
                shared_subscribers,
                shared_meters,
                running_tasks: sampler_running.load(std::sync::atomic::Ordering::Relaxed),
                completed: sampler_completed.load(std::sync::atomic::Ordering::Relaxed),
                planned_aborted: sampler_aborted.load(std::sync::atomic::Ordering::Relaxed),
                unexpected_failed: sampler_failed.load(std::sync::atomic::Ordering::Relaxed),
            };
            eprintln!(
                "[soak time_series] t={:4.1}s rss_kib={} conns={} starting={} active={} idle={} user_requests={} user_sessions={} shared_origins={} shared_subscribers={} shared_meters={} running_tasks={} completed={} planned_aborted={} unexpected_failed={}",
                sample.elapsed_secs,
                rss,
                sample.provider_connections,
                sample.lease_starting,
                sample.lease_active,
                sample.lease_idle,
                sample.user_requests,
                sample.user_sessions,
                sample.shared_origins,
                sample.shared_subscribers,
                sample.shared_meters,
                sample.running_tasks,
                sample.completed,
                sample.planned_aborted,
                sample.unexpected_failed,
            );
            sampler_samples.lock().unwrap().push(sample);
        }
    });

    let mut tasks = tokio::task::JoinSet::new();
    for task_index in 0..16u16 {
        let app = Arc::clone(&app);
        let input = Arc::clone(&input);
        let completed = Arc::clone(&completed);
        let aborted = Arc::clone(&aborted);
        let unexpected_failed = Arc::clone(&unexpected_failed);
        let peak_connections = Arc::clone(&peak_connections);
        let running_tasks = Arc::clone(&running_tasks);
        tasks.spawn(async move {
            running_tasks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let result = async {
                let addr = SocketAddr::from(([127, 0, 0, 1], 47_000 + task_index));
                let fingerprint = create_test_fingerprint(addr);
                let username = format!("soak-user-{task_index}");
                let user = load_test_user(&username);
                let token = format!("soak-reconnect-family-{task_index}");
                let mut round = 0u64;
                while std::time::Instant::now() < deadline {
                    round += 1;
                    let session = load_test_session(&input, origin_addr, &token, addr);
                    let channel = load_test_channel(&input, origin_addr);
                    let response = force_provider_stream_response(
                        &fingerprint,
                        &app,
                        &session,
                        channel,
                        ForceStreamRequestContext {
                            req_headers: &HeaderMap::new(),
                            input: &input,
                            user: &user,
                            session_reservation_ttl_secs: 2,
                            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
                        },
                        None,
                    )
                    .await
                    .into_response();

                    let status = response.status();
                    if status != StatusCode::OK {
                        unexpected_failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        return Err(format!("soak response status was {status}"));
                    }
                    // Alternate full playback (collect) and abort (drop without consuming).
                    if round.is_multiple_of(2) {
                        let collected = match response.into_body().collect().await {
                            Ok(body) => body.to_bytes(),
                            Err(err) => {
                                unexpected_failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                return Err(format!("soak body collection failed: {err}"));
                            }
                        };
                        if let Err(err) = validate_load_response(status, &collected, &SOAK_PACKET) {
                            unexpected_failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            return Err(err);
                        }
                        completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    } else {
                        drop(response);
                        aborted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }

                    let current = app.active_provider.get_provider_connections_count();
                    peak_connections.fetch_max(current, std::sync::atomic::Ordering::Relaxed);
                }
                Ok::<(), String>(())
            }
            .await;
            running_tasks.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            result
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.expect("soak task must join").unwrap_or_else(|err| panic!("soak task failed: {err}"));
    }

    sampler_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = sampler_task.await;

    let completed = completed.load(std::sync::atomic::Ordering::Relaxed);
    let aborted = aborted.load(std::sync::atomic::Ordering::Relaxed);
    let peak = peak_connections.load(std::sync::atomic::Ordering::Relaxed);
    let unexpected_failed = unexpected_failed.load(std::sync::atomic::Ordering::Relaxed);
    let rss_delta_kib = load_test_rss_kib()
        .zip(baseline_rss_kib)
        .map_or_else(|| "unsupported".to_string(), |(rss, baseline)| rss.saturating_sub(baseline).to_string());
    eprintln!(
        "soak duration={duration_secs}s completed={completed} planned_aborted={aborted} unexpected_failed={unexpected_failed} peak_connections={peak} rss_delta_kib={rss_delta_kib}"
    );

    let samples = Arc::try_unwrap(resource_samples).expect("resource samples unique").into_inner().unwrap();
    assert!(!samples.is_empty(), "soak must record at least one resource sample");
    let third = (samples.len() / 3).max(1);
    let middle_start = third.min(samples.len());
    let middle_end = (third.saturating_mul(2)).min(samples.len());
    let last_start = middle_end.min(samples.len());
    let mut middle_rss =
        samples[middle_start..middle_end].iter().filter_map(|sample| sample.rss_kib).collect::<Vec<_>>();
    let mut last_rss = samples[last_start..].iter().filter_map(|sample| sample.rss_kib).collect::<Vec<_>>();
    match (median_u64(&mut middle_rss), median_u64(&mut last_rss)) {
        (Some(middle), Some(last)) => {
            let delta = i128::from(last) - i128::from(middle);
            eprintln!("soak rss rolling-median middle_third={middle}KiB last_third={last}KiB delta={delta}KiB");
        }
        _ => eprintln!("soak rss rolling-median unsupported on this platform"),
    }

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let leases = app.active_provider.provider_lease_usage(&input.name);
            let (user_requests, user_sessions) = app.active_users.playback_resource_counts().await;
            let (shared_origins, shared_subscribers) = app.shared_stream_manager.resource_counts().await;
            let shared_meters = app.shared_stream_manager.meter_count();
            if app.active_provider.get_provider_connections_count() == 0
                && leases.total() == 0
                && user_requests == 0
                && user_sessions == 0
                && shared_origins == 0
                && shared_subscribers == 0
                && shared_meters == 0
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("soak resources must drain to baseline");

    assert_eq!(
        app.active_provider.get_provider_connections_count(),
        0,
        "provider slots must return to baseline after soak"
    );
    let usage = app.active_provider.provider_lease_usage(&input.name);
    assert_eq!(usage.total(), 0, "lease table must return to baseline after soak");
    assert_eq!(unexpected_failed, 0, "soak must not hide failed operations");
    assert!(peak <= 16, "active connections must stay bounded by the churn workers, observed {peak}");
}
