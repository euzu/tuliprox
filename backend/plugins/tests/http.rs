//! Outbound `wasi:http` from a guest, against local mock servers.
//!
//! The plugin client is caller-supplied, so the "public" case maps a made-up
//! public hostname onto the loopback mock with `ClientBuilder::resolve`. The
//! private case uses the host's own RFC 1918 address when there is one.

mod common;

use common::{first_title, items, load_probe, probe_entry, run_stage};
use serde_json::{json, Value};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use tuliprox_plugins::PluginServices;

/// Larger than the 16 MiB response bound.
const OVERSIZED: usize = 17 * 1024 * 1024;

/// A minimal HTTP/1.1 server: answers every connection with 200 and `body`.
struct MockServer {
    port: u16,
    connections: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl MockServer {
    async fn start(bind: &str, body: Vec<u8>) -> Self {
        let listener = TcpListener::bind(bind).await.expect("bind mock server");
        let port = listener.local_addr().expect("mock server address").port();
        let connections = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&connections);
        let body = Arc::new(body);
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                let body = Arc::clone(&body);
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0_u8; 1024];
                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => request.extend_from_slice(&chunk[..read]),
                        }
                    }
                    let head =
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    // The host may stop reading an oversized body; a failed write is expected then.
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self { port, connections, task }
    }

    fn connections(&self) -> usize { self.connections.load(Ordering::SeqCst) }
}

impl Drop for MockServer {
    fn drop(&mut self) { self.task.abort(); }
}

/// The host's RFC 1918 IPv4 address, from the route the OS picks. `None` when
/// the host has no private address (CI, or no network).
fn lan_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    // Connecting a UDP socket only selects the outgoing interface; nothing is sent.
    socket.connect("192.0.2.1:9").ok()?;
    let IpAddr::V4(ip) = socket.local_addr().ok()?.ip() else { return None };
    let [first, second, ..] = ip.octets();
    let private = first == 10 || (first == 172 && (16..32).contains(&second)) || (first == 192 && second == 168);
    private.then_some(ip)
}

fn fetch_settings(url: &str) -> Value { json!({ "mode": "http", "url": url }) }

#[tokio::test(flavor = "multi_thread")]
async fn loopback_destinations_are_reachable() {
    let server = MockServer::start("127.0.0.1:0", b"ok".to_vec()).await;
    let url = format!("http://127.0.0.1:{}/probe", server.port);
    let Some(probe) = load_probe(vec![probe_entry("probe-net", fetch_settings(&url))], |data| {
        PluginServices::new(reqwest::Client::new(), data)
    }) else {
        return;
    };

    let mut items = items(1);
    run_stage(&probe.system, &mut items).await;

    assert_eq!(first_title(&items), "http:200");
    assert_eq!(server.connections(), 1, "the guest reached the mock");
}

#[tokio::test(flavor = "multi_thread")]
async fn public_destinations_are_reachable_through_the_plugin_client() {
    let server = MockServer::start("127.0.0.1:0", b"ok".to_vec()).await;
    // A public name, resolved by the client to the loopback mock.
    let client = reqwest::Client::builder()
        .resolve("public.probe.test", SocketAddr::from(([127, 0, 0, 1], server.port)))
        .build()
        .expect("plugin client");
    let url = format!("http://public.probe.test:{}/probe", server.port);
    let Some(probe) =
        load_probe(vec![probe_entry("probe-net", fetch_settings(&url))], |data| PluginServices::new(client, data))
    else {
        return;
    };

    let mut items = items(1);
    run_stage(&probe.system, &mut items).await;

    assert_eq!(first_title(&items), "http:200");
    assert_eq!(server.connections(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn private_lan_destinations_are_reachable() {
    let Some(lan) = lan_ipv4() else {
        eprintln!("skipped: the host has no private IPv4 address");
        return;
    };
    let server = MockServer::start("0.0.0.0:0", b"ok".to_vec()).await;
    let url = format!("http://{lan}:{}/probe", server.port);
    let Some(probe) = load_probe(vec![probe_entry("probe-net", fetch_settings(&url))], |data| {
        PluginServices::new(reqwest::Client::new(), data)
    }) else {
        return;
    };

    let mut items = items(1);
    run_stage(&probe.system, &mut items).await;

    assert_eq!(first_title(&items), "http:200");
    assert_eq!(server.connections(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn http_is_denied_without_the_network_capability() {
    let server = MockServer::start("127.0.0.1:0", b"ok".to_vec()).await;
    let url = format!("http://127.0.0.1:{}/probe", server.port);
    // probe-bare grants no `network`, so the host never sends the request.
    let Some(probe) = load_probe(vec![probe_entry("probe-bare", fetch_settings(&url))], |data| {
        PluginServices::new(reqwest::Client::new(), data)
    }) else {
        return;
    };

    let mut items = items(1);
    run_stage(&probe.system, &mut items).await;

    assert_eq!(first_title(&items), "http:err:http-request-denied");
    assert_eq!(server.connections(), 0, "no connection was attempted");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_response_over_16_mib_is_rejected_with_a_size_error() {
    let server = MockServer::start("127.0.0.1:0", vec![b'a'; OVERSIZED]).await;
    let url = format!("http://127.0.0.1:{}/big", server.port);
    let Some(probe) = load_probe(vec![probe_entry("probe-net", fetch_settings(&url))], |data| {
        PluginServices::new(reqwest::Client::new(), data)
    }) else {
        return;
    };

    let mut items = items(1);
    run_stage(&probe.system, &mut items).await;

    assert_eq!(first_title(&items), "http:err:http-response-body-size");
}
