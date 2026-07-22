use super::{common::CommonInputForm, ConfigInputFormState};
use yew::{component, html, Html, Properties, UseReducerHandle};

#[derive(Properties, Clone)]
pub(super) struct LibraryInputFormProps {
    pub state: UseReducerHandle<ConfigInputFormState>,
    pub allow_write: bool,
}

impl PartialEq for LibraryInputFormProps {
    fn eq(&self, _other: &Self) -> bool { false }
}

#[component]
pub(super) fn LibraryInputForm(props: &LibraryInputFormProps) -> Html {
    html! { <CommonInputForm state={props.state.clone()} allow_write={props.allow_write} show_url={false} /> }
}
