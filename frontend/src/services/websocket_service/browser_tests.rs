use super::{WebSocketService, WS_HANDSHAKE_TIMEOUT_MS};
use crate::{
    model::EventMessage,
    services::{get_token, set_token, EventService, StatusService},
};
use shared::model::{ProtocolMessage, StatusCheck, UserCommand, VirtualId, PROTOCOL_VERSION, TOKEN_NO_AUTH};
use std::{
    cell::{Cell, RefCell},
    pin::pin,
    rc::Rc,
};
use wasm_bindgen::{prelude::wasm_bindgen, JsCast};
use wasm_bindgen_test::wasm_bindgen_test;
use web_sys::{js_sys::Uint8Array, CloseEvent, ErrorEvent, MessageEvent, MessageEventInit, WebSocket};

#[wasm_bindgen(module = "/src/services/websocket_service/browser_fixture.js")]
extern "C" {
    #[wasm_bindgen(js_name = installFixture)]
    fn install_fixture();
    #[wasm_bindgen(js_name = restoreFixture)]
    fn restore_fixture();
    #[wasm_bindgen(js_name = rejectConstructor)]
    fn reject_constructor(reject: bool);
    #[wasm_bindgen(js_name = rejectSend)]
    fn reject_send(reject: bool);
    #[wasm_bindgen(js_name = jitterSample)]
    fn jitter_sample(sample: f64);
    #[wasm_bindgen(js_name = timerCount)]
    fn timer_count() -> u32;
    #[wasm_bindgen(js_name = nextDelay)]
    fn next_delay() -> u32;
    #[wasm_bindgen(js_name = jumpClock)]
    fn jump_clock(ms: u32);
    #[wasm_bindgen(js_name = advanceClock)]
    fn advance_clock(ms: u32);
    #[wasm_bindgen(js_name = openSocket)]
    fn open_socket(socket: &WebSocket);
    #[wasm_bindgen(js_name = sentCount)]
    fn sent_count(socket: &WebSocket) -> u32;
    #[wasm_bindgen(js_name = sentFrame)]
    fn sent_frame(socket: &WebSocket, index: u32) -> Uint8Array;
    #[wasm_bindgen(js_name = resolveStatus)]
    fn resolve_status(index: u32, json: &str);
}

struct Fixture(Option<String>);

impl Fixture {
    fn new(with_token: bool) -> Self {
        install_fixture();
        let previous_token = get_token();
        set_token(with_token.then_some(TOKEN_NO_AUTH));
        Self(previous_token)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        restore_fixture();
        set_token(self.0.as_deref());
    }
}

fn service() -> WebSocketService { WebSocketService::new(Rc::new(StatusService::new()), Rc::new(EventService::new())) }

fn socket(service: &WebSocketService) -> WebSocket {
    service.socket.borrow().as_ref().expect("active attempt").ws.clone()
}

fn message_event(bytes: &[u8]) -> MessageEvent {
    let init = MessageEventInit::new();
    init.set_data(&Uint8Array::from(bytes).buffer());
    MessageEvent::new_with_event_init_dict("message", &init).unwrap()
}

fn message(socket: &WebSocket, frame: &ProtocolMessage) {
    let event = message_event(&frame.to_bytes().unwrap());
    socket.dispatch_event(event.unchecked_ref()).unwrap();
}

fn close(socket: &WebSocket) { socket.dispatch_event(CloseEvent::new("close").unwrap().unchecked_ref()).unwrap(); }

fn error(socket: &WebSocket) { socket.dispatch_event(ErrorEvent::new("error").unwrap().unchecked_ref()).unwrap(); }

fn version(socket: &WebSocket) { message(socket, &ProtocolMessage::Version(PROTOCOL_VERSION)); }

fn handshake(socket: &WebSocket) {
    open_socket(socket);
    version(socket);
    message(socket, &ProtocolMessage::Authorized);
}

fn sent(socket: &WebSocket, index: u32) -> ProtocolMessage {
    ProtocolMessage::from_bytes(bytes::Bytes::from(sent_frame(socket, index).to_vec())).unwrap()
}

fn assert_detached(socket: &WebSocket) {
    assert_eq!(socket.ready_state(), WebSocket::CLOSED);
    assert!(socket.onmessage().is_none());
    assert!(socket.onclose().is_none());
    assert!(socket.onopen().is_none());
    assert!(socket.onerror().is_none());
}

#[wasm_bindgen_test]
fn unauthorized_callback_disconnects_and_allows_signing_in_again() {
    let _fixture = Fixture::new(true);
    let websocket = service();
    websocket.connect_ws_with_backoff();
    let first = socket(&websocket);
    open_socket(&first);
    version(&first);
    let callback = first.onmessage().unwrap();
    let context = websocket.connection_context();
    assert!(websocket.send_message(ProtocolMessage::RecordingSnapshotRequest));
    assert!(websocket.send_message(ProtocolMessage::RecordingSnapshotRequest));
    assert_eq!(websocket.pending_messages.borrow().len(), 1);

    let unauthorized = Rc::new(Cell::new(false));
    let observed = Rc::clone(&unauthorized);
    let service = websocket.clone();
    let listener = websocket.event_service.subscribe(move |event| {
        if event == EventMessage::Unauthorized {
            observed.set(true);
            service.disconnect();
        }
    });

    // Dropping the current socket detaches the JS handler while it is executing.
    let event = message_event(&ProtocolMessage::Unauthorized.to_bytes().unwrap());
    callback.call1(&first, event.as_ref()).unwrap();
    assert!(unauthorized.get());
    assert!(!websocket.enabled.get());
    assert!(!websocket.is_connected());
    assert_detached(&first);
    assert!(websocket.pending_messages.borrow().is_empty());
    assert!(!websocket.is_current_connection(context));
    assert!(!websocket.send_message(ProtocolMessage::RecordingSnapshotRequest));
    advance_clock(45_000);
    assert_eq!(timer_count(), 0);
    assert!(websocket.socket.borrow().is_none());

    websocket.connect_ws_with_backoff();
    handshake(&socket(&websocket));
    assert!(websocket.is_connected());
    assert!(websocket.is_current_connection(websocket.connection_context()));
    websocket.event_service.unsubscribe(listener);
}

#[wasm_bindgen_test]
fn user_action_is_rejected_instead_of_queued_while_not_connected() {
    let _fixture = Fixture::new(true);
    let websocket = service();
    websocket.connect_ws_with_backoff();
    error(&socket(&websocket));
    let kick = UserCommand::Kick(([127, 0, 0, 1], 8080).into(), VirtualId::new(1), 30);
    assert!(!websocket.send_message(ProtocolMessage::UserAction(kick)));
    assert!(websocket.pending_messages.borrow().is_empty());
    for _ in 0..2 {
        assert!(websocket.send_message(ProtocolMessage::StatusRequest(TOKEN_NO_AUTH.into())));
    }
    assert_eq!(websocket.pending_messages.borrow().len(), 1);
    advance_clock(next_delay());
    let next = socket(&websocket);
    open_socket(&next);
    version(&next);
    assert_eq!(sent_count(&next), 2, "only Version and Auth before authorization");
    assert!(matches!(sent(&next, 0), ProtocolMessage::Version(PROTOCOL_VERSION)));
    assert!(matches!(sent(&next, 1), ProtocolMessage::Auth(_)));
    message(&next, &ProtocolMessage::Authorized);
    assert_eq!(sent_count(&next), 3);
    assert!(matches!(sent(&next, 2), ProtocolMessage::StatusRequest(_)));
    assert!(websocket.pending_messages.borrow().is_empty());
}

#[wasm_bindgen_test]
fn reconnect_timer_cannot_reopen_socket_after_logout() {
    let _fixture = Fixture::new(true);
    for in_backoff in [false, true] {
        let websocket = service();
        websocket.connect_ws_with_backoff();
        let first = socket(&websocket);
        if in_backoff {
            close(&first);
        } else {
            open_socket(&first);
            version(&first);
        }
        assert_eq!(timer_count(), 1);
        websocket.send_message(ProtocolMessage::RecordingSnapshotRequest);
        websocket.disconnect();
        let epoch = websocket.connection_epoch.get();
        advance_clock(45_000);
        assert!(!websocket.enabled.get());
        assert!(websocket.socket.borrow().is_none());
        assert_detached(&first);
        assert!(websocket.pending_messages.borrow().is_empty());
        assert_eq!(websocket.connection_epoch.get(), epoch);
        assert_eq!(websocket.attempt_counter.get(), 0);
        assert_eq!(timer_count(), 0);

        websocket.connect_ws_with_backoff();
        let next = socket(&websocket);
        websocket.connect_ws_with_backoff();
        assert_eq!(socket(&websocket), next);
        handshake(&next);
        assert!(websocket.is_connected());
    }
}

#[wasm_bindgen_test]
fn stale_reconnect_timer_cannot_open_a_second_socket() {
    let _fixture = Fixture::new(true);
    let websocket = service();
    websocket.connect_ws_with_backoff();
    close(&socket(&websocket));
    let mut late_retry = websocket.socket_handler(websocket.connection_epoch.get(), WebSocketService::on_retry);
    let retry = next_delay();
    websocket.connect_ws_with_backoff();
    assert!(websocket.socket.borrow().is_none(), "auth sync must respect backoff");
    assert_eq!(next_delay(), retry);
    advance_clock(retry);
    let next = socket(&websocket);
    handshake(&next);
    late_retry(());
    assert_eq!(socket(&websocket), next);
    assert!(websocket.is_connected());
    assert_eq!(timer_count(), 0);

    close(&next);
    let retry = next_delay();
    late_retry(());
    assert_eq!(timer_count(), 1, "an old callback must not clear the replacement retry");
    assert_eq!(next_delay(), retry);
}

#[wasm_bindgen_test]
fn close_callback_cannot_schedule_reconnect_for_a_new_login() {
    let _fixture = Fixture::new(true);
    let websocket = service();
    websocket.connect_ws_with_backoff();
    let first = socket(&websocket);
    let reconnected = Rc::new(Cell::new(false));
    let observed = Rc::clone(&reconnected);
    let service = websocket.clone();
    let subscription = websocket.event_service.subscribe(move |msg| {
        if msg == EventMessage::WebSocketStatus(false) && !observed.replace(true) {
            service.disconnect();
            service.connect_ws_with_backoff();
        }
    });
    close(&first);
    assert!(reconnected.get());
    assert_ne!(socket(&websocket), first);
    assert!(websocket.reconnect_timer.borrow().is_none());
    assert_eq!(websocket.attempt_counter.get(), 0);
    assert_eq!(timer_count(), 1, "only the fresh login's watchdog remains");
    assert_detached(&first);
    websocket.event_service.unsubscribe(subscription);
}

#[wasm_bindgen_test]
fn reconnect_timer_is_unique_and_does_not_keep_service_alive() {
    let _fixture = Fixture::new(false);
    let websocket = service();
    let weak = Rc::downgrade(&websocket.0);
    websocket.connect_ws_with_backoff();
    let first = socket(&websocket);
    error(&first);
    close(&first);
    assert_eq!(websocket.attempt_counter.get(), 1);
    assert_eq!(timer_count(), 1);
    drop(websocket);
    assert!(weak.upgrade().is_none());
    assert_eq!(timer_count(), 0);
}

#[wasm_bindgen_test]
fn retries_continue_after_fifty_failures_and_reset_only_after_authorization() {
    let _fixture = Fixture::new(true);
    let websocket = service();
    websocket.connect_ws_with_backoff();
    let nominals = [500, 1000, 2000, 4000, 8000, 16_000, 30_000];
    for attempt in 0..55 {
        // Observe the exact bounds as well as the sequence, with no elapsed wall-clock wait.
        jitter_sample(if attempt % 2 == 0 { 0.0 } else { 0.999_999 });
        let failed = socket(&websocket);
        close(&failed);
        assert_detached(&failed);
        let nominal = nominals[attempt.min(6)];
        let delay = if attempt % 2 == 0 { nominal * 4 / 5 } else { nominal };
        assert_eq!(next_delay(), delay);
        assert_eq!(timer_count(), 1);
        advance_clock(delay);
    }
    let next = socket(&websocket);
    open_socket(&next);
    version(&next);
    assert_eq!(websocket.attempt_counter.get(), 55);
    assert!(!websocket.is_connected());
    message(&next, &ProtocolMessage::Authorized);
    assert!(websocket.is_connected());
    assert_eq!(websocket.attempt_counter.get(), 0);
    assert_eq!(timer_count(), 0);
    close(&next);
    assert_eq!(next_delay(), 400);
}

#[wasm_bindgen_test]
fn one_deadline_covers_upgrade_version_and_authorization() {
    let _fixture = Fixture::new(true);
    let websocket = service();
    websocket.connect_ws_with_backoff();
    for stage in 0..3 {
        let stalled = socket(&websocket);
        advance_clock(WS_HANDSHAKE_TIMEOUT_MS - 1);
        if stage >= 1 {
            open_socket(&stalled);
        }
        if stage == 2 {
            version(&stalled);
        }
        assert!(!websocket.is_connected());
        assert_eq!(timer_count(), 1);
        assert_eq!(next_delay(), 1);
        advance_clock(1);
        assert_detached(&stalled);
        assert!(websocket.socket.borrow().is_none());
        assert_eq!(timer_count(), 1);
        assert_eq!(websocket.attempt_counter.get(), stage + 1);
        advance_clock(next_delay());
    }
    let ready = socket(&websocket);
    handshake(&ready);
    advance_clock(WS_HANDSHAKE_TIMEOUT_MS * 3);
    assert_eq!(socket(&websocket), ready);
    assert!(websocket.is_connected());
    assert_eq!(timer_count(), 0);
}

#[wasm_bindgen_test]
fn overdue_handshake_cannot_authorize_before_the_resumed_timer_runs() {
    let _fixture = Fixture::new(true);
    let websocket = service();
    websocket.connect_ws_with_backoff();
    let stalled = socket(&websocket);
    open_socket(&stalled);
    version(&stalled);
    jump_clock(WS_HANDSHAKE_TIMEOUT_MS + 1);
    message(&stalled, &ProtocolMessage::Authorized);
    assert!(!websocket.is_connected());
    assert_detached(&stalled);
    assert_eq!(timer_count(), 1);
    advance_clock(next_delay());
    handshake(&socket(&websocket));
    assert!(websocket.is_connected());
}

#[wasm_bindgen_test]
fn constructor_send_errors_and_late_callbacks_share_one_retry() {
    let _fixture = Fixture::new(true);
    let websocket = service();
    reject_constructor(true);
    websocket.connect_ws_with_backoff();
    assert!(websocket.socket.borrow().is_none());
    assert_eq!(timer_count(), 1);
    assert_eq!(websocket.attempt_counter.get(), 1);

    reject_constructor(false);
    reject_send(true);
    advance_clock(next_delay());
    let failed_version = socket(&websocket);
    open_socket(&failed_version);
    assert_detached(&failed_version);
    assert_eq!(websocket.attempt_counter.get(), 2);
    assert_eq!(timer_count(), 1);

    reject_send(false);
    advance_clock(next_delay());
    let failed_auth = socket(&websocket);
    open_socket(&failed_auth);
    reject_send(true);
    version(&failed_auth);
    assert_detached(&failed_auth);
    assert_eq!(websocket.attempt_counter.get(), 3);
    assert_eq!(timer_count(), 1);

    reject_send(false);
    advance_clock(next_delay());
    let broken = socket(&websocket);
    let old_epoch = websocket.connection_epoch.get();
    let mut late_close = websocket.socket_handler(old_epoch, WebSocketService::on_close);
    let mut late_watchdog = websocket.socket_handler(old_epoch, WebSocketService::on_handshake_timeout);
    let mut late_message = websocket.socket_handler(old_epoch, WebSocketService::on_message);
    error(&broken);
    let retry = next_delay();
    late_close(CloseEvent::new("close").unwrap());
    assert_detached(&broken);
    assert_eq!(websocket.attempt_counter.get(), 4);
    assert_eq!(timer_count(), 1);
    assert_eq!(next_delay(), retry);

    advance_clock(retry);
    let ready = socket(&websocket);
    let mut cleared_watchdog =
        websocket.socket_handler(websocket.connection_epoch.get(), WebSocketService::on_handshake_timeout);
    handshake(&ready);
    let context = websocket.connection_context();
    late_close(CloseEvent::new("close").unwrap());
    late_watchdog(());
    cleared_watchdog(());
    late_message(message_event(&ProtocolMessage::Unauthorized.to_bytes().unwrap()));
    assert_eq!(socket(&websocket), ready);
    assert!(websocket.is_current_connection(context));
    assert_eq!(timer_count(), 0);
}

#[wasm_bindgen_test]
fn only_the_ordered_handshake_releases_live_messages() {
    let _fixture = Fixture::new(true);
    let websocket = service();
    let connected = Rc::new(Cell::new(0));
    let statuses = Rc::new(Cell::new(0));
    let seen_connected = Rc::clone(&connected);
    let seen_status = Rc::clone(&statuses);
    websocket.event_service.subscribe(move |event| match event {
        EventMessage::WebSocketStatus(true) => seen_connected.set(seen_connected.get() + 1),
        EventMessage::ServerStatus(_) => seen_status.set(seen_status.get() + 1),
        _ => {}
    });
    websocket.connect_ws_with_backoff();
    let active = socket(&websocket);
    let status = ProtocolMessage::StatusResponse(StatusCheck::default());
    version(&active);
    open_socket(&active);
    active.dispatch_event(message_event(b"invalid json").unchecked_ref()).unwrap();
    message(&active, &ProtocolMessage::Version(PROTOCOL_VERSION - 1));
    message(&active, &ProtocolMessage::Authorized);
    message(&active, &status);
    assert_eq!(sent_count(&active), 1);
    assert!(!websocket.is_connected());
    version(&active);
    message(&active, &status);
    assert_eq!(connected.get(), 0);
    assert_eq!(statuses.get(), 0);
    assert_eq!(sent_count(&active), 2);
    message(&active, &ProtocolMessage::Authorized);
    message(&active, &ProtocolMessage::Authorized);
    version(&active);
    message(&active, &status);
    assert_eq!(connected.get(), 1);
    assert_eq!(statuses.get(), 1);
    assert_eq!(sent_count(&active), 2);
}

#[wasm_bindgen_test]
fn a_session_without_a_token_is_ready_after_the_matching_version() {
    let _fixture = Fixture::new(false);
    let websocket = service();
    websocket.connect_ws_with_backoff();
    let active = socket(&websocket);
    open_socket(&active);
    message(&active, &ProtocolMessage::Authorized);
    assert!(!websocket.is_connected());
    version(&active);
    assert!(websocket.is_connected());
    assert_eq!(sent_count(&active), 1);
    assert_eq!(timer_count(), 0);
}

#[wasm_bindgen_test]
async fn delayed_http_status_cannot_overwrite_newer_http_or_live_status() {
    let _fixture = Fixture::new(true);
    let websocket = service();
    let seen = Rc::new(RefCell::new(Vec::new()));
    let observed = Rc::clone(&seen);
    websocket.event_service.subscribe(move |event| {
        if let EventMessage::ServerStatus(status) = event {
            observed.borrow_mut().push(status.status.clone());
        }
    });
    websocket.connect_ws_with_backoff();
    let status = |name: &str| StatusCheck { status: name.into(), ..StatusCheck::default() };

    let mut older = pin!(websocket.get_server_status());
    let mut newer = pin!(websocket.get_server_status());
    assert!(futures::poll!(&mut older).is_pending());
    assert!(futures::poll!(&mut newer).is_pending());
    resolve_status(1, &serde_json::to_string(&status("new HTTP")).unwrap());
    newer.await;
    resolve_status(0, &serde_json::to_string(&status("old HTTP")).unwrap());
    older.await;
    assert_eq!(seen.borrow().as_slice(), ["new HTTP"]);

    let mut before_retry = pin!(websocket.get_server_status());
    assert!(futures::poll!(&mut before_retry).is_pending());
    close(&socket(&websocket));
    advance_clock(next_delay());
    resolve_status(2, &serde_json::to_string(&status("HTTP during outage")).unwrap());
    before_retry.await;
    assert_eq!(seen.borrow().as_slice(), ["new HTTP", "HTTP during outage"]);

    let mut before_ready = pin!(websocket.get_server_status());
    assert!(futures::poll!(&mut before_ready).is_pending());
    let active = socket(&websocket);
    handshake(&active);
    message(&active, &ProtocolMessage::StatusResponse(status("live")));
    resolve_status(3, &serde_json::to_string(&status("old outage HTTP")).unwrap());
    before_ready.await;
    assert_eq!(seen.borrow().as_slice(), ["new HTTP", "HTTP during outage", "live"]);
}
