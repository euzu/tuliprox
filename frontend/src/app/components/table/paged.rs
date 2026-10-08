use super::{Table, TableDefinition};
use crate::{app::components::AppIcon, i18n::use_translation};
use std::rc::Rc;
use wasm_bindgen::JsCast;
use yew::prelude::*;

/// Page size options for paged tables.
pub const PAGE_SIZES: &[u16] = &[25, 50, 100, 200];

pub const TP_PAGE_SIZE_KEY: &str = "tp-table-page-size";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaginationItem {
    Page(u32),
    Ellipsis,
}

fn build_pagination_items(current_page: u32, total_pages: u32) -> Vec<PaginationItem> {
    if total_pages == 0 {
        return Vec::new();
    }
    if total_pages <= 7 {
        return (1..=total_pages).map(PaginationItem::Page).collect();
    }

    let current_page = current_page.clamp(1, total_pages);
    let mut pages = Vec::with_capacity(9);

    pages.push(1);
    if current_page <= 3 {
        pages.extend(2..=3);
        pages.push(total_pages.saturating_sub(1));
    } else if current_page >= total_pages.saturating_sub(2) {
        pages.push(2);
        pages.extend(total_pages.saturating_sub(2)..total_pages);
    } else {
        pages.extend(current_page.saturating_sub(2)..=current_page.saturating_add(2).min(total_pages));
    }
    pages.push(total_pages);
    pages.sort_unstable();
    pages.dedup();

    let mut items = Vec::with_capacity(pages.len() + 2);
    let mut previous = None;
    for page in pages {
        if let Some(prev) = previous {
            if page > prev + 1 {
                items.push(PaginationItem::Ellipsis);
            }
        }
        items.push(PaginationItem::Page(page));
        previous = Some(page);
    }
    items
}

fn pagination_range(page: u32, page_size: u16, total_items: u64) -> (u64, u64) {
    if total_items == 0 {
        return (0, 0);
    }
    let page_size = u64::from(page_size.max(1));
    let offset = u64::from(page.saturating_sub(1)) * page_size;
    ((offset + 1).min(total_items), (offset + page_size).min(total_items))
}

#[derive(Properties, Clone, PartialEq)]
pub struct PagedTableProps<T: PartialEq + Clone + 'static> {
    pub definition: Rc<TableDefinition<T>>,
    /// Current page number (1-indexed)
    pub page: u32,
    /// Current page size
    pub page_size: u16,
    /// Total number of items across all pages
    pub total_items: u64,
    /// Total number of pages
    pub total_pages: u32,
    /// Whether there is a previous page
    pub has_prev: bool,
    /// Whether there is a next page
    pub has_next: bool,
    /// Callback when page changes
    pub on_page_change: Callback<u32>,
    /// Callback when page size changes
    pub on_page_size_change: Callback<u16>,
}

#[component]
pub fn PagedTable<T: PartialEq + Clone + 'static>(props: &PagedTableProps<T>) -> Html {
    let PagedTableProps {
        definition,
        page,
        page_size,
        total_items,
        total_pages,
        has_prev,
        has_next,
        on_page_change,
        on_page_size_change,
    } = props.clone();

    let translate = use_translation();

    let total_pages = total_pages.max(1);
    let page = page.clamp(1, total_pages);
    let page_size = page_size.max(1);
    let has_prev = has_prev && page > 1;
    let has_next = has_next && page < total_pages;
    let (range_start, range_end) = pagination_range(page, page_size, total_items);
    let pagination_items = build_pagination_items(page, total_pages);
    let first_page_label = translate.t("LABEL.FIRST_PAGE");
    let previous_page_label = translate.t("LABEL.PREVIOUS_PAGE");
    let next_page_label = translate.t("LABEL.NEXT_PAGE");
    let last_page_label = translate.t("LABEL.LAST_PAGE");

    let handle_first = {
        let on_page_change = on_page_change.clone();
        Callback::from(move |_: MouseEvent| on_page_change.emit(1))
    };

    let handle_prev = {
        let on_page_change = on_page_change.clone();
        Callback::from(move |_: MouseEvent| on_page_change.emit(page.saturating_sub(1).max(1)))
    };

    let handle_next = {
        let on_page_change = on_page_change.clone();
        Callback::from(move |_: MouseEvent| on_page_change.emit(page.saturating_add(1).min(total_pages)))
    };

    let handle_last = {
        let on_page_change = on_page_change.clone();
        Callback::from(move |_: MouseEvent| on_page_change.emit(total_pages))
    };

    let handle_page_size_change = {
        let on_page_size_change = on_page_size_change.clone();
        Callback::from(move |e: Event| {
            let target = e.target_unchecked_into::<web_sys::HtmlElement>();
            if let Some(select) = target.dyn_ref::<web_sys::HtmlSelectElement>() {
                if let Ok(size) = select.value().parse::<u16>() {
                    if PAGE_SIZES.contains(&size) {
                        on_page_size_change.emit(size);
                    }
                }
            }
        })
    };

    html! {
        <div class="tp__paged-table">
            <Table<T> {definition} />
            if total_items > 0 {
                <div class="tp__paged-table__controls">
                    <div class="tp__paged-table__paging-info">
                        <span class="tp__paged-table__info">
                            {format!("{range_start}-{range_end} of {total_items}")}
                        </span>
                        <div class="tp__paged-table__size">
                            <label for="page-size-select">{ translate.t("LABEL.ROWS") } {":"}</label>
                            <select
                                id="page-size-select"
                                class="tp__paged-table__select"
                                value={page_size.to_string()}
                                onchange={handle_page_size_change}
                            >
                                { for PAGE_SIZES.iter().map(|&size| {
                                    html! {
                                        <option value={size.to_string()} selected={size == page_size}>
                                            {size.to_string()}
                                        </option>
                                    }
                                }) }
                            </select>
                        </div>
                        <span class="tp__paged-table__page-info">
                            {format!("{} {page} / {total_pages}", translate.t("LABEL.PAGE"))}
                        </span>
                    </div>
                    <div class="tp__paged-table__buttons">
                        <button
                            type="button"
                            class="tp__paged-table__btn tp__icon-button"
                            disabled={!has_prev}
                            onclick={handle_first}
                            title={first_page_label.clone()}
                            aria-label={first_page_label}
                        >
                            <AppIcon name="ChevronDoubleLeft" />
                        </button>
                        <button
                            type="button"
                            class="tp__paged-table__btn tp__icon-button"
                            disabled={!has_prev}
                            onclick={handle_prev}
                            title={previous_page_label.clone()}
                            aria-label={previous_page_label}
                        >
                            <AppIcon name="ChevronLeft" />
                        </button>
                        <div class="tp__paged-table__pages" aria-label={translate.t("LABEL.PAGES")}>
                            {
                                for pagination_items.into_iter().map(|item| match item {
                                    PaginationItem::Page(page_number) => {
                                        let on_page_change = on_page_change.clone();
                                        let is_current = page_number == page;
                                        html! {
                                            <button
                                                type="button"
                                                class={classes!("tp__paged-table__btn", "tp__icon-button", "tp__paged-table__page", is_current.then_some("active"))}
                                                disabled={is_current}
                                                onclick={Callback::from(move |_: MouseEvent| on_page_change.emit(page_number))}
                                                title={format!("Page {page_number}")}
                                                aria-current={is_current.then_some("page")}
                                            >
                                                {page_number}
                                            </button>
                                        }
                                    }
                                    PaginationItem::Ellipsis => html! {
                                        <span class="tp__paged-table__ellipsis" aria-hidden="true">{"..."}</span>
                                    },
                                })
                            }
                        </div>
                        <button
                            type="button"
                            class="tp__paged-table__btn tp__icon-button"
                            disabled={!has_next}
                            onclick={handle_next}
                            title={next_page_label.clone()}
                            aria-label={next_page_label}
                        >
                            <AppIcon name="ChevronRight" />
                        </button>
                        <button
                            type="button"
                            class="tp__paged-table__btn tp__icon-button"
                            disabled={!has_next}
                            onclick={handle_last}
                            title={last_page_label.clone()}
                            aria-label={last_page_label}
                        >
                            <AppIcon name="ChevronDoubleRight" />
                        </button>
                    </div>
                </div>
            }
        </div>
    }
}

#[cfg(test)]
#[path = "paged.test.rs"]
mod tests;
