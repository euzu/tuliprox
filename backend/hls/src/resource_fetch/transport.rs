use super::{HlsOriginResourceBodyDeadline, HlsOriginResourceFetchError};
use hyper_util::client::legacy::connect::capture_connection;
use std::time::Duration;

/// Starts the transfer clock at Hyper's connection handoff, including pool reuse.
/// A redirect keeps the first deadline even when it needs another connection.
pub(super) async fn send(
    client: &reqwest::Client,
    request: reqwest::Request,
    timeout: Duration,
    deadline: &mut Option<HlsOriginResourceBodyDeadline>,
) -> Result<reqwest::Response, HlsOriginResourceFetchError> {
    let mut request: axum::http::Request<reqwest::Body> =
        request.try_into().map_err(|error| transport_error(&error))?;
    let mut capture = capture_connection(&mut request);
    let request = reqwest::Request::try_from(request).map_err(|error| transport_error(&error))?;
    let response = client.execute(request);
    tokio::pin!(response);
    if deadline.is_none() {
        tokio::select! {
            result = &mut response => {
                let response = result.map_err(|error| transport_error(&error))?;
                if capture.connection_metadata().is_none() {
                    return Err(HlsOriginResourceFetchError::Transport("HLS connection metadata missing".into()));
                }
                *deadline = Some(HlsOriginResourceBodyDeadline::new(timeout));
                return Ok(response);
            }
            connected = async { capture.wait_for_connection_metadata().await.is_some() } => {
                if !connected {
                    return Err(HlsOriginResourceFetchError::Transport("HLS connection metadata unavailable".into()));
                }
                *deadline = Some(HlsOriginResourceBodyDeadline::new(timeout));
            }
        }
    }
    let deadline =
        deadline.ok_or_else(|| HlsOriginResourceFetchError::Transport("HLS connection deadline unavailable".into()))?;
    tokio::time::timeout_at(deadline.deadline(), response)
        .await
        .map_err(|_| HlsOriginResourceFetchError::Timeout)?
        .map_err(|error| transport_error(&error))
}

fn transport_error(error: &reqwest::Error) -> HlsOriginResourceFetchError {
    HlsOriginResourceFetchError::Transport(shared::utils::sanitize_sensitive_info(&error.to_string()).to_string())
}

#[cfg(test)]
mod tests {
    use axum::http;
    use hyper_util::client::legacy::connect::capture_connection;
    use std::time::Duration;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::oneshot,
    };

    #[tokio::test]
    async fn reqwest_captures_cold_and_pooled_connection_before_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (first_tx, first_rx) = oneshot::channel();
        let (second_tx, second_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            // Both requests must use this one connection, including the second
            // request's independent capture extension.
            let (mut socket, _) = listener.accept().await.unwrap();
            for release in [first_rx, second_rx] {
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(socket.read_u8().await.unwrap());
                }
                release.await.unwrap();
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await.unwrap();
            }
        });
        let client = reqwest::Client::builder().http1_only().build().unwrap();
        for release in [first_tx, second_tx] {
            let request = client.get(format!("http://{address}/segment.ts")).build().unwrap();
            let mut request: http::Request<reqwest::Body> = request.try_into().unwrap();
            let mut capture = capture_connection(&mut request);
            let response = client.execute(reqwest::Request::try_from(request).unwrap());
            tokio::pin!(response);
            tokio::time::timeout(Duration::from_secs(3), async {
                tokio::select! {
                    result = &mut response => panic!("headers must remain gated: {result:?}"),
                    metadata = capture.wait_for_connection_metadata() => assert!(metadata.is_some()),
                }
            })
            .await
            .expect("connection capture must precede response headers");
            release.send(()).unwrap();
            let response = tokio::time::timeout(Duration::from_secs(3), response).await.unwrap().unwrap();
            assert_eq!(response.status(), http::StatusCode::OK);
            assert!(response.bytes().await.unwrap().is_empty());
        }
        server.await.unwrap();
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use std::sync::Arc;
    use tokio::{
        io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    async fn read_headers(stream: &mut (impl AsyncRead + Unpin)) -> Vec<u8> {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            bytes.push(stream.read_u8().await.unwrap());
        }
        bytes
    }

    #[tokio::test]
    async fn missing_partial_and_dripping_headers_expire_on_cold_and_pooled_connections() {
        for pooled in [false, true] {
            for (prefix, drip) in [
                (b"".as_slice(), false),
                (b"HTTP/1.1 200 OK\r\nX-Slow: ", false),
                (b"HTTP/1.1 200 OK\r\nX-Slow: ", true),
            ] {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let server = tokio::spawn(async move {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    if pooled {
                        read_headers(&mut socket).await;
                        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await.unwrap();
                    }
                    read_headers(&mut socket).await;
                    socket.write_all(prefix).await.unwrap();
                    if !drip {
                        std::future::pending::<()>().await;
                    }
                    loop {
                        tokio::time::sleep(Duration::from_millis(80)).await;
                        if socket.write_all(b"a").await.is_err() {
                            break;
                        }
                    }
                });
                let client = reqwest::Client::builder().no_proxy().http1_only().build().unwrap();
                let url = format!("http://{address}/segment");
                if pooled {
                    assert_eq!(client.get(&url).send().await.unwrap().bytes().await.unwrap().len(), 0);
                }
                let mut deadline = None;
                let start = tokio::time::Instant::now();
                let result =
                    send(&client, client.get(&url).build().unwrap(), Duration::from_millis(350), &mut deadline).await;
                assert!(matches!(result, Err(HlsOriginResourceFetchError::Timeout)), "{result:?}");
                assert!(start.elapsed() < Duration::from_secs(2));
                assert!(deadline.is_some());
                server.abort();
            }
        }
    }

    #[tokio::test]
    async fn delayed_tls_and_proxy_connect_do_not_consume_fetch_budget() {
        for proxy in [false, true] {
            let cert = rcgen::generate_simple_self_signed(vec!["origin.test".into()]).unwrap();
            let config = tokio_rustls::rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    vec![cert.cert.der().clone()],
                    tokio_rustls::rustls::pki_types::PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into()),
                )
                .unwrap();
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                if proxy {
                    let request = read_headers(&mut socket).await;
                    assert!(request.starts_with(b"CONNECT origin.test:"));
                }
                tokio::time::sleep(Duration::from_millis(600)).await;
                if proxy {
                    socket.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await.unwrap();
                }
                let mut tls = acceptor.accept(socket).await.unwrap();
                read_headers(&mut tls).await;
                tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await.unwrap();
            });
            let mut builder = reqwest::Client::builder()
                .http1_only()
                .tls_danger_accept_invalid_certs(true)
                .connect_timeout(Duration::from_secs(3));
            builder = if proxy {
                builder.proxy(reqwest::Proxy::all(format!("http://{address}")).unwrap())
            } else {
                builder.no_proxy().resolve("origin.test", address)
            };
            let client = builder.build().unwrap();
            let url = format!("https://origin.test:{}/segment", address.port());
            let mut deadline = None;
            let start = tokio::time::Instant::now();
            let response = send(&client, client.get(url).build().unwrap(), Duration::from_millis(250), &mut deadline)
                .await
                .unwrap();
            assert_eq!(response.bytes().await.unwrap().as_ref(), b"ok");
            assert!(start.elapsed() >= Duration::from_millis(600));
            assert!(deadline.unwrap().remaining() > Duration::from_millis(100));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn connect_timeout_remains_a_transport_error_without_transfer_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .tls_danger_accept_invalid_certs(true)
            .connect_timeout(Duration::from_millis(350))
            .build()
            .unwrap();
        let mut deadline = None;
        let result = send(
            &client,
            client.get(format!("https://{address}/segment")).build().unwrap(),
            Duration::from_millis(100),
            &mut deadline,
        )
        .await;
        assert!(matches!(result, Err(HlsOriginResourceFetchError::Transport(_))));
        assert!(deadline.is_none());
        server.abort();
    }
    #[tokio::test]
    async fn headers_spend_body_budget_and_regular_chunks_do_not_extend_it() {
        for timely_eof in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_headers(&mut socket).await;
                tokio::time::sleep(Duration::from_millis(250)).await;
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\n").await.unwrap();
                for _ in 0..4 {
                    tokio::time::sleep(Duration::from_millis(if timely_eof { 25 } else { 150 })).await;
                    let _ = socket.write_all(b"x").await;
                }
            });
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let mut deadline = None;
            let response = send(
                &client,
                client.get(format!("http://{address}/segment")).build().unwrap(),
                Duration::from_millis(600),
                &mut deadline,
            )
            .await
            .unwrap();
            let deadline = deadline.unwrap();
            assert!(deadline.remaining() < Duration::from_millis(450));
            let result = tokio::time::timeout_at(deadline.deadline(), response.bytes()).await;
            assert_eq!(result.is_ok(), timely_eof);
            server.abort();
        }
    }

    #[tokio::test]
    async fn parallel_requests_keep_independent_connection_signals_and_deadlines() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    read_headers(&mut socket).await;
                    tokio::time::sleep(Duration::from_millis(350)).await;
                    let _ =
                        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                });
            }
        });
        let client = reqwest::Client::builder().no_proxy().http1_only().build().unwrap();
        let fetch = |timeout_ms| {
            let client = client.clone();
            async move {
                let mut deadline = None;
                send(
                    &client,
                    client.get(format!("http://{address}/segment")).build().unwrap(),
                    Duration::from_millis(timeout_ms),
                    &mut deadline,
                )
                .await
            }
        };
        let (short, long) = tokio::join!(fetch(150), fetch(900));
        assert!(matches!(short, Err(HlsOriginResourceFetchError::Timeout)));
        assert!(long.is_ok());
        server.await.unwrap();
    }
}
