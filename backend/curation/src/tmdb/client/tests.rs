use super::*;
use crate::test_support::{http_response, TestServer};
use serde_json::{json, Value};
use tuliprox_core::utils::compression_utils::compress_string;

const EMPTY: &str = r#"{"page":1,"total_pages":0,"total_results":0,"results":[]}"#;
const TOKEN: &str = "private-test-token";

fn selector(limit: u32) -> TmdbTrendingConfig {
    TmdbTrendingConfig {
        kind: TmdbTrendingKind::Movie,
        time_window: TmdbTrendingTimeWindow::Week,
        limit,
        category_name: None,
        create_xtream_category: false,
    }
}

fn client(server: &TestServer) -> TmdbClient {
    // Adapter tests; the production configured profile is exercised independently in
    // core's transport fixtures and in HTTPS -> processing publication tests.
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("x-transport-context", HeaderValue::from_static("retained"));
    let http = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .default_headers(headers)
        .build()
        .unwrap();
    TmdbClient::for_test(&http, &TmdbCurationApiConfig { access_token: TOKEN.into() }, &server.url).unwrap()
}

impl TmdbClient {
    async fn fetch(&self, selector: &TmdbTrendingConfig) -> Result<Vec<CuratedMediaReference>, TmdbFailure> {
        self.trending(selector, &mut self.batch_budget()).await
    }
}

fn page(p: u64, total_pages: u64, ids: &[u32]) -> String {
    json!({"page":p,"total_pages":total_pages,"total_results":ids.len(),"results":ids.iter().map(|id| json!({"id":id})).collect::<Vec<_>>()}).to_string()
}

fn gzip_response(body: &str) -> Vec<u8> { encoded_response(&compress_string(body).unwrap()) }

fn encoded_response(bytes: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        bytes.len()
    )
    .into_bytes();
    response.extend_from_slice(bytes);
    response
}

fn padded(body: &str, length: usize) -> String { format!("{body}{}", " ".repeat(length - body.len())) }

#[tokio::test]
async fn tmdb_item_limit_cuts_unique_references_with_first_text_and_observed_row_rank() {
    for (limit, expected_ids, expected_ranks, requests) in [
        (1, vec![7], vec![1], 1),
        (2, vec![7, 8], vec![1, 3], 1),
        (3, vec![7, 8, 9], vec![1, 3, 5], 2),
        (4, vec![7, 8, 9, 10], vec![1, 3, 5, 6], 2),
        (100, vec![7, 8, 9, 10], vec![1, 3, 5, 6], 2),
    ] {
        let first = r#"{"page":1,"total_pages":9,"total_results":10,"results":[{"id":7,"title":"First"},{"id":7,"title":"Duplicate"},{"id":8}]}"#;
        let server =
            TestServer::sequence(vec![http_response(200, first), http_response(200, &page(2, 2, &[8, 9, 10]))]).await;
        let result = client(&server).fetch(&selector(limit)).await.unwrap();
        assert_eq!(result.iter().map(|r| r.tmdb_id.unwrap()).collect::<Vec<_>>(), expected_ids);
        assert_eq!(result.iter().map(|r| r.rank.unwrap()).collect::<Vec<_>>(), expected_ranks);
        assert_eq!(result[0].title, "First");
        let recorded = server.requests.lock().unwrap();
        assert_eq!(recorded.len(), requests);
        for (index, request) in recorded.iter().enumerate() {
            assert!(
                request.starts_with(&format!("GET /3/trending/movie/week?language=en-US&page={} HTTP/1.1", index + 1))
            );
            assert!(!request.lines().next().unwrap().contains("limit"));
        }
    }
}

#[tokio::test]
async fn tmdb_short_pages_and_drifting_totals_do_not_assume_a_snapshot() {
    let server = TestServer::sequence(vec![
        http_response(200, &page(1, 2, &[7])),
        http_response(200, &page(2, 5, &[8, 9, 10, 11])),
        http_response(200, &page(3, 3, &[12])),
    ])
    .await;
    let result = client(&server).fetch(&selector(100)).await.unwrap();
    assert_eq!(result.len(), 6);
    assert_eq!(result[5].rank, Some(6));
    assert_eq!(server.requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn tmdb_empty_is_valid_only_at_the_start_with_coherent_totals() {
    for pages in [0, 1] {
        let body = json!({"page":1,"total_pages":pages,"total_results":0,"results":[]}).to_string();
        let server = TestServer::new(http_response(200, &body)).await;
        assert!(client(&server).fetch(&selector(100)).await.unwrap().is_empty());
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
    for body in [
        page(2, 2, &[]),
        page(2, 9, &[]),
        EMPTY.into(),
        json!({"page":2,"total_pages":2,"total_results":1,"results":[]}).to_string(),
    ] {
        let server = TestServer::sequence(vec![http_response(200, &page(1, 2, &[7])), http_response(200, &body)]).await;
        assert_eq!(client(&server).fetch(&selector(100)).await.unwrap_err(), TmdbFailure::InvalidResponse);
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn tmdb_no_progress_including_terminal_permutations_fails_instead_of_accepting_a_prefix() {
    for pages in [2, 3] {
        for ids in [&[7, 8][..], &[8, 7], &[7, 7]] {
            let server = TestServer::sequence(vec![
                http_response(200, &page(1, 3, &[7, 8])),
                http_response(200, &page(2, pages, ids)),
            ])
            .await;
            assert_eq!(client(&server).fetch(&selector(100)).await.unwrap_err(), TmdbFailure::NoProgress);
            assert_eq!(server.requests.lock().unwrap().len(), 2);
        }
    }
}

#[tokio::test]
async fn tmdb_validates_envelope_duplicates_and_suffix_even_after_the_limit() {
    let mut invalid: Vec<Value> = vec![json!({}), json!([])];
    let base = json!({"page":1,"total_pages":1,"total_results":2,"results":[{"id":7},{"id":8}]});
    for (field, value) in [
        ("page", json!(2)),
        ("page", json!(0)),
        ("page", json!(1.0)),
        ("total_pages", json!(0)),
        ("total_results", json!(1)),
        ("total_results", json!(-1)),
        ("results", json!(null)),
    ] {
        let mut value_base = base.clone();
        value_base[field] = value;
        invalid.push(value_base);
    }
    for field in ["page", "total_pages", "total_results", "results"] {
        let mut value = base.clone();
        value.as_object_mut().unwrap().remove(field);
        invalid.push(value);
    }
    for bad in [
        json!({"id":0}),
        json!({"id":7,"media_type":"tv"}),
        json!({"id":7,"media_type":null}),
        json!({"id":7,"title":42}),
    ] {
        for index in [0, 1] {
            let mut value = base.clone();
            value["results"][index] = bad.clone();
            invalid.push(value);
        }
    }
    for value in invalid {
        let server = TestServer::new(http_response(200, &value.to_string())).await;
        assert_eq!(client(&server).fetch(&selector(1)).await.unwrap_err(), TmdbFailure::InvalidResponse, "{value}");
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
    // A current envelope below the requested page is invalid even at N.
    let server =
        TestServer::sequence(vec![http_response(200, &page(1, 2, &[7])), http_response(200, &page(2, 1, &[8]))]).await;
    assert_eq!(client(&server).fetch(&selector(2)).await.unwrap_err(), TmdbFailure::InvalidResponse);
}

#[tokio::test]
async fn tmdb_all_routes_preserve_headers_with_no_query_token_or_details() {
    for (kind, route) in [(TmdbTrendingKind::Movie, "movie"), (TmdbTrendingKind::Tv, "tv")] {
        for (window, period) in [(TmdbTrendingTimeWindow::Day, "day"), (TmdbTrendingTimeWindow::Week, "week")] {
            let server = TestServer::new(http_response(200, EMPTY)).await;
            let client = client(&server);
            assert!(client.authorization.is_sensitive());
            assert!(!format!("{:?}", client.authorization).contains(TOKEN));
            let mut selector = selector(100);
            selector.kind = kind;
            selector.time_window = window;
            assert!(client.fetch(&selector).await.unwrap().is_empty());
            let requests = server.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert!(
                requests[0].starts_with(&format!("GET /3/trending/{route}/{period}?language=en-US&page=1 HTTP/1.1"))
            );
            assert!(requests[0].to_lowercase().contains(&format!("authorization: bearer {TOKEN}")));
            assert!(requests[0].contains("x-transport-context: retained"));
        }
    }
}

#[tokio::test]
async fn tmdb_invalid_runtime_limit_and_unavailable_credentials_send_nothing() {
    let server = TestServer::new(http_response(200, EMPTY)).await;
    for token in ["", "  ", "bad\nheader"] {
        assert!(matches!(
            TmdbClient::for_test(&Client::new(), &TmdbCurationApiConfig { access_token: token.into() }, &server.url),
            Err(TmdbFailure::Configuration)
        ));
    }
    for limit in [0, 501, u32::MAX] {
        assert_eq!(client(&server).fetch(&selector(limit)).await.unwrap_err(), TmdbFailure::Configuration);
    }
    assert!(server.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn tmdb_status_failures_are_sanitized_without_retry_or_body_consumption() {
    for status in [401, 403, 429, 500, 503] {
        for late in [false, true] {
            let mut responses = Vec::new();
            if late {
                responses.push(http_response(200, &page(1, 2, &[7])));
            }
            responses.push(http_response(status, TOKEN));
            let server = TestServer::sequence(responses).await;
            let client = client(&server);
            let mut budget = client.batch_budget();
            let error = client.trending(&selector(100), &mut budget).await.unwrap_err();
            assert_eq!(error, TmdbFailure::Status(status));
            assert!(!format!("{error:?}").contains(TOKEN));
            assert_eq!(budget.bytes, if late { page(1, 2, &[7]).len() } else { 0 });
            assert_eq!(budget.requests, if late { 2 } else { 1 });
            assert_eq!(server.requests.lock().unwrap().len(), budget.requests);
        }
    }
}

#[tokio::test]
async fn tmdb_identity_chunked_and_gzip_exact_and_plus_one_count_decoded_bytes() {
    for length in [BODY_LIMIT, BODY_LIMIT + 1] {
        let body = padded(EMPTY, length);
        let chunked = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
            body.len()
        );
        for response in [http_response(200, &body).into_bytes(), chunked.into_bytes(), gzip_response(&body)] {
            let server = TestServer::new_bytes(response).await;
            let client = client(&server);
            let mut budget = client.batch_budget();
            let result = client.trending(&selector(100), &mut budget).await;
            if length == BODY_LIMIT {
                assert!(result.unwrap().is_empty());
            } else {
                assert_eq!(result.unwrap_err(), TmdbFailure::BodyLimit);
            }
            assert_eq!(budget.bytes, length);
        }
    }
}

#[tokio::test]
async fn tmdb_complete_gzip_member_does_not_hide_truncated_transport_or_later_members() {
    let compressed = compress_string(EMPTY).unwrap();
    let mut truncated_transport = format!(
        "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        compressed.len() + 1
    )
    .into_bytes();
    truncated_transport.extend_from_slice(&compressed);
    let mut broken_second_member = compressed.clone();
    let mut suffix = compress_string(" ").unwrap();
    suffix.truncate(suffix.len() - 8);
    broken_second_member.extend(suffix);
    for response in [truncated_transport, encoded_response(&broken_second_member)] {
        let server = TestServer::new_bytes(response).await;
        assert_eq!(client(&server).fetch(&selector(1)).await.unwrap_err(), TmdbFailure::Body);
    }
    let mut oversized_members = compressed;
    oversized_members.extend(compress_string(&" ".repeat(BODY_LIMIT + 1 - EMPTY.len())).unwrap());
    let server = TestServer::new_bytes(encoded_response(&oversized_members)).await;
    let client = client(&server);
    let mut budget = client.batch_budget();
    assert_eq!(client.trending(&selector(1), &mut budget).await.unwrap_err(), TmdbFailure::BodyLimit);
    assert_eq!(budget.bytes, BODY_LIMIT + 1);
}

#[tokio::test]
async fn tmdb_failed_bodies_and_detection_probes_are_debited_before_a_sibling() {
    let mut compressed = compress_string(&padded(EMPTY, 1000)).unwrap();
    compressed.truncate(compressed.len() - 8); // full decoded JSON, missing gzip trailer
    for response in [
        encoded_response(&compressed),
        format!("HTTP/1.1 200 OK\r\nContent-Length: 2000\r\nConnection: close\r\n\r\n{}", padded(EMPTY, 1000))
            .into_bytes(),
    ] {
        let server = TestServer::new_bytes(response).await;
        let mut client = client(&server);
        client.limits.batch_bytes = 1000;
        let mut budget = client.batch_budget();
        assert_eq!(client.trending(&selector(1), &mut budget).await.unwrap_err(), TmdbFailure::Body);
        assert_eq!(budget.bytes, 1000);
        assert_eq!(client.trending(&selector(1), &mut budget).await.unwrap_err(), TmdbFailure::BodyLimit);
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
    let server = TestServer::new(http_response(200, &padded(EMPTY, 1001))).await;
    let mut client = client(&server);
    client.limits.batch_bytes = 1000;
    let mut budget = client.batch_budget();
    assert_eq!(client.trending(&selector(1), &mut budget).await.unwrap_err(), TmdbFailure::BodyLimit);
    assert_eq!(budget.bytes, 1001);
    assert!(client.trending(&selector(1), &mut budget).await.is_err());
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn tmdb_selector_and_shared_batch_bytes_accept_exact_completion_but_not_excess() {
    for batch_bound in [false, true] {
        for excess in [0, 1] {
            let server = TestServer::sequence(vec![
                http_response(200, &padded(&page(1, 2, &[7]), 200)),
                http_response(200, &padded(&page(2, 2, &[8]), 200 + excess)),
            ])
            .await;
            let mut client = client(&server);
            if batch_bound {
                client.limits.batch_bytes = 400;
            } else {
                client.limits.selector_bytes = 400;
            }
            let mut budget = client.batch_budget();
            let result = client.trending(&selector(100), &mut budget).await;
            if excess == 0 {
                assert_eq!(result.unwrap().len(), 2);
            } else {
                assert_eq!(result.unwrap_err(), TmdbFailure::BodyLimit);
            }
            assert_eq!(budget.bytes, 400 + excess);
            assert_eq!(server.requests.lock().unwrap().len(), 2);
        }
    }
    let server = TestServer::new(http_response(200, &padded(EMPTY, 200))).await;
    let mut client = client(&server);
    client.limits.batch_bytes = 400;
    let mut budget = client.batch_budget();
    for _ in 0..2 {
        client.trending(&selector(1), &mut budget).await.unwrap();
    }
    assert_eq!(client.trending(&selector(1), &mut budget).await.unwrap_err(), TmdbFailure::BodyLimit);
    assert_eq!(budget.bytes, 400);
    assert_eq!(server.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn tmdb_production_8_and_32_mib_guards_count_all_decoded_pages_and_one_detection_byte() {
    for selectors in [1, 4] {
        for excess in [0, 1] {
            let mut responses = Vec::new();
            for index in 0..selectors {
                for p in 1..=16 {
                    let size = BODY_LIMIT / 2 + if index == selectors - 1 && p == 16 { excess } else { 0 };
                    responses.push(gzip_response(&padded(&page(p, 16, &[u32::try_from(p).unwrap()]), size)));
                }
            }
            let server = TestServer::serve(responses, Duration::ZERO, false).await;
            let client = client(&server);
            let mut budget = client.batch_budget();
            for index in 0..selectors {
                let result = client.trending(&selector(500), &mut budget).await;
                if index == selectors - 1 && excess != 0 {
                    assert_eq!(result.unwrap_err(), TmdbFailure::BodyLimit);
                } else {
                    assert_eq!(result.unwrap().len(), 16);
                }
            }
            assert_eq!(budget.bytes, selectors * 8 * BODY_LIMIT + excess);
            if selectors == 4 {
                assert_eq!(client.trending(&selector(500), &mut budget).await.unwrap_err(), TmdbFailure::BodyLimit);
            }
            assert_eq!(server.requests.lock().unwrap().len(), selectors * 16);
        }
    }
}

#[tokio::test]
async fn tmdb_request_guards_admit_32_and_128_but_never_33_or_129() {
    for complete in [false, true] {
        let responses = (1..=32)
            .map(|p| http_response(200, &page(p, if complete { 32 } else { 33 }, &[u32::try_from(p).unwrap()])))
            .collect();
        let server = TestServer::sequence(responses).await;
        let result = client(&server).fetch(&selector(500)).await;
        if complete {
            assert_eq!(result.unwrap().len(), 32);
        } else {
            assert_eq!(result.unwrap_err(), TmdbFailure::RequestLimit);
        }
        assert_eq!(server.requests.lock().unwrap().len(), 32);
    }
    let server = TestServer::new(http_response(200, EMPTY)).await;
    let client = client(&server);
    let mut budget = client.batch_budget();
    for _ in 0..128 {
        client.trending(&selector(1), &mut budget).await.unwrap();
    }
    assert_eq!(client.trending(&selector(1), &mut budget).await.unwrap_err(), TmdbFailure::RequestLimit);
    assert_eq!(server.requests.lock().unwrap().len(), 128);
}

#[tokio::test]
async fn tmdb_absolute_deadlines_cover_headers_bodies_and_accumulated_pages() {
    for headers in [false, true] {
        let server = TestServer::delayed_bytes(gzip_response(EMPTY), Duration::from_secs(2), headers).await;
        let mut client = client(&server);
        client.limits.request_timeout = Duration::from_millis(50);
        assert_eq!(client.fetch(&selector(1)).await.unwrap_err(), TmdbFailure::Deadline);
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
    for batch_bound in [false, true] {
        let server = TestServer::serve(
            (1..=4).map(|p| http_response(200, &page(p, 4, &[u32::try_from(p).unwrap()])).into_bytes()).collect(),
            Duration::from_millis(40),
            true,
        )
        .await;
        let mut client = client(&server);
        client.limits.request_timeout = Duration::from_secs(1);
        if batch_bound {
            client.limits.batch_timeout = Duration::from_millis(110);
        } else {
            client.limits.selector_timeout = Duration::from_millis(110);
        }
        let mut budget = client.batch_budget();
        assert_eq!(client.trending(&selector(100), &mut budget).await.unwrap_err(), TmdbFailure::Deadline);
        assert!(budget.requests <= 3 && budget.requests >= 2);
        if batch_bound {
            let requests = budget.requests;
            assert_eq!(client.trending(&selector(100), &mut budget).await.unwrap_err(), TmdbFailure::Deadline);
            assert_eq!(budget.requests, requests);
        }
    }
}

#[tokio::test]
async fn tmdb_slow_chunks_do_not_reset_deadline_or_accept_a_useful_json_prefix() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}/", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let mut buffer = [0; 1024];
            let n = stream.read(&mut buffer).await.unwrap();
            assert!(n > 0);
            request.extend_from_slice(&buffer[..n]);
        }
        let body = padded(&page(1, 1, &[7]), 1000);
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n").await.unwrap();
        for chunk in body.as_bytes().chunks(100) {
            if stream.write_all(chunk).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    });
    let http = Client::builder()
        .no_proxy()
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let mut client =
        TmdbClient::for_test(&http, &TmdbCurationApiConfig { access_token: TOKEN.into() }, &origin).unwrap();
    client.limits.request_timeout = Duration::from_millis(100);
    let mut budget = client.batch_budget();
    assert_eq!(client.trending(&selector(1), &mut budget).await.unwrap_err(), TmdbFailure::Deadline);
    assert!(budget.bytes >= 100 && budget.bytes < 1000);
    assert_eq!(budget.requests, 1);
    task.abort();
}

#[tokio::test]
async fn tmdb_failed_selector_bytes_and_probe_reduce_the_next_selector_allowance() {
    let server = TestServer::new(http_response(200, &padded(EMPTY, 1000))).await;
    let mut client = client(&server);
    client.limits.request_bytes = 128;
    client.limits.batch_bytes = 256;
    let mut budget = client.batch_budget();
    assert_eq!(client.trending(&selector(1), &mut budget).await.unwrap_err(), TmdbFailure::BodyLimit);
    assert_eq!(budget.bytes, 129);
    assert_eq!(client.trending(&selector(1), &mut budget).await.unwrap_err(), TmdbFailure::BodyLimit);
    assert_eq!(budget.bytes, 257, "remaining 127 plus one detection byte, not a reset budget");
    assert!(client.trending(&selector(1), &mut budget).await.is_err());
    assert_eq!(server.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn tmdb_transport_failure_debits_the_admitted_attempt_without_resetting_the_batch() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}/", listener.local_addr().unwrap());
    drop(listener);
    let http = Client::builder().no_proxy().retry(reqwest::retry::never()).build().unwrap();
    let mut client =
        TmdbClient::for_test(&http, &TmdbCurationApiConfig { access_token: TOKEN.into() }, &origin).unwrap();
    client.limits.batch_requests = 1;
    let mut budget = client.batch_budget();
    assert_eq!(client.trending(&selector(1), &mut budget).await.unwrap_err(), TmdbFailure::Transport);
    assert_eq!(budget.requests, 1);
    assert_eq!(client.trending(&selector(1), &mut budget).await.unwrap_err(), TmdbFailure::RequestLimit);
    assert_eq!(budget.requests, 1);
}

#[tokio::test]
async fn tmdb_batch_deadline_blocks_admission_with_matching_prefix_and_completed_sibling() {
    use crate::kernel::{CurationIncompleteReason, CurationRunOutcome, CurationSelectorKey, SelectorOutcome};
    use shared::{
        model::{
            CurationConfigDto, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType, StreamProperties,
            VideoStreamProperties, XtreamCluster,
        },
        utils::{hash_string, Internable},
    };
    let server = TestServer::serve(
        vec![
            http_response(200, &page(1, 1, &[7])).into_bytes(),
            http_response(200, &page(1, 9, &[8])).into_bytes(),
            http_response(200, &page(2, 9, &[7])).into_bytes(),
        ],
        Duration::from_millis(60),
        true,
    )
    .await;
    let mut client = client(&server);
    client.limits.batch_timeout = Duration::from_millis(150);
    let dto: CurationConfigDto = serde_json::from_value(json!({"tmdb":{"trending":[
        {"kind":"movie","time_window":"week","limit":1,"create_xtream_category":false},
        {"kind":"movie","time_window":"week","limit":100,"create_xtream_category":false},
        {"kind":"movie","time_window":"week","limit":1,"create_xtream_category":false}
    ]}}))
    .unwrap();
    let config = tuliprox_core::model::CurationConfig::from(&dto).tmdb.unwrap();
    let playlist = vec![PlaylistGroup {
        id: 1,
        title: "Movies".intern(),
        xtream_cluster: XtreamCluster::Video,
        channels: [7, 8]
            .into_iter()
            .map(|id| PlaylistItem {
                header: PlaylistItemHeader {
                    uuid: hash_string(&format!("subject-{id}")),
                    title: format!("Movie {id}").intern(),
                    item_type: PlaylistItemType::Video,
                    xtream_cluster: XtreamCluster::Video,
                    additional_properties: Some(StreamProperties::Video(Box::new(VideoStreamProperties {
                        tmdb: Some(id),
                        ..Default::default()
                    }))),
                    ..Default::default()
                },
            })
            .collect(),
    }];
    let outcomes = crate::tmdb::evaluate_selectors(
        Ok(client),
        &playlist,
        "deadline",
        &config,
        &[CurationSelectorKey(0), CurationSelectorKey(1), CurationSelectorKey(2)],
    )
    .await;
    assert!(matches!(&outcomes[0], SelectorOutcome::Complete { memberships, .. } if memberships.len() == 1));
    for outcome in &outcomes[1..] {
        assert!(matches!(outcome, SelectorOutcome::Incomplete { reason: CurationIncompleteReason::Interrupted, .. }));
    }
    assert!(matches!(crate::coordinator::complete_evaluation(outcomes), CurationRunOutcome::Failed(_)));
    assert_eq!(server.requests.lock().unwrap().len(), 3, "pending sibling does not reset the batch clock");
}

#[tokio::test(start_paused = true)]
async fn tmdb_budget_clock_uses_absolute_not_sliding_deadlines() {
    let budget = AcquisitionBudget::new(Duration::from_secs(60), 32, 8 * BODY_LIMIT);
    tokio::time::advance(Duration::from_secs(59)).await;
    budget.admit().unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(budget.admit(), Err(TmdbFailure::Deadline));
}

#[tokio::test]
async fn tmdb_redirects_never_visit_same_or_foreign_destination() {
    let foreign = TestServer::new(http_response(200, EMPTY)).await;
    for location in [
        "/3/trending/movie/day?language=en-US&page=1".into(),
        format!("{}foreign", foreign.url),
        "http://api.themoviedb.org/".into(),
        "/3/trending/movie/week?language=en-US&page=1".into(),
    ] {
        let server = TestServer::new(format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ))
        .await;
        assert_eq!(client(&server).fetch(&selector(1)).await.unwrap_err(), TmdbFailure::Redirect);
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
    assert!(foreign.requests.lock().unwrap().is_empty());
}
