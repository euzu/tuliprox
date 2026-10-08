use super::*;
use crate::{
    i18n::I18nProvider,
    provider::{IconContextProvider, UserSettingsAction, UserSettingsContext, UserSettingsState},
};
use gloo_timers::future::TimeoutFuture;
use wasm_bindgen::{prelude::wasm_bindgen, JsValue};
use wasm_bindgen_test::wasm_bindgen_test;
use web_sys::Event;

#[wasm_bindgen(inline_js = "
export function installResizeSpy() {
    const Original = globalThis.ResizeObserver;
    const spy = { created: 0, disconnected: 0, targets: new Set(), Original };
    globalThis.ResizeObserver = class extends Original {
        constructor(callback) { super(callback); spy.created++; }
        observe(target) { spy.targets.add(target); super.observe(target); }
        unobserve(target) { spy.targets.delete(target); super.unobserve(target); }
        disconnect() { spy.disconnected++; spy.targets.clear(); super.disconnect(); }
    };
    return spy;
}
export function restoreResizeSpy(spy) { globalThis.ResizeObserver = spy.Original; }
export function resizeCreated(spy) { return spy.created; }
export function resizeDisconnected(spy) { return spy.disconnected; }
export function resizeObserves(spy, target) { return spy.targets.has(target); }
")]
extern "C" {
    #[wasm_bindgen(js_name = installResizeSpy)]
    fn install_resize_spy() -> JsValue;
    #[wasm_bindgen(js_name = restoreResizeSpy)]
    fn restore_resize_spy(spy: &JsValue);
    #[wasm_bindgen(js_name = resizeCreated)]
    fn resize_created(spy: &JsValue) -> u32;
    #[wasm_bindgen(js_name = resizeDisconnected)]
    fn resize_disconnected(spy: &JsValue) -> u32;
    #[wasm_bindgen(js_name = resizeObserves)]
    fn resize_observes(spy: &JsValue, target: &Element) -> bool;
}

struct ResizeSpy(JsValue);

impl Drop for ResizeSpy {
    fn drop(&mut self) { restore_resize_spy(&self.0); }
}

#[component]
fn ObserverHarness() -> Html {
    let state = use_reducer_eq(UserSettingsState::default);
    let alternate = use_state_eq(|| false);
    let counter = use_state_eq(|| 0_u32);
    let columns = use_memo((), |()| vec![TableColumn::new("name", "Name")]);
    let load = {
        let state = state.clone();
        Callback::from(move |_| state.dispatch(UserSettingsAction::Loaded(0, Ok(Default::default()))))
    };
    let unload = {
        let state = state.clone();
        Callback::from(move |_| state.dispatch(UserSettingsAction::Begin(0)))
    };
    let replace = {
        let alternate = alternate.clone();
        Callback::from(move |_| alternate.set(!*alternate))
    };
    let refresh = {
        let counter = counter.clone();
        Callback::from(move |_| counter.set(*counter + 1))
    };
    let context = UserSettingsContext { state, open_panel: Callback::noop(), retry_load: Callback::noop() };
    html! {
        <I18nProvider><IconContextProvider icons={vec![]}>
            <ContextProvider<UserSettingsContext> context={context}>
                <button id="observer-load" onclick={load}>{"Load"}</button>
                <button id="observer-unload" onclick={unload}>{"Unload"}</button>
                <button id="observer-replace" onclick={replace}>{"Replace"}</button>
                <button id="observer-refresh" onclick={refresh}>{"Refresh"}</button>
                <TableShell table_id="observer-test" {columns}>
                    <table key={if *alternate { "second" } else { "first" }}>
                        <thead style={if *alternate { "height: 60px;" } else { "height: 30px;" }}>
                            <tr><th>{"Name"}</th></tr>
                        </thead>
                        <tbody><tr><td>{*counter}</td></tr></tbody>
                    </table>
                </TableShell>
            </ContextProvider<UserSettingsContext>>
        </IconContextProvider></I18nProvider>
    }
}

async fn settle() { TimeoutFuture::new(50).await; }

fn element(root: &Element, selector: &str) -> Result<Element, JsValue> {
    root.query_selector(selector)?.ok_or_else(|| JsValue::from_str(selector))
}

fn click(root: &Element, selector: &str) -> Result<(), JsValue> {
    element(root, selector)?.dispatch_event(&Event::new("click")?)?;
    Ok(())
}

fn header_size(root: &Element) -> Result<f64, JsValue> {
    let strip = element(root, ".tp__table-shell__rail")?.dyn_into::<HtmlElement>()?;
    let size = strip.style().get_property_value("--table-header-size")?;
    size.trim_end_matches("px")
        .parse()
        .map_err(|error: std::num::ParseFloatError| JsValue::from_str(&error.to_string()))
}

#[wasm_bindgen_test(async)]
async fn header_observer_survives_loading_rerenders_and_replaced_headers() -> Result<(), JsValue> {
    let spy = ResizeSpy(install_resize_spy());
    let document = gloo_utils::document();
    let root = document.create_element("div")?;
    document.body().ok_or_else(|| JsValue::from_str("missing body"))?.append_child(&root)?;
    let handle = yew::Renderer::<ObserverHarness>::with_root(root.clone()).render();
    settle().await;
    assert_eq!(resize_created(&spy.0), 1);
    assert!(root.query_selector("thead")?.is_none());
    assert_eq!(header_size(&root)?, 0.0);

    click(&root, "#observer-load")?;
    settle().await;
    let first = element(&root, "thead")?;
    let first_size = header_size(&root)?;
    assert!(first_size > 0.0);
    assert!(resize_observes(&spy.0, &first));
    assert_eq!(resize_created(&spy.0), 1);

    click(&root, "#observer-refresh")?;
    settle().await;
    assert_eq!(resize_created(&spy.0), 1);
    assert_eq!(resize_disconnected(&spy.0), 0);

    click(&root, "#observer-replace")?;
    settle().await;
    let second = element(&root, "thead")?;
    assert!(!first.is_same_node(Some(&second)));
    assert!(!resize_observes(&spy.0, &first));
    assert!(resize_observes(&spy.0, &second));
    assert!(header_size(&root)? > first_size);
    assert_eq!(resize_created(&spy.0), 1);

    click(&root, "#observer-unload")?;
    settle().await;
    assert!(root.query_selector("thead")?.is_none());
    assert!(!resize_observes(&spy.0, &second));
    assert_eq!(header_size(&root)?, 0.0);

    handle.destroy();
    settle().await;
    assert_eq!(resize_disconnected(&spy.0), 1);
    root.remove();
    Ok(())
}
