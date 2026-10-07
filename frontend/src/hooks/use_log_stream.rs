use crate::{
    hooks::use_service_context,
    model::EventMessage,
    services::{get_base_href, get_token, ActiveSocket, EventService},
};
use gloo_timers::callback::Timeout;
use log::{error, trace, warn};
use shared::{
    model::{LogEntry, LogLevel, LogWsMessage},
    utils::concat_path_leading_slash,
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use wasm_bindgen::closure::Closure;
use web_sys::{CloseEvent, ErrorEvent, Event, MessageEvent, WebSocket};
use yew::prelude::*;

const WS_RECONNECT_BASE_MS: u32 = 300;
const WS_RECONNECT_MAX_MS: u32 = 3000;
const DEFAULT_MAX_LOG_LINES: usize = 2000;

fn reconnect_delay(attempt: u16) -> u32 {
    if attempt < 6 {
        let d = WS_RECONNECT_BASE_MS * (u32::from(attempt) + 1);
        d.min(WS_RECONNECT_MAX_MS)
    } else {
        WS_RECONNECT_MAX_MS
    }
}

/// Owns a log subscription's socket and timer; callbacks only hold weak references.
struct LogStream {
    enabled: Cell<bool>,
    epoch: Cell<u64>,
    attempt: Cell<u16>,
    min_level: Cell<Option<LogLevel>>,
    socket: RefCell<Option<ActiveSocket>>,
    reconnect_timer: RefCell<Option<Timeout>>,
    connected: Callback<bool>,
    message: Callback<LogWsMessage>,
    events: Rc<EventService>,
}

impl LogStream {
    fn new(connected: Callback<bool>, message: Callback<LogWsMessage>, events: Rc<EventService>) -> Rc<Self> {
        Rc::new(Self {
            enabled: Cell::new(false),
            epoch: Cell::new(0),
            attempt: Cell::new(0),
            min_level: Cell::new(None),
            socket: RefCell::new(None),
            reconnect_timer: RefCell::new(None),
            connected,
            message,
            events,
        })
    }

    fn disconnect(&self) {
        self.enabled.set(false);
        self.epoch.set(self.epoch.get().wrapping_add(1));
        self.attempt.set(0);
        self.close_socket();
    }

    fn close_socket(&self) {
        let timer = self.reconnect_timer.borrow_mut().take();
        drop(timer);
        // Release the borrow before freeing any currently executing handler.
        let socket = self.socket.borrow_mut().take();
        drop(socket);
    }

    fn connect(self: &Rc<Self>) {
        if !self.enabled.get() {
            return;
        }
        if self
            .socket
            .borrow()
            .as_ref()
            .is_some_and(|socket| matches!(socket.websocket().ready_state(), WebSocket::CONNECTING | WebSocket::OPEN))
        {
            return;
        }
        self.close_socket();
        // Logout removes the token before the hook's effect cleanup runs.
        let Some(token) = get_token() else {
            self.disconnect();
            self.connected.emit(false);
            return;
        };
        let epoch = self.epoch.get().wrapping_add(1);
        self.epoch.set(epoch);
        let path = concat_path_leading_slash(&get_base_href(), "ws/logs");
        let ws = match WebSocket::new(&format!("{path}?token={token}")) {
            Ok(ws) => ws,
            Err(err) => {
                error!("Failed to create log websocket: {err:?}");
                self.schedule_reconnect();
                return;
            }
        };
        let socket = ActiveSocket::new(
            ws,
            Closure::new(self.handler(epoch, Self::on_message)),
            Closure::new(self.handler(epoch, Self::on_open)),
            Closure::new(self.handler(epoch, Self::on_close)),
            Closure::new(self.handler(epoch, Self::on_error)),
        );
        *self.socket.borrow_mut() = Some(socket);
    }

    fn handler<E: 'static>(self: &Rc<Self>, epoch: u64, handle: fn(&Rc<Self>, E)) -> impl FnMut(E) + 'static {
        let stream = Rc::downgrade(self);
        move |event| {
            if let Some(stream) = stream.upgrade() {
                if stream.enabled.get() && stream.epoch.get() == epoch {
                    handle(&stream, event);
                }
            }
        }
    }

    fn send(&self, message: &LogWsMessage) {
        if let Some(socket) = self.socket.borrow().as_ref() {
            if socket.websocket().ready_state() == WebSocket::OPEN {
                if let Ok(json) = serde_json::to_string(message) {
                    let _ = socket.websocket().send_with_str(&json);
                }
            }
        }
    }

    fn on_open(self: &Rc<Self>, _: Event) {
        trace!("Log WebSocket connected");
        if let Some(token) = get_token() {
            self.send(&LogWsMessage::Auth(token));
            self.send(&LogWsMessage::Filter { min_level: self.min_level.get() });
        } else {
            self.disconnect();
            self.connected.emit(false);
        }
    }

    fn on_message(self: &Rc<Self>, event: MessageEvent) {
        let Some(text) = event.data().as_string() else {
            return;
        };
        let Ok(message) = serde_json::from_str::<LogWsMessage>(&text) else {
            return;
        };
        match &message {
            LogWsMessage::Unauthorized => {
                warn!("Log WebSocket unauthorized");
                self.disconnect();
                self.connected.emit(false);
                self.events.broadcast(EventMessage::Unauthorized);
                return;
            }
            LogWsMessage::Authorized | LogWsMessage::History(_) | LogWsMessage::Entry(_) => {
                // Only successful authorization resets backoff, not a rejected upgrade.
                self.attempt.set(0);
                self.connected.emit(true);
            }
            _ => {}
        }
        self.message.emit(message);
    }

    fn on_close(self: &Rc<Self>, _: CloseEvent) {
        trace!("Log WebSocket closed");
        let epoch = self.epoch.get();
        self.close_socket();
        self.connected.emit(false);
        if self.epoch.get() == epoch {
            self.schedule_reconnect();
        }
    }

    fn on_error(self: &Rc<Self>, event: ErrorEvent) {
        trace!("Log WebSocket error: {event:?}");
        self.connected.emit(false);
    }

    fn schedule_reconnect(self: &Rc<Self>) {
        if !self.enabled.get() || self.reconnect_timer.borrow().is_some() {
            return;
        }
        let attempt = self.attempt.get();
        self.attempt.set(attempt.saturating_add(1));
        let epoch = self.epoch.get();
        let stream = Rc::downgrade(self);
        *self.reconnect_timer.borrow_mut() = Some(Timeout::new(reconnect_delay(attempt), move || {
            let Some(stream) = stream.upgrade() else {
                return;
            };
            let timer = stream.reconnect_timer.borrow_mut().take();
            drop(timer);
            if stream.enabled.get() && stream.epoch.get() == epoch {
                stream.connect();
            }
        }));
    }
}

pub struct UseLogStreamOptions {
    pub active: bool,
    pub max_lines: usize,
}

impl Default for UseLogStreamOptions {
    fn default() -> Self { Self { active: true, max_lines: DEFAULT_MAX_LOG_LINES } }
}

#[derive(Clone)]
pub struct UseLogStreamHandle {
    pub connected: bool,
    pub logs: Rc<Vec<LogEntry>>,
    pub min_level: Option<LogLevel>,
    pub set_min_level: Callback<Option<LogLevel>>,
    pub clear: Callback<()>,
}

#[hook]
pub fn use_log_stream(options: UseLogStreamOptions) -> UseLogStreamHandle {
    let services = use_service_context();
    let connected = use_state(|| false);
    let logs = use_state(|| Rc::new(Vec::<LogEntry>::new()));
    let min_level = use_state(|| None::<LogLevel>);
    let max_lines = use_mut_ref(|| options.max_lines);
    *max_lines.borrow_mut() = options.max_lines;
    // WebSocket events can arrive before Yew renders the previous state update.
    let log_buffer = use_mut_ref(|| Rc::new(Vec::<LogEntry>::new()));
    let stream = use_mut_ref({
        let connected = connected.setter();
        let logs = logs.setter();
        let buffer = Rc::clone(&log_buffer);
        let max_lines = Rc::clone(&max_lines);
        move || {
            LogStream::new(
                Callback::from(move |value| connected.set(value)),
                Callback::from(move |message| {
                    let updated = {
                        let mut buffer = buffer.borrow_mut();
                        match message {
                            LogWsMessage::History(entries) => *buffer = Rc::new(entries),
                            LogWsMessage::Entry(entry) => Rc::make_mut(&mut buffer).push(entry),
                            _ => return,
                        }
                        let max_lines = *max_lines.borrow();
                        let list = Rc::make_mut(&mut buffer);
                        let excess = list.len().saturating_sub(max_lines);
                        list.drain(..excess);
                        Rc::clone(&buffer)
                    };
                    logs.set(updated);
                }),
                Rc::clone(&services.event),
            )
        }
    });
    let stream = Rc::clone(&stream.borrow());

    let clear = {
        let logs = logs.setter();
        Callback::from(move |()| {
            let empty = Rc::new(Vec::new());
            *log_buffer.borrow_mut() = Rc::clone(&empty);
            logs.set(empty);
        })
    };

    let set_min_level = {
        let min_level_state = min_level.setter();
        let stream = Rc::clone(&stream);
        Callback::from(move |lvl: Option<LogLevel>| {
            min_level_state.set(lvl);
            stream.min_level.set(lvl);
            stream.send(&LogWsMessage::Filter { min_level: lvl });
        })
    };

    {
        let connected = connected.setter();
        use_effect_with(options.active, move |&active| {
            if active {
                stream.enabled.set(true);
                stream.connect();
            } else {
                stream.disconnect();
                connected.set(false);
            }
            move || stream.disconnect()
        });
    }

    UseLogStreamHandle { connected: *connected, logs: (*logs).clone(), min_level: *min_level, set_min_level, clear }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod browser_tests {
    use super::{reconnect_delay, LogStream};
    use crate::{
        model::EventMessage,
        services::{get_token, set_token, EventService},
    };
    use gloo_timers::future::TimeoutFuture;
    use shared::model::{LogWsMessage, TOKEN_NO_AUTH};
    use std::{cell::Cell, rc::Rc};
    use wasm_bindgen::JsValue;
    use wasm_bindgen_test::wasm_bindgen_test;
    use web_sys::{CloseEvent, MessageEvent, MessageEventInit, WebSocket};
    use yew::Callback;

    struct SavedToken(Option<String>);

    impl SavedToken {
        fn new() -> Self {
            let saved = Self(get_token());
            set_token(Some(TOKEN_NO_AUTH));
            saved
        }
    }

    impl Drop for SavedToken {
        fn drop(&mut self) { set_token(self.0.as_deref()); }
    }

    fn stream(events: Rc<EventService>) -> Rc<LogStream> { LogStream::new(Callback::noop(), Callback::noop(), events) }

    fn socket(stream: &LogStream) -> Result<WebSocket, JsValue> {
        stream
            .socket
            .borrow()
            .as_ref()
            .map(|socket| socket.websocket().clone())
            .ok_or_else(|| JsValue::from_str("missing log socket"))
    }

    fn assert_detached(socket: &WebSocket) {
        assert!(socket.onmessage().is_none());
        assert!(socket.onopen().is_none());
        assert!(socket.onclose().is_none());
        assert!(socket.onerror().is_none());
    }

    #[wasm_bindgen_test]
    fn unauthorized_callback_detaches_itself_and_stops_reconnecting() -> Result<(), JsValue> {
        let _saved = SavedToken::new();
        let events = Rc::new(EventService::new());
        let unauthorized = Rc::new(Cell::new(0));
        let observed = Rc::clone(&unauthorized);
        events.subscribe(move |event| {
            if event == EventMessage::Unauthorized {
                observed.set(observed.get() + 1);
            }
        });
        let stream = stream(events);
        stream.enabled.set(true);
        stream.connect();
        let socket = socket(&stream)?;
        let callback = socket.onmessage().ok_or_else(|| JsValue::from_str("missing message callback"))?;
        let init = MessageEventInit::new();
        let json =
            serde_json::to_string(&LogWsMessage::Unauthorized).map_err(|err| JsValue::from_str(&err.to_string()))?;
        init.set_data(&JsValue::from_str(&json));
        callback.call1(&socket, MessageEvent::new_with_event_init_dict("message", &init)?.as_ref())?;

        assert_eq!(unauthorized.get(), 1);
        assert!(!stream.enabled.get());
        assert!(stream.socket.borrow().is_none());
        assert!(stream.reconnect_timer.borrow().is_none());
        assert_detached(&socket);
        Ok(())
    }

    #[wasm_bindgen_test]
    async fn close_callback_reconnect_is_canceled_when_the_hook_unmounts() -> Result<(), JsValue> {
        let _saved = SavedToken::new();
        let stream = stream(Rc::new(EventService::new()));
        stream.enabled.set(true);
        stream.connect();
        let socket = socket(&stream)?;
        let callback = socket.onclose().ok_or_else(|| JsValue::from_str("missing close callback"))?;
        callback.call1(&socket, CloseEvent::new("close")?.as_ref())?;
        assert_detached(&socket);
        assert!(stream.socket.borrow().is_none());
        assert!(stream.reconnect_timer.borrow().is_some());

        stream.disconnect();
        let epoch = stream.epoch.get();
        assert!(stream.reconnect_timer.borrow().is_none());
        TimeoutFuture::new(reconnect_delay(0) + 50).await;
        assert!(stream.socket.borrow().is_none());
        assert_eq!(stream.epoch.get(), epoch);
        Ok(())
    }

    #[wasm_bindgen_test]
    async fn pending_reconnect_does_not_open_a_socket_after_logout() {
        let _saved = SavedToken::new();
        let stream = stream(Rc::new(EventService::new()));
        stream.enabled.set(true);
        stream.schedule_reconnect();
        set_token(None);
        TimeoutFuture::new(reconnect_delay(0) + 50).await;
        assert!(!stream.enabled.get());
        assert!(stream.socket.borrow().is_none());
        assert!(stream.reconnect_timer.borrow().is_none());
    }

    #[wasm_bindgen_test]
    fn socket_and_timer_do_not_keep_an_unmounted_stream_alive() -> Result<(), JsValue> {
        let _saved = SavedToken::new();
        let stream = stream(Rc::new(EventService::new()));
        let weak = Rc::downgrade(&stream);
        stream.enabled.set(true);
        stream.connect();
        let socket = socket(&stream)?;
        stream.schedule_reconnect();
        stream.schedule_reconnect();
        assert_eq!(stream.attempt.get(), 1);
        drop(stream);
        assert!(weak.upgrade().is_none());
        assert_detached(&socket);
        Ok(())
    }

    #[wasm_bindgen_test]
    fn callbacks_from_an_old_subscription_cannot_change_a_new_subscription() -> Result<(), JsValue> {
        let _saved = SavedToken::new();
        let calls = Rc::new(Cell::new(0));
        let observed = Rc::clone(&calls);
        let stream = LogStream::new(
            Callback::from(move |_| observed.set(observed.get() + 1)),
            Callback::noop(),
            Rc::new(EventService::new()),
        );
        stream.enabled.set(true);
        stream.connect();
        let old_socket = socket(&stream)?;
        let mut old_handler = stream.handler(stream.epoch.get(), LogStream::on_close);
        stream.disconnect();
        stream.enabled.set(true);
        stream.connect();
        let epoch = stream.epoch.get();
        old_handler(CloseEvent::new("close")?);
        assert_eq!(calls.get(), 0);
        assert_eq!(stream.epoch.get(), epoch);
        assert!(stream.socket.borrow().is_some());
        assert!(stream.reconnect_timer.borrow().is_none());
        assert_detached(&old_socket);
        stream.disconnect();
        Ok(())
    }
}
