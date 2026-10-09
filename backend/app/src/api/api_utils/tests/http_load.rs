use super::{
    create_test_app_state_for_config, create_test_fingerprint, create_test_provider_app_config,
    force_provider_stream_response, load_test_channel, load_test_rss_kib, load_test_session, load_test_user,
    spawn_controlled_fake_origin, spawn_load_test_origin, validate_load_response, FakeOriginMode,
    ForceStreamRequestContext,
};
use crate::model::{ConfigInput, SourcesConfig};
use arc_swap::ArcSwap;
use axum::{http::HeaderMap, response::IntoResponse};
use http_body_util::BodyExt;
use shared::{model::InputType, utils::Internable};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};

// Percentiles of a few thousand latency samples tolerate float rounding.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(in crate::api::api_utils::tests) fn load_test_latency_percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "HTTP load test: cargo +stable test -p tuliprox --release --bin tuliprox -- --ignored --nocapture http_provider_lease_load_test"]
#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
async fn http_provider_lease_load_test() {
    const TS_PACKET: [u8; 188] = [0x47; 188];
    let origin_addr = spawn_load_test_origin(&TS_PACKET).await;

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

    // Warm-up so allocator and connection pools are exercised before measurement.
    for index in 0..32u16 {
        let addr = SocketAddr::from(([127, 0, 0, 1], 45_000 + index));
        let fingerprint = create_test_fingerprint(addr);
        let session = load_test_session(&input, origin_addr, &format!("warmup-{index}"), addr);
        let channel = load_test_channel(&input, origin_addr);
        let response = force_provider_stream_response(
            &fingerprint,
            &app,
            &session,
            channel,
            ForceStreamRequestContext {
                req_headers: &HeaderMap::new(),
                input: &input,
                user: &load_test_user(&format!("warmup-user-{index}")),
                session_reservation_ttl_secs: 0,
                content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
            },
            None,
        )
        .await
        .into_response();
        let status = response.status();
        let body = response.into_body().collect().await.expect("warm-up body collects").to_bytes();
        validate_load_response(status, &body, &TS_PACKET).expect("warm-up response must match the fixture");
    }

    let baseline_rss_kib = load_test_rss_kib();
    eprintln!("HTTP provider lease load test (unlimited provider, real HTTP body, 5 repetitions per concurrency)");
    for concurrency in [1usize, 5, 50, 200] {
        let rounds_per_task = 20;
        let repetitions = 5;
        let mut all_samples = Vec::<(u64, u64)>::new();
        let mut total_ops = 0u64;
        let mut total_elapsed_secs = 0f64;
        for rep in 0..repetitions {
            let samples = Arc::new(std::sync::Mutex::new(Vec::<(u64, u64)>::new()));
            let started = std::time::Instant::now();
            let mut tasks = tokio::task::JoinSet::new();
            for task_index in 0..concurrency {
                let app = Arc::clone(&app);
                let input = Arc::clone(&input);
                let samples = Arc::clone(&samples);
                let username = format!("load-user-{rep}-{task_index}");
                tasks.spawn(async move {
                    for round in 0..rounds_per_task {
                        let addr = SocketAddr::from((
                            [127, 0, 0, 1],
                            46_000 + u16::try_from(task_index).expect("load task index fits a port offset"),
                        ));
                        let fingerprint = create_test_fingerprint(addr);
                        let token = format!("load-{rep}-{task_index}-{round}");
                        let session = load_test_session(&input, origin_addr, &token, addr);
                        let channel = load_test_channel(&input, origin_addr);
                        let user = load_test_user(&username);

                        let acquire_at = std::time::Instant::now();
                        let response = force_provider_stream_response(
                            &fingerprint,
                            &app,
                            &session,
                            channel,
                            ForceStreamRequestContext {
                                req_headers: &HeaderMap::new(),
                                input: &input,
                                user: &user,
                                session_reservation_ttl_secs: 0,
                                content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
                            },
                            None,
                        )
                        .await
                        .into_response();
                        let acquire_us = u64::try_from(acquire_at.elapsed().as_micros()).unwrap_or(u64::MAX);

                        let status = response.status();
                        let body = response.into_body();
                        let body_at = std::time::Instant::now();
                        let collected = body.collect().await.expect("load body collects").to_bytes();
                        validate_load_response(status, &collected, &TS_PACKET)
                            .expect("load response must match the TS packet");
                        let body_us = u64::try_from(body_at.elapsed().as_micros()).unwrap_or(u64::MAX);

                        samples.lock().unwrap().push((acquire_us, body_us));
                    }
                });
            }
            while let Some(result) = tasks.join_next().await {
                result.expect("load task must not fail");
            }

            total_elapsed_secs += started.elapsed().as_secs_f64();
            let rep_samples = Arc::try_unwrap(samples).expect("samples unique").into_inner().unwrap();
            total_ops += rep_samples.len() as u64;
            all_samples.extend(rep_samples);
        }

        let mut acquire = Vec::with_capacity(all_samples.len());
        let mut body = Vec::with_capacity(all_samples.len());
        for (a, b) in all_samples {
            acquire.push(a);
            body.push(b);
        }
        acquire.sort_unstable();
        body.sort_unstable();
        let throughput = total_ops as f64 / total_elapsed_secs.max(f64::EPSILON);
        let byte_throughput = total_ops as f64 * TS_PACKET.len() as f64 / total_elapsed_secs.max(f64::EPSILON);
        let rss_delta_kib = load_test_rss_kib()
            .zip(baseline_rss_kib)
            .map_or_else(|| "unsupported".to_string(), |(rss, baseline)| rss.saturating_sub(baseline).to_string());
        eprintln!(
            "concurrency={concurrency:>3} ops={total_ops:>5} throughput={throughput:>9.1} ops/s {byte_throughput:>11.1} bytes/s | acquire p50/p95/p99={}/{}/{}us | body p50/p95/p99={}/{}/{}us | rss_delta_kib={rss_delta_kib}",
            load_test_latency_percentile(&acquire, 0.50),
            load_test_latency_percentile(&acquire, 0.95),
            load_test_latency_percentile(&acquire, 0.99),
            load_test_latency_percentile(&body, 0.50),
            load_test_latency_percentile(&body, 0.95),
            load_test_latency_percentile(&body, 0.99),
        );
        let (user_requests, user_sessions) = app.active_users.playback_resource_counts().await;
        let (shared_origins, shared_subscribers) = app.shared_stream_manager.resource_counts().await;
        let shared_meters = app.shared_stream_manager.meter_count();
        let leases = app.active_provider.provider_lease_usage(&input.name);
        eprintln!(
            "[load resource] concurrency={concurrency} conns={} starting={} active={} idle={} user_requests={user_requests} user_sessions={user_sessions} shared_origins={shared_origins} shared_subscribers={shared_subscribers} shared_meters={shared_meters}",
            app.active_provider.get_provider_connections_count(),
            leases.starting,
            leases.active,
            leases.idle,
        );
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
    .expect("load resources must drain to baseline");
    assert_eq!(app.active_provider.get_provider_connections_count(), 0, "provider slots must return to baseline");
    let usage = app.active_provider.provider_lease_usage(&input.name);
    assert_eq!(usage.total(), 0, "lease table must return to baseline after HTTP churn");
}

#[tokio::test]
async fn http_provider_lease_load_test_negative_control_detects_payload_corruption() {
    const TS_PACKET_VALID: [u8; 188] = [0x47; 188];
    const TS_PACKET_CORRUPT: [u8; 188] = [0; 188];

    let (origin_addr, origin_task) =
        spawn_controlled_fake_origin(FakeOriginMode::FixedBytes(TS_PACKET_CORRUPT.to_vec()), 1).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_neg_ctrl".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 1,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app = create_test_app_state_for_config(Arc::new(config));

    let addr = SocketAddr::from(([127, 0, 0, 1], 55_650));
    let fingerprint = create_test_fingerprint(addr);
    let session = load_test_session(&input, origin_addr, "neg-session", addr);
    let channel = load_test_channel(&input, origin_addr);
    let user = load_test_user("neg_ctrl_user");

    let response = force_provider_stream_response(
        &fingerprint,
        &app,
        &session,
        channel,
        ForceStreamRequestContext {
            req_headers: &HeaderMap::new(),
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 0,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    let status = response.status();
    let body = response.into_body().collect().await.expect("body collects").to_bytes();
    assert!(
        validate_load_response(status, &body, &TS_PACKET_VALID).is_err(),
        "negative control must be rejected by the same validator as the load harness"
    );

    let _ = origin_task.await;
}
