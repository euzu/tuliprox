use super::{
    create_test_app_state, media_server_image_error_status, resource_input_for_url, resource_proxy_response,
    resource_redirect_or_proxy, resource_response, spawn_legacy_hls_test_origin, ResourceFetchPolicy,
};
use crate::{
    media_server::{MediaServerError, MediaServerErrorKind},
    model::{Config, ConfigInput},
    utils::LRUResourceCache,
};
use axum::{
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use bytes::Bytes;
use http_body_util::BodyExt;
use std::{fmt::Write as _, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::RwLock,
};

#[test]
fn media_server_image_error_status_classifies_client_and_upstream_failures() {
    let parse_error = MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
        .detail("media server image URL is missing required path parts");
    assert_eq!(media_server_image_error_status(&parse_error), StatusCode::BAD_REQUEST);

    let not_found = MediaServerError::new(MediaServerErrorKind::MediaServerItemNotFound)
        .detail("plex media-server image URL is missing image_path");
    assert_eq!(media_server_image_error_status(&not_found), StatusCode::NOT_FOUND);

    let upstream = MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
        .detail("media-server image request failed");
    assert_eq!(media_server_image_error_status(&upstream), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn resource_cache_is_used_only_by_matching_public_fetch_policy() {
    const CACHED_BODY: &[u8] = b"cached image";
    const UPSTREAM_BODY: &[u8] = b"upstream image";

    let app_state = create_test_app_state();
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let cache_dir = temp_dir.path().to_string_lossy();
    let mut cache = LRUResourceCache::new(1024, cache_dir.as_ref());
    let resource_url = "http://1.1.1.1/icon.png";
    let cached_path = cache.store_path(resource_url, Some("image/png"));
    tokio::fs::write(&cached_path, CACHED_BODY).await.expect("write cached image");
    cache.add_content(resource_url, Some("image/png".to_string()), CACHED_BODY.len()).expect("register cached image");
    app_state.cache.store(Some(Arc::new(RwLock::new(cache))));

    let response_head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        UPSTREAM_BODY.len()
    );
    let (upstream_addr, upstream_task) = spawn_legacy_hls_test_origin(response_head, UPSTREAM_BODY.to_vec()).await;
    let proxy = reqwest::Proxy::http(format!("http://{upstream_addr}")).expect("mock proxy URL");
    let mock_client = Arc::new(reqwest::Client::builder().proxy(proxy).build().expect("mock upstream client"));
    // The destination is public, so the hop is fetched through the public resource client; both resource
    // clients are mocked so the assertion below is about the cache, not about which client performed the
    // request.
    app_state.http_clients.resource_public_no_redirect.store(Arc::clone(&mock_client));
    app_state.http_clients.resource_no_redirect.store(mock_client);

    let public_response =
        resource_response(&app_state, ResourceFetchPolicy::Public, resource_url, &HeaderMap::new(), None)
            .await
            .into_response();
    assert_eq!(public_response.status(), StatusCode::OK);
    let public_body = public_response.into_body().collect().await.expect("read cached image").to_bytes();
    assert_eq!(public_body, Bytes::from_static(CACHED_BODY));

    let response = resource_response(&app_state, ResourceFetchPolicy::NonPublic, resource_url, &HeaderMap::new(), None)
        .await
        .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let response_body = response.into_body().collect().await.expect("read upstream image").to_bytes();
    assert_eq!(response_body, Bytes::from_static(UPSTREAM_BODY));
    assert_ne!(response_body, Bytes::from_static(CACHED_BODY));

    let upstream_request = upstream_task.await.expect("mock upstream task completes");
    assert!(upstream_request.starts_with("GET http://1.1.1.1/icon.png HTTP/1.1\r\n"));
}

#[tokio::test]
async fn no_redirect_resource_ignores_configured_proxy() {
    let app_state = create_test_app_state();
    let response_head =
        "HTTP/1.1 302 Found\r\nLocation: http://10.0.0.2/other.png\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let (proxy_addr, proxy_task) = spawn_legacy_hls_test_origin(response_head.to_string(), Vec::new()).await;
    app_state.app_config.config.store(Arc::new(Config {
        proxy: Some(crate::model::ProxyConfig { url: format!("http://{proxy_addr}"), username: None, password: None }),
        ..Config::default()
    }));
    let client = crate::api::model::create_resource_http_client_no_redirect(&app_state.app_config)
        .expect("resource HTTP client");
    // 192.0.2.1 (TEST-NET-1) is not routable, so a direct attempt fails by timing out or refusing the
    // connection. A response would mean the request was answered by the proxy after all.
    let result = client.get("http://192.0.2.1/icon.png").timeout(std::time::Duration::from_millis(100)).send().await;
    assert!(
        result.as_ref().is_err_and(|err| err.is_timeout() || err.is_connect()),
        "the direct path must be attempted: {result:?}"
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), proxy_task).await.is_err(),
        "resource client must not send requests through the configured proxy"
    );
}

#[tokio::test]
async fn public_resource_client_honours_the_configured_proxy() {
    // Counterpart of `no_redirect_resource_ignores_configured_proxy`: a resource hop that can leave the
    // local network must go through the configured proxy, or the fetch discloses this host's address.
    let app_state = create_test_app_state();
    let response_head = "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 3\r\nConnection: close\r\n\r\n";
    let (proxy_addr, proxy_task) = spawn_legacy_hls_test_origin(response_head.to_string(), b"png".to_vec()).await;
    app_state.app_config.config.store(Arc::new(Config {
        proxy: Some(crate::model::ProxyConfig { url: format!("http://{proxy_addr}"), username: None, password: None }),
        ..Config::default()
    }));
    let client = crate::api::model::create_resource_public_http_client_no_redirect(&app_state.app_config)
        .expect("resource HTTP client");

    let response = client.get("http://8.8.8.8/icon.png").send().await.expect("request reaches the proxy");

    assert_eq!(response.status(), StatusCode::OK);
    let request = tokio::time::timeout(std::time::Duration::from_secs(2), proxy_task)
        .await
        .expect("request must reach proxy")
        .expect("proxy task");
    assert!(request.starts_with("GET http://8.8.8.8/icon.png HTTP/1.1\r\n"), "{request}");
}

#[tokio::test]
async fn an_unresolved_resource_name_is_fetched_through_the_configured_proxy() {
    // A resource name that does not resolve locally is still resolvable through a configured proxy (for
    // example with remote DNS), so the fetch must not be pinned to the direct client.
    let app_state = create_test_app_state();
    let response_head = "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 3\r\nConnection: close\r\n\r\n";
    let (proxy_addr, proxy_task) = spawn_legacy_hls_test_origin(response_head.to_string(), b"png".to_vec()).await;
    app_state.app_config.config.store(Arc::new(Config {
        proxy: Some(crate::model::ProxyConfig { url: format!("http://{proxy_addr}"), username: None, password: None }),
        ..Config::default()
    }));
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("http://{proxy_addr}")).expect("test proxy"))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("mock proxy client");
    app_state.http_clients.resource_public_no_redirect.store(Arc::new(client));

    let response =
        resource_proxy_response(&app_state, "http://unresolved.invalid/logo.png", &HeaderMap::new(), None).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.into_body().collect().await.expect("image body").to_bytes(), Bytes::from_static(b"png"));
    let request = tokio::time::timeout(std::time::Duration::from_secs(2), proxy_task)
        .await
        .expect("request must reach proxy")
        .expect("proxy task");
    assert!(request.starts_with("GET http://unresolved.invalid/logo.png HTTP/1.1\r\n"), "{request}");
}

#[tokio::test]
async fn public_resource_client_reaches_a_proxy_on_loopback() {
    // A proxy is commonly configured on the loopback interface (`http://localhost:8118`), so the
    // connect-time guard of the public resource client must not reject the proxy host itself.
    let app_state = create_test_app_state();
    let response_head = "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 3\r\nConnection: close\r\n\r\n";
    let (proxy_addr, proxy_task) = spawn_legacy_hls_test_origin(response_head.to_string(), b"png".to_vec()).await;
    app_state.app_config.config.store(Arc::new(Config {
        proxy: Some(crate::model::ProxyConfig {
            url: format!("http://localhost:{}", proxy_addr.port()),
            username: None,
            password: None,
        }),
        ..Config::default()
    }));
    let client = crate::api::model::create_resource_public_http_client_no_redirect(&app_state.app_config)
        .expect("resource HTTP client");

    let response = client.get("http://8.8.8.8/icon.png").send().await.expect("request reaches the proxy");

    assert_eq!(response.status(), StatusCode::OK);
    let request = tokio::time::timeout(std::time::Duration::from_secs(2), proxy_task)
        .await
        .expect("request must reach proxy")
        .expect("proxy task");
    assert!(request.starts_with("GET http://8.8.8.8/icon.png HTTP/1.1\r\n"), "{request}");
}

#[tokio::test]
async fn no_redirect_resource_refuses_destinations_local_to_this_host() {
    let app_state = create_test_app_state();
    let (proxy_addr, proxy_task) =
        spawn_legacy_hls_test_origin("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_string(), Vec::new()).await;
    app_state.app_config.config.store(Arc::new(Config {
        proxy: Some(crate::model::ProxyConfig { url: format!("http://{proxy_addr}"), username: None, password: None }),
        ..Config::default()
    }));
    let client = crate::api::model::create_resource_http_client_no_redirect(&app_state.app_config)
        .expect("resource HTTP client");
    app_state.http_clients.resource_no_redirect.store(Arc::new(client));

    for local_only in ["http://127.0.0.1/icon.png", "http://169.254.169.254/latest/meta-data/", "http://[::1]/icon.png"]
    {
        let response =
            resource_response(&app_state, ResourceFetchPolicy::NonPublic, local_only, &HeaderMap::new(), None)
                .await
                .into_response();

        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{local_only}");
    }

    // The guard rejects before a request is built, so nothing may reach the configured proxy.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), proxy_task).await.is_err(),
        "no request may reach the proxy for local-only destinations"
    );
}

#[tokio::test]
async fn resource_redirect_hides_private_destination_and_upstream_location() {
    let app_state = create_test_app_state();
    let response_head = "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nLocation: http://10.0.0.2/private\r\nContent-Length: 3\r\nConnection: close\r\n\r\n";
    let (proxy_addr, proxy_task) = spawn_legacy_hls_test_origin(response_head.to_string(), b"png".to_vec()).await;
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("http://{proxy_addr}")).expect("test proxy"))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("mock upstream client");
    app_state.http_clients.resource_no_redirect.store(Arc::new(client));

    let private = resource_redirect_or_proxy(&app_state, "http://10.0.0.1/icon.png", &HeaderMap::new(), None).await;
    assert_eq!(private.status(), StatusCode::OK);
    assert!(private.headers().get("location").is_none());
    assert_eq!(private.into_body().collect().await.expect("private icon body").to_bytes(), Bytes::from_static(b"png"));
    let request = proxy_task.await.expect("mock request");
    assert!(request.starts_with("GET http://10.0.0.1/icon.png HTTP/1.1\r\n"));

    let public = resource_redirect_or_proxy(&app_state, "http://8.8.8.8/icon.png", &HeaderMap::new(), None).await;
    assert_eq!(public.status(), StatusCode::FOUND);
    assert_eq!(public.headers().get("location").and_then(|value| value.to_str().ok()), Some("http://8.8.8.8/icon.png"));

    let local_thumbnail =
        resource_redirect_or_proxy(&app_state, "/api/v1/library/thumbnail/item", &HeaderMap::new(), None).await;
    assert_eq!(local_thumbnail.status(), StatusCode::FOUND);
    assert_eq!(
        local_thumbnail.headers().get("location").and_then(|value| value.to_str().ok()),
        Some("/api/v1/library/thumbnail/item")
    );

    let blocked = resource_redirect_or_proxy(&app_state, "http://127.0.0.1/icon.png", &HeaderMap::new(), None).await;
    assert_eq!(blocked.status(), StatusCode::FORBIDDEN);
    assert!(blocked.headers().get("location").is_none());
}

#[test]
fn configured_resource_headers_are_limited_to_the_input_origin() {
    let input = ConfigInput { url: "http://10.0.0.1:8080/playlist.m3u".to_string(), ..ConfigInput::default() };
    assert!(resource_input_for_url(Some(&input), "http://10.0.0.1:8080/logo.png").is_some());
    assert!(resource_input_for_url(Some(&input), "http://10.0.0.2:8080/logo.png").is_none());
    assert!(resource_input_for_url(Some(&input), "http://10.0.0.1:8081/logo.png").is_none());
    assert!(resource_input_for_url(Some(&input), "https://10.0.0.1:8080/logo.png").is_none());
}

#[tokio::test]
async fn public_resource_is_fetched_through_the_configured_proxy() {
    // Regression lock: a resource fetch that leaves the local network must use the configured proxy,
    // otherwise it discloses the operator's address to the resource host. The Web UI resource route
    // reaches this path, because it wraps every icon of a public destination into a proxy link.
    let app_state = create_test_app_state();
    let response_head = "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 3\r\nConnection: close\r\n\r\n";
    let (proxy_addr, proxy_task) = spawn_legacy_hls_test_origin(response_head.to_string(), b"png".to_vec()).await;
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("http://{proxy_addr}")).expect("test proxy"))
        .build()
        .expect("mock proxy client");
    app_state.http_clients.resource_public_no_redirect.store(Arc::new(client));

    let response = resource_proxy_response(&app_state, "http://8.8.8.8/logo.png", &HeaderMap::new(), None).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.into_body().collect().await.expect("image body").to_bytes(), Bytes::from_static(b"png"));
    let request = tokio::time::timeout(std::time::Duration::from_secs(2), proxy_task)
        .await
        .expect("request must reach proxy")
        .expect("proxy task");
    assert!(request.starts_with("GET http://8.8.8.8/logo.png HTTP/1.1\r\n"), "{request}");
}

#[tokio::test]
async fn proxied_resource_follows_a_redirect_into_the_public_network_through_the_configured_proxy() {
    // Regression lock: the hop decides which client is used. A network-internal destination that
    // redirects to a public host must not make the follow-up request directly, or the redirect would
    // disclose the operator's address to that host despite a configured proxy.
    let app_state = create_test_app_state();
    let redirect_head =
        "HTTP/1.1 302 Found\r\nLocation: http://8.8.8.8/logo.png\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let (private_proxy_addr, private_proxy_task) =
        spawn_legacy_hls_test_origin(redirect_head.to_string(), Vec::new()).await;
    let private_client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("http://{private_proxy_addr}")).expect("test proxy"))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("mock private client");
    app_state.http_clients.resource_no_redirect.store(Arc::new(private_client));

    let response_head = "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 3\r\nConnection: close\r\n\r\n";
    let (public_proxy_addr, public_proxy_task) =
        spawn_legacy_hls_test_origin(response_head.to_string(), b"png".to_vec()).await;
    let public_client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("http://{public_proxy_addr}")).expect("test proxy"))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("mock public client");
    app_state.http_clients.resource_public_no_redirect.store(Arc::new(public_client));

    let response = resource_proxy_response(&app_state, "http://10.0.0.1/logo.png", &HeaderMap::new(), None).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.into_body().collect().await.expect("image body").to_bytes(), Bytes::from_static(b"png"));
    let first_hop = private_proxy_task.await.expect("first hop request");
    assert!(first_hop.starts_with("GET http://10.0.0.1/logo.png HTTP/1.1\r\n"), "{first_hop}");
    let second_hop = tokio::time::timeout(std::time::Duration::from_secs(2), public_proxy_task)
        .await
        .expect("public hop must be fetched")
        .expect("public hop request");
    assert!(second_hop.starts_with("GET http://8.8.8.8/logo.png HTTP/1.1\r\n"), "{second_hop}");
}

#[tokio::test]
async fn public_resource_redirect_to_a_destination_local_to_this_host_is_refused() {
    // Regression lock: a public host that redirects to loopback, link-local, or a cloud metadata
    // endpoint must not turn the resource route into a reader for this host. The redirect target is
    // never requested, and its location is never relayed to the client.
    let app_state = create_test_app_state();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("mock proxy binds");
    let proxy_addr = listener.local_addr().expect("mock proxy address");
    let proxy_task = tokio::spawn(async move {
        let mut requests = Vec::new();
        loop {
            let Ok(accepted) = tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept()).await
            else {
                break;
            };
            let Ok((mut socket, _)) = accepted else { break };
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0_u8; 1024];
                match socket.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => request.extend_from_slice(&chunk[..read]),
                }
            }
            requests.push(String::from_utf8_lossy(&request).into_owned());
            let head = "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1/secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(head.as_bytes()).await;
        }
        requests
    });
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("http://{proxy_addr}")).expect("test proxy"))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("mock client");
    app_state.http_clients.resource_public_no_redirect.store(Arc::new(client));

    let response = resource_proxy_response(&app_state, "http://8.8.8.8/logo.png", &HeaderMap::new(), None).await;

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(response.headers().get("location").is_none(), "the local destination must not be relayed");
    let requests = proxy_task.await.expect("mock proxy task");
    assert_eq!(requests.len(), 1, "the local destination must not be requested: {requests:?}");
}

#[tokio::test]
async fn no_redirect_resource_does_not_relay_upstream_error_details() {
    let app_state = create_test_app_state();
    let body = b"<html>internal service error</html>".to_vec();
    let response_head =
        format!("HTTP/1.1 404 Not Found\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n", body.len());
    let (proxy_addr, proxy_task) = spawn_legacy_hls_test_origin(response_head, body.clone()).await;
    app_state.app_config.config.store(Arc::new(Config {
        proxy: Some(crate::model::ProxyConfig { url: format!("http://{proxy_addr}"), username: None, password: None }),
        ..Config::default()
    }));
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("http://{proxy_addr}")).expect("test proxy"))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("mock upstream client");
    app_state.http_clients.resource_no_redirect.store(Arc::new(client));

    let response = resource_response(
        &app_state,
        ResourceFetchPolicy::NonPublic,
        "http://10.0.0.1/icon.png",
        &HeaderMap::new(),
        None,
    )
    .await
    .into_response();

    // The client learns that the fetch failed, not what the destination answered.
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let relayed = response.into_body().collect().await.expect("response body").to_bytes();
    assert!(relayed.is_empty(), "upstream error body must not be relayed: {relayed:?}");

    let request = tokio::time::timeout(std::time::Duration::from_secs(2), proxy_task)
        .await
        .expect("request must reach proxy")
        .expect("proxy task");
    assert!(request.starts_with("GET http://10.0.0.1/icon.png HTTP/1.1\r\n"), "{request}");
}

#[tokio::test]
async fn resource_client_refuses_names_that_resolve_to_local_addresses() {
    let app_state = create_test_app_state();
    let (origin_addr, _origin_task) =
        spawn_legacy_hls_test_origin("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_string(), Vec::new()).await;
    let guarded = crate::api::model::create_resource_http_client_no_redirect(&app_state.app_config)
        .expect("resource HTTP client");

    // `localhost` resolves to loopback on the client's DNS layer, so the request must fail while the
    // name is resolved and never reach the origin that is listening on that loopback address.
    let error = guarded
        .get(format!("http://localhost:{}/icon.png", origin_addr.port()))
        .send()
        .await
        .expect_err("a name resolving to a local address must be refused");

    // reqwest only reports the outer failure, so the cause chain has to be walked to see the refusal.
    let mut causes = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        let _ = write!(causes, " | {cause}");
        source = cause.source();
    }
    assert!(causes.contains("local to this host"), "{causes}");
}
