use crate::api::model::ConnectionManager;
use axum::{body::Body, extract::Request, response::Response};
use futures::FutureExt;
use hyper::body::Incoming;
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder,
    service::TowerToHyperService,
};
use log::{debug, error, trace};
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
    M: for<'a> Service<IncomingStream, Error = Infallible, Response = S> + Send + 'static,
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
    // When a client changes IP (e.g. WiFi → 4G) the old TCP connection dies
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
        let mut conn = pin!(builder.serve_connection_with_upgrades(io, hyper_service));
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
                Ok(msg) = addr_close_rx.recv() => {
                    if msg == addr {
                        connection_manager_clone.release_connection(&addr).await;
                        debug!("Forced client disconnect {msg}");
                        conn.as_mut().graceful_shutdown();
                        break;
                    }
                }
            }
        }
    });
}
