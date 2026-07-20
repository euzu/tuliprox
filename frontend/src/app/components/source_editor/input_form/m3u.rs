use super::{common::CommonInputForm, ConfigInputFormState};
use yew::{component, html, Html, Properties, UseReducerHandle};

#[derive(Properties, Clone, PartialEq)]
pub(super) struct M3uInputFormProps {
    pub state: UseReducerHandle<ConfigInputFormState>,
    pub allow_write: bool,
}

#[component]
pub(super) fn M3uInputForm(props: &M3uInputFormProps) -> Html {
    html! {
        <CommonInputForm state={props.state.clone()} allow_write={props.allow_write}
            connection={true} cache_duration={true} />
    }
}
