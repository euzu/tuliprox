use super::has_table_items;
use std::rc::Rc;

fn rendered_tags(node: &yew::Html) -> Vec<&yew::virtual_dom::VTag> {
    match node {
        yew::Html::VTag(tag) => {
            std::iter::once(tag.as_ref()).chain(tag.children().into_iter().flat_map(rendered_tags)).collect()
        }
        yew::Html::VList(list) => list.iter().flat_map(rendered_tags).collect(),
        _ => Vec::new(),
    }
}

#[test]
fn actual_table_markup_projects_callbacks_ids_keys_and_empty_colspan() {
    use super::{table_markup, RowInteraction, TableDefinition};
    use crate::app::components::{visible_columns, TableColumn};
    use shared::model::TableLayoutPreferencesDto;
    use std::cell::RefCell;
    use yew::prelude::*;
    let header_calls = Rc::new(RefCell::new(Vec::new()));
    let data_calls = Rc::new(RefCell::new(Vec::new()));
    let headers = header_calls.clone();
    let data = data_calls.clone();
    let mut definition = TableDefinition {
        table_id: "users".into(),
        columns: Rc::new(vec![
            TableColumn::new("username", "Name"),
            TableColumn::new("password", "Password"),
            TableColumn::new("filter", "Filter"),
        ]),
        items: Some(Rc::new(vec![Rc::new(1)])),

        row_key: Callback::from(|(_, item): (usize, Rc<i32>)| AttrValue::from(format!("row-{item}"))),

        on_sort: Callback::noop(),
        render_header_cell: Callback::from(move |column| {
            headers.borrow_mut().push(column);
            html! {column}
        }),
        render_data_cell: Callback::from(move |(_, column, _item)| {
            data.borrow_mut().push(column);
            html! {column}
        }),
    };
    let layout = TableLayoutPreferencesDto {
        column_order: vec!["filter".into(), "username".into()],
        column_visibility: [("password".into(), false)].into(),
    };
    let visible = visible_columns(&definition.columns, &layout);
    let markup = table_markup(&definition, &visible, None, Callback::noop(), String::new(), &RowInteraction::NONE);
    assert_eq!(*header_calls.borrow(), vec![2, 0]);
    assert_eq!(*data_calls.borrow(), vec![2, 0]);
    let tags = rendered_tags(&markup);
    for kind in ["th", "td"] {
        let cells: Vec<_> = tags.iter().filter(|tag| tag.tag() == kind).collect();
        let ids: Vec<_> = cells
            .iter()
            .flat_map(|tag| {
                tag.attributes.iter().filter(|(key, _)| *key == "data-column-id").map(|(_, value)| value.to_string())
            })
            .collect();
        assert_eq!(ids, vec!["filter", "username"]);
        let keys: Vec<_> = cells.iter().filter_map(|tag| tag.key.as_ref().map(ToString::to_string)).collect();
        assert_eq!(keys, ids);
    }
    assert!(tags.iter().any(|tag| tag.tag() == "tr" && tag.key.as_ref().is_some_and(|key| key.to_string() == "row-1")));
    assert!(!tags.iter().any(|tag| tag.tag() == "tr" && tag.attributes.iter().any(|(key, _)| key == "class")));

    let on_click = Callback::noop();
    let selected = AttrValue::from("row-1");
    let interactive = table_markup(
        &definition,
        &visible,
        None,
        Callback::noop(),
        String::new(),
        &RowInteraction { on_click: Some(&on_click), selected_key: Some(&selected) },
    );
    assert!(rendered_tags(&interactive).iter().any(|tag| tag.tag() == "tr"
        && tag.attributes.iter().any(|(key, value)| {
            key == "class" && value.contains("tp__table__row--clickable") && value.contains("tp__table__row--selected")
        })));

    definition.items = None;
    let empty = table_markup(&definition, &visible, None, Callback::noop(), String::new(), &RowInteraction::NONE);
    assert!(rendered_tags(&empty)
        .iter()
        .any(|tag| tag.tag() == "td" && tag.attributes.iter().any(|(key, value)| key == "colspan" && value == "2")));
}

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
fn structural_columns_render_empty_header_while_panel_lists_label() {
    use super::{table_markup, RowInteraction, TableDefinition};
    use crate::app::components::TableColumn;
    use std::cell::RefCell;
    use yew::prelude::*;

    let header_calls = Rc::new(RefCell::new(Vec::new()));
    let headers = header_calls.clone();

    let select_col = TableColumn { content: false, ..TableColumn::translated("select", "TABLE_COLUMNS.SELECT") };
    let actions_col = TableColumn { content: false, ..TableColumn::translated("actions", "TABLE_COLUMNS.ACTIONS") };
    let name_col = TableColumn::new("name", "Name");

    let definition = TableDefinition {
        table_id: "test".into(),
        columns: Rc::new(vec![select_col, actions_col, name_col]),
        items: Some(Rc::new(vec![Rc::new(1)])),
        row_key: Callback::from(|(_, item): (usize, Rc<i32>)| AttrValue::from(format!("row-{item}"))),
        on_sort: Callback::noop(),
        render_header_cell: Callback::from(move |column| {
            headers.borrow_mut().push(column);
            html! { format!("header-{column}") }
        }),
        render_data_cell: Callback::from(|_| html! {}),
    };

    let visible = vec![0, 1, 2];
    let markup = table_markup(&definition, &visible, None, Callback::noop(), String::new(), &RowInteraction::NONE);

    // Structural columns (select at 0, actions at 1) must not have their header rendered; only name at 2 renders
    assert_eq!(*header_calls.borrow(), vec![2]);

    // The panel lists the label for every column, including structural columns
    let panel_labels: Vec<AttrValue> = definition
        .columns
        .iter()
        .map(|col| {
            col.label.resolve(|key| match key {
                "TABLE_COLUMNS.SELECT" => "Selection".to_string(),
                "TABLE_COLUMNS.ACTIONS" => "Actions".to_string(),
                other => other.to_string(),
            })
        })
        .collect();
    assert_eq!(panel_labels, vec!["Selection", "Actions", "Name"]);
    for label in &panel_labels {
        assert!(!label.is_empty(), "panel label must not be empty");
    }

    // Structural <th> header nodes contain no header text
    let tags = rendered_tags(&markup);
    let th_tags: Vec<_> = tags.iter().filter(|tag| tag.tag() == "th").collect();
    assert_eq!(th_tags.len(), 3);
}
