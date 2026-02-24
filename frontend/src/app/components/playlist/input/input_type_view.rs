use crate::{app::components::Chip, i18n::use_translation};
use shared::model::InputType;
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct InputTypeViewProps {
    pub input_type: InputType,
}

#[component]
pub fn InputTypeView(props: &InputTypeViewProps) -> Html {
    let translate = use_translation();

    let label = match props.input_type {
        InputType::M3u => "LABEL.M3U",
        InputType::Xtream => "LABEL.XTREAM",
        InputType::M3uBatch => "LABEL.M3U_BATCH",
        InputType::XtreamBatch => "LABEL.XTREAM_BATCH",
        InputType::Library => "LABEL.LIBRARY",
    };

    html! {
        <Chip label={translate.t(label)} class={props.input_type.to_string()} />
    }
}
