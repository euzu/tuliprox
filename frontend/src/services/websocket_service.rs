use crate::{
    model::EventMessage,
    services::{get_base_href, get_token, EventService, StatusService},
};
use gloo_timers::callback::Timeout;
use log::{error, trace, warn};
use shared::{
    model::{ProtocolMessage, PROTOCOL_VERSION},
    utils::concat_path_leading_slash,
};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    ops::Deref,
    rc::Rc,
};
use wasm_bindgen::{closure::Closure, JsCast};
use web_sys::{
    js_sys::{ArrayBuffer, Uint8Array},
    CloseEvent, ErrorEvent, Event, MessageEvent, WebSocket,
};

const WS_HANDSHAKE_TIMEOUT_MS: u32 = 15_000;
const WS_RECONNECT_DELAYS_MS: [u32; 7] = [500, 1000, 2000, 4000, 8000, 16_000, 30_000];
const WS_MAX_PENDING_MESSAGES: usize = 256;

/// Frontend-local identity of one concrete WebSocket connection lifecycle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WebSocketConnectionContext {
    socket_epoch: u64,
    connected: bool,
}

impl WebSocketConnectionContext {
    #[must_use]
    pub const fn new(socket_epoch: u64, connected: bool) -> Self { Self { socket_epoch, connected } }

    #[must_use]
    pub const fn is_connected(self) -> bool { self.connected }

    #[must_use]
    pub fn is_same_live_connection(self, other: Self) -> bool { self.connected && self == other }
}

const fn next_socket_epoch(current: u64) -> u64 { current.saturating_add(1) }

const fn socket_epoch_is_current(current: u64, candidate: u64) -> bool { current == candidate }

fn reconnect_delay(attempt: u16) -> u32 {
    WS_RECONNECT_DELAYS_MS[usize::from(attempt).min(WS_RECONNECT_DELAYS_MS.len() - 1)]
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn jittered_delay(nominal: u32) -> u32 {
    let minimum = nominal * 4 / 5;
    // Math.random() is in [0, 1), so the offset stays within the integer range.
    minimum + (js_sys::Math::random() * f64::from(nominal - minimum + 1)) as u32
}

fn monotonic_ms() -> f64 {
    web_sys::window()
        .and_then(|window| window.performance())
        .map_or_else(js_sys::Date::now, |performance| performance.now())
}

/// Requests whose repeated delivery carries no extra meaning; a queued copy is enough.
const fn is_idempotent_request(msg: &ProtocolMessage) -> bool {
    matches!(
        msg,
        ProtocolMessage::RecordingSnapshotRequest
            | ProtocolMessage::StatusRequest(_)
            | ProtocolMessage::ActiveProviderCountRequest(_)
    )
}

/// User commands act on live server state; a delayed or silently dropped delivery is worse than an error.
const fn requires_live_connection(msg: &ProtocolMessage) -> bool { matches!(msg, ProtocolMessage::UserAction(_)) }

/// Queues `bytes` until the connection is authorized. Idempotent requests are queued once,
/// and the oldest message is dropped when the queue is full.
fn enqueue_pending(pending: &mut VecDeque<Vec<u8>>, bytes: &[u8], idempotent: bool) {
    if idempotent && pending.iter().any(|queued| queued.as_slice() == bytes) {
        return;
    }
    if pending.len() >= WS_MAX_PENDING_MESSAGES {
        warn!("WebSocket message queue is full; dropping the oldest message.");
        pending.pop_front();
    }
    pending.push_back(bytes.to_vec());
}

/// An open socket together with the JS handlers attached to it.
/// Dropping it detaches the handlers, closes the socket and frees the closures.
pub(crate) struct ActiveSocket {
    ws: WebSocket,
    _on_message_handler: Closure<dyn FnMut(MessageEvent)>,
    _on_open_handler: Closure<dyn FnMut(Event)>,
    _on_close_handler: Closure<dyn FnMut(CloseEvent)>,
    _on_error_handler: Closure<dyn FnMut(ErrorEvent)>,
}

impl ActiveSocket {
    pub(crate) fn new(
        ws: WebSocket,
        on_message_handler: Closure<dyn FnMut(MessageEvent)>,
        on_open_handler: Closure<dyn FnMut(Event)>,
        on_close_handler: Closure<dyn FnMut(CloseEvent)>,
        on_error_handler: Closure<dyn FnMut(ErrorEvent)>,
    ) -> Self {
        ws.set_onmessage(Some(on_message_handler.as_ref().unchecked_ref()));
        ws.set_onopen(Some(on_open_handler.as_ref().unchecked_ref()));
        ws.set_onclose(Some(on_close_handler.as_ref().unchecked_ref()));
        ws.set_onerror(Some(on_error_handler.as_ref().unchecked_ref()));
        Self {
            ws,
            _on_message_handler: on_message_handler,
            _on_open_handler: on_open_handler,
            _on_close_handler: on_close_handler,
            _on_error_handler: on_error_handler,
        }
    }

    pub(crate) fn websocket(&self) -> &WebSocket { &self.ws }
}

impl Drop for ActiveSocket {
    fn drop(&mut self) {
        self.ws.set_onmessage(None);
        self.ws.set_onopen(None);
        self.ws.set_onclose(None);
        self.ws.set_onerror(None);
        if let Err(err) = self.ws.close() {
            warn!("Failed to close websocket connection: {err:?}");
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SocketPhase {
    Disconnected,
    Opening,
    Version,
    Authorization,
    Ready,
}

pub struct WebSocketInner {
    enabled: Cell<bool>,
    phase: Cell<SocketPhase>,
    connection_epoch: Cell<u64>,
    attempt_counter: Cell<u16>,
    socket: RefCell<Option<ActiveSocket>>,
    reconnect_timer: RefCell<Option<Timeout>>,
    handshake_deadline: Cell<Option<f64>>,
    watchdog: RefCell<Option<Timeout>>,
    pending_messages: RefCell<VecDeque<Vec<u8>>>,
    status_request_epoch: Cell<u64>,
    status_service: Rc<StatusService>,
    event_service: Rc<EventService>,
    ws_path: String,
}

/// Cheap to clone; all clones share one connection state.
#[derive(Clone)]
pub struct WebSocketService(Rc<WebSocketInner>);

impl Deref for WebSocketService {
    type Target = WebSocketInner;

    fn deref(&self) -> &Self::Target { &self.0 }
}

impl WebSocketService {
    pub fn new(status_service: Rc<StatusService>, event_service: Rc<EventService>) -> Self {
        let base_href = get_base_href();
        Self(Rc::new(WebSocketInner {
            enabled: Cell::new(false),
            phase: Cell::new(SocketPhase::Disconnected),
            connection_epoch: Cell::new(0),
            attempt_counter: Cell::new(0),
            socket: RefCell::new(None),
            reconnect_timer: RefCell::new(None),
            handshake_deadline: Cell::new(None),
            watchdog: RefCell::new(None),
            pending_messages: RefCell::new(VecDeque::new()),
            status_request_epoch: Cell::new(0),
            status_service,
            event_service,
            ws_path: concat_path_leading_slash(&base_href, "ws"),
        }))
    }

    pub fn is_connected(&self) -> bool { self.phase.get() == SocketPhase::Ready }

    #[must_use]
    pub fn connection_context(&self) -> WebSocketConnectionContext {
        WebSocketConnectionContext::new(self.connection_epoch.get(), self.is_connected())
    }

    #[must_use]
    pub fn is_current_connection(&self, context: WebSocketConnectionContext) -> bool {
        self.connection_context().is_same_live_connection(context)
    }

    pub fn connect_ws_with_backoff(&self) {
        if !self.enabled.replace(true) {
            self.invalidate_http_status();
            self.connect_ws();
        }
    }

    pub fn disconnect(&self) {
        if !self.enabled.replace(false) {
            return;
        }
        self.attempt_counter.set(0);
        self.pending_messages.borrow_mut().clear();
        self.invalidate_http_status();
        self.end_attempt();
        self.event_service.broadcast(EventMessage::WebSocketStatus(false));
    }

    fn clear_watchdog(&self) {
        self.handshake_deadline.set(None);
        drop(self.watchdog.take());
    }

    fn end_attempt(&self) {
        self.connection_epoch.set(next_socket_epoch(self.connection_epoch.get()));
        self.phase.set(SocketPhase::Disconnected);
        self.clear_watchdog();
        drop(self.reconnect_timer.take());
        // Release the RefCell borrow before dropping a possibly executing handler.
        drop(self.socket.take());
    }

    fn connect_ws(&self) {
        let socket_epoch = next_socket_epoch(self.connection_epoch.get());
        self.connection_epoch.set(socket_epoch);
        self.phase.set(SocketPhase::Opening);
        self.handshake_deadline.set(Some(monotonic_ms() + f64::from(WS_HANDSHAKE_TIMEOUT_MS)));
        *self.watchdog.borrow_mut() = Some(self.timer(WS_HANDSHAKE_TIMEOUT_MS, Self::on_handshake_timeout));

        let ws = match WebSocket::new(&self.ws_path) {
            Ok(ws) => ws,
            Err(err) => {
                error!("Failed to open websocket connection: {err:?}");
                self.fail_attempt();
                return;
            }
        };
        ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

        let socket = ActiveSocket::new(
            ws,
            Closure::new(self.socket_handler(socket_epoch, Self::on_message)),
            Closure::new(self.socket_handler(socket_epoch, Self::on_open)),
            Closure::new(self.socket_handler(socket_epoch, Self::on_close)),
            Closure::new(self.socket_handler(socket_epoch, Self::on_error)),
        );
        *self.socket.borrow_mut() = Some(socket);
    }

    /// Guards socket events and timers before they touch any connection state.
    fn socket_handler<E: 'static>(&self, socket_epoch: u64, handle: fn(&Self, E)) -> impl FnMut(E) + 'static {
        let service = Rc::downgrade(&self.0);
        move |event| {
            let Some(service) = service.upgrade().map(Self) else {
                return;
            };
            if service.enabled.get() && socket_epoch_is_current(service.connection_epoch.get(), socket_epoch) {
                handle(&service, event);
            }
        }
    }

    fn timer(&self, delay: u32, handle: fn(&Self, ())) -> Timeout {
        let mut handler = self.socket_handler(self.connection_epoch.get(), handle);
        Timeout::new(delay, move || handler(()))
    }

    fn expire_handshake(&self) -> bool {
        if self.handshake_deadline.get().is_some_and(|deadline| monotonic_ms() >= deadline) {
            self.on_handshake_timeout(());
            return true;
        }
        false
    }

    fn on_handshake_timeout(&self, (): ()) {
        if self.handshake_deadline.get().is_some() {
            warn!("WebSocket handshake timed out.");
            self.fail_attempt();
        }
    }

    fn send_handshake(&self, message: &ProtocolMessage) {
        if !self.try_send_message(message) {
            self.fail_attempt();
        }
    }

    fn on_open(&self, _event: Event) {
        if self.expire_handshake() || self.phase.get() != SocketPhase::Opening {
            return;
        }
        trace!("WebSocket connection opened.");
        self.phase.set(SocketPhase::Version);
        self.send_handshake(&ProtocolMessage::Version(PROTOCOL_VERSION));
    }

    fn on_message(&self, event: MessageEvent) {
        if self.expire_handshake() {
            return;
        }
        let Ok(buffer) = event.data().dyn_into::<ArrayBuffer>() else {
            return;
        };
        let bytes = bytes::Bytes::from(Uint8Array::new(&buffer).to_vec());
        let version_matches = bytes.as_ref() == [PROTOCOL_VERSION];
        let message = match ProtocolMessage::from_bytes(bytes) {
            Ok(message) => message,
            Err(err) => {
                error!("Failed to decode websocket message: {err}");
                return;
            }
        };
        if matches!(message, ProtocolMessage::Unauthorized) {
            self.disconnect();
            self.event_service.broadcast(EventMessage::Unauthorized);
            return;
        }

        match self.phase.get() {
            SocketPhase::Version if version_matches => {
                if let Some(token) = get_token() {
                    self.phase.set(SocketPhase::Authorization);
                    self.send_handshake(&ProtocolMessage::Auth(token));
                } else {
                    self.mark_authorized();
                }
            }
            SocketPhase::Authorization if matches!(message, ProtocolMessage::Authorized) => self.mark_authorized(),
            SocketPhase::Ready => {
                let context = self.connection_context();
                if let Some(response) =
                    self.handle_live_message(message).filter(|_| self.is_current_connection(context))
                {
                    self.try_send_message(&response);
                }
            }
            _ => {}
        }
    }

    fn on_close(&self, event: CloseEvent) {
        trace!("WebSocket closed (Code {}, Reason: {}, Clean: {})", event.code(), event.reason(), event.was_clean());
        self.fail_attempt();
    }

    fn on_error(&self, event: ErrorEvent) {
        error!("WebSocket error: {event:?}");
        self.fail_attempt();
    }

    fn fail_attempt(&self) {
        self.end_attempt();
        let delay = jittered_delay(reconnect_delay(self.attempt_counter.get()));
        let attempt = self.attempt_counter.get().saturating_add(1);
        self.attempt_counter.set(attempt);
        warn!("WebSocket reconnect attempt #{attempt} scheduled in {delay} ms");
        *self.reconnect_timer.borrow_mut() = Some(self.timer(delay, Self::on_retry));
        self.event_service.broadcast(EventMessage::WebSocketStatus(false));
    }

    fn on_retry(&self, (): ()) {
        drop(self.reconnect_timer.take());
        self.connect_ws();
    }

    fn try_send_message(&self, msg: &ProtocolMessage) -> bool {
        match msg.to_bytes() {
            Ok(bytes) => self.try_send_bytes(bytes.as_ref()),
            Err(err) => {
                error!("Failed to encode websocket message: {err}");
                false
            }
        }
    }

    fn try_send_bytes(&self, bytes: &[u8]) -> bool {
        let socket = self.socket.borrow();
        let Some(ws) = socket.as_ref().map(|socket| &socket.ws).filter(|ws| ws.ready_state() == WebSocket::OPEN) else {
            return false;
        };
        match ws.send_with_u8_array(bytes) {
            Ok(()) => true,
            Err(err) => {
                error!("Failed to send a websocket message: {err:?}");
                false
            }
        }
    }

    fn flush_pending_messages(&self) {
        let mut pending = self.pending_messages.borrow_mut();
        while let Some(bytes) = pending.front() {
            if !self.try_send_bytes(bytes) {
                break;
            }
            pending.pop_front();
        }
    }

    pub fn send_message(&self, msg: ProtocolMessage) -> bool {
        if !self.enabled.get() {
            return false;
        }
        if self.is_connected() && self.try_send_message(&msg) {
            return true;
        }
        if requires_live_connection(&msg) {
            return false;
        }

        match msg.to_bytes() {
            Ok(bytes) => {
                enqueue_pending(&mut self.pending_messages.borrow_mut(), bytes.as_ref(), is_idempotent_request(&msg));
                trace!("Queued websocket message until connection is ready.");
                true
            }
            Err(err) => {
                error!("Failed to create WebSocket message: {err}");
                false
            }
        }
    }

    fn invalidate_http_status(&self) -> u64 {
        let request_epoch = next_socket_epoch(self.status_request_epoch.get());
        self.status_request_epoch.set(request_epoch);
        request_epoch
    }

    pub async fn get_server_status(&self) {
        if self.is_connected() {
            if let Some(token) = get_token() {
                self.send_message(ProtocolMessage::StatusRequest(token));
            }
        } else {
            // Retries in one outage can overlap the HTTP fallback. Only a newer
            // status request or a session/readiness change supersedes it.
            let request_epoch = self.invalidate_http_status();
            let response = self.status_service.get_server_status().await;
            if self.status_request_epoch.get() != request_epoch {
                return;
            }
            match response {
                Ok(Some(status)) => {
                    self.event_service.broadcast(EventMessage::ServerStatus(status));
                }
                Ok(None) => {
                    // ignore
                }
                Err(err) => {
                    error!("Failed to get server status: {err:?}");
                }
            }
        }
    }

    fn mark_authorized(&self) {
        self.clear_watchdog();
        self.phase.set(SocketPhase::Ready);
        self.attempt_counter.set(0);
        self.invalidate_http_status();
        self.flush_pending_messages();
        self.event_service.broadcast(EventMessage::WebSocketStatus(true));
    }

    fn handle_live_message(&self, message: ProtocolMessage) -> Option<ProtocolMessage> {
        let event_service = &self.event_service;
        match message {
            ProtocolMessage::Error(err) => {
                error!("{err}");
            }
            ProtocolMessage::ActiveUserResponse(event) => {
                event_service.broadcast(EventMessage::ActiveUser(event));
            }
            ProtocolMessage::ActiveProviderResponse(provider_name, connections) => {
                event_service.broadcast(EventMessage::ActiveProvider(provider_name, connections));
                return get_token().map(ProtocolMessage::ActiveProviderCountRequest);
            }
            ProtocolMessage::ActiveProviderCountResponse(connections) => {
                event_service.broadcast(EventMessage::ActiveProviderCount(connections));
            }
            ProtocolMessage::StatusResponse(status) => {
                event_service.broadcast(EventMessage::ServerStatus(Rc::new(status)));
            }
            ProtocolMessage::ConfigChangeResponse(config_type) => {
                event_service.broadcast(EventMessage::ConfigChange(config_type));
            }
            ProtocolMessage::ServerError(error) => {
                event_service.broadcast(EventMessage::ServerError(error));
            }
            ProtocolMessage::PlaylistUpdateResponse(update_state) => {
                event_service.broadcast(EventMessage::PlaylistUpdate(update_state));
            }
            ProtocolMessage::PlaylistUpdateProgressResponse(progress) => {
                event_service.broadcast(EventMessage::PlaylistUpdateProgress(progress));
            }
            ProtocolMessage::SystemInfoResponse(system_info) => {
                event_service.broadcast(EventMessage::SystemInfoUpdate(system_info));
            }
            ProtocolMessage::LibraryScanProgressResponse(progress) => {
                event_service.broadcast(EventMessage::LibraryScanProgress(progress));
            }
            ProtocolMessage::UserActionResponse(_success) => {
                // A successful kick shows up as an ActiveUser disconnect event; nothing to do here.
            }
            ProtocolMessage::StreamMeterBatchResponse(entries) => {
                event_service.broadcast(EventMessage::StreamMeterBatch(entries.into()));
            }
            ProtocolMessage::Version(_)
            | ProtocolMessage::Authorized
            | ProtocolMessage::Unauthorized
            | ProtocolMessage::Auth(_)
            | ProtocolMessage::StreamMeterSubscribe
            | ProtocolMessage::StreamMeterUnsubscribe
            | ProtocolMessage::ActiveProviderCountRequest(_)
            | ProtocolMessage::StatusRequest(_)
            | ProtocolMessage::UserAction(_)
            | ProtocolMessage::RecordingSnapshotRequest => {}
            ProtocolMessage::RecordingSnapshotResponse { revision, available, quota, tasks } => {
                event_service.broadcast(EventMessage::RecordingSnapshot {
                    revision: revision.0,
                    available,
                    quota,
                    tasks: Rc::new(tasks),
                });
            }
            ProtocolMessage::RecordingRulesChanged => {
                event_service.broadcast(EventMessage::RecordingRulesChanged);
            }
            ProtocolMessage::MaintenanceChangedResponse(entries) => {
                event_service.broadcast(EventMessage::MaintenanceChanged(Rc::new(entries)));
            }
            ProtocolMessage::FailoverOverridesChanged(targets) => {
                event_service.broadcast(EventMessage::FailoverOverridesChanged(Rc::new(targets)));
            }
            ProtocolMessage::RecordingWsError { code } => {
                event_service.broadcast(EventMessage::RecordingUnavailable { code });
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{
        enqueue_pending, is_idempotent_request, next_socket_epoch, requires_live_connection, socket_epoch_is_current,
        WebSocketConnectionContext, WS_MAX_PENDING_MESSAGES,
    };
    use shared::model::{ProtocolMessage, UserCommand, VirtualId};
    use std::collections::VecDeque;

    fn kick() -> ProtocolMessage {
        ProtocolMessage::UserAction(UserCommand::Kick(([127, 0, 0, 1], 8080).into(), VirtualId::new(1), 30))
    }

    #[test]
    fn playlist_update_status_socket_epoch_advances_for_each_connection_and_rejects_stale_callbacks() {
        let first = next_socket_epoch(0);
        let second = next_socket_epoch(first);

        assert_eq!(first, 1);
        assert_eq!(second, 2);
        assert!(socket_epoch_is_current(second, second));
        assert!(!socket_epoch_is_current(second, first));
    }

    #[test]
    fn playlist_update_status_connection_context_matches_only_the_same_live_socket() {
        let first = WebSocketConnectionContext::new(1, true);
        let first_disconnected = WebSocketConnectionContext::new(1, false);
        let second = WebSocketConnectionContext::new(2, true);

        assert!(first.is_same_live_connection(first));
        assert!(!first_disconnected.is_same_live_connection(first_disconnected));
        assert!(!second.is_same_live_connection(first));
    }

    #[test]
    fn idempotent_requests_are_queued_once_and_others_every_time() {
        let mut pending = VecDeque::new();
        enqueue_pending(&mut pending, b"snapshot", true);
        enqueue_pending(&mut pending, b"snapshot", true);
        enqueue_pending(&mut pending, b"kick", false);
        enqueue_pending(&mut pending, b"kick", false);

        assert_eq!(pending, [b"snapshot".to_vec(), b"kick".to_vec(), b"kick".to_vec()]);
    }

    #[test]
    fn full_pending_queue_drops_the_oldest_message() {
        let mut pending: VecDeque<Vec<u8>> = (0..WS_MAX_PENDING_MESSAGES).map(|i| i.to_le_bytes().to_vec()).collect();
        enqueue_pending(&mut pending, b"newest", false);

        assert_eq!(pending.len(), WS_MAX_PENDING_MESSAGES);
        assert_eq!(pending.front(), Some(&1_usize.to_le_bytes().to_vec()));
        assert_eq!(pending.back(), Some(&b"newest".to_vec()));
    }

    #[test]
    fn stream_meter_toggles_are_not_treated_as_idempotent() {
        assert!(is_idempotent_request(&ProtocolMessage::RecordingSnapshotRequest));
        assert!(!is_idempotent_request(&ProtocolMessage::StreamMeterSubscribe));
        assert!(!is_idempotent_request(&ProtocolMessage::StreamMeterUnsubscribe));
    }

    #[test]
    fn only_user_actions_require_a_live_connection() {
        assert!(requires_live_connection(&kick()));
        assert!(!requires_live_connection(&ProtocolMessage::RecordingSnapshotRequest));
        assert!(!requires_live_connection(&ProtocolMessage::StreamMeterSubscribe));
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod browser_tests;
