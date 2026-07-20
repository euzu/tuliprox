use crate::app::components::{DropDownOption, DropDownSelection};
use std::str::FromStr;
use yew::Html;

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

#[cfg(test)]
mod tests {
    use super::{selection_first, selection_first_owned, selection_parse_first, selection_vec};
    use crate::app::components::DropDownSelection;

    #[test]
    fn selection_helpers_handle_empty() {
        let selection = DropDownSelection::Empty;

        assert_eq!(selection_first(&selection), None);
        assert_eq!(selection_parse_first::<u8>(&selection), None);
        assert_eq!(selection_first_owned(selection.clone()), None);
        assert!(selection_vec(selection).is_empty());
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
}
