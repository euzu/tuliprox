use super::{common::CommonInputForm, ConfigInputFormState};
use yew::{component, html, Html, Properties, UseReducerHandle};

#[derive(Properties, Clone, PartialEq)]
pub(super) struct LibraryInputFormProps {
    pub state: UseReducerHandle<ConfigInputFormState>,
    pub allow_write: bool,
}

#[component]
pub(super) fn LibraryInputForm(props: &LibraryInputFormProps) -> Html {
    html! { <CommonInputForm state={props.state.clone()} allow_write={props.allow_write} show_url={false} /> }
}
