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
    use super::{table_markup, TableDefinition};
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
    let markup = table_markup(&definition, &visible, None, Callback::noop(), String::new());
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
    definition.items = None;
    let empty = table_markup(&definition, &visible, None, Callback::noop(), String::new());
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
