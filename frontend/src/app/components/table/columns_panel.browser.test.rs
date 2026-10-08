use super::*;
use crate::{
    hooks::ServiceContext,
    i18n::I18nProvider,
    model::WebConfig,
    provider::{DialogProvider, IconContextProvider, UserSettingsContext, UserSettingsState},
    services::FlagsService,
};
use gloo_timers::future::TimeoutFuture;
use std::rc::Rc;
use wasm_bindgen::{prelude::*, JsValue};
use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};
use web_sys::HtmlInputElement;
wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen(inline_js = r#"
let originalFetch;
let originalFrame;
let frames;
let calls;
let revision;
let layout;
let fail;
export function startSettingsMock() {
  originalFrame = window.requestAnimationFrame; frames = 0;
  window.requestAnimationFrame = callback => { frames++; return originalFrame.call(window, callback); };
  originalFetch = window.fetch; calls = []; revision = 1; layout = {column_order: [],column_visibility: {}}; fail = true;
  window.fetch = async function(request) {
    const method = request.method;
    const body = method === 'PUT' ? await request.clone().json() : null;
    calls.push({method,body,etag:request.headers.get('If-Match')});
    if (method === 'PUT' && fail) { fail = false; revision++; return new Response(JSON.stringify({error:'settings_precondition_failed'}),{status:412,headers:{'Content-Type':'application/json'}}); }
    if (method === 'PUT') { layout = body; revision++; }
    if (method === 'DELETE') { layout = {column_order:[],column_visibility:{}}; revision++; }
    return new Response(JSON.stringify(layout),{status:200,headers:{'Content-Type':'application/json',ETag:'"'+String(revision).repeat(64)+'"'}});
  };
}
export function stopSettingsMock() { window.fetch = originalFetch; window.requestAnimationFrame = originalFrame; }
export function dragFrames() { return frames; }
export function settingsCalls() { return JSON.stringify(calls); }
export function sendKey(node,key) { node.dispatchEvent(new KeyboardEvent('keydown',{key,bubbles:true,cancelable:true})); }
export function sendPointer(node,type,y) { node.dispatchEvent(new PointerEvent(type,{pointerId:13,pointerType:'touch',isPrimary:true,button:0,clientY:y,bubbles:true,cancelable:true})); }
"#)]
extern "C" {
    #[wasm_bindgen(js_name=startSettingsMock)]
    fn start_mock();
    #[wasm_bindgen(js_name=stopSettingsMock)]
    fn stop_mock();
    #[wasm_bindgen(js_name=settingsCalls)]
    fn calls() -> String;
    #[wasm_bindgen(js_name=dragFrames)]
    fn frames() -> u32;
    #[wasm_bindgen(js_name=sendKey)]
    fn key(node: &Element, key: &str);
    #[wasm_bindgen(js_name=sendPointer)]
    fn pointer(node: &Element, kind: &str, y: f64);
}
struct MockGuard;
impl Drop for MockGuard {
    fn drop(&mut self) { stop_mock(); }
}

#[component]
fn Harness() -> Html {
    let services = use_state(|| ServiceContext::new(&WebConfig::default(), FlagsService::new()));
    let state = use_reducer(|| UserSettingsState { ready: true, ..Default::default() });
    let open = use_state(|| false);
    let show = {
        let open = open.clone();
        Callback::from(move |_| open.set(true))
    };
    let close = {
        let open = open.clone();
        Callback::from(move |()| open.set(false))
    };
    let context = UserSettingsContext { state, open_panel: Callback::noop(), retry_load: Callback::noop() };
    let spec = TablePanelSpec {
        table_id: "users".into(),
        columns: Rc::new(vec![
            TableColumn::translated("a", "CUSTOM.COLUMN_TITLE"),
            TableColumn::new("b", "LABEL.NAME"),
            TableColumn::new("c", "TABLE_COLUMNS.COLUMNS"),
            TableColumn { can_hide: false, content: false, ..TableColumn::new("actions", "Actions") },
        ]),
        supported: true,
        anchor: None,
    };
    let translations = std::collections::HashMap::from([(
        "en".to_owned(),
        serde_json::json!({
            "CUSTOM": {"COLUMN_TITLE": "Translated A"},
            "LABEL": {"NAME": "Translated name"},
            "TABLE_COLUMNS": {"COLUMNS": "Localized columns"},
        }),
    )]);
    html! {<I18nProvider translations={translations}><IconContextProvider icons={vec![]}><ContextProvider<UseStateHandle<ServiceContext>> context={services}><ContextProvider<UserSettingsContext> context={context}>
        <style>{".tp__table-columns__list {max-height:132px;overflow-y:auto} .tp__table-columns__entry {height:44px;display:flex}"}</style>
        <button id="panel-open" onclick={show}>{"Open"}</button>
        if *open {<DialogProvider><TableColumnsPanel spec={spec} on_close={close}/></DialogProvider>}
    </ContextProvider<UserSettingsContext>></ContextProvider<UseStateHandle<ServiceContext>>></IconContextProvider></I18nProvider>}
}
async fn settle() {
    TimeoutFuture::new(0).await;
    TimeoutFuture::new(0).await;
}
fn find(selector: &str) -> Result<Element, JsValue> {
    gloo_utils::document().query_selector(selector)?.ok_or_else(|| JsValue::from_str(selector))
}
fn click(selector: &str) -> Result<(), JsValue> {
    find(selector)?.dispatch_event(&web_sys::Event::new("click")?)?;
    Ok(())
}
fn order() -> Result<Vec<String>, JsValue> {
    let list = find(".tp__table-columns__list")?.query_selector_all("li")?;
    Ok((0..list.length())
        .filter_map(|i| list.item(i)?.dyn_into::<Element>().ok()?.get_attribute("data-column-id"))
        .collect())
}

async fn wait_for_reorder(original: &[&str]) -> Result<Vec<String>, JsValue> {
    for _ in 0..50 {
        let current = order()?;
        if current.iter().map(String::as_str).ne(original.iter().copied()) {
            return Ok(current);
        }
        TimeoutFuture::new(16).await;
    }
    order()
}

#[wasm_bindgen_test(async)]
async fn panel_keyboard_touch_cancel_and_conflict_require_explicit_reapply() -> Result<(), JsValue> {
    start_mock();
    let _mock = MockGuard;
    let root = gloo_utils::document().create_element("div")?;
    gloo_utils::document().body().ok_or_else(|| JsValue::from_str("missing body"))?.append_child(&root)?;
    let handle = yew::Renderer::<Harness>::with_root(root.clone()).render();
    settle().await;
    find("#panel-open")?.dyn_into::<HtmlElement>()?.focus()?;
    click("#panel-open")?;
    settle().await;
    for _ in 0..50 {
        if find(".tp__table-columns__reset")?.get_attribute("disabled").is_none() {
            break;
        }
        TimeoutFuture::new(16).await;
    }
    assert!(find(".tp__table-columns__reset")?.get_attribute("disabled").is_none());
    assert_eq!(order()?, vec!["a", "b", "c", "actions"]);
    TimeoutFuture::new(40).await;
    assert_eq!(frames(), 0, "an idle panel must not schedule animation frames");
    assert_eq!(
        find("li[data-column-id='a'] .tp__table-columns__label")?.text_content().as_deref(),
        Some("Translated A")
    );
    assert_eq!(find("li[data-column-id='b'] .tp__table-columns__label")?.text_content().as_deref(), Some("LABEL.NAME"));
    assert_eq!(
        find("li[data-column-id='c'] .tp__table-columns__label")?.text_content().as_deref(),
        Some("TABLE_COLUMNS.COLUMNS")
    );
    let a = find("li[data-column-id='a'] button")?;
    key(&a, " ");
    settle().await;
    key(&a, "ArrowDown");
    settle().await;
    assert_eq!(order()?, vec!["b", "a", "c", "actions"]);
    assert!(!find(".tp__table-columns__live")?.text_content().unwrap_or_default().is_empty());
    key(&a, "Escape");
    settle().await;
    assert_eq!(order()?, vec!["a", "b", "c", "actions"]);
    assert!(gloo_utils::document().query_selector(".tp__table-columns")?.is_some());
    assert_eq!(frames(), 0, "keyboard reordering must not start the pointer animation");
    let row = find("li[data-column-id='a']")?.dyn_into::<HtmlElement>()?;
    let start = a.get_bounding_client_rect().top();
    let grab_offset = start - row.get_bounding_client_rect().top();
    pointer(&a, "pointerdown", start);
    settle().await;
    assert!(find(".tp__table-columns__list")?.class_list().contains("tp__table-columns__list--dragging"));
    let y = start + 50.0;
    pointer(&find(".tp__table-columns__list")?, "pointermove", y);
    TimeoutFuture::new(40).await;
    assert!((row.get_bounding_client_rect().top() - (y - grab_offset)).abs() < 1.0);
    assert!(!row.style().get_property_value("transform")?.is_empty());
    pointer(&find(".tp__table-columns__list")?, "pointercancel", y);
    settle().await;
    assert_eq!(order()?, vec!["a", "b", "c", "actions"]);
    assert!(row.style().get_property_value("transform")?.is_empty());
    assert!(!find(".tp__table-columns__list")?.class_list().contains("tp__table-columns__list--dragging"));
    let stopped_frames = frames();
    TimeoutFuture::new(40).await;
    assert_eq!(frames(), stopped_frames, "canceling a drag must stop the animation loop");
    let a = find("li[data-column-id='a'] button")?;
    let start = a.get_bounding_client_rect().top();
    pointer(&a, "pointerdown", start);
    pointer(&find(".tp__table-columns__list")?, "pointermove", start + 50.0);
    TimeoutFuture::new(40).await;
    key(&a, "Enter");
    settle().await;
    let stopped_frames = frames();
    TimeoutFuture::new(40).await;
    assert_eq!(frames(), stopped_frames, "finishing a pointer drag with Enter must stop animation");
    assert!(!find(".tp__table-columns__list")?.class_list().contains("tp__table-columns__list--dragging"));
    click(".tp__table-columns__reset")?;
    settle().await;
    let a = find("li[data-column-id='a'] button")?;
    let start = a.get_bounding_client_rect().top();
    let end = find("li[data-column-id='c']")?.get_bounding_client_rect().bottom() + 1.0;
    pointer(&a, "pointerdown", start);
    pointer(&find(".tp__table-columns__list")?, "pointermove", end);
    pointer(&find(".tp__table-columns__list")?, "pointerup", end);
    settle().await;
    assert_ne!(order()?, vec!["a", "b", "c", "actions"], "pointerup must apply movement before the next frame");
    TimeoutFuture::new(40).await;
    assert_eq!(frames(), stopped_frames, "a completed short drag must not leave an animation running");
    click(".tp__table-columns__reset")?;
    settle().await;
    assert_eq!(order()?, vec!["a", "b", "c", "actions"]);
    let a = find("li[data-column-id='a'] button")?;
    let start = a.get_bounding_client_rect().top();
    let end = find("li[data-column-id='c']")?.get_bounding_client_rect().bottom() + 1.0;
    pointer(&a, "pointerdown", start);
    settle().await;
    // Pointer capture routes movement and cancellation to the stable list.
    pointer(&find(".tp__table-columns__list")?, "pointermove", end);
    settle().await;
    assert_ne!(wait_for_reorder(&["a", "b", "c", "actions"]).await?, vec!["a", "b", "c", "actions"]);
    pointer(&find(".tp__table-columns__list")?, "pointercancel", end);
    settle().await;
    assert_eq!(order()?, vec!["a", "b", "c", "actions"]);
    let a = find("li[data-column-id='a'] button")?;
    let start = a.get_bounding_client_rect().top();
    let end = find("li[data-column-id='c']")?.get_bounding_client_rect().bottom() + 1.0;
    pointer(&a, "pointerdown", start);
    settle().await;
    pointer(&find(".tp__table-columns__list")?, "pointermove", end);
    settle().await;
    let moved = wait_for_reorder(&["a", "b", "c", "actions"]).await?;
    assert_ne!(moved, vec!["a", "b", "c", "actions"]);
    pointer(&find(".tp__table-columns__list")?, "pointerup", end);
    settle().await;
    assert_eq!(order()?, moved);
    click(".tp__table-columns__reset")?;
    settle().await;
    let a = find("li[data-column-id='a'] button")?;
    key(&a, " ");
    key(&a, "ArrowDown");
    settle().await;
    key(&a, " ");
    settle().await;
    let checkbox = find("li[data-column-id='b'] input")?.dyn_into::<HtmlInputElement>()?;
    click("li[data-column-id='b'] input")?;
    settle().await;
    click(".tp__table-columns__save")?;
    settle().await;
    assert!(gloo_utils::document().query_selector(".tp__table-columns")?.is_some());
    assert_eq!(order()?, vec!["b", "a", "c", "actions"]);
    assert!(!checkbox.checked());
    assert!(find(".tp__table-columns__save")?.has_attribute("disabled"));
    click(".tp__table-columns__reapply")?;
    settle().await;
    click(".tp__table-columns__save")?;
    settle().await;
    assert!(gloo_utils::document().query_selector(".tp__table-columns")?.is_none());
    let recorded_calls: serde_json::Value =
        serde_json::from_str(&calls()).map_err(|error| JsValue::from_str(&error.to_string()))?;
    let saves: Vec<_> = recorded_calls
        .as_array()
        .ok_or_else(|| JsValue::from_str("missing calls"))?
        .iter()
        .filter(|call| call["method"] == "PUT")
        .collect();
    assert_eq!(saves.len(), 2);
    assert_ne!(saves[0]["etag"], saves[1]["etag"]);
    assert_eq!(saves[1]["body"]["column_order"], serde_json::json!(["b", "a", "c", "actions"]));
    assert_eq!(saves[1]["body"]["column_visibility"]["b"], false);
    assert!(gloo_utils::document().active_element().is_some_and(|element| element.id() == "panel-open"));
    click("#panel-open")?;
    settle().await;
    click(".tp__table-columns__reset")?;
    settle().await;
    assert!(find("li[data-column-id='b'] input")?.dyn_into::<HtmlInputElement>()?.checked());
    click(".tp__table-columns__save")?;
    settle().await;
    assert!(gloo_utils::document().query_selector(".tp__table-columns")?.is_none());
    assert!(calls().contains("DELETE"));
    handle.destroy();
    root.remove();
    Ok(())
}
