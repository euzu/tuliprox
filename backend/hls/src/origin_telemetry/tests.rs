use super::{
    aggregate::OriginAggregates, FetchProbe, OriginFetchOutcome, OriginFetchPhaseError, OriginFetchRecord,
    OriginResourceKind, OriginTelemetryRecorder,
};
use std::time::Duration;
use tokio::sync::mpsc::Receiver;
use tuliprox_core::model::HlsOriginTelemetryConfig;

fn config(queue_capacity: usize) -> HlsOriginTelemetryConfig {
    HlsOriginTelemetryConfig { enabled: true, queue_capacity, freshness_secs: 60, window_secs: 300 }
}

fn new_recorder(queue_capacity: usize) -> (OriginTelemetryRecorder, Receiver<OriginFetchRecord>) {
    OriginTelemetryRecorder::with_channel(&config(queue_capacity))
}

fn probe(recorder: &OriginTelemetryRecorder, key: &str, attempt_no: u32) -> FetchProbe {
    recorder.begin("provider-a", OriginResourceKind::Segment, Some(key), attempt_no)
}

fn sample(input: &str, ts: u64, outcome: OriginFetchOutcome, network_ms: Option<u64>) -> OriginFetchRecord {
    OriginFetchRecord {
        seq: 0,
        ts_utc_ms: ts,
        input_name: input.to_string(),
        resource_kind: OriginResourceKind::Segment,
        segment_id: None,
        attempt_id: 0,
        attempt_no: 0,
        http_status: Some(200),
        bytes_read: 125_000,
        content_length: None,
        media_duration_ms: None,
        queue_wait_ms: 5,
        headers_ms: Some(10),
        first_byte_ms: Some(20),
        body_complete_ms: network_ms,
        network_ms,
        cache_write_ms: None,
        postprocess_ms: None,
        cache_ready_ms: None,
        total_ms: network_ms.unwrap_or(10_000),
        outcome,
        error_phase: None,
    }
}

#[tokio::test(start_paused = true)]
async fn phases_are_split() {
    let (recorder, mut rx) = new_recorder(16);
    let mut probe = probe(&recorder, "k", 0);
    probe.set_queue_wait(Duration::from_millis(7));
    tokio::time::advance(Duration::from_millis(30)).await;
    probe.mark_headers(200, Some(100));
    tokio::time::advance(Duration::from_millis(20)).await;
    probe.add_bytes(40);
    tokio::time::advance(Duration::from_millis(50)).await;
    probe.add_bytes(60);
    probe.mark_body_complete();
    probe.start_cache_write();
    tokio::time::advance(Duration::from_millis(15)).await;
    probe.end_cache_write();
    probe.start_postprocess();
    tokio::time::advance(Duration::from_millis(5)).await;
    probe.end_postprocess();
    probe.mark_cache_ready();
    probe.set_media_duration_ms(6000);
    probe.finish(OriginFetchOutcome::Ok, None);

    let record = rx.try_recv().expect("record");
    assert_eq!(record.queue_wait_ms, 7);
    assert_eq!(record.headers_ms, Some(30));
    assert_eq!(record.first_byte_ms, Some(50));
    assert_eq!(record.body_complete_ms, Some(100));
    assert_eq!(record.network_ms, Some(100));
    assert_eq!(record.cache_write_ms, Some(15));
    assert_eq!(record.postprocess_ms, Some(5));
    assert_eq!(record.cache_ready_ms, Some(120));
    assert_eq!(record.bytes_read, 100);
    assert_eq!(record.http_status, Some(200));
    assert_eq!(record.content_length, Some(100));
    assert_eq!(record.media_duration_ms, Some(6000));
    assert_eq!(record.outcome, OriginFetchOutcome::Ok);
    assert_eq!(record.error_phase, None);
}

#[tokio::test]
async fn queue_full_counts_drops_without_blocking() {
    let (recorder, mut rx) = new_recorder(16);
    for i in 0..20 {
        probe(&recorder, &format!("k{i}"), 0).finish(OriginFetchOutcome::Ok, None);
    }
    assert_eq!(recorder.dropped_events(), 4);
    let mut received = 0;
    while rx.try_recv().is_ok() {
        received += 1;
    }
    assert_eq!(received, 16);
    assert_eq!(recorder.snapshot_at(0).map(|s| s.dropped_events), Some(4));
}

#[tokio::test]
async fn drop_without_finish_is_cancelled() {
    let (recorder, mut rx) = new_recorder(16);
    drop(probe(&recorder, "k", 0));
    let record = rx.try_recv().expect("record");
    assert_eq!(record.outcome, OriginFetchOutcome::Cancelled);
    assert_eq!(record.error_phase, Some(OriginFetchPhaseError::Connect));

    let mut mid_body = probe(&recorder, "k", 0);
    mid_body.mark_headers(200, None);
    drop(mid_body);
    let record = rx.try_recv().expect("record");
    assert_eq!(record.outcome, OriginFetchOutcome::Cancelled);
    assert_eq!(record.error_phase, Some(OriginFetchPhaseError::Body));
}

#[tokio::test]
async fn retries_share_segment_id_with_distinct_attempt_ids() {
    let (recorder, mut rx) = new_recorder(16);
    probe(&recorder, "https://origin.example/secret/seg1.ts?token=abc", 0)
        .finish(OriginFetchOutcome::HttpError, Some(OriginFetchPhaseError::Headers));
    probe(&recorder, "https://origin.example/secret/seg1.ts?token=abc", 1).finish(OriginFetchOutcome::Ok, None);
    let first = rx.try_recv().expect("first");
    let second = rx.try_recv().expect("second");
    assert_eq!(first.segment_id, second.segment_id);
    assert_ne!(first.attempt_id, second.attempt_id);
    assert_eq!((first.attempt_no, second.attempt_no), (0, 1));
    assert_eq!(first.error_phase, Some(OriginFetchPhaseError::Headers));
    assert!(second.seq > first.seq);
}

#[tokio::test]
async fn segment_id_is_opaque() {
    let (recorder, mut rx) = new_recorder(16);
    let key = "https://origin.example/live/user:pass/seg1.ts";
    probe(&recorder, key, 0).finish(OriginFetchOutcome::Ok, None);
    let record = rx.try_recv().expect("record");
    let id = record.segment_id.clone().expect("segment id");
    assert_eq!(id.len(), 16);
    assert!(!key.contains(&id) && !id.contains("origin") && !id.contains("seg1"));
    let json = serde_json::to_string(&record).expect("json");
    assert!(!json.contains("origin.example") && !json.contains("user:pass") && !json.contains("://"));
    // A second recorder uses a different key, so ids are not comparable across processes.
    let (other, mut other_rx) = new_recorder(16);
    probe(&other, key, 0).finish(OriginFetchOutcome::Ok, None);
    assert_ne!(other_rx.try_recv().expect("record").segment_id.expect("id"), id);
}

#[tokio::test]
async fn disabled_recorder_is_noop() {
    let recorder = OriginTelemetryRecorder::start(None);
    assert!(!recorder.is_enabled());
    let mut probe = recorder.begin("a", OriginResourceKind::Key, None, 0);
    probe.mark_headers(200, None);
    probe.add_bytes(10);
    probe.finish(OriginFetchOutcome::Ok, None);
    drop(FetchProbe::default());
    assert_eq!(recorder.dropped_events(), 0);
    assert!(recorder.snapshot().is_none());

    let mut cfg = config(16);
    cfg.enabled = false;
    assert!(!OriginTelemetryRecorder::start(Some(&cfg)).is_enabled());
}

#[tokio::test]
async fn started_recorder_aggregates_records() {
    let recorder = OriginTelemetryRecorder::start(Some(&config(16)));
    recorder.begin("a", OriginResourceKind::Segment, Some("k"), 0).finish(OriginFetchOutcome::HttpError, None);
    for _ in 0..50 {
        tokio::task::yield_now().await;
        if recorder.snapshot().is_some_and(|s| !s.inputs.is_empty()) {
            break;
        }
    }
    let snapshot = recorder.snapshot().expect("snapshot");
    assert_eq!(snapshot.scope, "source");
    assert_eq!(snapshot.queue_capacity, 16);
    assert_eq!(snapshot.inputs[0].availability.http_error, 1);
}

#[test]
fn empty_window_serializes_null_quality() {
    let mut aggregates = OriginAggregates::new(300, 60);
    let snapshot = aggregates.snapshot(1_000, 0, 16);
    assert!(snapshot.inputs.is_empty());

    // Only failures: quality has no completed transfer, so it is null, not 0.
    aggregates.record(&sample("a", 1_000, OriginFetchOutcome::Timeout, None));
    let snapshot = aggregates.snapshot(2_000, 0, 16);
    let json = serde_json::to_value(&snapshot).expect("json");
    let quality = &json["inputs"][0]["transfer_quality"];
    assert_eq!(quality["sample_count"], 0);
    assert!(quality["network_ms_p95"].is_null());
    assert!(quality["throughput_kbps"].is_null());
    assert_eq!(json["inputs"][0]["availability"]["sample_count"], 1);
    assert_eq!(json["inputs"][0]["availability"]["timeout"], 1);
}

#[test]
fn stale_window_serializes_null_quality() {
    let mut aggregates = OriginAggregates::new(300, 60);
    aggregates.record(&sample("a", 0, OriginFetchOutcome::Ok, Some(100)));
    let fresh = aggregates.snapshot(10_000, 0, 16);
    let quality = &fresh.inputs[0].transfer_quality;
    assert_eq!(quality.network_ms_p50, Some(100));
    assert_eq!(quality.throughput_kbps, Some(10_000.0));
    assert_eq!(quality.last_sample_age_ms, Some(10_000));

    let stale = aggregates.snapshot(61_000, 0, 16);
    let quality = &stale.inputs[0].transfer_quality;
    assert_eq!(quality.sample_count, 1);
    assert!(quality.network_ms_p50.is_none() && quality.throughput_kbps.is_none());
    // Availability still reports the sample.
    assert_eq!(stale.inputs[0].availability.ok, 1);

    // Outside the window the input disappears.
    assert!(aggregates.snapshot(301_000, 0, 16).inputs.is_empty());
}

#[test]
fn windows_are_bounded_and_percentiles_use_nearest_rank() {
    let mut aggregates = OriginAggregates::new(300, 60);
    for i in 1..=100 {
        aggregates.record(&sample("a", 1_000, OriginFetchOutcome::Ok, Some(i)));
    }
    let snapshot = aggregates.snapshot(1_000, 0, 16);
    let quality = &snapshot.inputs[0].transfer_quality;
    assert_eq!(quality.network_ms_p50, Some(50));
    assert_eq!(quality.network_ms_p95, Some(95));

    for _ in 0..10_000 {
        aggregates.record(&sample("a", 1_000, OriginFetchOutcome::Ok, Some(1)));
    }
    assert!(aggregates.snapshot(1_000, 0, 16).inputs[0].availability.sample_count <= 4096);
}

#[test]
fn failed_attempts_stay_visible_without_a_completed_transfer() {
    let mut aggregates = OriginAggregates::new(300, 60);
    aggregates.record(&sample("a", 1_000, OriginFetchOutcome::Timeout, None));
    aggregates.record(&sample("a", 1_500, OriginFetchOutcome::Cancelled, None));
    let snapshot = aggregates.snapshot(2_000, 0, 16);
    let quality = &snapshot.inputs[0].transfer_quality;
    assert!(quality.network_ms_p95.is_none());
    // The timeout counts with its full duration, the cancelled attempt does not.
    assert_eq!(quality.attempt_ms_p95_incl_failed, Some(10_000));

    // Stale failures go back to unknown.
    let stale = aggregates.snapshot(62_000, 0, 16);
    assert!(stale.inputs[0].transfer_quality.attempt_ms_p95_incl_failed.is_none());
}

#[test]
fn cache_side_times_are_reported_apart_from_network_time() {
    let mut aggregates = OriginAggregates::new(300, 60);
    let mut record = sample("a", 1_000, OriginFetchOutcome::Ok, Some(100));
    record.cache_write_ms = Some(40);
    record.postprocess_ms = Some(7);
    record.cache_ready_ms = Some(150);
    aggregates.record(&record);
    let snapshot = aggregates.snapshot(2_000, 0, 16);
    let quality = &snapshot.inputs[0].transfer_quality;
    assert_eq!(quality.network_ms_p95, Some(100));
    assert_eq!(quality.cache_write_ms_p95, Some(40));
    assert_eq!(quality.postprocess_ms_p95, Some(7));
    assert_eq!(quality.cache_ready_ms_p95, Some(150));
}

#[test]
fn capped_window_reports_the_time_it_really_covers() {
    let mut aggregates = OriginAggregates::new(300, 60);
    for i in 0..5_000u64 {
        aggregates.record(&sample("a", 100_000 + i, OriginFetchOutcome::Ok, Some(1)));
    }
    let snapshot = aggregates.snapshot(105_000, 0, 16);
    let availability = &snapshot.inputs[0].availability;
    assert!(availability.window_covered_ms.expect("covered") < availability.window_secs * 1000);
}

#[test]
fn max_queue_capacity_matches_tokio_channel_limit() {
    assert_eq!(shared::model::HLS_ORIGIN_TELEMETRY_MAX_QUEUE_CAPACITY, tokio::sync::Semaphore::MAX_PERMITS);
}
