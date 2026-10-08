use super::*;
use crate::{
    app::components::TableColumn,
    i18n::I18nProvider,
    provider::{IconContextProvider, UserSettingsAction, UserSettingsContext, UserSettingsState},
    services::TableLayoutSection,
};
use gloo_timers::future::TimeoutFuture;
use shared::model::TableLayoutPreferencesDto;
use std::cell::RefCell;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};
use web_sys::{Element, Event, HtmlElement};
wasm_bindgen_test_configure!(run_in_browser);

type SortEvents = Rc<RefCell<Vec<Option<(usize, SortOrder)>>>>;

#[derive(Properties, PartialEq)]
struct HarnessProps {
    sorts: SortEvents,
}

#[component]
fn Harness(props: &HarnessProps) -> Html {
    let state = use_reducer(|| UserSettingsState { ready: true, ..Default::default() });
    let items = use_memo((), |()| Rc::new(vec![Rc::new(7)]));
    let set_layout = |hide: bool| {
        let state = state.clone();
        Callback::from(move |_| {
            state.dispatch(UserSettingsAction::Section(
                0,
                "users".into(),
                TableLayoutSection {
                    layout: TableLayoutPreferencesDto {
                        column_order: vec!["last".into(), "first".into(), "sorted".into()],
                        column_visibility: if hide { [("sorted".into(), false)].into() } else { Default::default() },
                    },
                    etag: "test".into(),
                },
            ));
        })
    };
    let sorts = props.sorts.clone();
    let definition = Rc::new(TableDefinition {
        items: Some((*items).clone()),

        table_id: "users".into(),
        columns: Rc::new(vec![
            TableColumn::new("first", "First"),
            TableColumn { sortable: true, ..TableColumn::new("sorted", "Sorted") },
            TableColumn::new("last", "Last"),
        ]),
        row_key: Callback::from(|(_, item): (usize, Rc<i32>)| AttrValue::from(item.to_string())),

        render_header_cell: Callback::from(|column| html! {format!("header-{column}")}),
        render_data_cell: Callback::from(|(_, column, item)| html! {format!("value-{column}-{item}")}),
        on_sort: Callback::from(move |sort| sorts.borrow_mut().push(sort)),
    });
    let context =
        UserSettingsContext { state: state.clone(), open_panel: Callback::noop(), retry_load: Callback::noop() };
    html! {<I18nProvider><IconContextProvider icons={vec![]}><ContextProvider<UserSettingsContext> context={context}>
        <button id="reorder" onclick={set_layout(false)}>{"Reorder"}</button>
        <button id="hide" onclick={set_layout(true)}>{"Hide"}</button>
        <Table<i32> definition={definition}/>
    </ContextProvider<UserSettingsContext>></IconContextProvider></I18nProvider>}
}
async fn settle() {
    TimeoutFuture::new(0).await;
    TimeoutFuture::new(0).await;
}
fn element(root: &Element, selector: &str) -> Result<Element, JsValue> {
    root.query_selector(selector)?.ok_or_else(|| JsValue::from_str(selector))
}
fn click(root: &Element, selector: &str) -> Result<(), JsValue> {
    element(root, selector)?.dispatch_event(&Event::new("click")?)?;
    Ok(())
}

#[wasm_bindgen_test(async)]
async fn rendered_columns_keep_cell_indices_and_neutralize_hidden_sort_once() -> Result<(), JsValue> {
    let document = gloo_utils::document();
    let root = document.create_element("div")?;
    document.body().ok_or_else(|| JsValue::from_str("missing body"))?.append_child(&root)?;
    let sorts = Rc::new(RefCell::new(Vec::new()));
    let handle =
        yew::Renderer::<Harness>::with_root_and_props(root.clone(), HarnessProps { sorts: sorts.clone() }).render();
    settle().await;
    click(&root, "th[data-column-id='sorted'] button")?;
    settle().await;
    assert_eq!(*sorts.borrow(), vec![Some((1, SortOrder::Asc))]);
    click(&root, "#reorder")?;
    settle().await;
    assert_eq!(element(&root, "th:first-child")?.get_attribute("data-column-id").as_deref(), Some("last"));
    assert_eq!(element(&root, "td:first-child")?.text_content().as_deref(), Some("value-2-7"));
    assert_eq!(element(&root, "th[data-column-id='sorted']")?.get_attribute("aria-sort").as_deref(), Some("ascending"));
    assert_eq!(sorts.borrow().len(), 1);
    click(&root, "#hide")?;
    settle().await;
    assert_eq!(root.query_selector_all("thead th")?.length(), 2);
    assert_eq!(root.query_selector_all("tbody td")?.length(), 2);
    assert!(root.query_selector("[data-column-id='sorted']")?.is_none());
    assert_eq!(*sorts.borrow(), vec![Some((1, SortOrder::Asc)), None]);
    click(&root, "#hide")?;
    settle().await;
    assert_eq!(sorts.borrow().len(), 2);
    element(&root, ".tp__table-shell__columns")?.dyn_into::<HtmlElement>()?.focus()?;
    handle.destroy();
    root.remove();
    Ok(())
}

#[derive(Properties, PartialEq)]
struct LayoutHarnessProps {
    rendered_columns: Rc<RefCell<Vec<usize>>>,
}

#[component]
fn LayoutHarness(props: &LayoutHarnessProps) -> Html {
    let state = use_reducer_eq(|| UserSettingsState { ready: true, ..Default::default() });
    let change = {
        let state = state.clone();
        Callback::from(move |_| {
            state.dispatch(UserSettingsAction::Section(
                0,
                "config.schedules".into(),
                TableLayoutSection {
                    layout: TableLayoutPreferencesDto {
                        column_order: vec!["last".into(), "first".into()],
                        column_visibility: [("hidden".into(), false)].into(),
                    },
                    etag: "changed".into(),
                },
            ));
        })
    };
    let rendered = props.rendered_columns.clone();
    let render = use_callback((), move |visible: Rc<Vec<usize>>, ()| {
        const IDS: [&str; 3] = ["first", "hidden", "last"];
        html! {<table>
            <thead><tr>{for visible.iter().map(|&column| html! {
                <th key={IDS[column]} data-column-id={IDS[column]}>{IDS[column]}</th>
            })}</tr></thead>
            <tbody><tr key="row">{for visible.iter().map(|&column| {
                rendered.borrow_mut().push(column);
                html! {<td key={IDS[column]} data-column-id={IDS[column]}>
                    if column == 2 { <input id="layout-input"/> } else { {IDS[column]} }
                </td>}
            })}</tr></tbody>
        </table>}
    });
    let context = UserSettingsContext { state, open_panel: Callback::noop(), retry_load: Callback::noop() };
    html! {<I18nProvider><IconContextProvider icons={vec![]}><ContextProvider<UserSettingsContext> context={context}>
        <button id="layout-change" onclick={change}>{"Change"}</button>
        <crate::app::components::LayoutTable table_id="config.schedules"
            columns={Rc::new(vec![TableColumn::new("first", "First"), TableColumn::new("hidden", "Hidden"), TableColumn::new("last", "Last")])}
            {render}/>
    </ContextProvider<UserSettingsContext>></IconContextProvider></I18nProvider>}
}

#[wasm_bindgen_test(async)]
async fn layout_table_renders_only_visible_cells_and_preserves_input_nodes() -> Result<(), JsValue> {
    let document = gloo_utils::document();
    let root = document.create_element("div")?;
    document.body().ok_or_else(|| JsValue::from_str("missing body"))?.append_child(&root)?;
    let rendered_columns = Rc::new(RefCell::new(Vec::new()));
    let handle = yew::Renderer::<LayoutHarness>::with_root_and_props(
        root.clone(),
        LayoutHarnessProps { rendered_columns: rendered_columns.clone() },
    )
    .render();
    settle().await;
    assert_eq!(*rendered_columns.borrow(), vec![0, 1, 2]);
    let input = element(&root, "#layout-input")?.dyn_into::<web_sys::HtmlInputElement>()?;
    input.set_value("keep my edit");
    rendered_columns.borrow_mut().clear();
    click(&root, "#layout-change")?;
    settle().await;
    assert_eq!(*rendered_columns.borrow(), vec![2, 0]);
    assert_eq!(element(&root, "th:first-child")?.get_attribute("data-column-id").as_deref(), Some("last"));
    assert_eq!(element(&root, "td:first-child")?.get_attribute("data-column-id").as_deref(), Some("last"));
    assert!(root.query_selector("[data-column-id='hidden']")?.is_none());
    let after = element(&root, "#layout-input")?.dyn_into::<web_sys::HtmlInputElement>()?;
    assert!(input.is_same_node(Some(&after)));
    assert_eq!(after.value(), "keep my edit");
    handle.destroy();
    root.remove();
    Ok(())
}

#[derive(Properties, PartialEq)]
struct PaginationHarnessProps {
    page: u32,
    page_size: u16,
    total_pages: u32,
    total_items: u64,
    events: Rc<RefCell<Vec<u32>>>,
}

#[component]
fn PaginationHarness(props: &PaginationHarnessProps) -> Html {
    let definition = use_memo((), |()| TableDefinition {
        table_id: "pagination-test".into(),
        columns: Rc::new(vec![TableColumn::new("value", "Value")]),
        items: Some(Rc::new(vec![Rc::new(7_u32)])),
        row_key: Callback::from(|(_, item): (usize, Rc<u32>)| item.to_string().into()),
        render_header_cell: Callback::from(|_| html! {"Value"}),
        render_data_cell: Callback::from(|(_, _, item): (usize, usize, Rc<u32>)| html! {*item}),
        on_sort: Callback::noop(),
    });
    let events = props.events.clone();
    html! {<I18nProvider><IconContextProvider icons={vec![]}>
        <PagedTable<u32> {definition} page={props.page} page_size={props.page_size}
            total_pages={props.total_pages} total_items={props.total_items}
            has_prev={true} has_next={true}
            on_page_change={Callback::from(move |page| events.borrow_mut().push(page))}
            on_page_size_change={Callback::noop()}/>
    </IconContextProvider></I18nProvider>}
}

#[wasm_bindgen_test(async)]
async fn pagination_normalizes_pages_and_bounds_navigation_callbacks() -> Result<(), JsValue> {
    let document = gloo_utils::document();
    for (page, page_size, total_pages, total_items, range, normalized) in [
        (0, 25, 3, 60, "1-25 of 60".to_owned(), 1),
        (u32::MAX, 25, 3, 60, "51-60 of 60".to_owned(), 3),
        (
            u32::MAX,
            u16::MAX,
            u32::MAX,
            u64::MAX,
            format!(
                "{}-{} of {}",
                (u64::from(u32::MAX) - 1) * u64::from(u16::MAX) + 1,
                u64::from(u32::MAX) * u64::from(u16::MAX),
                u64::MAX
            ),
            u32::MAX,
        ),
    ] {
        let root = document.create_element("div")?;
        document.body().ok_or_else(|| JsValue::from_str("missing body"))?.append_child(&root)?;
        let events = Rc::new(RefCell::new(Vec::new()));
        let handle = yew::Renderer::<PaginationHarness>::with_root_and_props(
            root.clone(),
            PaginationHarnessProps { page, page_size, total_pages, total_items, events: events.clone() },
        )
        .render();
        settle().await;
        assert_eq!(element(&root, ".tp__paged-table__info")?.text_content().as_deref(), Some(range.as_str()));
        let previous = element(&root, ".tp__paged-table__buttons > button:nth-of-type(2)")?;
        let next = element(&root, ".tp__paged-table__buttons > button:nth-of-type(3)")?;
        assert_eq!(previous.has_attribute("disabled"), normalized == 1);
        assert_eq!(next.has_attribute("disabled"), normalized == total_pages);
        // Dispatch directly to also exercise callback bounds on disabled buttons.
        previous.dispatch_event(&Event::new("click")?)?;
        next.dispatch_event(&Event::new("click")?)?;
        assert_eq!(
            *events.borrow(),
            vec![normalized.saturating_sub(1).max(1), normalized.saturating_add(1).min(total_pages)]
        );
        handle.destroy();
        root.remove();
    }
    Ok(())
}
