//! Sandboxed plugin frame and the parent-side `postMessage` bridge (plan §11
//! *Sandboxed-frame bridge*, frozen in §18 *W6b seam freeze*). W6b lane U.
//!
//! Security notes, since this is the edge between plugin JavaScript and the
//! admin's session:
//!
//! - The frame's `sandbox` attribute is `allow-scripts` and nothing else. It never gets the
//!   same-origin token, so it has an opaque origin and cannot read the app's
//!   token from `localStorage`.
//! - The parent performs every rpc. The token stays in [`PluginUiService`] and
//!   no message built here has a field that could carry it.
//! - A message counts only when `event.source` is the `contentWindow` of the
//!   frame this component renders. `event.origin` is ignored (it is `"null"`).
//! - The set of messages the frame may send is closed, each shape is checked,
//!   and everything else is dropped without an answer.

use super::{plugin_i18n::PluginStrings, plugin_slot::use_plugin_ui};
use crate::{
    i18n::{use_active_locale, use_translation},
    services::{plugin_slot_id, PluginUiService, ToastType, ToastrService, TokenBucket},
};
use serde_json::{json, Map, Value};
use shared::model::{PluginSlotDto, PluginUiDto};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashSet},
    rc::Rc,
};
use wasm_bindgen::{closure::Closure, JsCast, JsValue};
use web_sys::{
    window, HtmlElement, HtmlIFrameElement, MessageEvent, MutationObserver, MutationObserverInit, ResizeObserver,
};
use yew::{
    component, html, platform::spawn_local, use_effect_with, use_mut_ref, use_node_ref, use_state, Html, Properties,
};

/// The protocol version in every message.
const PROTOCOL: u64 = 1;
/// A serialized rpc payload may not be larger.
pub const MAX_RPC_PAYLOAD_BYTES: usize = 64 * 1024;
/// A message from a frame is not even parsed above this size.
const MAX_MESSAGE_BYTES: usize = 128 * 1024;
/// rpc calls a frame may have outstanding.
pub const MAX_RPC_IN_FLIGHT: usize = 8;
/// Client-side rpc limit of a frame: 5 per second, burst 10.
const RPC_BURST: u32 = 10;
const RPC_PER_SECOND: f64 = 5.0;
/// A frame may ask for a height in this range only (CSS px).
pub const MIN_FRAME_HEIGHT: u32 = 80;
pub const MAX_FRAME_HEIGHT: u32 = 2000;
const INITIAL_FRAME_HEIGHT: u32 = 200;
/// Longest error text handed to a frame.
const MAX_ERROR_CHARS: usize = 500;

/// The iframe `sandbox` attribute: scripts only. Without `allow-same-origin` the
/// document gets an opaque origin, which keeps it away from the admin token.
const FRAME_SANDBOX: &str = "allow-scripts";

/// The CSS custom properties a frame receives as its theme. A fixed list: the
/// frame sees the semantic tokens it needs to match the app, not the whole
/// computed style.
pub const THEME_TOKENS: [&str; 22] = [
    "--theme-brand-page",
    "--theme-brand-surface",
    "--theme-brand-surface-soft",
    "--theme-brand-surface-input",
    "--theme-brand-text",
    "--theme-brand-text-muted",
    "--theme-brand-text-soft",
    "--theme-brand-text-inverse",
    "--theme-brand-border",
    "--theme-brand-border-soft",
    "--theme-brand-border-focus",
    "--theme-brand-primary",
    "--theme-brand-primary-soft",
    "--theme-brand-primary-hover",
    "--theme-brand-positive",
    "--theme-brand-warning",
    "--theme-brand-danger",
    "--theme-brand-info",
    "--theme-brand-accent",
    "--font-size-base",
    "--font-size-sm",
    "--border-radius",
];

// ---------------------------------------------------------------------------
// Messages (pure)
// ---------------------------------------------------------------------------

/// What a frame may ask for. Anything else it posts is dropped.
#[derive(Debug, Clone, PartialEq)]
pub enum FrameMessage {
    Ready,
    Rpc { id: i64, method: String, payload: Value },
    Toast { severity: ToastType, message: String },
    Resize { height: f64 },
}

fn parse_severity(severity: &str) -> Option<ToastType> {
    match severity {
        "info" => Some(ToastType::Info),
        "success" => Some(ToastType::Success),
        "warning" => Some(ToastType::Warning),
        "error" => Some(ToastType::Error),
        _ => None,
    }
}

/// The shape check: `{ tlpx: 1, type, ... }` with the fields of the type.
/// `None` for anything that is not exactly one of the four messages.
pub fn parse_frame_message(value: &Value) -> Option<FrameMessage> {
    let object = value.as_object()?;
    if object.get("tlpx")?.as_u64()? != PROTOCOL {
        return None;
    }
    match object.get("type")?.as_str()? {
        "ready" => Some(FrameMessage::Ready),
        "rpc" => Some(FrameMessage::Rpc {
            id: object.get("id")?.as_i64()?,
            method: object.get("method")?.as_str()?.to_owned(),
            payload: object.get("payload").cloned().unwrap_or(Value::Null),
        }),
        "toast" => Some(FrameMessage::Toast {
            severity: parse_severity(object.get("severity")?.as_str()?)?,
            message: object.get("message")?.as_str()?.to_owned(),
        }),
        "resize" => {
            let height = object.get("height")?.as_f64()?;
            height.is_finite().then_some(FrameMessage::Resize { height })
        }
        _ => None,
    }
}

/// A requested frame height clamped to 80–2000 px.
pub fn clamp_frame_height(height: f64) -> u32 {
    height.clamp(f64::from(MIN_FRAME_HEIGHT), f64::from(MAX_FRAME_HEIGHT)).round() as u32
}

fn envelope(kind: &str, fields: Value) -> Value {
    let mut message = Map::new();
    message.insert("tlpx".to_owned(), json!(PROTOCOL));
    message.insert("type".to_owned(), json!(kind));
    if let Value::Object(fields) = fields {
        message.extend(fields);
    }
    Value::Object(message)
}

/// Everything a frame is told when it asks `ready`.
pub struct InitMessage<'a> {
    pub plugin_id: &'a str,
    pub slot: PluginSlotDto,
    pub locale: &'a str,
    pub dir: &'a str,
    pub strings: &'a BTreeMap<String, String>,
    pub theme: &'a BTreeMap<String, String>,
    pub context: &'a Value,
}

/// The answer to every `ready`.
pub fn init_message(init: &InitMessage<'_>) -> Value {
    envelope(
        "init",
        json!({
            "pluginId": init.plugin_id,
            "slot": plugin_slot_id(init.slot),
            "locale": init.locale,
            "dir": init.dir,
            "strings": init.strings,
            "theme": init.theme,
            "context": init.context,
        }),
    )
}

pub fn locale_message(locale: &str, dir: &str, strings: &BTreeMap<String, String>) -> Value {
    envelope("locale", json!({ "locale": locale, "dir": dir, "strings": strings }))
}

pub fn theme_message(theme: &BTreeMap<String, String>) -> Value { envelope("theme", json!({ "theme": theme })) }

pub fn context_message(context: &Value) -> Value { envelope("context", json!({ "context": context })) }

pub fn rpc_ok_message(id: i64, result: &Value) -> Value {
    envelope("rpc-result", json!({ "id": id, "ok": true, "result": result }))
}

pub fn rpc_error_message(id: i64, status: u16, error: &str) -> Value {
    let error: String = error.chars().take(MAX_ERROR_CHARS).collect();
    envelope("rpc-result", json!({ "id": id, "ok": false, "status": status, "error": error }))
}

// ---------------------------------------------------------------------------
// Per-frame state (pure)
// ---------------------------------------------------------------------------

/// What the parent decides about an `rpc` request before any call is made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcAdmission {
    /// Make the call; the id is now in flight.
    Run,
    /// Answer with this error and make no call.
    Reject { status: u16, error: &'static str },
    /// A request with this id is already in flight; drop the duplicate.
    Ignore,
}

/// The limits and bookkeeping of one mounted frame.
pub struct FrameRuntime {
    /// The frame has asked `ready` at least once.
    pub ready: bool,
    /// The frame is still mounted; answers of late rpc calls are dropped when it is not.
    pub alive: bool,
    /// Bumped when the component starts showing another plugin or document, so
    /// a late rpc answer for the old one is never posted into the new frame.
    pub generation: u64,
    in_flight: HashSet<i64>,
    bucket: TokenBucket,
}

impl FrameRuntime {
    pub fn new(now_ms: f64) -> Self {
        Self {
            ready: false,
            alive: true,
            generation: 0,
            in_flight: HashSet::new(),
            bucket: TokenBucket::new(RPC_BURST, RPC_PER_SECOND, now_ms),
        }
    }

    /// The rpc rules of the frozen bridge: 5 requests per second with a burst of
    /// 10 (every request counts, so a frame cannot flood the parent with bad ones),
    /// the method must be exported, the payload at most 64 KiB, and at most 8 calls
    /// in flight.
    pub fn admit_rpc(
        &mut self,
        exports: &[&str],
        id: i64,
        method: &str,
        payload_bytes: usize,
        now_ms: f64,
    ) -> RpcAdmission {
        if self.in_flight.contains(&id) {
            return RpcAdmission::Ignore;
        }
        if !self.bucket.try_take(now_ms) {
            return RpcAdmission::Reject { status: 429, error: "rate limit exceeded" };
        }
        if !exports.contains(&method) {
            return RpcAdmission::Reject { status: 404, error: "unknown rpc method" };
        }
        if payload_bytes > MAX_RPC_PAYLOAD_BYTES {
            return RpcAdmission::Reject { status: 413, error: "payload too large" };
        }
        if self.in_flight.len() >= MAX_RPC_IN_FLIGHT {
            return RpcAdmission::Reject { status: 429, error: "too many calls in flight" };
        }
        self.in_flight.insert(id);
        RpcAdmission::Run
    }

    pub fn finish_rpc(&mut self, id: i64) { self.in_flight.remove(&id); }

    /// Start over for another plugin or document: a fresh bucket, nothing in
    /// flight, not ready, and a new generation.
    pub fn reset(&mut self, now_ms: f64) {
        let generation = self.generation + 1;
        *self = Self::new(now_ms);
        self.generation = generation;
    }

    #[cfg(test)]
    pub fn in_flight(&self) -> usize { self.in_flight.len() }
}

// ---------------------------------------------------------------------------
// Browser glue
// ---------------------------------------------------------------------------

/// The values of [`THEME_TOKENS`] on `<body>`, where the theme attribute lives.
pub fn read_theme() -> BTreeMap<String, String> {
    let mut theme = BTreeMap::new();
    let Some(win) = window() else { return theme };
    let Some(body) = win.document().and_then(|document| document.body()) else { return theme };
    let Ok(Some(style)) = win.get_computed_style(&body) else { return theme };
    for name in THEME_TOKENS {
        if let Ok(value) = style.get_property_value(name) {
            let value = value.trim();
            if !value.is_empty() {
                theme.insert(name.to_owned(), value.to_owned());
            }
        }
    }
    theme
}

/// What the live frame callbacks read. Refreshed on every render, so a message
/// handler registered once always sees the current plugin, locale and context.
struct FrameInputs {
    plugin: Rc<PluginUiDto>,
    slot: PluginSlotDto,
    fill: bool,
    locale: String,
    dir: &'static str,
    strings: PluginStrings,
    theme: BTreeMap<String, String>,
    context: Value,
}

#[derive(Clone)]
struct FrameCtx {
    iframe: yew::NodeRef,
    inputs: Rc<RefCell<FrameInputs>>,
    runtime: Rc<RefCell<FrameRuntime>>,
    service: Rc<PluginUiService>,
    toastr: Rc<ToastrService>,
    set_height: yew::Callback<u32>,
}

impl FrameCtx {
    fn content_window(&self) -> Option<web_sys::Window> {
        self.iframe.cast::<HtmlIFrameElement>().and_then(|frame| frame.content_window())
    }

    /// Posts to the frame this component created and to nobody else. The
    /// target origin is `*` because the frame's origin is opaque.
    fn post(&self, message: &Value) {
        let Some(target) = self.content_window() else { return };
        let Ok(text) = serde_json::to_string(message) else { return };
        let Ok(data) = js_sys::JSON::parse(&text) else { return };
        let _ = target.post_message(&data, "*");
    }

    fn post_init(&self) {
        let inputs = self.inputs.borrow();
        self.post(&init_message(&InitMessage {
            plugin_id: &inputs.plugin.id,
            slot: inputs.slot,
            locale: &inputs.locale,
            dir: inputs.dir,
            strings: &inputs.strings,
            theme: &inputs.theme,
            context: &inputs.context,
        }));
    }
}

/// The data of a message event as JSON, if it is a plain value of sane size.
fn message_data(event: &MessageEvent) -> Option<Value> {
    let text = js_sys::JSON::stringify(&event.data()).ok()?.as_string()?;
    if text.len() > MAX_MESSAGE_BYTES {
        return None;
    }
    serde_json::from_str(&text).ok()
}

/// The handler of the window `message` listener: source check, shape check,
/// then dispatch over the closed set of frame messages.
fn on_frame_message(ctx: &FrameCtx, event: &MessageEvent) {
    // 1. Source: only the window of the frame this component renders.
    let (Some(frame_window), Some(source)) = (ctx.content_window(), event.source()) else { return };
    if !js_sys::Object::is(frame_window.as_ref(), source.as_ref()) {
        return;
    }
    // 2. Shape: `{ tlpx: 1, type, ... }` of one of the four types.
    let Some(message) = message_data(event).as_ref().and_then(parse_frame_message) else { return };
    // 3. Dispatch.
    match message {
        FrameMessage::Ready => {
            ctx.runtime.borrow_mut().ready = true;
            ctx.post_init();
        }
        FrameMessage::Rpc { id, method, payload } => {
            let payload_bytes = serde_json::to_string(&payload).map_or(usize::MAX, |text| text.len());
            let (plugin, admission) = {
                let plugin = Rc::clone(&ctx.inputs.borrow().plugin);
                let exports: Vec<&str> = plugin.rpc_exports.iter().map(|export| export.name.as_str()).collect();
                let admission =
                    ctx.runtime.borrow_mut().admit_rpc(&exports, id, &method, payload_bytes, js_sys::Date::now());
                (plugin, admission)
            };
            match admission {
                RpcAdmission::Ignore => {}
                RpcAdmission::Reject { status, error } => ctx.post(&rpc_error_message(id, status, error)),
                RpcAdmission::Run => {
                    let ctx = ctx.clone();
                    let generation = ctx.runtime.borrow().generation;
                    spawn_local(async move {
                        let result = ctx.service.rpc(&plugin.id, &method, &payload).await;
                        let current = {
                            let mut runtime = ctx.runtime.borrow_mut();
                            if runtime.generation == generation {
                                runtime.finish_rpc(id);
                                runtime.alive
                            } else {
                                false
                            }
                        };
                        if current {
                            ctx.post(&match result {
                                Ok(value) => rpc_ok_message(id, &value),
                                Err(failure) => rpc_error_message(id, failure.status, &failure.error),
                            });
                        }
                    });
                }
            }
        }
        FrameMessage::Toast { severity, message } => {
            let plugin = Rc::clone(&ctx.inputs.borrow().plugin);
            ctx.toastr.plugin_toast(&plugin.id, &plugin.name, severity, &message);
        }
        FrameMessage::Resize { height } => {
            // The menu page fills the content area and ignores the frame's own height.
            if !ctx.inputs.borrow().fill {
                ctx.set_height.emit(clamp_frame_height(height));
            }
        }
    }
}

#[derive(Properties, Clone, PartialEq)]
pub struct PluginFrameProps {
    pub plugin: Rc<PluginUiDto>,
    /// The package-relative frame document of the contribution.
    pub element: String,
    pub slot: PluginSlotDto,
    /// The slot subject (channel, programme, ...); `None` is sent as `null`.
    #[prop_or_default]
    pub context: Option<Value>,
    /// Fill the content area instead of following the frame's `resize` messages.
    #[prop_or_default]
    pub fill: bool,
}

/// One sandboxed plugin frame with its bridge. Mounting registers the window
/// `message` listener and unmounting removes it together with the iframe.
#[component]
pub fn PluginFrame(props: &PluginFrameProps) -> Html {
    let translate = use_translation();
    let ui = use_plugin_ui();
    let locale = use_active_locale();
    let iframe = use_node_ref();
    let height = use_state(|| INITIAL_FRAME_HEIGHT);
    let theme = use_state(read_theme);
    let strings = ui.as_ref().map_or_else(PluginStrings::default, |ui| ui.strings_for(&props.plugin.id));
    let service = ui.as_ref().map_or_else(|| Rc::new(PluginUiService::new()), |ui| Rc::clone(&ui.service));

    let inputs = use_mut_ref({
        let plugin = Rc::clone(&props.plugin);
        let strings = Rc::clone(&strings);
        let (slot, fill, locale_code, dir) = (props.slot, props.fill, locale.code.clone(), locale.dir);
        let (theme, context) = ((*theme).clone(), props.context.clone().unwrap_or(Value::Null));
        move || FrameInputs { plugin, slot, fill, locale: locale_code, dir, strings, theme, context }
    });
    {
        let mut current = inputs.borrow_mut();
        current.plugin = Rc::clone(&props.plugin);
        current.slot = props.slot;
        current.fill = props.fill;
        current.locale.clone_from(&locale.code);
        current.dir = locale.dir;
        current.strings = Rc::clone(&strings);
        current.theme = (*theme).clone();
        current.context = props.context.clone().unwrap_or(Value::Null);
    }
    let runtime = use_mut_ref(|| FrameRuntime::new(js_sys::Date::now()));
    // One plugin document per frame element. Yew may reuse this component for
    // another plugin (a dialog or tab switching its subject): the keyed
    // `<iframe>` below is then recreated, so the old document's window fails the
    // source check, and the runtime starts a new generation.
    let identity = format!("{}\u{0}{}", props.plugin.id, props.element);
    {
        let shown = use_mut_ref(|| identity.clone());
        if *shown.borrow() != identity {
            runtime.borrow_mut().reset(js_sys::Date::now());
            shown.borrow_mut().clone_from(&identity);
        }
    }

    // Inactive slots unmount their frame (plan §11). Views stay mounted and are
    // hidden with CSS, so the frame exists only while its host box is shown: a
    // hidden ancestor gives the host a zero width. Hiding also starts a new
    // generation, so answers of calls the old document made are dropped.
    let host = use_node_ref();
    let shown = use_state(|| false);
    // The observer outlives renders, so it compares against this, not `shown`.
    let last_shown = use_mut_ref(|| false);
    {
        let (host, shown, runtime, last_shown) = (host.clone(), shown.clone(), Rc::clone(&runtime), last_shown);
        use_effect_with((), move |()| {
            let update = {
                let host = host.clone();
                move || {
                    let visible = host.cast::<HtmlElement>().is_some_and(|element| element.client_width() > 0);
                    if visible != *last_shown.borrow() {
                        *last_shown.borrow_mut() = visible;
                        if !visible {
                            runtime.borrow_mut().reset(js_sys::Date::now());
                        }
                        shown.set(visible);
                    }
                }
            };
            let callback = Closure::<dyn FnMut(js_sys::Array, ResizeObserver)>::new(move |_, _| update());
            let observer = ResizeObserver::new(callback.as_ref().unchecked_ref()).ok();
            if let (Some(observer), Some(element)) = (observer.as_ref(), host.cast::<web_sys::Element>()) {
                observer.observe(&element);
            }
            move || {
                if let Some(observer) = observer {
                    observer.disconnect();
                }
                drop(callback);
            }
        });
    }

    let services = crate::hooks::use_service_context();
    let ctx = FrameCtx {
        iframe: iframe.clone(),
        inputs: Rc::clone(&inputs),
        runtime: Rc::clone(&runtime),
        service,
        toastr: Rc::clone(&services.toastr),
        set_height: {
            let height = height.clone();
            yew::Callback::from(move |next: u32| height.set(next))
        },
    };

    // The `message` listener lives exactly as long as the component.
    {
        let ctx = ctx.clone();
        use_effect_with((), move |()| {
            let handler_ctx = ctx.clone();
            let handler = Closure::<dyn FnMut(MessageEvent)>::wrap(Box::new(move |event: MessageEvent| {
                on_frame_message(&handler_ctx, &event);
            }));
            let win = window();
            if let Some(win) = win.as_ref() {
                if win.add_event_listener_with_callback("message", handler.as_ref().unchecked_ref()).is_err() {
                    log::error!("failed to register the plugin bridge listener");
                }
            }
            move || {
                ctx.runtime.borrow_mut().alive = false;
                if let Some(win) = win {
                    let _ = win.remove_event_listener_with_callback("message", handler.as_ref().unchecked_ref());
                }
            }
        });
    }

    // A theme switch changes `data-theme` on <body>: read the tokens again.
    {
        let theme = theme.clone();
        use_effect_with((), move |()| {
            let callback =
                Closure::<dyn FnMut(js_sys::Array, MutationObserver)>::wrap(Box::new(move |_records, _observer| {
                    let next = read_theme();
                    if next != *theme {
                        theme.set(next);
                    }
                }));
            let observer = MutationObserver::new(callback.as_ref().unchecked_ref()).ok();
            if let (Some(observer), Some(body)) =
                (observer.as_ref(), window().and_then(|win| win.document()).and_then(|document| document.body()))
            {
                let options = MutationObserverInit::new();
                options.set_attributes(true);
                let filter = js_sys::Array::new();
                filter.push(&JsValue::from_str("data-theme"));
                options.set_attribute_filter(&filter);
                let _ = observer.observe_with_options(&body, &options);
            }
            move || {
                if let Some(observer) = observer {
                    observer.disconnect();
                }
                drop(callback);
            }
        });
    }

    // Changes after the first `ready` are pushed; before it, `init` carries them.
    {
        let ctx = ctx.clone();
        use_effect_with((locale.code.clone(), Rc::as_ptr(&strings) as usize), move |_| {
            if ctx.runtime.borrow().ready {
                let inputs = ctx.inputs.borrow();
                ctx.post(&locale_message(&inputs.locale, inputs.dir, &inputs.strings));
            }
        });
    }
    {
        let ctx = ctx.clone();
        use_effect_with((*theme).clone(), move |theme| {
            if ctx.runtime.borrow().ready {
                ctx.post(&theme_message(theme));
            }
        });
    }
    {
        let ctx = ctx.clone();
        use_effect_with(props.context.clone(), move |context| {
            if ctx.runtime.borrow().ready {
                ctx.post(&context_message(context.as_ref().unwrap_or(&Value::Null)));
            }
        });
    }

    let Some(src) = service_frame_src(&ctx.service, &props.plugin, &props.element, props.slot) else {
        return html! {
            <div class="tp__plugin-frame__error tp__input-error" role="alert">
                { translate.t("MESSAGES.PLUGIN_FRAME_INVALID") }
            </div>
        };
    };
    let style = if props.fill {
        "display:block;width:100%;height:100%;min-height:70vh;border:0".to_owned()
    } else {
        format!("display:block;width:100%;height:{}px;border:0", *height)
    };

    let host_style = if props.fill { "display:block;width:100%;height:100%" } else { "display:block;width:100%" };
    html! {
        <div ref={host} class="tp__plugin-frame__host" style={host_style}>
            if *shown {
                <iframe
                    key={identity}
                    ref={iframe}
                    class="tp__plugin-frame"
                    src={src}
                    sandbox={FRAME_SANDBOX}
                    referrerpolicy="no-referrer"
                    title={props.plugin.name.clone()}
                    style={style}
                />
            }
        </div>
    }
}

fn service_frame_src(
    service: &PluginUiService,
    plugin: &PluginUiDto,
    element: &str,
    slot: PluginSlotDto,
) -> Option<String> {
    service.frame_src(&plugin.id, &plugin.version, element, slot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeSet;

    fn parse(value: Value) -> Option<FrameMessage> { parse_frame_message(&value) }

    #[test]
    fn the_four_frame_messages_parse() {
        assert_eq!(parse(json!({"tlpx": 1, "type": "ready"})), Some(FrameMessage::Ready));
        assert_eq!(
            parse(json!({"tlpx": 1, "type": "rpc", "id": 7, "method": "list", "payload": {"a": 1}})),
            Some(FrameMessage::Rpc { id: 7, method: "list".to_string(), payload: json!({"a": 1}) })
        );
        assert_eq!(
            parse(json!({"tlpx": 1, "type": "rpc", "id": 8, "method": "ping"})),
            Some(FrameMessage::Rpc { id: 8, method: "ping".to_string(), payload: Value::Null })
        );
        assert_eq!(
            parse(json!({"tlpx": 1, "type": "toast", "severity": "warning", "message": "hi"})),
            Some(FrameMessage::Toast { severity: ToastType::Warning, message: "hi".to_string() })
        );
        assert_eq!(
            parse(json!({"tlpx": 1, "type": "resize", "height": 320.5})),
            Some(FrameMessage::Resize { height: 320.5 })
        );
    }

    #[test]
    fn anything_else_is_dropped() {
        for value in [
            json!(null),
            json!("ready"),
            json!([1, "ready"]),
            json!({"type": "ready"}),
            json!({"tlpx": 2, "type": "ready"}),
            json!({"tlpx": "1", "type": "ready"}),
            json!({"tlpx": 1}),
            json!({"tlpx": 1, "type": 5}),
            json!({"tlpx": 1, "type": "init"}),
            json!({"tlpx": 1, "type": "rpc-result", "id": 1, "ok": true}),
            json!({"tlpx": 1, "type": "navigate", "url": "https://example.test"}),
            json!({"tlpx": 1, "type": "rpc", "id": "1", "method": "x"}),
            json!({"tlpx": 1, "type": "rpc", "id": 1.5, "method": "x"}),
            json!({"tlpx": 1, "type": "rpc", "id": 1}),
            json!({"tlpx": 1, "type": "rpc", "id": 1, "method": 3}),
            json!({"tlpx": 1, "type": "toast", "severity": "fatal", "message": "x"}),
            json!({"tlpx": 1, "type": "toast", "severity": "info"}),
            json!({"tlpx": 1, "type": "resize", "height": "tall"}),
            json!({"tlpx": 1, "type": "resize"}),
        ] {
            assert_eq!(parse(value.clone()), None, "{value}");
        }
    }

    #[test]
    fn heights_are_clamped_to_the_frozen_range() {
        assert_eq!(clamp_frame_height(0.0), 80);
        assert_eq!(clamp_frame_height(-50.0), 80);
        assert_eq!(clamp_frame_height(80.0), 80);
        assert_eq!(clamp_frame_height(300.4), 300);
        assert_eq!(clamp_frame_height(2000.0), 2000);
        assert_eq!(clamp_frame_height(1.0e9), 2000);
    }

    #[test]
    fn rpc_requires_an_exported_method_and_a_small_payload() {
        let mut runtime = FrameRuntime::new(0.0);
        let exports = ["list", "save"];
        assert_eq!(
            runtime.admit_rpc(&exports, 1, "delete", 10, 0.0),
            RpcAdmission::Reject { status: 404, error: "unknown rpc method" }
        );
        assert_eq!(
            runtime.admit_rpc(&exports, 2, "list", MAX_RPC_PAYLOAD_BYTES + 1, 0.0),
            RpcAdmission::Reject { status: 413, error: "payload too large" }
        );
        assert_eq!(runtime.in_flight(), 0, "a refused call is never in flight");
        assert_eq!(runtime.admit_rpc(&exports, 3, "list", MAX_RPC_PAYLOAD_BYTES, 0.0), RpcAdmission::Run);
        assert_eq!(runtime.in_flight(), 1);
    }

    #[test]
    fn rpc_has_at_most_eight_calls_in_flight() {
        let mut runtime = FrameRuntime::new(0.0);
        for id in 0..8 {
            // Spread over time so the rate limit does not interfere.
            assert_eq!(runtime.admit_rpc(&["m"], id, "m", 1, id as f64 * 1_000.0), RpcAdmission::Run);
        }
        assert_eq!(
            runtime.admit_rpc(&["m"], 8, "m", 1, 9_000.0),
            RpcAdmission::Reject { status: 429, error: "too many calls in flight" }
        );
        runtime.finish_rpc(0);
        assert_eq!(runtime.admit_rpc(&["m"], 9, "m", 1, 10_000.0), RpcAdmission::Run);
    }

    #[test]
    fn reset_starts_a_new_generation_with_nothing_in_flight() {
        let mut runtime = FrameRuntime::new(0.0);
        runtime.ready = true;
        assert_eq!(runtime.admit_rpc(&["m"], 1, "m", 1, 0.0), RpcAdmission::Run);
        runtime.reset(1_000.0);
        assert_eq!(runtime.generation, 1);
        assert!(!runtime.ready && runtime.alive);
        assert_eq!(runtime.in_flight(), 0, "the old document's calls are forgotten");
        // the same id is a new call for the new document, not a duplicate
        assert_eq!(runtime.admit_rpc(&["m"], 1, "m", 1, 1_000.0), RpcAdmission::Run);
    }

    #[test]
    fn rpc_has_a_burst_of_ten_and_five_per_second() {
        let mut runtime = FrameRuntime::new(0.0);
        for id in 0..10 {
            assert_eq!(runtime.admit_rpc(&["m"], id, "m", 1, 0.0), RpcAdmission::Run, "{id}");
            runtime.finish_rpc(id);
        }
        assert_eq!(
            runtime.admit_rpc(&["m"], 10, "m", 1, 0.0),
            RpcAdmission::Reject { status: 429, error: "rate limit exceeded" }
        );
        // 500 ms refills two and a half tokens.
        assert_eq!(runtime.admit_rpc(&["m"], 11, "m", 1, 500.0), RpcAdmission::Run);
        runtime.finish_rpc(11);
        assert_eq!(runtime.admit_rpc(&["m"], 12, "m", 1, 500.0), RpcAdmission::Run);
        runtime.finish_rpc(12);
        assert_eq!(
            runtime.admit_rpc(&["m"], 13, "m", 1, 500.0),
            RpcAdmission::Reject { status: 429, error: "rate limit exceeded" }
        );
    }

    #[test]
    fn refused_requests_count_against_the_rate_limit_too() {
        let mut runtime = FrameRuntime::new(0.0);
        for id in 0..10 {
            assert_eq!(
                runtime.admit_rpc(&["m"], id, "not-exported", 1, 0.0),
                RpcAdmission::Reject { status: 404, error: "unknown rpc method" }
            );
        }
        assert_eq!(
            runtime.admit_rpc(&["m"], 10, "m", 1, 0.0),
            RpcAdmission::Reject { status: 429, error: "rate limit exceeded" }
        );
    }

    #[test]
    fn a_duplicate_id_in_flight_is_dropped_without_an_answer() {
        let mut runtime = FrameRuntime::new(0.0);
        assert_eq!(runtime.admit_rpc(&["m"], 5, "m", 1, 0.0), RpcAdmission::Run);
        assert_eq!(runtime.admit_rpc(&["m"], 5, "m", 1, 0.0), RpcAdmission::Ignore);
        runtime.finish_rpc(5);
        assert_eq!(runtime.admit_rpc(&["m"], 5, "m", 1, 1_000.0), RpcAdmission::Run);
    }

    fn keys(value: &Value) -> BTreeSet<&str> { value.as_object().unwrap().keys().map(String::as_str).collect() }

    #[test]
    fn parent_messages_carry_exactly_the_frozen_fields() {
        let strings = BTreeMap::from([("k".to_string(), "v".to_string())]);
        let theme = BTreeMap::from([("--theme-brand-text".to_string(), "#fff".to_string())]);
        let context = json!({"channel": {"name": "x"}});

        let init = init_message(&InitMessage {
            plugin_id: "demo",
            slot: PluginSlotDto::EpgOverlay,
            locale: "ar",
            dir: "rtl",
            strings: &strings,
            theme: &theme,
            context: &context,
        });
        assert_eq!(
            keys(&init),
            BTreeSet::from(["tlpx", "type", "pluginId", "slot", "locale", "dir", "strings", "theme", "context"])
        );
        assert_eq!(
            (init["tlpx"].clone(), init["type"].clone(), init["slot"].clone()),
            (json!(1), json!("init"), json!("epg-overlay"))
        );

        let locale = locale_message("es", "ltr", &strings);
        assert_eq!(keys(&locale), BTreeSet::from(["tlpx", "type", "locale", "dir", "strings"]));
        let theme_msg = theme_message(&theme);
        assert_eq!(keys(&theme_msg), BTreeSet::from(["tlpx", "type", "theme"]));
        let context_msg = context_message(&Value::Null);
        assert_eq!(keys(&context_msg), BTreeSet::from(["tlpx", "type", "context"]));
        let ok = rpc_ok_message(3, &json!({"n": 1}));
        assert_eq!(keys(&ok), BTreeSet::from(["tlpx", "type", "id", "ok", "result"]));
        assert_eq!(ok["ok"], json!(true));
        let err = rpc_error_message(4, 429, "slow down");
        assert_eq!(keys(&err), BTreeSet::from(["tlpx", "type", "id", "ok", "status", "error"]));
        assert_eq!(err["ok"], json!(false));
    }

    #[test]
    fn error_text_is_bounded() {
        let long = "e".repeat(2_000);
        let message = rpc_error_message(1, 502, &long);
        assert_eq!(message["error"].as_str().unwrap().chars().count(), MAX_ERROR_CHARS);
    }

    #[test]
    fn the_theme_allowlist_is_a_fixed_list_of_theme_properties() {
        let unique: BTreeSet<&str> = THEME_TOKENS.iter().copied().collect();
        assert_eq!(unique.len(), THEME_TOKENS.len(), "no duplicates");
        let scss = include_str!("../../../../scss/_theme.scss");
        for name in THEME_TOKENS {
            assert!(name.starts_with("--"), "{name}");
            assert!(scss.contains(&format!("{name}:")), "{name} is not defined in _theme.scss");
        }
    }

    #[test]
    fn the_iframe_is_sandboxed_without_same_origin() {
        assert_eq!(FRAME_SANDBOX, "allow-scripts");
        assert!(!FRAME_SANDBOX.contains("same-origin"));
    }
}
