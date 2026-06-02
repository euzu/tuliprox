use crate::{
    app::components::{AppIcon, NoContent},
    i18n::use_translation,
};
use shared::model::SortOrder;
use std::rc::Rc;
use wasm_bindgen::JsCast;
use yew::prelude::*;

#[derive(Properties, PartialEq)]
pub struct TableDefinition<T: PartialEq + Clone + 'static> {
    pub items: Option<Rc<Vec<Rc<T>>>>,
    pub num_cols: usize,
    // Return true if a given column is sortable
    pub is_sortable: Callback<usize, bool>,
    pub render_header_cell: Callback<usize, Html>,
    pub render_data_cell: Callback<(usize, usize, Rc<T>), Html>,
    #[prop_or_else(Callback::noop)]
    pub on_sort: Callback<Option<(usize, SortOrder)>, ()>,
}

#[derive(Properties, Clone, PartialEq)]
pub struct TableProps<T: PartialEq + Clone + 'static> {
    pub definition: Rc<TableDefinition<T>>,
}

fn has_table_items<T: PartialEq + Clone + 'static>(items: &Option<Rc<Vec<Rc<T>>>>) -> bool {
    items.as_ref().is_some_and(|list| !list.is_empty())
}

#[component]
pub fn Table<T: PartialEq + Clone + 'static>(props: &TableProps<T>) -> Html {
    let TableDefinition { items, num_cols, is_sortable, on_sort, render_header_cell, render_data_cell } =
        &*props.definition;

    // Local sort state: None = neutral; Some((col, order)) = sorted column and order
    let sort_state = use_state::<Option<(usize, SortOrder)>, _>(|| None);

    let on_header_click = {
        let sort_state = sort_state.clone();
        let is_sortable = is_sortable.clone();
        let on_sort = on_sort.clone();
        Callback::from(move |col_index: usize| {
            if !is_sortable.emit(col_index) {
                return;
            }
            let state = match *sort_state {
                Some((c, SortOrder::Asc)) if c == col_index => Some((col_index, SortOrder::Desc)),
                Some((c, SortOrder::Desc)) if c == col_index => None,
                _ => Some((col_index, SortOrder::Asc)),
            };

            sort_state.set(state);
            on_sort.emit(state);
        })
    };

    html! {
        <div class={"tp__table"}>
        <div class={"tp__table__container"}>
        <table class="tp__table__table">
            <thead>
                <tr>
                    {
                        for (0..*num_cols).map(|col_index| {
                            // Determine if this column is sortable
                            let sortable = is_sortable.emit(col_index);

                            // Decide which icon to show for this column
                            let icon_html = if sortable {
                                match *sort_state {
                                    Some((c, SortOrder::Asc)) if c == col_index => html!{ <AppIcon name="SortAsc"/> },
                                    Some((c, SortOrder::Desc)) if c == col_index => html!{ <AppIcon name="SortDesc"/> },
                                    _ => html!{ <AppIcon name="Sort"/> }, // neutral
                                }
                            } else {
                                html!{}
                            };

                            // Click handler per column
                            let on_click_col = {
                                let on_header_click = on_header_click.clone();
                                Callback::from(move |_| on_header_click.emit(col_index))
                            };

                            html!{
                               <th
                                 class={classes!(format!("tp__table__th--{}", col_index+1),
                                     if sortable { Some("tp__table__th--sortable") } else { None }
                                 )}
                                 onclick={if sortable { Some(on_click_col) } else { None }}
                                 role={if sortable { Some("button") } else { None }}
                                 aria-sort={
                                     if let Some((c, order)) = &*sort_state {
                                         if *c == col_index {
                                             Some(match order {
                                                 SortOrder::Asc => "ascending",
                                                 SortOrder::Desc => "descending",
                                                 SortOrder::None => "none",
                                             }.to_string())
                                         } else { Some("none".to_string()) }
                                     } else { Some("none".to_string()) }
                                 }
                               >
                                  <span class="tp__table-header">
                                   {render_header_cell.emit(col_index)}
                                   {icon_html}
                                  </span>
                               </th>
                            }
                        })
                    }
                </tr>
            </thead>
            <tbody>
                {
                    if has_table_items(items) {
                      html! {
                          <>
                          {
                            for items.as_ref().into_iter().flat_map(|list| list.iter().enumerate()).map(|(row_index, item)| {
                                html! {
                                    <tr>
                                        {
                                            for (0..*num_cols).map(|col_index| {
                                                html!{
                                                   <td>{render_data_cell.emit((row_index, col_index, Rc::clone(item)))}</td>
                                                }
                                            })
                                        }
                                    </tr>
                                }
                            })
                          }
                          </>
                      }
                    } else {
                       html!{
                          <tr><td colspan={num_cols.to_string()}><NoContent/></td></tr>
                        }
                    }
                }
            </tbody>
        </table>
        </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::{build_pagination_items, has_table_items, PaginationItem};
    use std::rc::Rc;

    #[test]
    fn table_treats_none_and_empty_items_as_no_content() {
        let none_items: Option<Rc<Vec<Rc<i32>>>> = None;
        let empty_items: Option<Rc<Vec<Rc<i32>>>> = Some(Rc::new(Vec::new()));
        let populated_items: Option<Rc<Vec<Rc<i32>>>> = Some(Rc::new(vec![Rc::new(1)]));

        assert!(!has_table_items(&none_items));
        assert!(!has_table_items(&empty_items));
        assert!(has_table_items(&populated_items));
    }

    #[test]
    fn pagination_items_show_all_pages_for_small_page_counts() {
        assert_eq!(
            build_pagination_items(1, 5),
            vec![
                PaginationItem::Page(1),
                PaginationItem::Page(2),
                PaginationItem::Page(3),
                PaginationItem::Page(4),
                PaginationItem::Page(5),
            ]
        );
    }

    #[test]
    fn pagination_items_collapse_middle_near_start() {
        assert_eq!(
            build_pagination_items(1, 9),
            vec![
                PaginationItem::Page(1),
                PaginationItem::Page(2),
                PaginationItem::Page(3),
                PaginationItem::Ellipsis,
                PaginationItem::Page(8),
                PaginationItem::Page(9),
            ]
        );
    }

    #[test]
    fn pagination_items_show_window_around_current_page() {
        assert_eq!(
            build_pagination_items(6, 11),
            vec![
                PaginationItem::Page(1),
                PaginationItem::Ellipsis,
                PaginationItem::Page(4),
                PaginationItem::Page(5),
                PaginationItem::Page(6),
                PaginationItem::Page(7),
                PaginationItem::Page(8),
                PaginationItem::Ellipsis,
                PaginationItem::Page(11),
            ]
        );
    }
}

/// Page size options for paged tables.
pub const PAGE_SIZES: &[u16] = &[25, 50, 100, 200];

/// localStorage key used to remember the last selected table page size.
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

    let range_start = if total_items == 0 { 0 } else { ((page - 1) * page_size as u32) as u64 + 1 };
    let range_end = (page as u64) * page_size as u64;
    let range_end = range_end.min(total_items);
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
        Callback::from(move |_: MouseEvent| on_page_change.emit(page.saturating_sub(1)))
    };

    let handle_next = {
        let on_page_change = on_page_change.clone();
        Callback::from(move |_: MouseEvent| on_page_change.emit(page + 1))
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
                    on_page_size_change.emit(size);
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
