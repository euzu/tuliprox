use rcgen::generate_simple_self_signed;
use rustls::{
    crypto::aws_lc_rs::sign::any_supported_type,
    pki_types::PrivateKeyDer,
    server::{ClientHello, ResolvesServerCert, ServerConfig},
    sign::CertifiedKey,
};
use std::{fmt, io, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_rustls::TlsAcceptor;

#[derive(Clone)]
struct StrictSniResolver {
    expected_host: String,
    cert: Arc<CertifiedKey>,
}

impl fmt::Debug for StrictSniResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StrictSniResolver").field("expected_host", &self.expected_host).finish()
    }
}

impl ResolvesServerCert for StrictSniResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        (client_hello.server_name() == Some(self.expected_host.as_str())).then(|| Arc::clone(&self.cert))
    }
}

fn create_tls_acceptor(expected_host: &str) -> io::Result<TlsAcceptor> {
    let generated = generate_simple_self_signed(vec![expected_host.to_string()])
        .map_err(|err| io::Error::other(format!("failed to create self-signed cert: {err}")))?;

    let cert_der = generated.cert.der().clone();
    let key_der = PrivateKeyDer::Pkcs8(generated.signing_key.serialize_der().into());
    let signing_key =
        any_supported_type(&key_der).map_err(|err| io::Error::other(format!("failed to create signing key: {err}")))?;
    let certified_key = Arc::new(CertifiedKey::new(vec![cert_der], signing_key));

    let resolver = Arc::new(StrictSniResolver { expected_host: expected_host.to_string(), cert: certified_key });

    let mut config = ServerConfig::builder().with_no_client_auth().with_cert_resolver(resolver);
    config.alpn_protocols.push(b"http/1.1".to_vec());

    Ok(TlsAcceptor::from(Arc::new(config)))
}

async fn start_tls_server(expected_host: &str) -> io::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let acceptor = create_tls_acceptor(expected_host)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    let handle = tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                continue;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls_stream) = acceptor.accept(socket).await else {
                    return;
                };

                let mut request = vec![0_u8; 4096];
                let _ = tls_stream.read(&mut request).await;
                let _ =
                    tls_stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok").await;
                let _ = tls_stream.shutdown().await;
            });
        }
    });

    Ok((addr, handle))
}

#[tokio::test]
async fn https_ip_connect_uses_hostname_sni_with_resolve_to_addrs() {
    let expected_host = "sni-test.local";
    let wrong_host = "wrong-sni.local";

    let (server_addr, handle) = start_tls_server(expected_host).await.expect("tls server should start");

    let client_ok = reqwest::Client::builder()
        .no_proxy()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(5))
        .resolve_to_addrs(expected_host, &[server_addr])
        .build()
        .expect("reqwest client should build");

    let ok_url = format!("https://{expected_host}:{}/health", server_addr.port());
    let ok_response = client_ok.get(ok_url).send().await.expect("request with matching SNI should succeed");
    assert_eq!(ok_response.status(), reqwest::StatusCode::OK);

    let client_wrong = reqwest::Client::builder()
        .no_proxy()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(5))
        .resolve_to_addrs(wrong_host, &[server_addr])
        .build()
        .expect("reqwest client should build");

    let wrong_url = format!("https://{wrong_host}:{}/health", server_addr.port());
    let wrong_result = client_wrong.get(wrong_url).send().await;
    assert!(wrong_result.is_err(), "request with wrong SNI must fail TLS handshake");

    handle.abort();
}
