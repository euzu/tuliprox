use crate::{
    services::{get_base_href, get_token},
    utils::set_timeout,
};
use log::{error, trace, warn};
use shared::{
    model::{LogEntry, LogLevel, LogWsMessage},
    utils::concat_path_leading_slash,
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use wasm_bindgen::{closure::Closure, JsCast};
use web_sys::{CloseEvent, ErrorEvent, Event, MessageEvent, WebSocket};
use yew::prelude::*;

const WS_RECONNECT_BASE_MS: i32 = 300;
const WS_RECONNECT_MAX_MS: i32 = 3000;
const DEFAULT_MAX_LOG_LINES: usize = 2000;

fn reconnect_delay(attempt: u16) -> i32 {
    if attempt < 6 {
        let d = WS_RECONNECT_BASE_MS * (i32::from(attempt) + 1);
        d.min(WS_RECONNECT_MAX_MS)
    } else {
        WS_RECONNECT_MAX_MS
    }
}

pub struct UseLogStreamOptions {
    pub active: bool,
    pub max_lines: usize,
}

impl Default for UseLogStreamOptions {
    fn default() -> Self {
        Self { active: true, max_lines: DEFAULT_MAX_LOG_LINES }
    }
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
    let connected = use_state(|| false);
    let logs = use_state(|| Rc::new(Vec::<LogEntry>::new()));
    let min_level = use_state(|| None::<LogLevel>);

    let ws_cell = use_mut_ref(|| None::<WebSocket>);
    let attempt_cell = use_mut_ref(|| 0u16);
    let active_cell = use_mut_ref(|| options.active);
    let max_lines = options.max_lines;

    *active_cell.borrow_mut() = options.active;

    // Clear callback
    let clear = {
        let logs = logs.clone();
        Callback::from(move |()| {
            logs.set(Rc::new(Vec::new()));
        })
    };

    // Set min level callback
    let set_min_level = {
        let min_level_state = min_level.clone();
        let ws_cell = ws_cell.clone();
        Callback::from(move |lvl: Option<LogLevel>| {
            min_level_state.set(lvl);
            if let Some(ws) = ws_cell.borrow().as_ref() {
                if ws.ready_state() == WebSocket::OPEN {
                    let msg = LogWsMessage::Filter { min_level: lvl };
                    if let Ok(json) = serde_json::to_string(&msg) {
                        let _ = ws.send_with_str(&json);
                    }
                }
            }
        })
    };

    {
        let connected = connected.clone();
        let logs = logs.clone();
        let min_level = min_level.clone();
        let ws_cell = ws_cell.clone();
        let attempt_cell = attempt_cell.clone();
        let active = options.active;

        use_effect_with(active, move |&active| {
            if !active {
                if let Some(ws) = ws_cell.borrow_mut().take() {
                    ws.set_onmessage(None);
                    ws.set_onopen(None);
                    ws.set_onerror(None);
                    ws.set_onclose(None);
                    let _ = ws.close();
                }
                connected.set(false);
                return Box::new(|| {}) as Box<dyn FnOnce()>;
            }

            let is_alive = Rc::new(Cell::new(true));

            type ConnectFn = Rc<dyn Fn()>;
            type ConnectFnCell = Rc<RefCell<Option<ConnectFn>>>;

            let connect_fn: ConnectFnCell = Rc::new(RefCell::new(None));
            let connect_fn_clone = connect_fn.clone();

            let do_connect = {
                let connected = connected.clone();
                let logs = logs.clone();
                let min_level = min_level.clone();
                let ws_cell = ws_cell.clone();
                let attempt_cell = attempt_cell.clone();
                let is_alive = is_alive.clone();
                let connect_fn = connect_fn_clone.clone();

                Rc::new(move || {
                    if !is_alive.get() {
                        return;
                    }

                    if let Some(ws) = ws_cell.borrow().as_ref() {
                        let state = ws.ready_state();
                        if state == WebSocket::CONNECTING || state == WebSocket::OPEN {
                            return;
                        }
                    }

                    let base_href = get_base_href();
                    let path = concat_path_leading_slash(&base_href, "ws/logs");
                    let token_opt = get_token();
                    let ws_url = if let Some(ref token) = token_opt { format!("{path}?token={token}") } else { path };

                    match WebSocket::new(&ws_url) {
                        Err(err) => {
                            error!("Failed to create log websocket: {err:?}");
                            let attempt = *attempt_cell.borrow();
                            *attempt_cell.borrow_mut() = attempt.saturating_add(1);
                            let delay = reconnect_delay(attempt);
                            let connect_fn_inner = connect_fn.clone();
                            set_timeout(
                                move || {
                                    if let Some(f) = connect_fn_inner.borrow().as_ref() {
                                        f();
                                    }
                                },
                                delay,
                            );
                        }
                        Ok(socket) => {
                            *ws_cell.borrow_mut() = Some(socket.clone());

                            // onopen
                            {
                                let socket_clone = socket.clone();
                                let token_opt = token_opt.clone();
                                let min_level_val = *min_level;
                                let attempt_cell = attempt_cell.clone();
                                let is_alive = is_alive.clone();

                                let onopen = Closure::<dyn FnMut(Event)>::wrap(Box::new(move |_| {
                                    if !is_alive.get() {
                                        return;
                                    }
                                    trace!("Log WebSocket connected");
                                    *attempt_cell.borrow_mut() = 0;

                                    if let Some(ref token) = token_opt {
                                        let auth_msg = LogWsMessage::Auth(token.clone());
                                        if let Ok(json) = serde_json::to_string(&auth_msg) {
                                            let _ = socket_clone.send_with_str(&json);
                                        }
                                    }

                                    if let Some(lvl) = min_level_val {
                                        let filter_msg = LogWsMessage::Filter { min_level: Some(lvl) };
                                        if let Ok(json) = serde_json::to_string(&filter_msg) {
                                            let _ = socket_clone.send_with_str(&json);
                                        }
                                    }
                                }));
                                socket.set_onopen(Some(onopen.as_ref().unchecked_ref()));
                                onopen.forget();
                            }

                            // onmessage
                            {
                                let connected = connected.clone();
                                let logs = logs.clone();
                                let is_alive = is_alive.clone();

                                let onmessage =
                                    Closure::<dyn FnMut(MessageEvent)>::wrap(Box::new(move |e: MessageEvent| {
                                        if !is_alive.get() {
                                            return;
                                        }
                                        if let Ok(text) = e.data().dyn_into::<js_sys::JsString>() {
                                            let text_str: String = text.into();
                                            if let Ok(msg) = serde_json::from_str::<LogWsMessage>(&text_str) {
                                                match msg {
                                                    LogWsMessage::Authorized => {
                                                        connected.set(true);
                                                    }
                                                    LogWsMessage::Unauthorized => {
                                                        connected.set(false);
                                                        warn!("Log WebSocket unauthorized");
                                                    }
                                                    LogWsMessage::History(entries) => {
                                                        connected.set(true);
                                                        let mut new_logs = entries;
                                                        if new_logs.len() > max_lines {
                                                            let skip = new_logs.len() - max_lines;
                                                            new_logs.drain(0..skip);
                                                        }
                                                        logs.set(Rc::new(new_logs));
                                                    }
                                                    LogWsMessage::Entry(entry) => {
                                                        connected.set(true);
                                                        logs.set({
                                                            let mut list = (**logs).clone();
                                                            list.push(entry);
                                                            if list.len() > max_lines {
                                                                list.remove(0);
                                                            }
                                                            Rc::new(list)
                                                        });
                                                    }
                                                    _ => {}
                                                }
                                            }
                                        }
                                    }));
                                socket.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
                                onmessage.forget();
                            }

                            // onclose
                            {
                                let connected = connected.clone();
                                let ws_cell = ws_cell.clone();
                                let attempt_cell = attempt_cell.clone();
                                let is_alive = is_alive.clone();
                                let connect_fn_inner = connect_fn.clone();

                                let onclose = Closure::<dyn FnMut(CloseEvent)>::wrap(Box::new(move |_| {
                                    if !is_alive.get() {
                                        return;
                                    }
                                    trace!("Log WebSocket closed");
                                    connected.set(false);
                                    *ws_cell.borrow_mut() = None;

                                    let attempt = *attempt_cell.borrow();
                                    *attempt_cell.borrow_mut() = attempt.saturating_add(1);
                                    let delay = reconnect_delay(attempt);
                                    let connect_fn_reconnect = connect_fn_inner.clone();
                                    set_timeout(
                                        move || {
                                            if let Some(f) = connect_fn_reconnect.borrow().as_ref() {
                                                f();
                                            }
                                        },
                                        delay,
                                    );
                                }));
                                socket.set_onclose(Some(onclose.as_ref().unchecked_ref()));
                                onclose.forget();
                            }

                            // onerror
                            {
                                let onerror = Closure::<dyn FnMut(ErrorEvent)>::wrap(Box::new(move |e: ErrorEvent| {
                                    trace!("Log WebSocket error: {e:?}");
                                }));
                                socket.set_onerror(Some(onerror.as_ref().unchecked_ref()));
                                onerror.forget();
                            }
                        }
                    }
                })
            };

            *connect_fn.borrow_mut() = Some(do_connect.clone());
            do_connect();

            Box::new(move || {
                is_alive.set(false);
                if let Some(ws) = ws_cell.borrow_mut().take() {
                    ws.set_onmessage(None);
                    ws.set_onopen(None);
                    ws.set_onerror(None);
                    ws.set_onclose(None);
                    let _ = ws.close();
                }
            }) as Box<dyn FnOnce()>
        });
    }

    UseLogStreamHandle { connected: *connected, logs: (*logs).clone(), min_level: *min_level, set_min_level, clear }
}
