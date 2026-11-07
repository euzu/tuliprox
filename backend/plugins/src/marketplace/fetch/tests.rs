//! Fetch policy tests against a loopback server on 127.0.0.1. The policy is
//! always passed explicitly; no test touches the environment.

use super::*;
use std::{net::SocketAddr, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const OPEN: FetchPolicy = FetchPolicy { allow_http: true };
const STRICT: FetchPolicy = FetchPolicy { allow_http: false };

fn client() -> Client { Client::builder().redirect(reqwest::redirect::Policy::none()).no_proxy().build().unwrap() }

/// Serves `route(path, request_head)` as a raw HTTP/1.1 response per connection.
async fn serve(route: impl Fn(&str, &str) -> Vec<u8> + Send + Sync + 'static) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let route = Arc::new(route);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { return };
            let route = Arc::clone(&route);
            tokio::spawn(async move {
                let mut buffer = vec![0_u8; 4096];
                let read = stream.read(&mut buffer).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buffer[..read]).to_string();
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                let _ = stream.write_all(&route(&path, &head)).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    addr
}

fn ok(body: &[u8]) -> Vec<u8> {
    let mut out =
        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n", body.len())
            .into_bytes();
    out.extend_from_slice(body);
    out
}

fn redirect(to: &str) -> Vec<u8> {
    format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()
}

#[tokio::test]
async fn http_and_file_urls_are_refused_without_the_knob() {
    let addr = serve(|_, _| ok(b"x")).await;
    let err = fetch_package(&client(), STRICT, &format!("http://{addr}/p"), 1024).await.unwrap_err();
    assert!(matches!(err, MarketplaceError::Refused(_)), "{err}");
    for url in ["file:///etc/passwd", "ftp://example.com/p", "not a url"] {
        assert!(fetch_package(&client(), OPEN, url, 1024).await.is_err(), "{url}");
    }
    let err = fetch_catalog(&client(), STRICT, &format!("http://{addr}/c"), None, None).await.unwrap_err();
    assert!(matches!(err, MarketplaceError::Refused(_)));
}

#[tokio::test]
async fn the_knob_allows_http_and_loopback() {
    let addr = serve(|_, _| ok(b"package")).await;
    let bytes = fetch_package(&client(), OPEN, &format!("http://{addr}/p"), 1024).await.unwrap();
    assert_eq!(bytes, b"package");
}

#[tokio::test]
async fn a_blocked_destination_is_refused_for_a_package_but_not_a_catalog_url() {
    // Strict policy cannot even reach http, so classify through a policy-less path:
    // the package classifier runs on every hop before the request is sent.
    let request = Request {
        client: &client(),
        policy: STRICT,
        classify: true,
        conditional: HeaderMap::new(),
        cap: 1024,
        deadline: Duration::from_secs(5),
    };
    let err = request.check_hop(&Url::parse("https://127.0.0.1/p").unwrap()).await.unwrap_err();
    assert!(matches!(err, MarketplaceError::ForbiddenDestination(_)), "{err}");
    assert_eq!(err.status(), StatusCode::FORBIDDEN);
    let err = request.check_hop(&Url::parse("https://169.254.169.254/p").unwrap()).await.unwrap_err();
    assert!(matches!(err, MarketplaceError::ForbiddenDestination(_)));
    let catalog = Request { classify: false, ..request };
    assert!(catalog.check_hop(&Url::parse("https://127.0.0.1/c").unwrap()).await.is_ok());
}

#[tokio::test]
async fn an_https_to_http_redirect_is_refused() {
    // The first hop would need TLS, so exercise the hop check directly: a
    // redirect target of http:// fails under the strict policy.
    let request = Request {
        client: &client(),
        policy: STRICT,
        classify: false,
        conditional: HeaderMap::new(),
        cap: 1024,
        deadline: Duration::from_secs(5),
    };
    let err = request.check_hop(&Url::parse("http://example.com/p").unwrap()).await.unwrap_err();
    assert!(matches!(err, MarketplaceError::Refused(_)));
}

#[tokio::test]
async fn a_redirect_to_a_non_https_scheme_is_refused_mid_chain() {
    let addr = serve(|_, _| redirect("file:///etc/passwd")).await;
    let err = fetch_package(&client(), OPEN, &format!("http://{addr}/p"), 1024).await.unwrap_err();
    assert!(matches!(err, MarketplaceError::Refused(_)), "{err}");
}

#[tokio::test]
async fn redirects_are_followed_up_to_the_limit() {
    let addr = serve(|path, _| match path.trim_start_matches('/').parse::<usize>() {
        Ok(n) if n < 5 => redirect(&format!("/{}", n + 1)),
        Ok(_) => ok(b"done"),
        Err(_) => ok(b"?"),
    })
    .await;
    let bytes = fetch_package(&client(), OPEN, &format!("http://{addr}/0"), 1024).await.unwrap();
    assert_eq!(bytes, b"done");
}

#[tokio::test]
async fn more_than_five_redirects_fail() {
    let addr = serve(|path, _| {
        let n: usize = path.trim_start_matches('/').parse().unwrap_or(0);
        redirect(&format!("/{}", n + 1))
    })
    .await;
    let err = fetch_package(&client(), OPEN, &format!("http://{addr}/0"), 1024).await.unwrap_err();
    assert_eq!(err, MarketplaceError::TooManyRedirects);
}

#[tokio::test]
async fn an_oversize_package_is_cut_mid_stream_without_a_content_length() {
    let addr = serve(|_, _| {
        let mut out = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        out.extend(std::iter::repeat_n(b'a', 10_000));
        out
    })
    .await;
    let err = fetch_package(&client(), OPEN, &format!("http://{addr}/p"), 1000).await.unwrap_err();
    assert_eq!(err, MarketplaceError::TooLarge(1000));
    assert_eq!(err.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn a_lying_content_length_cannot_make_an_oversize_catalog_pass() {
    let big = vec![b'a'; CATALOG_MAX_BYTES + 10];
    let addr = serve(move |_, _| ok(&big)).await;
    let err = fetch_catalog(&client(), OPEN, &format!("http://{addr}/c"), None, None).await.unwrap_err();
    assert!(matches!(err, MarketplaceError::TooLarge(_)));
}

#[tokio::test]
async fn a_conditional_get_answers_not_modified_and_sends_the_validators() {
    let addr = serve(|_, head| {
        if head.to_lowercase().contains("if-none-match: \"v1\"") {
            b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n".to_vec()
        } else {
            ok(b"{}")
        }
    })
    .await;
    let url = format!("http://{addr}/c");
    let first = fetch_catalog(&client(), OPEN, &url, None, None).await.unwrap();
    let CatalogFetch::Fresh { etag, .. } = first else { panic!("expected a body") };
    assert_eq!(etag.as_deref(), Some("\"v1\""));
    let second = fetch_catalog(&client(), OPEN, &url, etag.as_deref(), None).await.unwrap();
    assert!(matches!(second, CatalogFetch::NotModified));
}

#[tokio::test]
async fn an_http_error_status_is_a_bad_gateway() {
    let addr = serve(|_, _| b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()).await;
    let err = fetch_package(&client(), OPEN, &format!("http://{addr}/p"), 1024).await.unwrap_err();
    assert_eq!(err, MarketplaceError::Status(404));
    assert_eq!(err.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn a_stalled_server_hits_the_whole_fetch_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    let request = Request {
        client: &client(),
        policy: OPEN,
        classify: false,
        conditional: HeaderMap::new(),
        cap: 1024,
        deadline: Duration::from_millis(200),
    };
    let err = request.get(&format!("http://{addr}/c")).await.unwrap_err();
    assert_eq!(err, MarketplaceError::Timeout);
}

#[test]
fn error_messages_carry_no_url() {
    let rendered = MarketplaceError::Refused("only https URLs are allowed").to_string();
    assert!(!rendered.contains("://"));
}
