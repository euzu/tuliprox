use crate::origin_events::{redact_path_and_query, BodyCloseReason, OriginEvent, OriginEventKind, OriginStats};
use pin_project_lite::pin_project;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::SocketAddr,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Instant,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::{oneshot, Mutex},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginLimitMode {
    ObserveOnly,
    RejectNew,
    EvictOldest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginPolicy {
    pub account_limit: Option<usize>,
    pub limit_mode: OriginLimitMode,
}

impl Default for OriginPolicy {
    fn default() -> Self { Self { account_limit: None, limit_mode: OriginLimitMode::ObserveOnly } }
}

#[derive(Clone, Debug)]
pub struct OriginConnectionMeta {
    pub conn_id: u64,
    pub remote_addr: SocketAddr,
}

struct ActiveConnection {
    conn_id: u64,
    #[allow(dead_code)]
    remote_addr: SocketAddr,
    connected_at: Instant,
    active_body_req_id: Option<u64>,
}

#[derive(Clone)]
pub struct OriginTracker {
    run_id: String,
    policy: Arc<Mutex<OriginPolicy>>,
    events: Arc<Mutex<Vec<OriginEvent>>>,
    stats: Arc<Mutex<OriginStats>>,
    active_conns: Arc<Mutex<HashMap<u64, ActiveConnection>>>,
    eviction_channels: Arc<Mutex<HashMap<u64, oneshot::Sender<()>>>>,
    seq_counter: Arc<AtomicU64>,
    conn_counter: Arc<AtomicU64>,
    req_counter: Arc<AtomicU64>,
}

impl OriginTracker {
    #[must_use]
    pub fn new(run_id: impl Into<String>, policy: OriginPolicy) -> Self {
        Self {
            run_id: run_id.into(),
            policy: Arc::new(Mutex::new(policy)),
            events: Arc::new(Mutex::new(Vec::new())),
            stats: Arc::new(Mutex::new(OriginStats::default())),
            active_conns: Arc::new(Mutex::new(HashMap::new())),
            eviction_channels: Arc::new(Mutex::new(HashMap::new())),
            seq_counter: Arc::new(AtomicU64::new(0)),
            conn_counter: Arc::new(AtomicU64::new(0)),
            req_counter: Arc::new(AtomicU64::new(0)),
        }
    }

    fn next_seq(&self) -> u64 { self.seq_counter.fetch_add(1, Ordering::Relaxed).saturating_add(1) }

    pub async fn set_policy(&self, policy: OriginPolicy) { *self.policy.lock().await = policy; }

    pub async fn policy(&self) -> OriginPolicy { self.policy.lock().await.clone() }

    pub async fn on_tcp_accepted(&self, remote_addr: SocketAddr) -> u64 {
        let conn_id = self.conn_counter.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        let seq = self.next_seq();

        let mut stats = self.stats.lock().await;
        stats.active_tcp_connections = stats.active_tcp_connections.saturating_add(1);
        stats.total_tcp_connections = stats.total_tcp_connections.saturating_add(1);
        if stats.active_tcp_connections > stats.max_active_tcp_connections {
            stats.max_active_tcp_connections = stats.active_tcp_connections;
        }
        drop(stats);

        self.active_conns.lock().await.insert(
            conn_id,
            ActiveConnection { conn_id, remote_addr, connected_at: Instant::now(), active_body_req_id: None },
        );

        self.events.lock().await.push(OriginEvent::new(
            seq,
            &self.run_id,
            OriginEventKind::TcpAccepted { conn_id, remote_addr: remote_addr.to_string() },
        ));

        conn_id
    }

    pub async fn on_request_started(
        &self,
        conn_id: u64,
        method: &str,
        path_and_query: &str,
        range: Option<&str>,
        user_agent: Option<&str>,
        account: Option<&str>,
    ) -> Result<u64, (u64, u16)> {
        let req_id = self.req_counter.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        let seq = self.next_seq();

        let mut stats = self.stats.lock().await;
        stats.total_requests = stats.total_requests.saturating_add(1);
        let active_bodies = stats.active_body_connections;
        drop(stats);

        let policy = self.policy.lock().await.clone();
        if let Some(limit) = policy.account_limit {
            if active_bodies >= limit {
                match policy.limit_mode {
                    OriginLimitMode::ObserveOnly => {}
                    OriginLimitMode::RejectNew => {
                        let reject_seq = self.next_seq();
                        self.events.lock().await.push(OriginEvent::new(
                            reject_seq,
                            &self.run_id,
                            OriginEventKind::LimitRejected {
                                conn_id,
                                request_id: req_id,
                                limit,
                                current_active: active_bodies,
                            },
                        ));
                        return Err((req_id, 429));
                    }
                    OriginLimitMode::EvictOldest => {
                        self.evict_oldest_body(conn_id).await;
                    }
                }
            }
        }

        self.events.lock().await.push(OriginEvent::new(
            seq,
            &self.run_id,
            OriginEventKind::RequestStarted {
                conn_id,
                request_id: req_id,
                method: method.to_owned(),
                path: redact_path_and_query(path_and_query),
                range: range.map(str::to_owned),
                user_agent: user_agent.map(str::to_owned),
                account: account.map(str::to_owned),
            },
        ));

        Ok(req_id)
    }

    async fn evict_oldest_body(&self, triggering_conn_id: u64) {
        let conns = self.active_conns.lock().await;
        let oldest_active = conns
            .values()
            .filter(|c| c.active_body_req_id.is_some() && c.conn_id != triggering_conn_id)
            .min_by_key(|c| c.connected_at);

        if let Some(oldest) = oldest_active {
            let evicted_conn_id = oldest.conn_id;
            let evicted_req_id = oldest.active_body_req_id.unwrap_or(0);
            drop(conns);

            if let Some(sender) = self.eviction_channels.lock().await.remove(&evicted_conn_id) {
                let _ = sender.send(());
            }

            let seq = self.next_seq();
            self.events.lock().await.push(OriginEvent::new(
                seq,
                &self.run_id,
                OriginEventKind::EvictionTriggered {
                    evicted_conn_id,
                    evicted_request_id: evicted_req_id,
                    triggering_conn_id,
                },
            ));
        }
    }

    pub async fn on_body_started(
        &self,
        conn_id: u64,
        req_id: u64,
        content_length: Option<u64>,
    ) -> oneshot::Receiver<()> {
        let seq = self.next_seq();

        let mut stats = self.stats.lock().await;
        stats.active_body_connections = stats.active_body_connections.saturating_add(1);
        if stats.active_body_connections > stats.max_active_body_connections {
            stats.max_active_body_connections = stats.active_body_connections;
        }
        drop(stats);

        if let Some(conn) = self.active_conns.lock().await.get_mut(&conn_id) {
            conn.active_body_req_id = Some(req_id);
        }

        let (tx, rx) = oneshot::channel();
        self.eviction_channels.lock().await.insert(conn_id, tx);

        self.events.lock().await.push(OriginEvent::new(
            seq,
            &self.run_id,
            OriginEventKind::BodyStarted { conn_id, request_id: req_id, content_length },
        ));

        rx
    }

    pub async fn on_body_closed(
        &self,
        conn_id: u64,
        req_id: u64,
        bytes_emitted: u64,
        duration_ms: u64,
        reason: BodyCloseReason,
    ) {
        let seq = self.next_seq();

        let mut stats = self.stats.lock().await;
        stats.active_body_connections = stats.active_body_connections.saturating_sub(1);
        stats.total_bytes_emitted = stats.total_bytes_emitted.saturating_add(bytes_emitted);
        drop(stats);

        self.eviction_channels.lock().await.remove(&conn_id);
        if let Some(conn) = self.active_conns.lock().await.get_mut(&conn_id) {
            if conn.active_body_req_id == Some(req_id) {
                conn.active_body_req_id = None;
            }
        }

        self.events.lock().await.push(OriginEvent::new(
            seq,
            &self.run_id,
            OriginEventKind::BodyClosed { conn_id, request_id: req_id, bytes_emitted, duration_ms, reason },
        ));
    }

    pub async fn on_tcp_closed(
        &self,
        conn_id: u64,
        duration_ms: u64,
        bytes_read: u64,
        bytes_written: u64,
        reason: &str,
    ) {
        let seq = self.next_seq();

        let mut stats = self.stats.lock().await;
        stats.active_tcp_connections = stats.active_tcp_connections.saturating_sub(1);
        drop(stats);

        self.active_conns.lock().await.remove(&conn_id);
        self.eviction_channels.lock().await.remove(&conn_id);

        self.events.lock().await.push(OriginEvent::new(
            seq,
            &self.run_id,
            OriginEventKind::TcpClosed { conn_id, duration_ms, bytes_read, bytes_written, reason: reason.to_owned() },
        ));
    }

    pub async fn stats(&self) -> OriginStats { self.stats.lock().await.clone() }

    pub async fn events(&self) -> Vec<OriginEvent> { self.events.lock().await.clone() }

    pub async fn reset(&self) {
        *self.stats.lock().await = OriginStats::default();
        self.events.lock().await.clear();
        self.active_conns.lock().await.clear();
        self.eviction_channels.lock().await.clear();
        self.seq_counter.store(0, Ordering::Relaxed);
        self.conn_counter.store(0, Ordering::Relaxed);
        self.req_counter.store(0, Ordering::Relaxed);
    }
}

struct TcpDropGuard {
    conn_id: u64,
    tracker: OriginTracker,
    start_time: Instant,
    bytes_read: Arc<AtomicU64>,
    bytes_written: Arc<AtomicU64>,
    closed: Arc<AtomicBool>,
}

impl Drop for TcpDropGuard {
    fn drop(&mut self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            let conn_id = self.conn_id;
            let tracker = self.tracker.clone();
            let duration_ms = u64::try_from(self.start_time.elapsed().as_millis()).unwrap_or(u64::MAX);
            let bytes_read = self.bytes_read.load(Ordering::Relaxed);
            let bytes_written = self.bytes_written.load(Ordering::Relaxed);
            tokio::spawn(async move {
                tracker.on_tcp_closed(conn_id, duration_ms, bytes_read, bytes_written, "io_dropped").await;
            });
        }
    }
}

pin_project! {
    pub struct TrackedIo<T> {
        #[pin]
        inner: T,
        bytes_read: Arc<AtomicU64>,
        bytes_written: Arc<AtomicU64>,
        _guard: TcpDropGuard,
    }
}

impl<T> TrackedIo<T> {
    pub fn new(inner: T, conn_id: u64, tracker: OriginTracker) -> (Self, Arc<AtomicBool>) {
        let closed = Arc::new(AtomicBool::new(false));
        let bytes_read = Arc::new(AtomicU64::new(0));
        let bytes_written = Arc::new(AtomicU64::new(0));
        let guard = TcpDropGuard {
            conn_id,
            tracker,
            start_time: Instant::now(),
            bytes_read: bytes_read.clone(),
            bytes_written: bytes_written.clone(),
            closed: closed.clone(),
        };
        (Self { inner, bytes_read, bytes_written, _guard: guard }, closed)
    }
}

impl<T: AsyncRead> AsyncRead for TrackedIo<T> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let this = self.project();
        let prev_len = buf.filled().len();
        let poll_res = this.inner.poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &poll_res {
            let newly_read = buf.filled().len().saturating_sub(prev_len) as u64;
            this.bytes_read.fetch_add(newly_read, Ordering::Relaxed);
        }
        poll_res
    }
}

impl<T: AsyncWrite> AsyncWrite for TrackedIo<T> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        let this = self.project();
        let poll_res = this.inner.poll_write(cx, buf);
        if let Poll::Ready(Ok(written)) = &poll_res {
            this.bytes_written.fetch_add(*written as u64, Ordering::Relaxed);
        }
        poll_res
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.project().inner.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.project().inner.poll_shutdown(cx)
    }
}

pub async fn serve_tracked_media_listener(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    tracker: OriginTracker,
) -> Result<(), std::io::Error> {
    loop {
        let (tcp_stream, remote_addr) = listener.accept().await?;
        let conn_id = tracker.on_tcp_accepted(remote_addr).await;
        let (tracked_io, _closed) = TrackedIo::new(tcp_stream, conn_id, tracker.clone());
        let io = hyper_util::rt::TokioIo::new(tracked_io);
        let app = app.clone();
        let conn_meta = OriginConnectionMeta { conn_id, remote_addr };

        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let mut req = req.map(axum::body::Body::new);
                req.extensions_mut().insert(conn_meta.clone());
                let mut app = app.clone();
                async move { tower_service::Service::call(&mut app, req).await }
            });

            let builder = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
            let _ = builder.serve_connection(io, service).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[tokio::test]
    async fn tracking_records_tcp_lifecycle_and_peak_counters() {
        let tracker = OriginTracker::new("test-run", OriginPolicy::default());
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let conn_id = tracker.on_tcp_accepted(addr).await;
        assert_eq!(conn_id, 1);

        let stats = tracker.stats().await;
        assert_eq!(stats.active_tcp_connections, 1);
        assert_eq!(stats.max_active_tcp_connections, 1);

        let req_id = tracker
            .on_request_started(conn_id, "GET", "/live/17.ts", None, Some("test/1"), None)
            .await
            .expect("request admitted");
        assert_eq!(req_id, 1);

        let _evict_rx = tracker.on_body_started(conn_id, req_id, None).await;
        let stats = tracker.stats().await;
        assert_eq!(stats.active_body_connections, 1);
        assert_eq!(stats.max_active_body_connections, 1);

        tracker.on_body_closed(conn_id, req_id, 1024, 50, BodyCloseReason::Completed).await;
        let stats = tracker.stats().await;
        assert_eq!(stats.active_body_connections, 0);
        assert_eq!(stats.max_active_body_connections, 1);

        tracker.on_tcp_closed(conn_id, 100, 200, 1024, "clean_close").await;
        let stats = tracker.stats().await;
        assert_eq!(stats.active_tcp_connections, 0);
        assert_eq!(stats.max_active_tcp_connections, 1);

        let events = tracker.events().await;
        assert_eq!(events.len(), 5);
    }

    #[tokio::test]
    async fn policy_rejects_new_when_account_limit_reached() {
        let policy = OriginPolicy { account_limit: Some(1), limit_mode: OriginLimitMode::RejectNew };
        let tracker = OriginTracker::new("test-run", policy);
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);

        let conn1 = tracker.on_tcp_accepted(addr).await;
        let req1 = tracker.on_request_started(conn1, "GET", "/live/1.ts", None, None, None).await.unwrap();
        let _rx1 = tracker.on_body_started(conn1, req1, None).await;

        let conn2 = tracker.on_tcp_accepted(addr).await;
        let req2_res = tracker.on_request_started(conn2, "GET", "/live/2.ts", None, None, None).await;
        assert_eq!(req2_res, Err((2, 429)));
    }

    #[tokio::test]
    async fn policy_evicts_oldest_when_account_limit_reached() {
        let policy = OriginPolicy { account_limit: Some(1), limit_mode: OriginLimitMode::EvictOldest };
        let tracker = OriginTracker::new("test-run", policy);
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);

        let conn1 = tracker.on_tcp_accepted(addr).await;
        let req1 = tracker.on_request_started(conn1, "GET", "/live/1.ts", None, None, None).await.unwrap();
        let mut rx1 = tracker.on_body_started(conn1, req1, None).await;

        let conn2 = tracker.on_tcp_accepted(addr).await;
        let req2 = tracker.on_request_started(conn2, "GET", "/live/2.ts", None, None, None).await.unwrap();
        assert_eq!(req2, 2);

        // conn1 should have received eviction signal
        assert!(rx1.try_recv().is_ok());
    }
}
