use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{WebSocket, MessageEvent, Event, ErrorEvent, CloseEvent};
use std::cell::RefCell;
use std::collections::{HashMap};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use web_sys::js_sys::{Uint8Array, ArrayBuffer};
use log::{debug, error, trace};
use shared::model::{ProtocolMessage, StatusCheck, PROTOCOL_VERSION};
use crate::services::{get_token, StatusService};

#[derive(Clone)]
pub enum WsMessage {
    Unauthorized,
    ServerStatus(Rc<StatusCheck>),
    ActiveUser(usize, usize),
    ActiveProvider(String, usize),
}

const WS_PATH: &str = "/ws";

type Subscriber = RefCell<HashMap<usize, Box<dyn Fn(WsMessage)>>>;

pub struct WebSocketService {
    connected: Rc<AtomicBool>,
    ws: Rc<RefCell<Option<WebSocket>>>,
    status_service: Rc<StatusService>,
    subscriber_id: Rc<AtomicUsize>,
    subscribers: Rc<Subscriber>,
}

impl WebSocketService {
    pub fn new(status_service: Rc<StatusService>) -> Self {
        Self {
            connected: Rc::new(AtomicBool::new(false)),
            ws: Rc::new(RefCell::new(None)),
            status_service,
            subscriber_id: Rc::new(AtomicUsize::new(0)),
            subscribers: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    pub fn subscribe<F: Fn(WsMessage) + 'static>(&self, callback: F) -> usize {
        let sub_id = self.subscriber_id.fetch_add(1, Ordering::SeqCst);
        self.subscribers.borrow_mut().insert(sub_id, Box::new(callback));
        sub_id
    }

    pub fn unsubscribe(&self, sub_id: usize) {
        self.subscribers.borrow_mut().remove(&sub_id);
    }

    pub fn broadcast(&self, msg: WsMessage) {
        for (_, cb) in self.subscribers.borrow().iter() {
            cb(msg.clone());
        }
    }

    pub fn connect_ws(&self) {
        if self.connected.load(Ordering::SeqCst) {
            return;
        }
        match WebSocket::new(WS_PATH) {
            Err(err) => error!("Failed to open websocket connection: {err:?}"),
            Ok(socket) => {
                socket.set_binary_type(web_sys::BinaryType::Arraybuffer);
                let ws_clone = self.ws.clone();
                *ws_clone.borrow_mut() = Some(socket.clone());
                let subscribers_clone = self.subscribers.clone();
                let broadcast = move |msg: WsMessage| {
                    for (_, cb) in subscribers_clone.borrow().iter() {
                        cb(msg.clone());
                    }
                };

                // onmessage
                let onmessage_callback = Closure::<dyn FnMut(_)>::wrap(Box::new(move |event: MessageEvent| {
                    trace!("WebSocket received message: {event:?}");
                    if let Ok(buf) = event.data().dyn_into::<ArrayBuffer>() {
                         let array = Uint8Array::new(&buf);
                         let bytes = bytes::Bytes::from(array.to_vec());
                        match ProtocolMessage::from_bytes(bytes) {
                            Ok(message) => {
                                match message {
                                    ProtocolMessage::Unauthorized => {
                                        broadcast(WsMessage::Unauthorized);
                                    },
                                    ProtocolMessage::Error(err) => {
                                        error!("{err}");
                                    },
                                    ProtocolMessage::ActiveUserResponse(user_count, connections) => {
                                        broadcast(WsMessage::ActiveUser(user_count, connections));
                                    },
                                    ProtocolMessage::ActiveProviderResponse(user_count, connections) => {
                                        broadcast(WsMessage::ActiveProvider(user_count, connections));
                                    },
                                    ProtocolMessage::StatusResponse(status) => {
                                        let data = Rc::new(status);
                                        broadcast(WsMessage::ServerStatus(data));
                                    }
                                    ProtocolMessage::Version(_)
                                    | ProtocolMessage::StatusRequest(_) => {}
                                }
                            }
                            Err(err) => error!("Failed to decode websocket message: {err}")
                        }
                    }
                }));
                socket.set_onmessage(Some(onmessage_callback.as_ref().unchecked_ref()));
                onmessage_callback.forget(); // Important: leak the closure to keep it alive

                let ws_open_clone = Rc::clone(&ws_clone);
                let connected_clone = self.connected.clone();
                // onopen
                let onopen_callback = Closure::<dyn FnMut(_)>::wrap(Box::new(move |_event: Event| {
                    trace!("WebSocket connection opened.");
                    connected_clone.store(true, Ordering::SeqCst);
                    match ProtocolMessage::Version(PROTOCOL_VERSION).to_bytes() {
                        Ok(bytes) => Self::try_send_message(ws_open_clone.borrow().as_ref(), bytes),
                        Err(err) => error!("Failed to create WebSocket protocol version message: {err}"),
                    }
                }));
                socket.set_onopen(Some(onopen_callback.as_ref().unchecked_ref()));
                onopen_callback.forget();

                let ws_close_rc = self.ws.clone();
                let connected_clone = self.connected.clone();
                let onclose_callback = Closure::<dyn FnMut(_)>::wrap(Box::new(move |e: CloseEvent| {
                    debug!("Websocket closed, Code: {}, Reason: {}, WasClean: {}", e.code(), e.reason(), e.was_clean());
                    *ws_close_rc.borrow_mut() = None;
                    connected_clone.store(false, Ordering::SeqCst);
                }));
                socket.set_onclose(Some(onclose_callback.as_ref().unchecked_ref()));
                onclose_callback.forget();

                let connected_clone = self.connected.clone();
                // onerror
                let onerror_callback = Closure::<dyn FnMut(_)>::wrap(Box::new(move |e: ErrorEvent| {
                    error!("WebSocket error");
                    connected_clone.store(false, Ordering::SeqCst);
                    web_sys::console::error_1(&e);
                }));
                socket.set_onerror(Some(onerror_callback.as_ref().unchecked_ref()));
                onerror_callback.forget();
            }
        }
    }

    fn try_send_message(ws_opt: Option<&WebSocket>, data: bytes::Bytes) {
        if let Some(ws) = ws_opt {
            if let Err(err) = ws.send_with_u8_array(data.as_ref()) {
                error!("Failed to send a websocket message: {err:?}");
            }
        }
    }

    pub fn send_message(&self, data: bytes::Bytes) {
        Self::try_send_message(self.ws.borrow().as_ref(), data);
    }

    pub async fn get_server_status(&self) {
        if self.connected.load(Ordering::SeqCst) {
            self.send_message(ProtocolMessage::StatusRequest(get_token().unwrap_or_default()).to_bytes().unwrap());
        } else {
            match self.status_service.get_server_status().await {
                Ok(status) => {
                    self.broadcast(WsMessage::ServerStatus(status));
                }
                Err(err) => {error!("Failed to get server status: {err:?}");}
            }
        }


        // TODO
        // on no wesocket connection

        //         let fetch_status = {
        //             let status = status_signal.clone();
        //             let services_ctx = services_ctx.clone();
        //             move || {
        //                 let status = status.clone();
        //                 let services_ctx = services_ctx.clone();
        //                 spawn_local(async move {
        //                     status.set(services_ctx.status.get_server_status().await.ok());
        //                 });
        //             }
        //         };
        //
        //         fetch_status();
        //         // all 5 seconds
        //         let interval = Interval::new(5000, move || {
        //             fetch_status();
        //         });
        //
        //         // Cleanup function
        //         || drop(interval)
    }
}