mod column_enum;
mod columns_panel;
mod csv;
mod layout;
mod paged;
mod shell;

pub use self::{columns_panel::*, csv::*, layout::*, paged::*, shell::*};
use crate::{
    app::components::{AppIcon, NoContent},
    i18n::{use_translation, YewI18n},
};
pub(crate) use column_enum::{define_table_columns, TableColumnSpec};
use shared::model::SortOrder;
use std::rc::Rc;
use yew::prelude::*;

#[derive(Properties, PartialEq)]
pub struct TableDefinition<T: PartialEq + Clone + 'static> {
    pub items: Option<Rc<Vec<Rc<T>>>>,
    pub table_id: AttrValue,
    pub columns: Rc<Vec<TableColumn>>,
    pub row_key: Callback<(usize, Rc<T>), AttrValue>,
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
    let columns = &props.definition.columns;
    let on_sort = &props.definition.on_sort;

    let translate = use_translation();

    // Local sort state: None = neutral; Some((col, order)) = sorted column and order
    let sort_state = use_state::<Option<(usize, SortOrder)>, _>(|| None);

    let on_header_click = {
        let sort_state = sort_state.clone();
        let columns = columns.clone();
        let on_sort = on_sort.clone();
        Callback::from(move |col_index: usize| {
            if !columns.get(col_index).is_some_and(|column| column.sortable) {
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

    let settings = crate::provider::use_user_settings();
    let default_layout = shared::model::TableLayoutPreferencesDto::default();
    let layout = settings
        .as_ref()
        .and_then(|settings| settings.state.settings.preferences.web_ui.tables.get(props.definition.table_id.as_str()))
        .unwrap_or(&default_layout);
    let visible = visible_columns(&props.definition.columns, layout);
    {
        let sort_state = sort_state.clone();
        let on_sort = on_sort.clone();
        use_effect_with(visible.clone(), move |visible| {
            if sort_state.is_some_and(|(index, _)| !visible.contains(&index)) {
                sort_state.set(None);
                on_sort.emit(None);
            }
            || ()
        });
    }
    html! { <div class="tp__table"><TableShell table_id={props.definition.table_id.clone()} columns={props.definition.columns.clone()}
        show_columns={has_table_items(&props.definition.items)}>
        {table_markup(&props.definition, &visible, *sort_state, on_header_click, translate.t("LABEL.NO_CONTENT"))}
    </TableShell></div> }
}

fn table_markup<T: PartialEq + Clone + 'static>(
    definition: &TableDefinition<T>,
    visible: &[usize],
    sort_state: Option<(usize, SortOrder)>,
    on_header_click: Callback<usize>,
    no_content: String,
) -> Html {
    let TableDefinition { items, render_header_cell, render_data_cell, columns, row_key, .. } = definition;
    html! {
        <table class="tp__table__table">
            <thead>
                <tr>
                    {
                        for visible.iter().copied().map(|col_index| {
                            // Determine if this column is sortable
                            let sortable = columns[col_index].sortable;

                            // Decide which icon to show for this column
                            let icon_html = if sortable {
                                match sort_state {
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

                            // Enter/Space activate sorting for keyboard users
                            let on_key_col = {
                                let on_header_click = on_header_click.clone();
                                Callback::from(move |event: KeyboardEvent| {
                                    let key = event.key();
                                    if key == "Enter" || key == " " {
                                        event.prevent_default();
                                        on_header_click.emit(col_index);
                                    }
                                })
                            };

                            html!{
                               <th key={columns[col_index].id.as_str()} data-column-id={columns[col_index].id.clone()}
                                 class={classes!(format!("tp__table__th--{}", col_index+1),
                                     if sortable { Some("tp__table__th--sortable") } else { None }
                                 )}
                                 aria-sort={
                                     if let Some((c, order)) = &sort_state {
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
                                  // Sortable headers expose a real button so the <th> keeps its columnheader semantics
                                  if sortable {
                                      <button type="button" class="tp__table-header"
                                          onclick={on_click_col} onkeydown={on_key_col}>
                                       {render_header_cell.emit(col_index)}
                                       {icon_html}
                                      </button>
                                  } else {
                                      <span class="tp__table-header">
                                       {render_header_cell.emit(col_index)}
                                       {icon_html}
                                      </span>
                                  }
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
                                    <tr key={row_key.emit((row_index, Rc::clone(item))).as_str()}>
                                        {
                                            for visible.iter().copied().map(|col_index| {
                                                html!{
                                                   <td key={columns[col_index].id.as_str()} data-column-id={columns[col_index].id.clone()}>{render_data_cell.emit((row_index, col_index, Rc::clone(item)))}</td>
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
                          <tr><td colspan={visible.len().to_string()}><NoContent text={no_content}/></td></tr>
                        }
                    }
                }
            </tbody>
        </table>
    }
}

/// Build a `Callback<usize, Html>` that returns the i18n-translated column
/// header at the given index, or an empty string when `col` is out of bounds.
/// Used by every `Table`/`PagedTable` caller to produce a uniform header row
/// without each component re-implementing the same closure body.
pub fn make_translated_header_callback(
    translator: YewI18n,
    label: impl Fn(usize) -> Option<&'static str> + 'static,
) -> Callback<usize, Html> {
    Callback::<usize, Html>::from(move |col| {
        html! {
            {
                if let Some(key) = label(col) {
                    translator.t(key)
                } else {
                    String::new()
                }
            }
        }
    })
}

#[cfg(test)]
#[path = "mod.test.rs"]
mod tests;

#[cfg(all(test, target_arch = "wasm32"))]
#[path = "mod.browser.test.rs"]
mod browser_tests;
