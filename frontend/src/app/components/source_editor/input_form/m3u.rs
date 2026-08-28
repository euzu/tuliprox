use super::{common::CommonInputForm, ConfigInputFormState};
use yew::{component, html, Html, Properties, UseReducerHandle};

#[derive(Properties, Clone)]
pub(super) struct M3uInputFormProps {
    pub state: UseReducerHandle<ConfigInputFormState>,
    pub allow_write: bool,
}

impl PartialEq for M3uInputFormProps {
    fn eq(&self, _other: &Self) -> bool {
        false
    }
}

#[component]
pub(super) fn M3uInputForm(props: &M3uInputFormProps) -> Html {
    html! {
        <CommonInputForm state={props.state.clone()} allow_write={props.allow_write}
            connection={true} cache_duration={true} />
    }
}
