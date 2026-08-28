use crate::api::model::{CloseConnectionSignal, ConnectionManager};
use axum::{body::Body, extract::Request, response::Response};
use futures::FutureExt;
use hyper::body::Incoming;
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder,
    service::TowerToHyperService,
};
use log::{debug, error, trace};
use shared::model::DisconnectReason;
use socket2::{SockRef, TcpKeepalive};
use std::{convert::Infallible, fmt::Debug, net::SocketAddr, pin::pin, sync::Arc, time::Duration};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tower::{Service, ServiceExt};

#[derive(Debug)]
struct IncomingStream {
    remote_addr: SocketAddr,
}

impl IncomingStream {
    /// Returns the remote address that this stream is bound to.
    pub fn remote_addr(&self) -> &SocketAddr { &self.remote_addr }
}

impl axum::extract::connect_info::Connected<IncomingStream> for SocketAddr {
    fn connect_info(target: IncomingStream) -> SocketAddr { *target.remote_addr() }
}

pub async fn serve(
    listener: tokio::net::TcpListener,
    router: axum::Router<()>,
    cancel_token: Option<CancellationToken>,
    connection_manager: &Arc<ConnectionManager>,
) {
    let (signal_tx, _signal_rx) = watch::channel(());
    let mut make_service = router.into_make_service_with_connect_info::<SocketAddr>();

    match cancel_token {
        Some(token) => loop {
            tokio::select! {
                () = token.cancelled() => {
                    break;
                }
                accept_result = listener.accept() => {
                    let Ok((socket, remote_addr)) = accept_result else { continue };
                    handle_connection(&mut make_service, &signal_tx, socket, remote_addr, Arc::clone(connection_manager)).await;
                }
            }
        },
        None => loop {
            let Ok((socket, remote_addr)) = listener.accept().await else { continue };
            handle_connection(&mut make_service, &signal_tx, socket, remote_addr, Arc::clone(connection_manager)).await;
        },
    }
}

async fn handle_connection<M, S>(
    make_service: &mut M,
    signal_tx: &watch::Sender<()>,
    socket: tokio::net::TcpStream,
    remote_addr: SocketAddr,
    connection_manager: Arc<ConnectionManager>,
) where
    M: Service<IncomingStream, Error = Infallible, Response = S> + Send + 'static,
    for<'a> <M as Service<IncomingStream>>::Future: Send,
    S: Service<Request, Response = Response, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send,
{
    let Ok(tcp_stream_std) = socket.into_std() else {
        return;
    };
    //tcp_stream_std.set_nonblocking(true).ok(); // this is not necessary

    // Configure keep alive with socket2
    let sock_ref = SockRef::from(&tcp_stream_std);

    let keep_alive_first_probe = 10;
    let keep_alive_interval = 5;

    let mut keepalive = TcpKeepalive::new();
    keepalive = keepalive
        .with_time(Duration::from_secs(keep_alive_first_probe)) // Time until the first keepalive probe (idle time)
        .with_interval(Duration::from_secs(keep_alive_interval)); // Interval between keep alives
    #[cfg(not(target_os = "windows"))]
    {
        let keep_alive_retries = 3;
        keepalive = keepalive.with_retries(keep_alive_retries); // Number of failed probes before the connection is closed
    }

    if let Err(e) = sock_ref.set_tcp_keepalive(&keepalive) {
        error!("Failed to set keepalive for {remote_addr}: {e}");
    }

    // TCP_USER_TIMEOUT: max time (ms) that transmitted data may remain
    // unacknowledged before the kernel forcibly closes the connection.
    //
    // TCP keepalive only fires on *idle* connections and therefore does NOT
    // help for active live-streams where the server sends data continuously.
    // When a client changes IP (e.g. WiFi -> 4G) the old TCP connection dies
    // without a FIN; without this option the kernel retransmits with
    // exponential back-off for 2–15 minutes before giving up, holding the
    // user connection slot occupied the entire time.
    //
    // With TCP_USER_TIMEOUT = 30 s the kernel closes the dead connection
    // after at most 30 s of unacknowledged data, freeing the slot promptly.
    #[cfg(target_os = "linux")]
    if let Err(e) = sock_ref.set_tcp_user_timeout(Some(Duration::from_secs(30))) {
        error!("Failed to set TCP_USER_TIMEOUT for {remote_addr}: {e}");
    }

    let Ok(socket) = tokio::net::TcpStream::from_std(tcp_stream_std) else {
        return;
    };

    let io = TokioIo::new(socket);
    trace!("connection {remote_addr:?} accepted");

    make_service.ready().await.unwrap_or_else(|err| match err {});

    let tower_service = make_service
        .call(IncomingStream {
            // io: &io,
            remote_addr,
        })
        .await
        .unwrap_or_else(|err| match err {})
        .map_request(|req: Request<Incoming>| req.map(Body::new));

    let hyper_service = TowerToHyperService::new(tower_service);
    let signal_tx = signal_tx.clone();
    let addr = remote_addr;

    tokio::spawn(async move {
        #[allow(unused_mut)]
        let mut builder = Builder::new(TokioExecutor::new());
        // Pin<Box<T>> is Unpin, so conn is moveable and can be awaited without extra Pin<> wrappers.
        let mut conn = Box::pin(builder.serve_connection_with_upgrades(io, hyper_service));
        let mut signal_closed = pin!(signal_tx.closed().fuse());

        let connection_manager_clone = Arc::clone(&connection_manager);
        let mut addr_close_rx = connection_manager_clone.get_close_connection_channel();

        trace!("Connection opened: {addr}");
        connection_manager.add_connection(&addr).await;

        loop {
            tokio::select! {
                result = conn.as_mut() => {
                    if let Err(err) = result {
                        trace!("failed to serve connection: {err:#}");
                    }
                    connection_manager_clone.release_connection(&addr).await;
                    break;
                }
                () = &mut signal_closed => {
                    connection_manager_clone.release_connection(&addr).await;
                    debug!("Connection gracefully closed: {remote_addr}");
                    conn.as_mut().graceful_shutdown();
                }
                Ok(signal) = addr_close_rx.recv() => {
                    match signal {
                        CloseConnectionSignal::WithReason(msg, reason) if msg == addr => {
                            debug!("Forced client close {msg} reason={reason:?}");
                            if matches!(reason, DisconnectReason::ClientKicked) {
                                connection_manager_clone.release_user_sessions_only(&addr).await;
                                connection_manager_clone.release_provider_deferred(&addr).await;
                            } else {
                                connection_manager_clone.release_connection_with_reason(&addr, reason).await;
                            }
                            conn.as_mut().graceful_shutdown();
                            break;
                        }
                        CloseConnectionSignal::WithReason(..) => {
                            trace!("Ignored CloseConnectionSignal for a different connection");
                        }
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::serve;
    use crate::{
        api::model::{
            create_active_client_stream, create_test_app_state, ActiveClientStreamParams, AppState, ConnectionKind,
            StreamDetails, StreamError,
        },
        auth::Fingerprint,
        model::{Config, GracePeriodOptions, ProxyUserCredentials},
    };
    use axum::{
        body::Body,
        extract::{ConnectInfo, State},
        http::HeaderMap,
        response::Response,
        routing::get,
        Router,
    };
    use bytes::Bytes;
    use futures::Stream;
    use shared::{
        model::{PlaylistItemType, StreamChannel, UserConnectionPermission, XtreamCluster},
        utils::Internable,
    };
    use socket2::SockRef;
    use std::{
        pin::Pin,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        task::{Context, Poll},
        time::Duration,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_util::sync::CancellationToken;

    #[derive(Clone)]
    struct DisconnectTestState {
        app_state: Arc<AppState>,
        upstream_dropped: Arc<AtomicBool>,
    }

    struct PendingDropProbeStream {
        dropped: Arc<AtomicBool>,
    }

    impl Stream for PendingDropProbeStream {
        type Item = Result<Bytes, StreamError>;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> { Poll::Pending }
    }

    impl Drop for PendingDropProbeStream {
        fn drop(&mut self) { self.dropped.store(true, Ordering::Release); }
    }

    async fn pending_direct_series_response(
        State(state): State<DisconnectTestState>,
        ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    ) -> Response {
        let mut user = ProxyUserCredentials::default();
        user.username = "socket-series-user".to_string();
        user.max_connections = 1;
        let fingerprint = Fingerprint::new(format!("socket-{addr}"), addr.ip().to_string(), addr);
        let stream_channel = StreamChannel {
            target_id: 1,
            virtual_id: 1,
            provider_id: 1,
            input_name: "input".intern(),
            item_type: PlaylistItemType::Series,
            cluster: XtreamCluster::Series,
            group: "Series".intern(),
            title: "Episode".intern(),
            url: "http://provider.example/series/1.mkv".intern(),
            shared: false,
            shared_joined_existing: None,
            shared_stream_id: None,
            technical: None,
            epg_channel_id: None,
            epg_reference_ts: None,
            upstream_user_agent: None,
        };
        let upstream = PendingDropProbeStream { dropped: Arc::clone(&state.upstream_dropped) };
        let stream_details = StreamDetails::from_stream(Box::pin(upstream), GracePeriodOptions::default());
        let stream = create_active_client_stream(ActiveClientStreamParams {
            stream_details,
            app_state: &state.app_state,
            user: &user,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: ConnectionKind::Normal,
            fingerprint: &fingerprint,
            stream_channel,
            socket_bound: false,
            session_token: None,
            req_headers: &HeaderMap::new(),
            meter_uid: 0,
            meter_stream: false,
        })
        .await;
        Response::new(Body::from_stream(stream))
    }

    #[derive(Clone, Copy)]
    enum ClientDisconnect {
        Fin,
        Reset,
    }

    async fn assert_socket_disconnect_cleans_direct_series(disconnect: ClientDisconnect) {
        let app_state = create_test_app_state(Config::default());
        let upstream_dropped = Arc::new(AtomicBool::new(false));
        let router =
            Router::new().route("/series", get(pending_direct_series_response)).with_state(DisconnectTestState {
                app_state: Arc::clone(&app_state),
                upstream_dropped: Arc::clone(&upstream_dropped),
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("test listener");
        let server_addr = listener.local_addr().expect("listener address");
        let server_cancel = CancellationToken::new();
        let server_cancel_task = server_cancel.clone();
        let connection_manager = Arc::clone(&app_state.connection_manager);
        let server = tokio::spawn(async move {
            serve(listener, router, Some(server_cancel_task), &connection_manager).await;
        });

        let mut client = tokio::net::TcpStream::connect(server_addr).await.expect("test client connection");
        client
            .write_all(b"GET /series HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n")
            .await
            .expect("test request");
        let mut response_head = [0_u8; 1024];
        let read = tokio::time::timeout(Duration::from_secs(1), client.read(&mut response_head))
            .await
            .expect("response head timeout")
            .expect("response head read");
        assert!(read > 0, "streaming response should begin before client disconnects");
        assert_eq!(app_state.active_users.active_users_and_connections().await, (1, 1));
        assert_eq!(app_state.active_users.active_streams().await.len(), 1);

        match disconnect {
            ClientDisconnect::Fin => {
                client.shutdown().await.expect("client FIN");
                drop(client);
            }
            ClientDisconnect::Reset => {
                let client = client.into_std().expect("convert test client to std socket");
                SockRef::from(&client).set_linger(Some(Duration::ZERO)).expect("configure reset-on-close");
                drop(client);
            }
        }

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if app_state.active_users.active_users_and_connections().await == (0, 0)
                    && app_state.active_users.active_streams().await.is_empty()
                    && upstream_dropped.load(Ordering::Acquire)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("socket disconnect should release registry state and drop the upstream");
        assert_eq!(app_state.active_provider.active_connections().await.unwrap_or_default().values().sum::<usize>(), 0);

        server_cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("server shutdown timeout")
            .expect("server task");
    }

    #[tokio::test]
    async fn client_fin_cleans_direct_series_response() {
        assert_socket_disconnect_cleans_direct_series(ClientDisconnect::Fin).await;
    }

    #[tokio::test]
    async fn client_reset_cleans_direct_series_response() {
        assert_socket_disconnect_cleans_direct_series(ClientDisconnect::Reset).await;
    }
}
