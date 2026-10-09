use super::{
    send_input_with_retry_and_provider_policy_with_options_result, test_input_source, test_retry_config,
    RequestFetchOptions, STREAM_IDLE_TIMEOUT,
};
use std::{
    io::ErrorKind,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{io::AsyncReadExt, net::TcpListener, sync::oneshot};
use url::Url;

pub(in crate::utils::network::request::tests) async fn start_hanging_http_server(
) -> std::io::Result<(SocketAddr, oneshot::Receiver<()>, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let (request_seen_tx, request_seen_rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut request = Vec::new();
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut chunk = [0_u8; 2048];
            let Ok(read) = socket.read(&mut chunk).await else {
                return;
            };
            if read == 0 {
                return;
            }
            request.extend_from_slice(&chunk[..read]);
        }
        let _ = request_seen_tx.send(());
        std::future::pending::<()>().await;
        drop(socket);
    });

    Ok((addr, request_seen_rx, handle))
}

#[tokio::test(start_paused = true)]
async fn provider_failover_only_default_options_keep_default_idle_timeout() {
    let (addr, request_seen, server) = start_hanging_http_server().await.expect("hanging origin should start");
    let url = Url::parse(&format!("http://{addr}/manifest.m3u8")).expect("origin URL");
    let input = test_input_source(url.to_string(), None);
    let app_config = test_retry_config(5);
    let client = reqwest::Client::builder().no_proxy().build().expect("test client");
    let request_url = url.clone();
    let request = tokio::spawn(async move {
        send_input_with_retry_and_provider_policy_with_options_result(
            &app_config,
            &client,
            &input,
            None,
            &request_url,
            RequestFetchOptions::default().without_resource_retries(),
        )
        .await
    });

    // A ready task prevents Tokio's paused clock from auto-advancing through the default timeout while the local
    // TCP handshake is still in progress. After the request arrives, this guard stops and the test advances time
    // explicitly to the production deadline.
    let hold_virtual_time = Arc::new(AtomicBool::new(true));
    let hold_virtual_time_for_task = Arc::clone(&hold_virtual_time);
    let virtual_time_guard = tokio::spawn(async move {
        while hold_virtual_time_for_task.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    });
    let request_seen = request_seen.await;
    hold_virtual_time.store(false, Ordering::SeqCst);
    virtual_time_guard.await.expect("virtual time guard should stop");
    request_seen.expect("origin should receive the request");
    tokio::time::advance(Duration::from_secs(STREAM_IDLE_TIMEOUT + 1)).await;
    tokio::task::yield_now().await;
    if !request.is_finished() {
        request.abort();
        panic!("provider-only request lost the default idle-timeout guard");
    }

    let Err(error) = request.await.expect("request task should join") else {
        panic!("hanging request must time out");
    };
    assert_eq!(error.kind(), ErrorKind::TimedOut);
    server.abort();
}
