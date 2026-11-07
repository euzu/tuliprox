use crate::{
    app::components::{input::Input, RadioButtonGroup, TextButton},
    hooks::{use_clipboard_copy, use_service_context},
    i18n::use_translation,
    model::EventMessage,
    services::{get_base_href, get_token, ActiveSocket, EventService},
};
use gloo_timers::callback::Timeout;
use js_sys::{encode_uri_component, Date};
use log::{error, trace, warn};
use shared::{
    model::{LogEntry, LogLevel, LogWsMessage, PluginLogEntryDto, PluginLogLevelDto, PluginLogQueryDto},
    utils::concat_path_leading_slash,
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use wasm_bindgen::{closure::Closure, JsValue};
use web_sys::{CloseEvent, ErrorEvent, Event, HtmlElement, MessageEvent, WebSocket};
use yew::{
    classes, component, html, platform::spawn_local, use_effect_with, use_mut_ref, use_node_ref, use_state, Callback,
    Html, Properties,
};

/// Lines kept in the viewer; the oldest are dropped first.
const MAX_LOG_LINES: usize = 1000;
/// Lines requested from the REST backlog.
const BACKLOG_LIMIT: usize = 500;
const WS_RECONNECT_BASE_MS: u32 = 300;
const WS_RECONNECT_MAX_MS: u32 = 3000;
/// Distance from the bottom, in pixels, within which the view counts as scrolled to the end.
const SCROLL_BOTTOM_THRESHOLD: i32 = 35;

fn reconnect_delay(attempt: u16) -> u32 {
    if attempt < 6 {
        let d = WS_RECONNECT_BASE_MS * (u32::from(attempt) + 1);
        d.min(WS_RECONNECT_MAX_MS)
    } else {
        WS_RECONNECT_MAX_MS
    }
}

/// The log target the backend gives a plugin's records.
fn plugin_target(plugin_id: &str) -> String { format!("plugin::{plugin_id}") }

/// One displayed line, from either the REST backlog or the live stream.
#[derive(Clone, PartialEq, Debug)]
struct LogRow {
    timestamp: String,
    level: LogLevel,
    message: String,
}

/// Local time as `YYYY-MM-DD HH:MM:SS`, the same for backlog and live lines.
fn format_timestamp(ts_ms: i64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let date = Date::new(&JsValue::from_f64(ts_ms as f64));
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        date.get_full_year(),
        date.get_month() + 1,
        date.get_date(),
        date.get_hours(),
        date.get_minutes(),
        date.get_seconds()
    )
}

/// The wire levels of the REST endpoint and the websocket are the same set.
const fn level_from_dto(level: PluginLogLevelDto) -> LogLevel {
    match level {
        PluginLogLevelDto::Error => LogLevel::Error,
        PluginLogLevelDto::Warn => LogLevel::Warn,
        PluginLogLevelDto::Info => LogLevel::Info,
        PluginLogLevelDto::Debug => LogLevel::Debug,
        PluginLogLevelDto::Trace => LogLevel::Trace,
    }
}

fn row_from_backlog(entry: &PluginLogEntryDto) -> LogRow {
    let ts_ms = i64::try_from(entry.ts_ms).unwrap_or(i64::MAX);
    LogRow { timestamp: format_timestamp(ts_ms), level: level_from_dto(entry.level), message: entry.message.clone() }
}

fn row_from_live(entry: LogEntry) -> LogRow {
    let timestamp = chrono::DateTime::parse_from_rfc3339(&entry.timestamp)
        .map_or(entry.timestamp.clone(), |parsed| format_timestamp(parsed.timestamp_millis()));
    LogRow { timestamp, level: entry.level, message: entry.message }
}

/// Appends `rows`, dropping the oldest lines beyond the cap.
fn push_rows(buffer: &mut Rc<Vec<LogRow>>, rows: impl IntoIterator<Item = LogRow>) {
    let list = Rc::make_mut(buffer);
    list.extend(rows);
    let excess = list.len().saturating_sub(MAX_LOG_LINES);
    list.drain(..excess);
}

fn filter_level_from_selection(selections: &[String]) -> Option<Option<LogLevel>> {
    let selection = selections.first()?;
    if selection.eq_ignore_ascii_case("ALL") {
        Some(None)
    } else {
        selection.parse().ok().map(Some)
    }
}

/// A line is shown when it is at least as severe as `min_level` and contains the lowercased `query` in its
/// timestamp, level or message.
fn row_matches(row: &LogRow, min_level: Option<LogLevel>, query: &str) -> bool {
    if min_level.is_some_and(|level| !row.level.matches(level)) {
        return false;
    }
    query.is_empty()
        || row.message.to_lowercase().contains(query)
        || row.timestamp.contains(query)
        || row.level.as_str().contains(query)
}

fn format_row(row: &LogRow) -> String {
    format!("{} [{:>5}] {}", row.timestamp, row.level.as_str().to_uppercase(), row.message)
}

/// Owns the live-tail socket and its reconnect timer; callbacks only hold weak references.
///
/// The socket asks the backend for `target=plugin::<id>` so only this plugin's records cross the wire. It ignores the
/// replayed history: the backlog comes from the plugin's own REST ring buffer.
struct PluginLogTail {
    target: String,
    enabled: Cell<bool>,
    epoch: Cell<u64>,
    attempt: Cell<u16>,
    socket: RefCell<Option<ActiveSocket>>,
    reconnect_timer: RefCell<Option<Timeout>>,
    connected: Callback<bool>,
    entry: Callback<LogEntry>,
    events: Rc<EventService>,
}

impl PluginLogTail {
    fn new(target: String, connected: Callback<bool>, entry: Callback<LogEntry>, events: Rc<EventService>) -> Rc<Self> {
        Rc::new(Self {
            target,
            enabled: Cell::new(false),
            epoch: Cell::new(0),
            attempt: Cell::new(0),
            socket: RefCell::new(None),
            reconnect_timer: RefCell::new(None),
            connected,
            entry,
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
        // Logout removes the token before the effect cleanup runs.
        let Some(token) = get_token() else {
            self.disconnect();
            self.connected.emit(false);
            return;
        };
        let epoch = self.epoch.get().wrapping_add(1);
        self.epoch.set(epoch);
        let path = concat_path_leading_slash(&get_base_href(), "ws/logs");
        let target = String::from(encode_uri_component(&self.target));
        let ws = match WebSocket::new(&format!("{path}?token={token}&target={target}")) {
            Ok(ws) => ws,
            Err(err) => {
                error!("Failed to create plugin log websocket: {err:?}");
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
        let tail = Rc::downgrade(self);
        move |event| {
            if let Some(tail) = tail.upgrade() {
                if tail.enabled.get() && tail.epoch.get() == epoch {
                    handle(&tail, event);
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
        trace!("Plugin log WebSocket connected");
        if let Some(token) = get_token() {
            self.send(&LogWsMessage::Auth(token));
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
        match message {
            LogWsMessage::Unauthorized => {
                warn!("Plugin log WebSocket unauthorized");
                self.disconnect();
                self.connected.emit(false);
                self.events.broadcast(EventMessage::Unauthorized);
            }
            LogWsMessage::Authorized | LogWsMessage::History(_) => {
                // Only successful authorization resets backoff, not a rejected upgrade.
                self.attempt.set(0);
                self.connected.emit(true);
            }
            LogWsMessage::Entry(entry) => {
                self.attempt.set(0);
                self.connected.emit(true);
                self.entry.emit(entry);
            }
            _ => {}
        }
    }

    fn on_close(self: &Rc<Self>, _: CloseEvent) {
        trace!("Plugin log WebSocket closed");
        let epoch = self.epoch.get();
        self.close_socket();
        self.connected.emit(false);
        if self.epoch.get() == epoch {
            self.schedule_reconnect();
        }
    }

    fn on_error(self: &Rc<Self>, event: ErrorEvent) {
        trace!("Plugin log WebSocket error: {event:?}");
        self.connected.emit(false);
    }

    fn schedule_reconnect(self: &Rc<Self>) {
        if !self.enabled.get() || self.reconnect_timer.borrow().is_some() {
            return;
        }
        let attempt = self.attempt.get();
        self.attempt.set(attempt.saturating_add(1));
        let epoch = self.epoch.get();
        let tail = Rc::downgrade(self);
        *self.reconnect_timer.borrow_mut() = Some(Timeout::new(reconnect_delay(attempt), move || {
            let Some(tail) = tail.upgrade() else {
                return;
            };
            let timer = tail.reconnect_timer.borrow_mut().take();
            drop(timer);
            if tail.enabled.get() && tail.epoch.get() == epoch {
                tail.connect();
            }
        }));
    }
}

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct PluginLogsProps {
    pub plugin_id: String,
}

/// The log viewer of one plugin: the REST backlog followed by a live tail of the records logged under the plugin's
/// target, with a level filter, a text search and an auto-scroll toggle. The page is expected to render it keyed by
/// plugin id.
#[component]
pub fn PluginLogs(props: &PluginLogsProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let copy_clipboard = use_clipboard_copy();

    let rows = use_state(|| Rc::new(Vec::<LogRow>::new()));
    // Websocket events can arrive before Yew renders the previous state update.
    let buffer = use_mut_ref(|| Rc::new(Vec::<LogRow>::new()));
    let connected = use_state(|| false);
    let load_error = use_state(|| None::<String>);
    let auto_scroll = use_state(|| true);
    // Collapsed by default: only an opened viewer holds a socket and a backlog.
    let open = use_state(|| false);
    let search_query = use_state(String::new);
    let selected_level = use_state(|| None::<LogLevel>);
    let console_ref = use_node_ref();

    // Backlog first, merged in front of whatever the live tail already delivered.
    {
        let rows = rows.setter();
        let buffer = Rc::clone(&buffer);
        let load_error = load_error.setter();
        let service = services.plugins.clone();
        use_effect_with((props.plugin_id.clone(), *open), move |(plugin_id, open)| {
            let empty = Rc::new(Vec::new());
            *buffer.borrow_mut() = Rc::clone(&empty);
            rows.set(empty);
            load_error.set(None);
            let cancelled = Rc::new(Cell::new(false));
            if !*open {
                return Box::new(|| ()) as Box<dyn FnOnce()>;
            }
            let plugin_id = plugin_id.clone();
            let task_cancelled = Rc::clone(&cancelled);
            let (rows, buffer, load_error) = (rows.clone(), Rc::clone(&buffer), load_error.clone());
            spawn_local(async move {
                let query = PluginLogQueryDto { level: None, since: None, limit: Some(BACKLOG_LIMIT) };
                let result = service.get_logs(&plugin_id, &query).await;
                // The viewer was closed, or switched to another plugin, while the request was in flight.
                if task_cancelled.get() {
                    return;
                }
                match result {
                    Ok(entries) => {
                        let updated = {
                            let mut buffer = buffer.borrow_mut();
                            let live = std::mem::take(Rc::make_mut(&mut buffer));
                            push_rows(&mut buffer, entries.iter().map(row_from_backlog).chain(live));
                            Rc::clone(&buffer)
                        };
                        rows.set(updated);
                        load_error.set(None);
                    }
                    Err(err) => load_error.set(Some(err.to_string())),
                }
            });
            Box::new(move || cancelled.set(true))
        });
    }

    // Live tail.
    {
        let rows = rows.setter();
        let buffer = Rc::clone(&buffer);
        let connected = connected.setter();
        let events = Rc::clone(&services.event);
        use_effect_with((props.plugin_id.clone(), *open), move |(plugin_id, open)| {
            if !*open {
                return Box::new(|| ()) as Box<dyn FnOnce()>;
            }
            let tail = PluginLogTail::new(
                plugin_target(plugin_id),
                {
                    let connected = connected.clone();
                    Callback::from(move |value| connected.set(value))
                },
                Callback::from(move |entry: LogEntry| {
                    let updated = {
                        let mut buffer = buffer.borrow_mut();
                        push_rows(&mut buffer, std::iter::once(row_from_live(entry)));
                        Rc::clone(&buffer)
                    };
                    rows.set(updated);
                }),
                events,
            );
            tail.enabled.set(true);
            tail.connect();
            Box::new(move || {
                tail.disconnect();
                connected.set(false);
            })
        });
    }

    // Keep the view on the newest line while auto-scroll is on.
    {
        let console_ref = console_ref.clone();
        use_effect_with((rows.len(), *auto_scroll), move |(_, auto_scroll)| {
            if *auto_scroll {
                if let Some(el) = console_ref.cast::<HtmlElement>() {
                    el.set_scroll_top(el.scroll_height());
                }
            }
            || ()
        });
    }

    // Scrolling away from the end pauses auto-scroll; scrolling back to the end resumes it.
    let on_scroll = {
        let console_ref = console_ref.clone();
        let auto_scroll = auto_scroll.clone();
        Callback::from(move |_| {
            if let Some(el) = console_ref.cast::<HtmlElement>() {
                let at_bottom = el.scroll_top() + el.client_height() >= el.scroll_height() - SCROLL_BOTTOM_THRESHOLD;
                if at_bottom != *auto_scroll {
                    auto_scroll.set(at_bottom);
                }
            }
        })
    };

    let on_toggle_auto_scroll = {
        let auto_scroll = auto_scroll.clone();
        let console_ref = console_ref.clone();
        Callback::from(move |_| {
            let next = !*auto_scroll;
            auto_scroll.set(next);
            if next {
                if let Some(el) = console_ref.cast::<HtmlElement>() {
                    el.set_scroll_top(el.scroll_height());
                }
            }
        })
    };

    let on_toggle_open = {
        let open = open.clone();
        Callback::from(move |_| open.set(!*open))
    };

    let on_select_level = {
        let selected_level = selected_level.clone();
        Callback::from(move |selections: Rc<Vec<String>>| {
            if let Some(level) = filter_level_from_selection(&selections) {
                selected_level.set(level);
            }
        })
    };

    let on_search_input = {
        let search_query = search_query.clone();
        Callback::from(move |value: String| search_query.set(value))
    };

    let on_clear = {
        let rows = rows.setter();
        let buffer = Rc::clone(&buffer);
        Callback::from(move |_| {
            let empty = Rc::new(Vec::new());
            *buffer.borrow_mut() = Rc::clone(&empty);
            rows.set(empty);
        })
    };

    let query = search_query.to_lowercase();
    let filtered: Vec<&LogRow> = rows.iter().filter(|row| row_matches(row, *selected_level, &query)).collect();

    let on_copy = {
        let copy_clipboard = copy_clipboard.clone();
        let text = filtered.iter().map(|row| format_row(row)).collect::<Vec<_>>().join("\n");
        Callback::from(move |_| copy_clipboard.emit(text.clone()))
    };

    let level_options =
        Rc::new(["ALL", "ERROR", "WARN", "INFO", "DEBUG", "TRACE"].into_iter().map(String::from).collect::<Vec<_>>());
    let level_selection = Rc::new(vec![(*selected_level).as_ref().map_or("ALL", LogLevel::as_str).to_uppercase()]);

    html! {
        <div class="tp__plugins__logs">
            <TextButton
                name={format!("toggle_log-{}", props.plugin_id)}
                icon={if *open { "ChevronDown" } else { "ArrowRight" }}
                title={translate.t("LABEL.PLUGIN_LOGS")}
                onclick={on_toggle_open}
            />
            if *open {
            if let Some(message) = load_error.as_ref() {
                <div class="tp__plugins__error tp__input-error" role="alert">
                    { format!("{} {message}", translate.t("MESSAGES.PLUGIN_LOGS_LOAD_FAILED")) }
                </div>
            }
            <div class="tp__log-console" style="height: 24rem;">
                <div class="tp__log-console__toolbar">
                    <div class="tp__log-console__filters">
                        <RadioButtonGroup
                            multi_select={false}
                            none_allowed={false}
                            options={level_options}
                            selected={level_selection}
                            on_select={on_select_level}
                        />
                    </div>
                    <div class="tp__log-console__controls">
                        <div class="tp__log-console__search">
                            <Input
                                placeholder={translate.t("LABEL.SEARCH_LOGS")}
                                value={(*search_query).clone()}
                                on_change={on_search_input}
                            />
                        </div>
                        <div class="tp__log-console__actions">
                            <TextButton
                                name={format!("autoscroll_log-{}", props.plugin_id)}
                                icon={if *auto_scroll { "Pause" } else { "Play" }}
                                class={if *auto_scroll { "tp__log-console__action-btn tp__log-console__action-btn--active" } else { "tp__log-console__action-btn" }}
                                title={translate.t("LABEL.AUTO_SCROLL")}
                                hint={if *auto_scroll { translate.t("LABEL.PAUSE_SCROLL") } else { translate.t("LABEL.RESUME_SCROLL") }}
                                onclick={on_toggle_auto_scroll}
                            />
                            <TextButton
                                name={format!("copy_log-{}", props.plugin_id)}
                                icon="Copy"
                                class="tp__log-console__action-btn"
                                title={translate.t("LABEL.COPY_LOGS")}
                                hint={translate.t("LABEL.COPY_LOGS")}
                                onclick={on_copy}
                            />
                            <TextButton
                                name={format!("delete_log-{}", props.plugin_id)}
                                icon="Delete"
                                class="tp__log-console__action-btn"
                                title={translate.t("LABEL.CLEAR_LOGS")}
                                hint={translate.t("LABEL.CLEAR_LOGS")}
                                onclick={on_clear}
                            />
                        </div>
                        <div class="tp__log-console__status">
                            <span class={classes!(
                                "tp__log-console__status-dot",
                                if *connected { "tp__log-console__status-dot--connected" } else { "tp__log-console__status-dot--disconnected" }
                            )} />
                        </div>
                    </div>
                </div>
                <div class="tp__log-console__output" ref={console_ref} onscroll={on_scroll}>
                    if filtered.is_empty() {
                        <div class="tp__log-console__empty">{ translate.t("LABEL.NO_LOGS") }</div>
                    } else {
                        { for filtered.iter().enumerate().map(|(idx, row)| {
                            let level_str = row.level.as_str();
                            let msg_cls = match row.level {
                                LogLevel::Error => "tp__log-console__message--error",
                                LogLevel::Warn => "tp__log-console__message--warn",
                                _ => "",
                            };
                            html! {
                                <div class="tp__log-console__row" key={format!("{idx}-{}-{}", row.timestamp, row.message)}>
                                    <span class="tp__log-console__timestamp">{ &row.timestamp }</span>
                                    <span class={classes!("tp__log-console__level", format!("tp__log-console__level--{level_str}"))}>
                                        { level_str }
                                    </span>
                                    <span class={classes!("tp__log-console__message", msg_cls)}>{ &row.message }</span>
                                </div>
                            }
                        }) }
                    }
                </div>
            </div>
            }
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(level: LogLevel, message: &str) -> LogRow {
        LogRow { timestamp: "2026-10-30 12:00:00".to_string(), level, message: message.to_string() }
    }

    #[test]
    fn target_is_the_plugin_namespace() {
        assert_eq!(plugin_target("demo"), "plugin::demo");
    }

    #[test]
    fn level_filter_is_a_minimum() {
        let warn = row(LogLevel::Warn, "slow");
        assert!(row_matches(&warn, None, ""));
        assert!(row_matches(&warn, Some(LogLevel::Info), ""));
        assert!(row_matches(&warn, Some(LogLevel::Warn), ""));
        assert!(!row_matches(&warn, Some(LogLevel::Error), ""));
    }

    #[test]
    fn search_covers_message_level_and_timestamp() {
        let info = row(LogLevel::Info, "Fetched Guide");
        assert!(row_matches(&info, None, "guide"));
        assert!(row_matches(&info, None, "info"));
        assert!(row_matches(&info, None, "2026-10-30"));
        assert!(!row_matches(&info, None, "missing"));
    }

    #[test]
    fn rows_are_capped_oldest_first() {
        let mut buffer = Rc::new(Vec::new());
        push_rows(&mut buffer, (0..MAX_LOG_LINES + 5).map(|i| row(LogLevel::Info, &i.to_string())));
        assert_eq!(buffer.len(), MAX_LOG_LINES);
        assert_eq!(buffer[0].message, "5");
    }

    #[test]
    fn level_selection_maps_all_and_levels() {
        assert_eq!(filter_level_from_selection(&["ALL".to_string()]), Some(None));
        assert_eq!(filter_level_from_selection(&["WARN".to_string()]), Some(Some(LogLevel::Warn)));
        assert_eq!(filter_level_from_selection(&["bogus".to_string()]), None);
        assert_eq!(filter_level_from_selection(&[]), None);
    }
}
