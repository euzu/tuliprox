use base64::Engine;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::http::{self, HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use reqwest::redirect::Policy;
use reqwest::Proxy;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use url::Url;

struct ProxyState {
    expected_auth: HeaderValue,
    request_count: AtomicUsize,
    missing_auth: AtomicUsize,
    uris: Mutex<Vec<String>>,
}

impl ProxyState {
    fn new(expected_auth: HeaderValue) -> Self {
        Self {
            expected_auth,
            request_count: AtomicUsize::new(0),
            missing_auth: AtomicUsize::new(0),
            uris: Mutex::new(Vec::new()),
        }
    }
}

fn response_with_status(status: StatusCode, headers: Vec<(HeaderName, HeaderValue)>) -> Response<Full<Bytes>> {
    let mut builder = Response::builder().status(status);
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    builder
        .body(Full::new(Bytes::from_static(b"")))
        .unwrap()
}

async fn proxy_handler(
    req: Request<Incoming>,
    state: Arc<ProxyState>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let request_index = state.request_count.fetch_add(1, Ordering::SeqCst) + 1;
    {
        let mut uris = state.uris.lock().await;
        uris.push(req.uri().to_string());
    }

    let auth = req.headers().get(http::header::PROXY_AUTHORIZATION);
    let auth_ok = auth.is_some_and(|value| value.as_bytes() == state.expected_auth.as_bytes());
    if !auth_ok {
        state.missing_auth.fetch_add(1, Ordering::SeqCst);
        return Ok(response_with_status(
            StatusCode::PROXY_AUTHENTICATION_REQUIRED,
            vec![(
                http::header::PROXY_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"proxy\""),
            )],
        ));
    }

    if request_index == 1 {
        return Ok(response_with_status(
            StatusCode::FOUND,
            vec![(
                http::header::LOCATION,
                HeaderValue::from_static("http://redirect.local/ok"),
            )],
        ));
    }

    Ok(response_with_status(StatusCode::OK, vec![]))
}

async fn start_proxy(state: Arc<ProxyState>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else { continue };
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                let io = TokioIo::new(socket);
                let service = service_fn(move |req| proxy_handler(req, Arc::clone(&state)));
                let builder = Builder::new(TokioExecutor::new());
                let conn = builder.serve_connection(io, service);
                let _ = conn.await;
            });
        }
    });

    (addr, handle)
}

async fn send_with_manual_redirects(
    client: &reqwest::Client,
    mut url: Url,
    max_redirects: u8,
) -> reqwest::Response {
    let mut remaining = max_redirects;
    loop {
        let response = client.get(url.clone()).send().await.unwrap();
        let status = response.status();
        if status.is_redirection() {
            if remaining == 0 {
                return response;
            }
            let location = response.headers().get(reqwest::header::LOCATION);
            let Some(location) = location else {
                return response;
            };
            let Ok(location_str) = location.to_str() else {
                return response;
            };
            let next_url = url
                .join(location_str)
                .or_else(|_| Url::parse(location_str));
            let Ok(next_url) = next_url else {
                return response;
            };
            url = next_url;
            remaining = remaining.saturating_sub(1);
            continue;
        }
        return response;
    }
}

async fn run_proxy_flow(manual_redirects: bool) -> (StatusCode, usize, Vec<String>) {
    let credentials = "user:pass";
    let encoded = base64::engine::general_purpose::STANDARD.encode(credentials);
    let expected_auth =
        HeaderValue::from_str(&format!("Basic {encoded}")).unwrap();

    let state = Arc::new(ProxyState::new(expected_auth.clone()));
    let (addr, server_handle) = start_proxy(Arc::clone(&state)).await;

    let proxy_url = format!("http://{addr}");
    let client = reqwest::Client::builder()
        .proxy(Proxy::all(&proxy_url).unwrap().basic_auth("user", "pass"))
        .redirect(if manual_redirects {
            Policy::none()
        } else {
            Policy::limited(5)
        })
        .build()
        .unwrap();

    let response = if manual_redirects {
        send_with_manual_redirects(
            &client,
            Url::parse("http://origin.local/start").unwrap(),
            5,
        )
        .await
    } else {
        client
            .get("http://origin.local/start")
            .send()
            .await
            .unwrap()
    };

    let status = response.status();
    let missing_auth = state.missing_auth.load(Ordering::SeqCst);
    let uris = state.uris.lock().await.clone();

    server_handle.abort();

    (status, missing_auth, uris)
}

#[tokio::test]
#[ignore = "Run to reproduce proxy-auth loss on redirect (reqwest#2177)."]
async fn proxy_auth_lost_on_redirect_repro() {
    let (status, missing_auth, uris) = run_proxy_flow(false).await;
    assert_eq!(
        status,
        StatusCode::PROXY_AUTHENTICATION_REQUIRED,
        "Expected 407 when proxy auth is lost on redirect. missing_auth={}, uris={:?}",
        missing_auth,
        uris
    );
    assert!(
        missing_auth > 0,
        "Expected missing proxy auth on redirect. missing_auth={}, uris={:?}",
        missing_auth,
        uris
    );
}

#[cfg(feature = "proxy-auth-regression")]
#[tokio::test]
async fn proxy_auth_survives_redirect_regression() {
    let (status, missing_auth, uris) = run_proxy_flow(true).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "Expected 200 OK after fix. missing_auth={}, uris={:?}",
        missing_auth,
        uris
    );
    assert_eq!(
        missing_auth,
        0,
        "Expected no missing proxy auth after fix. missing_auth={}, uris={:?}",
        missing_auth,
        uris
    );
}
