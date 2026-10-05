use crate::{
    model::EventMessage,
    services::{get_base_href, get_token, EventService, StatusService},
    utils::set_timeout,
};
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

const WS_RECONNECT_BASE_MS: u32 = 300;
const WS_RECONNECT_MAX_MS: u32 = 2000;
const WS_RECONNECT_MAX_ATTEMPTS: u16 = 20;
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
    if attempt < 6 {
        let d = WS_RECONNECT_BASE_MS * (u32::from(attempt) + 1u32);
        d.min(WS_RECONNECT_MAX_MS)
    } else {
        WS_RECONNECT_MAX_MS
    }
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
struct ActiveSocket {
    ws: WebSocket,
    on_message_handler: Closure<dyn FnMut(MessageEvent)>,
    on_open_handler: Closure<dyn FnMut(Event)>,
    on_close_handler: Closure<dyn FnMut(CloseEvent)>,
    on_error_handler: Closure<dyn FnMut(ErrorEvent)>,
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

pub struct WebSocketInner {
    enabled: Cell<bool>,
    connected: Cell<bool>,
    connection_epoch: Cell<u64>,
    attempt_counter: Cell<u16>,
    socket: RefCell<Option<ActiveSocket>>,
    pending_messages: RefCell<VecDeque<Vec<u8>>>,
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
            connected: Cell::new(false),
            connection_epoch: Cell::new(0),
            attempt_counter: Cell::new(0),
            socket: RefCell::new(None),
            pending_messages: RefCell::new(VecDeque::new()),
            status_service,
            event_service,
            ws_path: concat_path_leading_slash(&base_href, "ws"),
        }))
    }

    pub fn is_connected(&self) -> bool { self.connected.get() }

    #[must_use]
    pub fn connection_context(&self) -> WebSocketConnectionContext {
        WebSocketConnectionContext::new(self.connection_epoch.get(), self.connected.get())
    }

    #[must_use]
    pub fn is_current_connection(&self, context: WebSocketConnectionContext) -> bool {
        self.connection_context().is_same_live_connection(context)
    }

    pub fn connect_ws_with_backoff(&self) {
        self.enabled.set(true);
        self.connect_ws();
    }

    pub fn disconnect(&self) {
        if !self.enabled.replace(false) {
            return;
        }
        self.connection_epoch.set(next_socket_epoch(self.connection_epoch.get()));
        self.connected.set(false);
        self.attempt_counter.set(0);
        self.pending_messages.borrow_mut().clear();
        self.close_socket();
        self.event_service.broadcast(EventMessage::WebSocketStatus(false));
    }

    fn close_socket(&self) {
        // Drop outside the borrow; this may free the handler that is currently running,
        // which wasm-bindgen defers until that invocation returns.
        let socket = self.socket.borrow_mut().take();
        drop(socket);
    }

    fn connect_ws(&self) {
        if !self.enabled.get() {
            return;
        }
        let has_active_socket = self.socket.borrow().as_ref().is_some_and(|socket| {
            let state = socket.ws.ready_state();
            state == WebSocket::CONNECTING || state == WebSocket::OPEN
        });
        if has_active_socket {
            return;
        }
        self.close_socket();
        let ws = match WebSocket::new(&self.ws_path) {
            Ok(ws) => ws,
            Err(err) => {
                error!("Failed to open websocket connection: {err:?}");
                return;
            }
        };
        let socket_epoch = next_socket_epoch(self.connection_epoch.get());
        self.connection_epoch.set(socket_epoch);
        ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

        let socket = ActiveSocket {
            on_message_handler: Closure::new(self.socket_handler(socket_epoch, Self::on_message)),
            on_open_handler: Closure::new(self.socket_handler(socket_epoch, Self::on_open)),
            on_close_handler: Closure::new(self.socket_handler(socket_epoch, Self::on_close)),
            on_error_handler: Closure::new(self.socket_handler(socket_epoch, Self::on_error)),
            ws,
        };
        socket.ws.set_onmessage(Some(socket.on_message_handler.as_ref().unchecked_ref()));
        socket.ws.set_onopen(Some(socket.on_open_handler.as_ref().unchecked_ref()));
        socket.ws.set_onclose(Some(socket.on_close_handler.as_ref().unchecked_ref()));
        socket.ws.set_onerror(Some(socket.on_error_handler.as_ref().unchecked_ref()));
        *self.socket.borrow_mut() = Some(socket);
    }

    /// Wraps a handler so it only runs for the socket it was created for. The handler holds a
    /// weak reference, so the socket's closures never keep the service alive.
    fn socket_handler<E: 'static>(&self, socket_epoch: u64, handle: fn(&Self, E)) -> impl FnMut(E) + 'static {
        let service = Rc::downgrade(&self.0);
        move |event| {
            let Some(service) = service.upgrade().map(Self) else {
                return;
            };
            if socket_epoch_is_current(service.connection_epoch.get(), socket_epoch) {
                handle(&service, event);
            }
        }
    }

    fn on_open(&self, _event: Event) {
        // on open starts the protocol handshake; application messages wait until authorization.
        trace!("WebSocket connection opened.");
        self.connected.set(false);
        self.try_send_message(&ProtocolMessage::Version(PROTOCOL_VERSION));
    }

    fn on_message(&self, event: MessageEvent) {
        trace!("WebSocket received message: {event:?}");
        if let Some(response) = self.handle_protocol_msg(&event) {
            self.try_send_message(&response);
        }
    }

    fn on_close(&self, event: CloseEvent) {
        trace!("WebSocket closed (Code {}, Reason: {}, Clean: {})", event.code(), event.reason(), event.was_clean());
        self.close_socket();
        self.connected.set(false);
        self.event_service.broadcast(EventMessage::WebSocketStatus(false));
        self.schedule_reconnect();
    }

    fn on_error(&self, event: ErrorEvent) {
        error!("WebSocket error: {event:?}");
        self.connected.set(false);
        self.event_service.broadcast(EventMessage::WebSocketStatus(false));
    }

    fn schedule_reconnect(&self) {
        if !self.enabled.get() {
            return;
        }
        let attempt = self.attempt_counter.get().saturating_add(1);
        self.attempt_counter.set(attempt);

        if attempt >= WS_RECONNECT_MAX_ATTEMPTS {
            warn!("WebSocket reconnect attempts exceeded ({attempt}). Giving up.");
            return;
        }
        let delay = reconnect_delay(attempt);

        warn!("WebSocket reconnect attempt #{attempt} scheduled in {delay} ms");

        let socket_epoch = self.connection_epoch.get();
        let service = self.clone();
        set_timeout(
            move || {
                if socket_epoch_is_current(service.connection_epoch.get(), socket_epoch) {
                    service.connect_ws();
                }
            },
            delay as i32,
        );
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
        if self.connected.get() && self.try_send_message(&msg) {
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

    pub async fn get_server_status(&self) {
        if self.connected.get() {
            if let Some(token) = get_token() {
                self.send_message(ProtocolMessage::StatusRequest(token));
            }
        } else {
            match self.status_service.get_server_status().await {
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
        self.connected.set(true);
        self.flush_pending_messages();
        self.event_service.broadcast(EventMessage::WebSocketStatus(true));
    }

    fn handle_protocol_msg(&self, event: &MessageEvent) -> Option<ProtocolMessage> {
        let buf = event.data().dyn_into::<ArrayBuffer>().ok()?;
        let bytes = bytes::Bytes::from(Uint8Array::new(&buf).to_vec());
        let message = match ProtocolMessage::from_bytes(bytes) {
            Ok(message) => message,
            Err(err) => {
                error!("Failed to decode websocket message: {err}");
                return None;
            }
        };
        let event_service = &self.event_service;
        match message {
            ProtocolMessage::Unauthorized => {
                self.connected.set(false);
                self.pending_messages.borrow_mut().clear();
                event_service.broadcast(EventMessage::Unauthorized);
            }
            ProtocolMessage::Error(err) => {
                error!("{err}");
            }
            ProtocolMessage::Authorized => self.mark_authorized(),
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
                if !event_service.is_config_change_message_blocked() {
                    event_service.broadcast(EventMessage::ConfigChange(config_type));
                }
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
            ProtocolMessage::Version(_version) => {
                self.attempt_counter.set(0);
                if let Some(token) = get_token() {
                    return Some(ProtocolMessage::Auth(token));
                }
                self.mark_authorized();
            }
            ProtocolMessage::UserActionResponse(_success) => {
                // A successful kick shows up as an ActiveUser disconnect event; nothing to do here.
            }
            ProtocolMessage::StreamMeterBatchResponse(entries) => {
                event_service.broadcast(EventMessage::StreamMeterBatch(entries.into()));
            }
            ProtocolMessage::Auth(_)
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
mod browser_tests {
    use super::{reconnect_delay, WebSocketService};
    use crate::{
        model::EventMessage,
        services::{EventService, StatusService},
    };
    use shared::model::{ProtocolMessage, UserCommand, VirtualId};
    use std::{cell::Cell, rc::Rc};
    use wasm_bindgen::{JsCast, JsValue};
    use wasm_bindgen_test::wasm_bindgen_test;
    use web_sys::{js_sys::Uint8Array, MessageEvent, MessageEventInit, WebSocket};

    fn message_event(message: &ProtocolMessage) -> Result<MessageEvent, JsValue> {
        let bytes = message.to_bytes().map_err(|err| JsValue::from_str(&err.to_string()))?;
        let init = MessageEventInit::new();
        init.set_data(&Uint8Array::from(bytes.as_ref()).buffer());
        MessageEvent::new_with_event_init_dict("message", &init)
    }

    fn current_socket(service: &WebSocketService) -> Option<WebSocket> {
        service.socket.borrow().as_ref().map(|socket| socket.ws.clone())
    }

    fn reconnect_wait_ms() -> u32 { reconnect_delay(1) + 100 }

    #[wasm_bindgen_test]
    fn unauthorized_callback_disconnects_and_allows_signing_in_again() -> Result<(), JsValue> {
        let events = Rc::new(EventService::new());
        let websocket = WebSocketService::new(Rc::new(StatusService::new()), Rc::clone(&events));
        websocket.connect_ws_with_backoff();
        let socket = current_socket(&websocket).ok_or_else(|| JsValue::from_str("missing websocket"))?;
        let callback = socket.onmessage().ok_or_else(|| JsValue::from_str("missing callback"))?;
        let context = websocket.connection_context();
        assert!(websocket.send_message(ProtocolMessage::RecordingSnapshotRequest));
        assert!(websocket.send_message(ProtocolMessage::RecordingSnapshotRequest));
        assert_eq!(websocket.pending_messages.borrow().len(), 1);

        let observed = Rc::new(Cell::new(false));
        let disconnected = Rc::clone(&observed);
        let listener = events.subscribe(move |msg| {
            if msg == EventMessage::WebSocketStatus(false) {
                disconnected.set(true);
            }
        });
        let service = websocket.clone();
        let event_service = Rc::clone(&events);
        let id = Rc::new(Cell::new(0));
        let subscription = Rc::clone(&id);
        id.set(events.subscribe(move |msg| {
            if msg == EventMessage::Unauthorized {
                event_service.unsubscribe(subscription.get());
                service.disconnect();
            }
        }));

        // Invoke the JS handler so disconnect drops the currently executing WASM closure.
        callback.call1(&socket, message_event(&ProtocolMessage::Unauthorized)?.as_ref())?;
        assert!(observed.get());
        assert!(!websocket.enabled.get());
        assert!(websocket.socket.borrow().is_none());
        assert!(socket.onmessage().is_none());
        assert!(socket.onclose().is_none());
        assert!(socket.onopen().is_none());
        assert!(socket.onerror().is_none());
        assert!(websocket.pending_messages.borrow().is_empty());
        assert!(!websocket.is_current_connection(context));
        assert!(!websocket.send_message(ProtocolMessage::RecordingSnapshotRequest));
        assert!(websocket.pending_messages.borrow().is_empty());

        websocket.connect_ws_with_backoff();
        let socket = current_socket(&websocket).ok_or_else(|| JsValue::from_str("missing new websocket"))?;
        socket.dispatch_event(message_event(&ProtocolMessage::Authorized)?.unchecked_ref())?;
        assert!(websocket.is_connected());
        assert!(websocket.is_current_connection(websocket.connection_context()));
        websocket.disconnect();
        events.unsubscribe(listener);
        Ok(())
    }

    #[wasm_bindgen_test]
    fn user_action_is_rejected_instead_of_queued_while_not_connected() {
        let websocket = WebSocketService::new(Rc::new(StatusService::new()), Rc::new(EventService::new()));
        websocket.connect_ws_with_backoff();
        assert!(!websocket.is_connected());
        let kick = UserCommand::Kick(([127, 0, 0, 1], 8080).into(), VirtualId::new(1), 30);

        assert!(!websocket.send_message(ProtocolMessage::UserAction(kick)));
        assert!(websocket.pending_messages.borrow().is_empty());
        assert!(websocket.send_message(ProtocolMessage::RecordingSnapshotRequest));
        assert_eq!(websocket.pending_messages.borrow().len(), 1);
        websocket.disconnect();
    }

    #[wasm_bindgen_test]
    async fn reconnect_timer_cannot_reopen_socket_after_logout() {
        let websocket = WebSocketService::new(Rc::new(StatusService::new()), Rc::new(EventService::new()));
        websocket.enabled.set(true);
        websocket.schedule_reconnect();
        assert_eq!(websocket.attempt_counter.get(), 1);
        websocket.disconnect();
        let epoch = websocket.connection_epoch.get();
        gloo_timers::future::TimeoutFuture::new(reconnect_wait_ms()).await;
        assert!(!websocket.enabled.get());
        assert!(websocket.socket.borrow().is_none());
        assert_eq!(websocket.connection_epoch.get(), epoch);
        assert_eq!(websocket.attempt_counter.get(), 0);
    }

    #[wasm_bindgen_test]
    async fn stale_reconnect_timer_cannot_open_a_second_socket() {
        let websocket = WebSocketService::new(Rc::new(StatusService::new()), Rc::new(EventService::new()));
        websocket.enabled.set(true);
        websocket.schedule_reconnect();
        // A newer connection supersedes the pending timer; it then goes away without its own onclose firing.
        websocket.connect_ws_with_backoff();
        assert!(websocket.socket.borrow().is_some());
        websocket.close_socket();
        let epoch = websocket.connection_epoch.get();
        gloo_timers::future::TimeoutFuture::new(reconnect_wait_ms()).await;
        assert!(websocket.enabled.get());
        assert!(websocket.socket.borrow().is_none());
        assert_eq!(websocket.connection_epoch.get(), epoch);
        websocket.disconnect();
    }
}
