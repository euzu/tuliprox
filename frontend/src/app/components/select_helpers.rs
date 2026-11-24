use crate::app::components::{DropDownOption, DropDownSelection};
use std::str::FromStr;
use yew::{html, virtual_dom::VNode, Html};

pub(crate) fn build_options<T, I, F>(values: I, selected: &T, label: F) -> Vec<DropDownOption>
where
    T: PartialEq + ToString,
    I: IntoIterator<Item = T>,
    F: Fn(&T) -> Html,
{
    values
        .into_iter()
        .map(|value| {
            let is_selected = &value == selected;
            DropDownOption { id: value.to_string(), label: label(&value), selected: is_selected }
        })
        .collect()
}

pub(crate) fn selection_first(selection: &DropDownSelection) -> Option<&str> {
    match selection {
        DropDownSelection::Empty => None,
        DropDownSelection::Single(value) => Some(value.as_str()),
        DropDownSelection::Multi(values) => values.first().map(String::as_str),
    }
}

pub(crate) fn selection_first_owned(selection: DropDownSelection) -> Option<String> {
    match selection {
        DropDownSelection::Empty => None,
        DropDownSelection::Single(value) => Some(value),
        DropDownSelection::Multi(values) => values.into_iter().next(),
    }
}

pub(crate) fn selection_parse_first<T>(selection: &DropDownSelection) -> Option<T>
where
    T: FromStr,
{
    selection_first(selection).and_then(|value| value.parse::<T>().ok())
}

pub(crate) fn selection_vec(selection: DropDownSelection) -> Vec<String> {
    match selection {
        DropDownSelection::Empty => Vec::new(),
        DropDownSelection::Single(value) => vec![value],
        DropDownSelection::Multi(values) => values,
    }
}

pub(crate) fn build_tri_state_options(
    current: Option<bool>,
    inherit_label: &str,
    enabled_label: &str,
    disabled_label: &str,
) -> Vec<DropDownOption> {
    vec![
        DropDownOption { id: String::new(), label: html! { inherit_label }, selected: current.is_none() },
        DropDownOption { id: "true".to_string(), label: html! { enabled_label }, selected: current == Some(true) },
        DropDownOption { id: "false".to_string(), label: html! { disabled_label }, selected: current == Some(false) },
    ]
}

pub(crate) fn parse_tri_state_id(id: Option<&str>) -> Option<bool> {
    match id {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    }
}

pub(crate) fn parse_tri_state_selection(selection: &DropDownSelection) -> Option<bool> {
    parse_tri_state_id(selection_first(selection))
}

#[cfg(test)]
mod tests {
    use super::{
        build_tri_state_options, parse_tri_state_selection, selection_first, selection_first_owned,
        selection_parse_first, selection_vec,
    };
    use crate::app::components::DropDownSelection;

    #[test]
    fn selection_helpers_handle_empty() {
        let selection = DropDownSelection::Empty;

        assert_eq!(selection_first(&selection), None);
        assert_eq!(selection_parse_first::<u8>(&selection), None);
        assert_eq!(selection_first_owned(selection.clone()), None);
        assert_eq!(selection_vec(selection), [] as [std::string::String; 0]);
    }

    #[test]
    fn selection_helpers_handle_single() {
        let selection = DropDownSelection::Single("42".to_string());

        assert_eq!(selection_first(&selection), Some("42"));
        assert_eq!(selection_parse_first::<u8>(&selection), Some(42));
        assert_eq!(selection_first_owned(selection.clone()), Some("42".to_string()));
        assert_eq!(selection_vec(selection), vec!["42".to_string()]);
    }

    #[test]
    fn selection_helpers_handle_multi() {
        let selection = DropDownSelection::Multi(vec!["7".to_string(), "9".to_string()]);

        assert_eq!(selection_first(&selection), Some("7"));
        assert_eq!(selection_parse_first::<u8>(&selection), Some(7));
        assert_eq!(selection_first_owned(selection.clone()), Some("7".to_string()));
        assert_eq!(selection_vec(selection), vec!["7".to_string(), "9".to_string()]);
    }

    #[test]
    fn tri_state_options_handles_all_variants() {
        let none_opts = build_tri_state_options(None, "Inherit", "Enabled", "Disabled");
        assert_eq!(none_opts.len(), 3);
        assert!(none_opts[0].selected);
        assert!(!none_opts[1].selected);
        assert!(!none_opts[2].selected);

        let true_opts = build_tri_state_options(Some(true), "Inherit", "Enabled", "Disabled");
        assert!(!true_opts[0].selected);
        assert!(true_opts[1].selected);
        assert!(!true_opts[2].selected);

        let false_opts = build_tri_state_options(Some(false), "Inherit", "Enabled", "Disabled");
        assert!(!false_opts[0].selected);
        assert!(!false_opts[1].selected);
        assert!(false_opts[2].selected);
    }

    #[test]
    fn tri_state_selection_parsing() {
        assert_eq!(parse_tri_state_selection(&DropDownSelection::Empty), None);
        assert_eq!(parse_tri_state_selection(&DropDownSelection::Single(String::new())), None);
        assert_eq!(parse_tri_state_selection(&DropDownSelection::Single("true".to_string())), Some(true));
        assert_eq!(parse_tri_state_selection(&DropDownSelection::Single("false".to_string())), Some(false));
        assert_eq!(parse_tri_state_selection(&DropDownSelection::Single("other".to_string())), None);
    }
}

/// Plain text of the markup of an option label, for filtering. Components in the label contribute no text.
pub(crate) fn html_text(node: &Html) -> String {
    fn collect(node: &VNode, out: &mut String) {
        match node {
            VNode::VText(text) => out.push_str(&text.text),
            VNode::VTag(tag) => {
                if let Some(children) = tag.children() {
                    collect(children, out);
                }
            }
            VNode::VList(list) => list.iter().for_each(|child| collect(child, out)),
            _ => {}
        }
    }
    let mut text = String::new();
    collect(node, &mut text);
    text
}

#[cfg(test)]
mod label_tests {
    use super::html_text;
    use yew::html;

    #[test]
    fn html_text_reads_text_of_nested_markup() {
        assert_eq!(html_text(&html! { "Plain" }), "Plain");
        assert_eq!(html_text(&html! { <span>{ "Nested " }<b>{ "label" }</b></span> }), "Nested label");
        assert_eq!(html_text(&html! { <>{ "a" }{ "b" }</> }), "ab");
    }
}
