use super::{visible_columns, TableColumn, TablePanelSpec};
use crate::{
    app::components::{LoadingIndicator, TextButton},
    i18n::use_translation,
    provider::use_user_settings,
};
use std::rc::Rc;
use wasm_bindgen::{closure::Closure, JsCast};
use web_sys::{Element, HtmlElement, MutationObserver, MutationObserverInit, ResizeObserver};
use yew::prelude::*;

fn update_header_size(content: &HtmlElement, strip: &HtmlElement) {
    let offset = content.query_selector("thead").ok().flatten().map_or(0.0, |header| {
        (header.get_bounding_client_rect().bottom() - content.get_bounding_client_rect().top()).max(0.0)
    });
    let _ = strip.style().set_property("--table-header-size", &format!("{offset}px"));
}

struct HeaderObservers {
    resize: ResizeObserver,
    mutation: MutationObserver,
    _on_resize: Closure<dyn FnMut(js_sys::Array, ResizeObserver)>,
    _on_mutation: Closure<dyn FnMut(js_sys::Array, MutationObserver)>,
}

impl HeaderObservers {
    fn new(content: &HtmlElement, strip: &HtmlElement) -> Option<Self> {
        let on_resize = {
            let content = content.clone();
            let strip = strip.clone();
            Closure::<dyn FnMut(js_sys::Array, ResizeObserver)>::new(move |_, _| {
                update_header_size(&content, &strip);
            })
        };
        let resize = ResizeObserver::new(on_resize.as_ref().unchecked_ref()).ok()?;
        let header = content.query_selector("thead").ok().flatten();
        let on_mutation = {
            let content = content.clone();
            let strip = strip.clone();
            let resize = resize.clone();
            let mut observed_header: Option<Element> = header.clone();
            Closure::<dyn FnMut(js_sys::Array, MutationObserver)>::new(move |_, _| {
                // The shell stays mounted when loading finishes or its table is replaced.
                let header = content.query_selector("thead").ok().flatten();
                if header != observed_header {
                    if let Some(previous) = observed_header.as_ref() {
                        resize.unobserve(previous);
                    }
                    if let Some(current) = header.as_ref() {
                        resize.observe(current);
                    }
                    observed_header = header;
                }
                update_header_size(&content, &strip);
            })
        };
        let mutation = MutationObserver::new(on_mutation.as_ref().unchecked_ref()).ok()?;
        let observers = Self { resize, mutation, _on_resize: on_resize, _on_mutation: on_mutation };
        let options = MutationObserverInit::new();
        options.set_child_list(true);
        options.set_subtree(true);
        observers.mutation.observe_with_options(content, &options).ok()?;
        observers.resize.observe(content);
        if let Some(header) = header {
            observers.resize.observe(&header);
        }
        update_header_size(content, strip);
        Some(observers)
    }
}

impl Drop for HeaderObservers {
    fn drop(&mut self) {
        self.mutation.disconnect();
        self.resize.disconnect();
    }
}

#[derive(Properties, PartialEq)]
pub struct TableShellProps {
    pub table_id: AttrValue,
    pub columns: Rc<Vec<TableColumn>>,
    pub children: Children,
    #[prop_or(true)]
    pub supported: bool,
    /// Hides the column settings rail, e.g. while the table shows its empty state.
    #[prop_or(true)]
    pub show_columns: bool,
}

#[component]
pub fn TableShell(props: &TableShellProps) -> Html {
    let content_ref = use_node_ref();
    let strip_ref = use_node_ref();
    {
        let content_ref = content_ref.clone();
        let strip_ref = strip_ref.clone();
        use_effect_with(props.show_columns, move |show_columns| {
            let observers = content_ref.cast::<HtmlElement>().filter(|_| *show_columns).and_then(|content| {
                let strip = strip_ref.cast::<HtmlElement>()?;
                HeaderObservers::new(&content, &strip)
            });
            move || drop(observers)
        });
    }
    let translate = use_translation();
    let context = use_user_settings();
    let ready = context.as_ref().is_none_or(|context| context.state.ready);
    let open = {
        let context = context.clone();
        let strip_ref = strip_ref.clone();
        let spec = TablePanelSpec {
            table_id: props.table_id.clone(),
            columns: props.columns.clone(),
            supported: props.supported,
            anchor: None,
        };
        Callback::from(move |_| {
            if let Some(context) = &context {
                let mut spec = spec.clone();
                spec.anchor = strip_ref
                    .cast::<HtmlElement>()
                    .and_then(|rail| rail.query_selector(".tp__table-shell__columns").ok().flatten());
                context.open_panel.emit(spec);
            }
        })
    };
    let retry = {
        let context = context.clone();
        Callback::from(move |_| {
            if let Some(context) = &context {
                context.retry_load.emit(());
            }
        })
    };
    html! { <div class="tp__table-shell">
        <div ref={content_ref} class="tp__table-shell__content">
            if context.as_ref().is_some_and(|context| context.state.error.is_some()) {
                <div role="alert">{translate.t("TABLE_COLUMNS.LOAD_ERROR")}<button type="button" onclick={retry}>{translate.t("TABLE_COLUMNS.RETRY")}</button></div>
            }
            if ready { <div class="tp__table__container">{for props.children.iter()}</div> } else { <LoadingIndicator loading={true}/> }
        </div>
        if props.show_columns {
            <div ref={strip_ref} class="tp__table-shell__rail">
                <div class="tp__table-shell__corner" aria-hidden="true"/>
                <TextButton name="columns" class="tp__table-shell__columns" icon="Columns" title={translate.t("TABLE_COLUMNS.COLUMNS")}
                    onclick={open} disabled={!ready || context.is_none()} aria_haspopup={Some("dialog".to_owned())}
                    hint={Some(translate.t("TABLE_COLUMNS.OPEN"))} aria_label={Some(translate.t("TABLE_COLUMNS.OPEN"))}/>
            </div>
        }
    </div> }
}

#[derive(Properties, PartialEq)]
pub struct LayoutTableProps {
    pub table_id: AttrValue,
    pub columns: Rc<Vec<TableColumn>>,
    pub render: Callback<Rc<Vec<usize>>, Html>,
}

/// Renders visible columns directly without copying existing virtual DOM cells.
#[component]
pub fn LayoutTable(props: &LayoutTableProps) -> Html {
    let context = use_user_settings();
    let default_layout = shared::model::TableLayoutPreferencesDto::default();
    let layout = context
        .as_ref()
        .and_then(|context| context.state.settings.preferences.web_ui.tables.get(props.table_id.as_str()))
        .unwrap_or(&default_layout);
    let visible = Rc::new(visible_columns(&props.columns, layout));
    html! { <TableShell table_id={props.table_id.clone()} columns={props.columns.clone()}>
        {props.render.emit(visible)}
    </TableShell> }
}

#[cfg(all(test, target_arch = "wasm32"))]
#[path = "shell.browser.test.rs"]
mod browser_tests;
