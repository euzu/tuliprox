use crate::model::ExplorerSourceType;
use std::{rc::Rc, str::FromStr};
use web_sys::KeyboardEvent;
use yew::{Callback, UseStateHandle};

pub(super) fn build_source_type_options(
    source_types: &Option<Vec<ExplorerSourceType>>,
    default: &[ExplorerSourceType],
) -> Vec<String> {
    source_types.as_ref().map_or(default, Vec::as_slice).iter().map(ToString::to_string).collect()
}

pub(super) fn source_selection_callback(
    active_source: UseStateHandle<ExplorerSourceType>,
) -> Callback<Rc<Vec<String>>> {
    Callback::from(move |source_selection: Rc<Vec<String>>| {
        if let Some(source_type_str) = source_selection.first() {
            if let Ok(source_type) = ExplorerSourceType::from_str(source_type_str) {
                active_source.set(source_type);
            }
        }
    })
}

pub(super) fn submit_on_enter<T>(submit: Callback<T>, value: T) -> Callback<KeyboardEvent>
where
    T: Clone + 'static,
{
    Callback::from(move |event: KeyboardEvent| {
        if event.key() == "Enter" {
            submit.emit(value.clone());
        }
    })
}
