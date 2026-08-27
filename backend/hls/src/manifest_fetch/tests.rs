//! Tests for origin-manifest fetching.
//!
//! These drive the fetch and the recovery chain end to end, so they stay with
//! the orchestration in `super` rather than with the individual steps in
//! `error`, `http`, `fingerprint`, `episode`, `recovery`, `quality` and
//! `selection_log`.

use super::{
    episode::candidate_resource_timeline_evidence,
    error::{HlsManifestRejectLogReason, OriginManifestFetchError},
    fingerprint::{
        build_manifest_timeline_fingerprint, deterministic_conflict_fingerprint,
        deterministic_timeline_conflict_from_rejection,
    },
    http::{
        fetch_origin_manifest_once, origin_manifest_content_coding_error, origin_manifest_fetch_error_from_io_error,
        origin_manifest_fetch_error_from_request_error, request_failed_status_from_message,
    },
    recovery::{acceptance_attempt_may_start, attempt_limit_for_started_requalification, selected_manifest_candidate},
    selection_log::ManifestRetryLogKind,
    FetchedOriginManifest, HlsManifestFetchSelection, RetryPolicy, TimelineMapError, MAX_HLS_MANIFEST_BYTES,
};
use crate::{
    manifest_acceptance::{
        HlsEmergencyLiveHandoffCompatibility, HlsManifestCommitKind, HlsManifestCommitPlan,
        HlsResourceTimelineEvidence, HlsTerminalAlternativeCompatibility,
    },
    recovery_timing::HlsAcceptanceDeadlineMs,
    resource_identity::{HlsMediaResourceIdentity, HlsMediaResourceSemanticKey},
    timeline::HlsResourceReplayDecision,
};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use flate2::{
    write::{DeflateEncoder, GzEncoder},
    Compression,
};
use reqwest::{redirect::Policy, Client};
use std::{io, io::Write, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
};
use tuliprox_core::utils::content_coding::{ContentCoding, ContentCodingError};
use tuliprox_parser::hls::origin_manifest::OriginManifestParseOutcome;
use url::Url;

const TEST_MANIFEST: &[u8] = b"#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\nsegment.ts\n";

#[test]
fn hls_recovery_timing_deadline_never_abandons_started_full_burst_but_blocks_late_follow_ups() {
    let deadline = HlsAcceptanceDeadlineMs::from_millis_since_epoch;
    assert!(acceptance_attempt_may_start(true, 1_501, 500, deadline(1_000)));

    let reduced_retry_before_deadline = acceptance_attempt_may_start(false, 1_000, 499, deadline(1_500));
    let reduced_retry_at_deadline = acceptance_attempt_may_start(false, 1_000, 500, deadline(1_500));
    let requalification_after_deadline = acceptance_attempt_may_start(false, 1_501, 0, deadline(1_500));
    let saturated_follow_up = acceptance_attempt_may_start(false, u64::MAX - 5, 10, deadline(u64::MAX));

    assert!(reduced_retry_before_deadline);
    assert!(!reduced_retry_at_deadline);
    assert!(!requalification_after_deadline);
    assert!(!saturated_follow_up);
}

#[test]
fn requalification_in_last_retry_slot_reserves_exactly_one_mandatory_full_burst() {
    assert_eq!(attempt_limit_for_started_requalification(5, 1), 5);
    assert_eq!(attempt_limit_for_started_requalification(5, 4), 6);
    assert_eq!(attempt_limit_for_started_requalification(usize::MAX, usize::MAX), usize::MAX);
}

#[test]
fn encrypted_normal_candidate_is_not_critical_emergency_handoff_evidence() {
    let encrypted =
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4,\nsegment.ts\n";
    let (_, has_switch_segment, evidence) =
        build_manifest_timeline_fingerprint(encrypted, "http://origin.example/live/index.m3u8");

    assert!(has_switch_segment);
    assert_eq!(evidence.live_handoff, HlsEmergencyLiveHandoffCompatibility::Incompatible);
    assert_eq!(evidence.terminal_alternative, HlsTerminalAlternativeCompatibility::TerminalTailPreferred);
}

#[test]
fn stage_alternative_is_forwarded_to_commit_callback_selection() {
    let plan = HlsManifestCommitPlan::StageAlternative {
        candidate_index: 7,
        kind: HlsManifestCommitKind::AlternativeAsNewEpoch,
    };

    assert_eq!(selected_manifest_candidate(plan), Some((7, HlsManifestCommitKind::AlternativeAsNewEpoch)));
}

#[test]
fn timeline_fingerprint_is_structured_and_ignores_origin_host_and_query_tokens() {
    let manifest_a = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:7\n\
        #EXT-X-PROGRAM-DATE-TIME:2026-07-16T10:00:00Z\n#EXTINF:4,\n\
        https://origin-a.example/live/7.ts?token=secret-a\n#EXT-X-DISCONTINUITY\n#EXTINF:4,\n\
        https://origin-a.example/live/8.ts?token=secret-a\n";
    let manifest_b = manifest_a
        .replace("origin-a.example", "origin-b.example")
        .replace("secret-a", "secret-b")
        .replace("#EXT-X-TARGETDURATION:4", "#EXT-X-TARGETDURATION:9");

    let (fingerprint_a, has_media_a, _) =
        build_manifest_timeline_fingerprint(manifest_a, "https://origin-a.example/live/index.m3u8");
    let (fingerprint_b, has_media_b, _) =
        build_manifest_timeline_fingerprint(&manifest_b, "https://origin-b.example/live/index.m3u8");

    assert!(has_media_a);
    assert!(has_media_b);
    assert_eq!(fingerprint_a, fingerprint_b);
    assert_eq!(fingerprint_a.segment_count, 2);
    assert_eq!(fingerprint_a.first_program_date_time_ms, Some(1_784_196_000_000));
    assert!(fingerprint_a.segment_samples[1].discontinuity_before);
}

#[test]
fn rotating_volatile_parent_has_one_semantic_conflict_fingerprint() {
    let body_a = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:490\n#EXTINF:4,\n\
        /stream/0123456789abcdef/1745190_490.ts\n#EXTINF:4,\n\
        /stream/0123456789abcdef/1745180_480.ts\n#EXTINF:4,\n\
        /stream/0123456789abcdef/1745191_491.ts\n";
    let body_b = body_a.replace("0123456789abcdef", "fedcba9876543210");
    let parsed_a = match tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(
        body_a,
        "https://origin-a.example/live/index.m3u8",
    ) {
        OriginManifestParseOutcome::Normal(manifest) => manifest,
        OriginManifestParseOutcome::TransientPassthrough { .. } => panic!("normal manifest expected"),
    };
    let parsed_b = match tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(
        &body_b,
        "https://origin-b.example/live/index.m3u8",
    ) {
        OriginManifestParseOutcome::Normal(manifest) => manifest,
        OriginManifestParseOutcome::TransientPassthrough { .. } => panic!("normal manifest expected"),
    };

    assert_eq!(
        deterministic_conflict_fingerprint(&parsed_a, body_a, "https://origin-a.example/live/index.m3u8"),
        deterministic_conflict_fingerprint(&parsed_b, &body_b, "https://origin-b.example/live/index.m3u8")
    );
    assert_ne!(
        build_manifest_timeline_fingerprint(body_a, "https://origin-a.example/live/index.m3u8").0,
        build_manifest_timeline_fingerprint(&body_b, "https://origin-b.example/live/index.m3u8").0,
        "ordinary origin-acceptance fingerprint remains exact-path based"
    );
}

#[test]
fn different_stream_namespace_is_not_the_same_conflict() {
    let body_a = "#EXTM3U\n#EXTINF:4,\n/stream-a/0123456789abcdef/1745180_480.ts\n";
    let body_b = body_a.replace("stream-a", "stream-b");
    let parse = |body: &str| match tuliprox_parser::hls::origin_manifest::parse_origin_media_manifest(
        body,
        "https://origin.example/live/index.m3u8",
    ) {
        OriginManifestParseOutcome::Normal(manifest) => manifest,
        OriginManifestParseOutcome::TransientPassthrough { .. } => panic!("normal manifest expected"),
    };

    assert_ne!(
        deterministic_conflict_fingerprint(&parse(body_a), body_a, "https://origin.example/live/index.m3u8",),
        deterministic_conflict_fingerprint(&parse(&body_b), &body_b, "https://origin.example/live/index.m3u8",)
    );
}

#[test]
fn resource_timeline_evidence_rejects_replay_after_new_even_with_forward_sequence() {
    let published = HlsMediaResourceIdentity::from_url("https://old.example/live/484.ts", None);
    let replay_only = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:490\n#EXTINF:4,\n484.ts\n";
    let prefix_then_new = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:490\n#EXTINF:4,\n484.ts\n#EXTINF:4,\n490.ts\n";
    let new_then_replay = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:490\n#EXTINF:4,\n490.ts\n#EXTINF:4,\n484.ts\n";
    let fingerprint = |body| build_manifest_timeline_fingerprint(body, "https://new.example/live/index.m3u8").0;

    assert_eq!(
        candidate_resource_timeline_evidence(&fingerprint(replay_only), &[published]),
        HlsResourceTimelineEvidence::ReplayOnly
    );
    assert_eq!(
        candidate_resource_timeline_evidence(&fingerprint(prefix_then_new), &[published]),
        HlsResourceTimelineEvidence::Eligible
    );
    assert_eq!(
        candidate_resource_timeline_evidence(&fingerprint(new_then_replay), &[published]),
        HlsResourceTimelineEvidence::ContradictoryOrder
    );
}

#[test]
fn resource_replay_diagnostic_is_bounded_and_contains_decision_evidence() {
    let reason = HlsManifestRejectLogReason::from(TimelineMapError::PublishedResourceReplay {
        previous_proxy_tail: Some(23),
        existing_proxy_seq: 17,
        candidate_position: 2,
        candidate_origin_seq: 490,
        resource_key: HlsMediaResourceSemanticKey::for_test([0xab; 32]),
        decision: HlsResourceReplayDecision::RejectContradictoryOrder,
    })
    .status_label();

    assert!(reason.contains("previous_proxy_tail=23"));
    assert!(reason.contains("candidate_position=2"));
    assert!(reason.contains("repeated_resource=abababababababab"));
    assert!(reason.contains("decision=reject-contradictory-order"));
    assert!(!reason.contains("http"));
}

#[test]
fn deterministic_conflict_rejects_matching_log_token_with_different_full_semantic_key() {
    let fetched = FetchedOriginManifest {
        body: "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:490\n\
               #EXTINF:4,\n490.ts\n#EXTINF:4,\n480.ts\n#EXTINF:4,\n491.ts\n"
            .to_string(),
        final_manifest_url: "https://origin.example/live/index.m3u8".to_string(),
        resolved_request_url: "https://origin.example/live/index.m3u8".to_string(),
        redirect_host: None,
        provider_url_index: None,
        provider_session_headers: HeaderMap::new(),
        status: StatusCode::OK,
        attempts: 1,
        candidate_requests: 1,
        selection: HlsManifestFetchSelection::Initial,
    };
    let actual_key = HlsMediaResourceIdentity::from_url("https://origin.example/live/480.ts", None).semantic_key();
    let mut different_bytes = actual_key.bytes();
    different_bytes[31] ^= 0xff;
    let different_key = HlsMediaResourceSemanticKey::for_test(different_bytes);
    assert_eq!(actual_key.diagnostic_token(), different_key.diagnostic_token());
    assert_ne!(actual_key, different_key);

    let reason = HlsManifestRejectLogReason::PublishedResourceReplay {
        previous_proxy_tail: Some(2),
        existing_proxy_seq: 0,
        candidate_position: 1,
        candidate_origin_seq: 480,
        resource_key: different_key,
        decision: HlsResourceReplayDecision::RejectContradictoryOrder,
    };
    assert!(deterministic_timeline_conflict_from_rejection(&fetched, &reason).is_none());
}

#[test]
fn timeline_fingerprint_distinguishes_technical_state_and_keeps_compatible_aes_media_stageable() {
    let clear_ts = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:4,\n1.ts\n";
    let discontinuous_ts = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXT-X-DISCONTINUITY\n#EXTINF:4,\n1.ts\n";
    let encrypted_ts = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n\
        #EXT-X-KEY:METHOD=AES-128,URI=\"https://keys.example/key.bin?token=secret\"\n#EXTINF:4,\n1.ts\n";
    let mapped = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n\
        #EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4,\n1.m4s\n";

    let (clear, _, _) = build_manifest_timeline_fingerprint(clear_ts, "https://origin.example/live/index.m3u8");
    let (discontinuous, _, _) =
        build_manifest_timeline_fingerprint(discontinuous_ts, "https://origin.example/live/index.m3u8");
    let (encrypted, encrypted_has_media, _) =
        build_manifest_timeline_fingerprint(encrypted_ts, "https://origin.example/live/index.m3u8");
    let (mapped, _, _) = build_manifest_timeline_fingerprint(mapped, "https://origin.example/live/index.m3u8");

    assert_ne!(clear.discontinuity_pattern_hash, discontinuous.discontinuity_pattern_hash);
    assert_ne!(clear.map_and_encryption_hash, encrypted.map_and_encryption_hash);
    assert_ne!(clear.map_and_encryption_hash, mapped.map_and_encryption_hash);
    assert_ne!(clear.container_signature_hash, mapped.container_signature_hash);
    assert!(encrypted_has_media);
}

struct TestOriginResponse {
    status: &'static str,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
    body_delay: Duration,
}

impl TestOriginResponse {
    fn ok(body: Vec<u8>) -> Self { Self { status: "200 OK", headers: Vec::new(), body, body_delay: Duration::ZERO } }

    fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

async fn spawn_test_origin(response: TestOriginResponse) -> (Url, oneshot::Receiver<String>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind test origin");
    let address = listener.local_addr().expect("test origin address");
    let (request_sender, request_receiver) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept test request");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = socket.read(&mut buffer).await.expect("read test request");
            if count == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..count]);
        }
        let _ = request_sender.send(String::from_utf8_lossy(&request).into_owned());

        let mut response_head =
            format!("HTTP/1.1 {}\r\nConnection: close\r\nContent-Length: {}\r\n", response.status, response.body.len());
        for (name, value) in response.headers {
            response_head.push_str(name);
            response_head.push_str(": ");
            response_head.push_str(&value);
            response_head.push_str("\r\n");
        }
        response_head.push_str("\r\n");
        socket.write_all(response_head.as_bytes()).await.expect("write test response headers");
        if !response.body_delay.is_zero() {
            tokio::time::sleep(response.body_delay).await;
        }
        let _ = socket.write_all(&response.body).await;
    });
    (Url::parse(&format!("http://{address}/manifest.m3u8")).expect("test origin URL"), request_receiver)
}

fn gzip(input: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(input).expect("gzip input");
    encoder.finish().expect("finish gzip")
}

fn raw_deflate(input: &[u8]) -> Vec<u8> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(input).expect("raw-deflate input");
    encoder.finish().expect("finish raw-deflate")
}

fn brotli(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    {
        let mut encoder = brotli::CompressorWriter::new(&mut output, 4096, 5, 22);
        encoder.write_all(input).expect("brotli input");
    }
    output
}

async fn zstd(input: &[u8]) -> Vec<u8> {
    let (writer, mut reader) = tokio::io::duplex(64 * 1024);
    let input = input.to_vec();
    let encoder_task = tokio::spawn(async move {
        let mut encoder = async_compression::tokio::write::ZstdEncoder::new(writer);
        encoder.write_all(&input).await.expect("zstd input");
        encoder.shutdown().await.expect("finish zstd");
    });
    let mut output = Vec::new();
    reader.read_to_end(&mut output).await.expect("read zstd output");
    encoder_task.await.expect("join zstd encoder");
    output
}

fn captured_header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request.lines().skip(1).find_map(|line| {
        let (header_name, value) = line.split_once(':')?;
        header_name.eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

fn test_clients() -> (Client, Client) {
    let client = Client::builder().build().expect("test client");
    let no_redirect_client = Client::builder().redirect(Policy::none()).build().expect("no-redirect client");
    (client, no_redirect_client)
}

#[test]
fn request_failed_status_is_extracted_from_global_provider_policy_error() {
    assert_eq!(
        request_failed_status_from_message(
            "Request failed (407 Proxy Authentication Required): provider://demo/live/u/p/1.m3u8",
        ),
        Some(StatusCode::PROXY_AUTHENTICATION_REQUIRED)
    );
}

#[test]
fn initial_manifest_retry_delay_uses_next_slot_bounded_jitter_and_retry_after_override() {
    let retry_policy = RetryPolicy { delays_ms: [0, 100, 250, 500, 750], jitter_max_ms: 25 };

    assert_eq!(super::next_retry_delay_ms(&retry_policy, 0, None, 17), 117);
    assert_eq!(super::next_retry_delay_ms(&retry_policy, 0, None, 100), 125);
    assert_eq!(super::next_retry_delay_ms(&retry_policy, 1, None, 10), 260);
    assert_eq!(super::next_retry_delay_ms(&retry_policy, 0, Some(3_000), 25), 3_000);
}

#[test]
fn initial_manifest_retry_status_is_not_labeled_as_recovery() {
    let error = OriginManifestFetchError::Timeout;

    let initial =
        super::selection_log::manifest_retry_status_label(ManifestRetryLogKind::InitialFetch, None, Some(&error));
    let recovery =
        super::selection_log::manifest_retry_status_label(ManifestRetryLogKind::PinnedHostRecovery, None, Some(&error));

    assert!(initial.starts_with("initial-fetch error="));
    assert!(!initial.contains("pinned-host-recovery"));
    assert!(recovery.starts_with("pinned-host-recovery error="));
}

#[test]
fn manifest_content_coding_log_labels_never_include_origin_controlled_details() {
    let unsupported =
        OriginManifestFetchError::ContentCoding(ContentCodingError::Unsupported("signed-token-secret".to_string()));
    assert_eq!(unsupported.log_label(), "content_coding class=unsupported");
    assert!(!unsupported.log_label().contains("signed-token-secret"));

    let decoding = OriginManifestFetchError::ContentDecoding { coding: ContentCoding::Brotli };
    assert_eq!(decoding.log_label(), "content_decoding coding=br");
}

#[test]
fn request_failed_407_maps_to_retryable_manifest_status() {
    let err = origin_manifest_fetch_error_from_request_error(
        &"Request failed (407 Proxy Authentication Required): provider://demo/live/u/p/1.m3u8",
    );
    assert!(matches!(err, OriginManifestFetchError::RetryableStatus(StatusCode::PROXY_AUTHENTICATION_REQUIRED, None)));
}

#[test]
fn request_failed_retryable_statuses_map_to_retryable_manifest_status() {
    for (message, expected) in [
        ("Request failed (429 Too Many Requests): http://example.test/live.m3u8", StatusCode::TOO_MANY_REQUESTS),
        (
            "Request failed (500 Internal Server Error): http://example.test/live.m3u8",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ] {
        let err = origin_manifest_fetch_error_from_request_error(&message);
        assert!(matches!(err, OriginManifestFetchError::RetryableStatus(status, None) if status == expected));
    }
}

#[test]
fn request_failed_404_maps_to_permanent_manifest_status() {
    let err = origin_manifest_fetch_error_from_request_error(
        &"Request failed (404 Not Found): http://example.test/live.m3u8",
    );
    assert!(matches!(err, OriginManifestFetchError::PermanentStatus(StatusCode::NOT_FOUND)));
}

#[test]
fn transport_error_without_http_status_stays_request_error() {
    let err = origin_manifest_fetch_error_from_request_error(&"error sending request for url");
    assert!(matches!(err, OriginManifestFetchError::Request(message) if message == "error sending request for url"));
}

#[test]
fn io_timeout_maps_to_structured_manifest_timeout() {
    let error = io::Error::new(io::ErrorKind::TimedOut, "origin body timed out");
    assert!(matches!(origin_manifest_fetch_error_from_io_error(&error), OriginManifestFetchError::Timeout));
    let prefix_timeout =
        ContentCodingError::PrefixRead(io::Error::new(io::ErrorKind::TimedOut, "origin prefix timed out"));
    assert!(matches!(origin_manifest_content_coding_error(prefix_timeout), OriginManifestFetchError::Timeout));
}

#[tokio::test]
async fn shared_manifest_decodes_supported_origin_content_codings_and_keeps_provider_cookie() {
    let encoded_bodies = [
        ("gzip", gzip(TEST_MANIFEST)),
        ("deflate", raw_deflate(TEST_MANIFEST)),
        ("br", brotli(TEST_MANIFEST)),
        ("zstd", zstd(TEST_MANIFEST).await),
    ];

    for (coding, encoded_body) in encoded_bodies {
        let response = TestOriginResponse::ok(encoded_body)
            .with_header("Content-Encoding", coding)
            .with_header("Set-Cookie", format!("sid={coding}; Path=/"));
        let (entry_url, request_receiver) = spawn_test_origin(response).await;
        let (client, no_redirect_client) = test_clients();
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));

        let fetched =
            fetch_origin_manifest_once(&entry_url, &headers, &client, &no_redirect_client, false, None, 1_000)
                .await
                .unwrap_or_else(|error| panic!("decode {coding} manifest: {error:?}"));

        assert_eq!(fetched.body.as_bytes(), TEST_MANIFEST, "coding={coding}");
        assert_eq!(
            fetched
                .provider_session_headers
                .get(header::COOKIE)
                .expect("provider cookie")
                .to_str()
                .expect("provider cookie text"),
            format!("sid={coding}"),
            "coding={coding}"
        );
        let request = request_receiver.await.expect("captured request");
        assert_eq!(captured_header(&request, "accept-encoding"), Some("identity"), "coding={coding}");
    }
}

#[tokio::test]
async fn shared_manifest_magic_sniffs_gzip_only_in_manifest_mode() {
    let (entry_url, _) = spawn_test_origin(TestOriginResponse::ok(gzip(TEST_MANIFEST))).await;
    let (client, no_redirect_client) = test_clients();

    let fetched =
        fetch_origin_manifest_once(&entry_url, &HeaderMap::new(), &client, &no_redirect_client, false, None, 1_000)
            .await
            .expect("decode magic-sniffed gzip manifest");

    assert_eq!(fetched.body.as_bytes(), TEST_MANIFEST);
}

#[tokio::test]
async fn direct_manual_redirect_reapplies_identity_after_cross_origin_scrubbing() {
    let (target_url, target_request_receiver) = spawn_test_origin(TestOriginResponse::ok(TEST_MANIFEST.to_vec())).await;
    let redirect_response = TestOriginResponse {
        status: "302 Found",
        headers: vec![("Location", target_url.to_string())],
        body: Vec::new(),
        body_delay: Duration::ZERO,
    };
    let (entry_url, redirect_request_receiver) = spawn_test_origin(redirect_response).await;
    let (client, no_redirect_client) = test_clients();
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
    headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
    headers.insert(header::COOKIE, HeaderValue::from_static("sid=secret"));

    let fetched = fetch_origin_manifest_once(&entry_url, &headers, &client, &no_redirect_client, true, None, 1_000)
        .await
        .expect("follow manual manifest redirect");
    assert_eq!(fetched.body.as_bytes(), TEST_MANIFEST);

    let redirect_request = redirect_request_receiver.await.expect("captured redirect request");
    let target_request = target_request_receiver.await.expect("captured target request");
    assert_eq!(captured_header(&redirect_request, "accept-encoding"), Some("identity"));
    assert_eq!(captured_header(&target_request, "accept-encoding"), Some("identity"));
    assert!(captured_header(&target_request, "authorization").is_none());
    assert!(captured_header(&target_request, "cookie").is_none());
}

#[tokio::test]
async fn shared_manifest_limit_applies_after_decompression() {
    let decoded_body = vec![b'x'; MAX_HLS_MANIFEST_BYTES + 1];
    let response = TestOriginResponse::ok(gzip(&decoded_body)).with_header("Content-Encoding", "gzip");
    let (entry_url, _) = spawn_test_origin(response).await;
    let (client, no_redirect_client) = test_clients();

    let error =
        fetch_origin_manifest_once(&entry_url, &HeaderMap::new(), &client, &no_redirect_client, false, None, 2_000)
            .await
            .expect_err("decoded manifest above limit must fail");

    assert!(matches!(
        error,
        OriginManifestFetchError::DecodedBodyLimitExceeded { limit } if limit == MAX_HLS_MANIFEST_BYTES
    ));
}

#[tokio::test]
async fn shared_manifest_deadline_includes_magic_prefix_read() {
    let response =
        TestOriginResponse { body_delay: Duration::from_millis(100), ..TestOriginResponse::ok(TEST_MANIFEST.to_vec()) };
    let (entry_url, _) = spawn_test_origin(response).await;
    let (client, no_redirect_client) = test_clients();

    let error =
        fetch_origin_manifest_once(&entry_url, &HeaderMap::new(), &client, &no_redirect_client, false, None, 10)
            .await
            .expect_err("prefix read must honor manifest deadline");

    assert!(matches!(error, OriginManifestFetchError::Timeout));
}

#[tokio::test]
async fn shared_manifest_encoded_body_timeout_stays_structured_timeout() {
    let response = TestOriginResponse {
        body_delay: Duration::from_millis(100),
        ..TestOriginResponse::ok(gzip(TEST_MANIFEST)).with_header("Content-Encoding", "gzip")
    };
    let (entry_url, _) = spawn_test_origin(response).await;
    let (client, no_redirect_client) = test_clients();

    let error =
        fetch_origin_manifest_once(&entry_url, &HeaderMap::new(), &client, &no_redirect_client, false, None, 10)
            .await
            .expect_err("encoded body read must honor manifest deadline");

    assert!(matches!(error, OriginManifestFetchError::Timeout));
}

#[tokio::test]
async fn shared_manifest_distinguishes_invalid_utf8_from_decoder_failure() {
    let (invalid_utf8_url, _) = spawn_test_origin(TestOriginResponse::ok(vec![0xff])).await;
    let corrupt_gzip = TestOriginResponse::ok(vec![0x1f, 0x8b, 0x08, 0x00]).with_header("Content-Encoding", "gzip");
    let (corrupt_gzip_url, _) = spawn_test_origin(corrupt_gzip).await;
    let (client, no_redirect_client) = test_clients();

    let invalid_utf8 = fetch_origin_manifest_once(
        &invalid_utf8_url,
        &HeaderMap::new(),
        &client,
        &no_redirect_client,
        false,
        None,
        1_000,
    )
    .await
    .expect_err("invalid UTF-8 must fail");
    let decoder_failure = fetch_origin_manifest_once(
        &corrupt_gzip_url,
        &HeaderMap::new(),
        &client,
        &no_redirect_client,
        false,
        None,
        1_000,
    )
    .await
    .expect_err("corrupt gzip must fail");

    assert!(matches!(invalid_utf8, OriginManifestFetchError::InvalidUtf8 { .. }));
    assert!(matches!(decoder_failure, OriginManifestFetchError::ContentDecoding { .. }));
}

#[tokio::test]
async fn shared_manifest_rejects_encoded_partial_content() {
    let response = TestOriginResponse {
        status: "206 Partial Content",
        ..TestOriginResponse::ok(gzip(TEST_MANIFEST)).with_header("Content-Encoding", "gzip")
    };
    let (entry_url, _) = spawn_test_origin(response).await;
    let (client, no_redirect_client) = test_clients();

    let error =
        fetch_origin_manifest_once(&entry_url, &HeaderMap::new(), &client, &no_redirect_client, false, None, 1_000)
            .await
            .expect_err("encoded partial manifest must fail");

    assert!(matches!(error, OriginManifestFetchError::ContentCoding(ContentCodingError::EncodedPartialContent)));
}
