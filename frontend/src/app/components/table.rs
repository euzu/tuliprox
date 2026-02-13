use crate::app::components::{AppIcon, NoContent};
use shared::model::SortOrder;
use std::rc::Rc;
use yew::prelude::*;

#[derive(Properties, PartialEq)]
pub struct TableDefinition<T: PartialEq + Clone> {
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
pub struct TableProps<T: PartialEq + Clone> {
    pub definition: Rc<TableDefinition<T>>,
}

#[function_component]
pub fn Table<T: PartialEq + Clone>(props: &TableProps<T>) -> Html {
    let TableDefinition {
        items,
        num_cols,
        is_sortable,
        on_sort,
        render_header_cell,
        render_data_cell,
    } = &*props.definition;

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
