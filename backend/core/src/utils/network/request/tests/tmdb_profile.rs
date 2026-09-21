//! Physical traffic tests for the production configured discovery profile.
use super::{make_test_app_config, response_with_body, start_hanging_http_server, start_recording_http_server};
use crate::{
    model::{Config, ProxyConfig},
    utils::network::request::{create_client, create_tmdb_client},
};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[tokio::test]
async fn tmdb_profile_redirects_send_only_one_get_and_never_contact_a_second_destination() {
    let (foreign, foreign_requests, foreign_task) =
        start_recording_http_server(vec![response_with_body("200 OK", "ok")]).await.unwrap();
    let cfg = make_test_app_config(Config::default());
    for location in [
        format!("http://{foreign}/foreign"),
        "/same-origin".into(),
        "http://api.themoviedb.org/downgrade".into(),
        "/start".into(),
    ] {
        let (address, requests, task) = start_recording_http_server(vec![format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )])
        .await
        .unwrap();
        let client = create_tmdb_client(&cfg).unwrap().no_proxy().build().unwrap();
        let response =
            client.get(format!("http://{address}/start")).header(AUTHORIZATION, "Bearer fixture").send().await.unwrap();
        assert_eq!(response.status(), 302);
        assert_eq!(requests.lock().await.len(), 1);
        assert!(foreign_requests.lock().await.is_empty());
        task.abort();
    }
    foreign_task.abort();
}

#[tokio::test]
async fn tmdb_profile_retains_authenticated_proxy_and_default_headers() {
    let (proxy, requests, task) =
        start_recording_http_server(vec![response_with_body("200 OK", "proxied")]).await.unwrap();
    let cfg = make_test_app_config(Config {
        proxy: Some(ProxyConfig {
            url: format!("http://{proxy}"),
            username: Some("proxy-user".into()),
            password: Some("proxy-password".into()),
        }),
        connect_timeout_secs: 1,
        ..Config::default()
    });
    let mut headers = HeaderMap::new();
    headers.insert("x-context", HeaderValue::from_static("retained"));
    let client = create_tmdb_client(&cfg).unwrap().default_headers(headers).build().unwrap();
    let response = client
        .get("http://unresolvable.invalid/trending")
        .header(AUTHORIZATION, "Bearer discovery-token")
        .send()
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), "proxied");
    let requests = requests.lock().await;
    assert_eq!(requests.len(), 1);
    let request = requests[0].to_lowercase();
    assert!(request.starts_with("get http://unresolvable.invalid/trending http/1.1"));
    assert!(request.contains("proxy-authorization: basic "));
    assert!(request.contains("authorization: bearer discovery-token"));
    assert!(request.contains("x-context: retained"));
    task.abort();
}

#[tokio::test]
async fn tmdb_profile_connect_timeout_covers_a_hanging_proxy_tunnel() {
    let (proxy, seen, task) = start_hanging_http_server().await.unwrap();
    let cfg = make_test_app_config(Config {
        proxy: Some(ProxyConfig { url: format!("http://{proxy}"), username: None, password: None }),
        connect_timeout_secs: 1,
        ..Config::default()
    });
    let client = create_tmdb_client(&cfg).unwrap().build().unwrap();
    let result =
        tokio::time::timeout(Duration::from_secs(3), client.get("https://unresolvable.invalid/trending").send())
            .await
            .unwrap();
    seen.await.unwrap();
    assert!(result.unwrap_err().is_timeout());
    task.abort();
}

#[test]
fn tmdb_profile_construction_errors_are_not_unconfigured_fallbacks() {
    for proxy in ["not a url", "file:///tmp/proxy", "http://[invalid"] {
        let cfg = make_test_app_config(Config {
            proxy: Some(ProxyConfig { url: proxy.into(), username: None, password: None }),
            ..Config::default()
        });
        let error = create_tmdb_client(&cfg).err().expect("malformed proxy must fail closed");
        assert!(!error.to_string().contains(proxy));
    }
    let cfg = make_test_app_config(Config::default());
    assert!(create_tmdb_client(&cfg).unwrap().user_agent("bad\nheader").build().is_err());
}

#[tokio::test]
async fn tmdb_profile_refused_http2_stream_is_not_replayed_unlike_the_generic_default() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let gets = Arc::new(AtomicUsize::new(0));
    let recorded = Arc::clone(&gets);
    let task = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let gets = Arc::clone(&recorded);
            connections.spawn(async move {
                let mut preface = [0; 24];
                stream.read_exact(&mut preface).await.unwrap();
                assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
                // Empty SETTINGS. Each HEADERS is an actually received request;
                // REFUSED_STREAM (7) is reqwest's default protocol retry trigger.
                stream.write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0]).await.unwrap();
                loop {
                    let mut header = [0; 9];
                    if stream.read_exact(&mut header).await.is_err() {
                        break;
                    }
                    let length =
                        (usize::from(header[0]) << 16) | (usize::from(header[1]) << 8) | usize::from(header[2]);
                    assert!(length <= 65536);
                    let mut payload = vec![0; length];
                    stream.read_exact(&mut payload).await.unwrap();
                    if header[3] == 4 && header[4] == 0 {
                        stream.write_all(&[0, 0, 0, 4, 1, 0, 0, 0, 0]).await.unwrap();
                    } else if header[3] == 1 {
                        gets.fetch_add(1, Ordering::SeqCst);
                        let mut reset = vec![0, 0, 4, 3, 0];
                        reset.extend_from_slice(&header[5..9]);
                        reset.extend_from_slice(&[0, 0, 0, 7]);
                        stream.write_all(&reset).await.unwrap();
                    }
                }
            });
        }
    });
    let cfg = make_test_app_config(Config::default());
    for (builder, expected) in [(create_tmdb_client(&cfg).unwrap(), 1), (create_client(&cfg), 3)] {
        let before = gets.load(Ordering::SeqCst);
        let client = builder.no_proxy().http2_prior_knowledge().timeout(Duration::from_secs(2)).build().unwrap();
        assert!(client.get(format!("http://{address}/trending")).send().await.is_err());
        assert_eq!(gets.load(Ordering::SeqCst) - before, expected, "count actual HEADERS, not send calls");
    }
    task.abort();
}

#[tokio::test]
async fn tmdb_profile_reuses_or_recovers_a_closed_idle_connection_without_replaying_sent_gets() {
    let (address, requests, task) = start_recording_http_server(vec![
        // Advertise keepalive but close after replying: the second logical request
        // may recover an unused idle connection, never replay a GET already sent.
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".into(),
    ])
    .await
    .unwrap();
    let cfg = make_test_app_config(Config::default());
    let client = create_tmdb_client(&cfg).unwrap().no_proxy().build().unwrap();
    for _ in 0..2 {
        let response =
            tokio::time::timeout(Duration::from_secs(1), client.get(format!("http://{address}/trending")).send())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(response.text().await.unwrap(), "ok");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(requests.lock().await.len(), 2);
    task.abort();
}
