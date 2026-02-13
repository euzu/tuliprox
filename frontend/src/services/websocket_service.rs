use crate::model::EventMessage;
use crate::services::{get_base_href, get_token, EventService, StatusService};
use crate::utils::set_timeout;
use log::{error, trace, warn};
use shared::model::{ProtocolMessage, PROTOCOL_VERSION};
use shared::utils::concat_path_leading_slash;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use web_sys::js_sys::{ArrayBuffer, Uint8Array};
use web_sys::{CloseEvent, ErrorEvent, Event, MessageEvent, WebSocket};

const WS_RECONNECT_BASE_MS: u32 = 300;
const WS_RECONNECT_MAX_MS: u32 = 2000;
const WS_RECONNECT_MAX_ATTEMPTS: u16 = 20;

fn reconnect_delay(attempt: u16) -> u32 {
    if attempt < 6 {
        let d = WS_RECONNECT_BASE_MS * (attempt as u32 + 1u32);
        d.min(WS_RECONNECT_MAX_MS)
    } else {
        WS_RECONNECT_MAX_MS
    }
}

type JsOnMessageCallback = Option<Closure<dyn FnMut(MessageEvent)>>;
type JsOnCloseCallback = Option<Closure<dyn FnMut(CloseEvent)>>;
type JsOnErrorCallback = Option<Closure<dyn FnMut(ErrorEvent)>>;
type JsOnOpenCallback = Option<Closure<dyn FnMut(Event)>>;

pub struct WebSocketService {
    connected: Rc<Cell<bool>>,
    attempt_counter: Rc<Cell<u16>>,
    ws: Rc<RefCell<Option<WebSocket>>>,
    status_service: Rc<StatusService>,
    event_service: Rc<EventService>,
    ws_path: String,

    // store closures so they live as long as we want and can be dropped on close
    ws_onmessage: Rc<RefCell<JsOnMessageCallback>>,
    ws_onopen: Rc<RefCell<JsOnOpenCallback>>,
    ws_onclose: Rc<RefCell<JsOnCloseCallback>>,
    ws_onerror: Rc<RefCell<JsOnErrorCallback>>,
}

impl WebSocketService {
    pub fn new(status_service: Rc<StatusService>, event_service: Rc<EventService>) -> Self {
        let base_href = get_base_href();
        Self {
            connected: Rc::new(Cell::new(false)),
            attempt_counter: Rc::new(Cell::new(0)),
            ws: Rc::new(RefCell::new(None)),
            status_service,
            event_service,
            ws_path: concat_path_leading_slash(&base_href, "ws"),
            ws_onmessage: Rc::new(RefCell::new(None)),
            ws_onopen: Rc::new(RefCell::new(None)),
            ws_onclose: Rc::new(RefCell::new(None)),
            ws_onerror: Rc::new(RefCell::new(None)),
        }
    }

    /// Helper function to allow cloning the service into JS closures for reconnect
    fn clone_for_reconnect(&self) -> Self {
        Self {
            connected: self.connected.clone(),
            attempt_counter: self.attempt_counter.clone(),
            ws: self.ws.clone(),
            status_service: self.status_service.clone(),
            event_service: self.event_service.clone(),
            ws_path: self.ws_path.clone(),
            ws_onmessage: self.ws_onmessage.clone(),
            ws_onopen: self.ws_onopen.clone(),
            ws_onclose: self.ws_onclose.clone(),
            ws_onerror: self.ws_onerror.clone(),
        }
    }

    pub fn connect_ws_with_backoff(&self) {
        if self.connected.get() {
            return;
        }
        match WebSocket::new(&self.ws_path) {
            Err(err) => error!("Failed to open websocket connection: {err:?}"),
            Ok(socket) => {
                socket.set_binary_type(web_sys::BinaryType::Arraybuffer);
                let ws_clone = self.ws.clone();
                *ws_clone.borrow_mut() = Some(socket.clone());

                // onmessage
                {
                    let ws_onmessage_ref = self.ws_onmessage.clone();
                    let ws_onmessage_clone = ws_clone.clone();
                    let event_service = self.event_service.clone();
                    let attempt_counter = self.attempt_counter.clone();
                    let onmessage_callback = Closure::<dyn FnMut(MessageEvent)>::wrap(Box::new(
                        move |event: MessageEvent| {
                            trace!("WebSocket received message: {event:?}");
                            if let Some(response) =
                                handle_socket_protocol_msg(event, &event_service, &attempt_counter)
                            {
                                Self::try_send_message(
                                    ws_onmessage_clone.borrow().as_ref(),
                                    response,
                                );
                            }
                        },
                    ));
                    socket.set_onmessage(Some(onmessage_callback.as_ref().unchecked_ref()));
                    // store closure so we can drop it later (no forget)
                    ws_onmessage_ref.borrow_mut().replace(onmessage_callback);
                }

                // onopen
                {
                    let ws_onopen_ref = self.ws_onopen.clone();
                    let ws_open_clone = ws_clone.clone();
                    let connected_clone = self.connected.clone();
                    let onopen_callback =
                        Closure::<dyn FnMut(Event)>::wrap(Box::new(move |_event: Event| {
                            // on open is called on a connect attempt, it does not mean it is connected!
                            trace!("WebSocket connection opened.");
                            if Self::try_send_message(
                                ws_open_clone.borrow().as_ref(),
                                ProtocolMessage::Version(PROTOCOL_VERSION),
                            ) {
                                connected_clone.set(true);
                            }
                        }));
                    socket.set_onopen(Some(onopen_callback.as_ref().unchecked_ref()));
                    ws_onopen_ref.borrow_mut().replace(onopen_callback);
                }

                // prepare a reconnect-holder (shared clone)
                let ws_service_reconnect = Rc::new(self.clone_for_reconnect());

                // onclose
                {
                    let ws_onclose_ref = self.ws_onclose.clone();
                    let ws_onmessage_ref = self.ws_onmessage.clone();
                    let ws_onopen_ref = self.ws_onopen.clone();
                    let ws_onerror_ref = self.ws_onerror.clone();
                    let ws_close_rc = self.ws.clone();
                    let connected_clone = self.connected.clone();
                    let event_service_clone = Rc::clone(&self.event_service);
                    let ws_service_reconnect_clone = Rc::clone(&ws_service_reconnect);

                    let onclose_callback =
                        Closure::<dyn FnMut(CloseEvent)>::wrap(Box::new(move |e: CloseEvent| {
                            trace!(
                                "WebSocket closed (Code {}, Reason: {}, Clean: {})",
                                e.code(),
                                e.reason(),
                                e.was_clean()
                            );

                            // clear JS handlers on the underlying socket if present
                            if let Some(s) = ws_close_rc.borrow().as_ref() {
                                s.set_onmessage(None);
                                s.set_onopen(None);
                                s.set_onerror(None);
                                s.set_onclose(None);
                            }

                            // drop stored closures so they are freed
                            ws_onmessage_ref.borrow_mut().take();
                            ws_onopen_ref.borrow_mut().take();
                            ws_onerror_ref.borrow_mut().take();
                            // Note: we keep ws_onclose alive (this closure) until function returns;
                            // it will be dropped when the service or field is taken elsewhere if desired.

                            *ws_close_rc.borrow_mut() = None;
                            connected_clone.set(false);
                            event_service_clone.broadcast(EventMessage::WebSocketStatus(false));

                            // schedule reconnect
                            ws_service_reconnect_clone.schedule_reconnect();
                        }));
                    socket.set_onclose(Some(onclose_callback.as_ref().unchecked_ref()));
                    ws_onclose_ref.borrow_mut().replace(onclose_callback);
                }

                // onerror
                {
                    let ws_onerror_ref = self.ws_onerror.clone();
                    let connected_clone = self.connected.clone();
                    let event_service_clone = Rc::clone(&self.event_service);
                    //let ws_service_reconnect_clone = Rc::clone(&ws_service_reconnect);

                    let onerror_callback =
                        Closure::<dyn FnMut(ErrorEvent)>::wrap(Box::new(move |e: ErrorEvent| {
                            error!("WebSocket error: {:?}", e);
                            connected_clone.set(false);
                            event_service_clone.broadcast(EventMessage::WebSocketStatus(false));
                            // ws_service_reconnect_clone.schedule_reconnect();
                        }));
                    socket.set_onerror(Some(onerror_callback.as_ref().unchecked_ref()));
                    ws_onerror_ref.borrow_mut().replace(onerror_callback);
                }
            }
        }
    }

    fn schedule_reconnect(&self) {
        // increment attempts atomically and get the previous value
        let attempt = self.attempt_counter.get() + 1;
        self.attempt_counter.set(attempt);

        if attempt >= WS_RECONNECT_MAX_ATTEMPTS {
            warn!("WebSocket reconnect attempts exceeded ({attempt}). Giving up.");
            return;
        }
        let delay = reconnect_delay(attempt);

        warn!(
            "WebSocket reconnect attempt #{attempt} scheduled in {} ms",
            delay
        );

        // clone_for_reconnect returns a Service with shared internals (Atomic, etc.)
        let ws_clone = Rc::new(self.clone_for_reconnect());
        set_timeout(
            move || {
                ws_clone.connect_ws_with_backoff();
            },
            delay as i32,
        );
    }

    fn try_send_message(ws_opt: Option<&WebSocket>, msg: ProtocolMessage) -> bool {
        if let Some(ws) = ws_opt {
            match msg.to_bytes() {
                Ok(bytes) => {
                    if let Err(err) = ws.send_with_u8_array(bytes.as_ref()) {
                        error!("Failed to send a websocket message: {err:?}");
                    } else {
                        return true;
                    }
                }
                Err(err) => {
                    error!("Failed to create WebSocket protocol version message: {err}")
                }
            }
        }
        false
    }

    pub fn send_message(&self, msg: ProtocolMessage) -> bool {
        Self::try_send_message(self.ws.borrow().as_ref(), msg)
    }

    pub async fn get_server_status(&self) {
        if self.connected.get() {
            if let Some(token) = get_token() {
                self.send_message(ProtocolMessage::StatusRequest(token));
            }
        } else {
            match self.status_service.get_server_status().await {
                Ok(Some(status)) => {
                    self.event_service
                        .broadcast(EventMessage::ServerStatus(status));
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
}

fn handle_socket_protocol_msg(
    event: MessageEvent,
    event_service: &Rc<EventService>,
    attempt_counter: &Rc<Cell<u16>>,
) -> Option<ProtocolMessage> {
    if let Ok(buf) = event.data().dyn_into::<ArrayBuffer>() {
        let array = Uint8Array::new(&buf);
        let bytes = bytes::Bytes::from(array.to_vec());
        match ProtocolMessage::from_bytes(bytes) {
            Ok(message) => {
                match message {
                    ProtocolMessage::Unauthorized => {
                        event_service.broadcast(EventMessage::Unauthorized);
                    }
                    ProtocolMessage::Error(err) => {
                        error!("{err}");
                    }
                    ProtocolMessage::ActiveUserResponse(event) => {
                        event_service.broadcast(EventMessage::ActiveUser(event));
                    }
                    ProtocolMessage::ActiveProviderResponse(provider_name, connections) => {
                        event_service
                            .broadcast(EventMessage::ActiveProvider(provider_name, connections));
                        if let Some(token) = get_token() {
                            return Some(ProtocolMessage::ActiveProviderCountRequest(token));
                        }
                    }
                    ProtocolMessage::ActiveProviderCountResponse(connections) => {
                        event_service.broadcast(EventMessage::ActiveProviderCount(connections));
                    }
                    ProtocolMessage::StatusResponse(status) => {
                        let data = Rc::new(status);
                        event_service.broadcast(EventMessage::ServerStatus(data));
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
                    ProtocolMessage::PlaylistUpdateProgressResponse(target, msg) => {
                        event_service.broadcast(EventMessage::PlaylistUpdateProgress(target, msg));
                    }
                    ProtocolMessage::SystemInfoResponse(system_info) => {
                        event_service.broadcast(EventMessage::SystemInfoUpdate(system_info));
                    }
                    ProtocolMessage::LibraryScanProgressResponse(msg) => {
                        event_service.broadcast(EventMessage::LibraryScanProgress(msg));
                    }
                    ProtocolMessage::Version(_) => {
                        attempt_counter.set(0);
                        event_service.broadcast(EventMessage::WebSocketStatus(true));
                        if let Some(token) = get_token() {
                            return Some(ProtocolMessage::Auth(token));
                        }
                    }
                    ProtocolMessage::UserActionResponse(_success) => {
                        // Success is already handled in the UI component that initiated the action
                    }
                    ProtocolMessage::Auth(_)
                    | ProtocolMessage::Authorized
                    | ProtocolMessage::ActiveProviderCountRequest(_)
                    | ProtocolMessage::StatusRequest(_)
                    | ProtocolMessage::UserAction(_) => {}
                }
            }
            Err(err) => error!("Failed to decode websocket message: {err}"),
        }
    }
    None
}
