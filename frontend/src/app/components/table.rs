use crate::app::components::{AppIcon, NoContent};
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
                    if let Some(list) = items.as_ref() {
                      html! {
                          <>
                          {
                            for list.iter().enumerate().map(|(row_index, item)| {
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

/// Page size options for paged tables.
pub const PAGE_SIZES: &[u16] = &[25, 50, 100, 200];

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

    let range_start = if total_items == 0 { 0 } else { ((page - 1) * page_size as u32) as u64 + 1 };
    let range_end = (page as u64) * page_size as u64;
    let range_end = range_end.min(total_items);

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
                    <span class="tp__paged-table__info">
                        {format!("{}-{} of {}", range_start, range_end, total_items)}
                    </span>
                    <div class="tp__paged-table__buttons">
                        <button
                            type="button"
                            class="tp__paged-table__btn"
                            disabled={!has_prev}
                            onclick={handle_first}
                            title="First page"
                        >
                            <AppIcon name="ChevronDoubleLeft" />
                        </button>
                        <button
                            type="button"
                            class="tp__paged-table__btn"
                            disabled={!has_prev}
                            onclick={handle_prev}
                            title="Previous page"
                        >
                            <AppIcon name="ChevronLeft" />
                        </button>
                        <span class="tp__paged-table__page-info">
                            {format!("Page {} of {}", page, total_pages)}
                        </span>
                        <button
                            type="button"
                            class="tp__paged-table__btn"
                            disabled={!has_next}
                            onclick={handle_next}
                            title="Next page"
                        >
                            <AppIcon name="ChevronRight" />
                        </button>
                        <button
                            type="button"
                            class="tp__paged-table__btn"
                            disabled={!has_next}
                            onclick={handle_last}
                            title="Last page"
                        >
                            <AppIcon name="ChevronDoubleRight" />
                        </button>
                    </div>
                    <div class="tp__paged-table__size">
                        <label for="page-size-select">{ "Rows:" }</label>
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
                </div>
            }
        </div>
    }
}
