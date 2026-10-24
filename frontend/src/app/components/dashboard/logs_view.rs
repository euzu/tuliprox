use crate::{app::components::LogConsole, i18n::use_translation};
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct LogsViewProps {
    #[prop_or(true)]
    pub active: bool,
}

#[component]
pub fn LogsView(props: &LogsViewProps) -> Html {
    let translate = use_translation();
    html! {
        <div class="tp__logs">
            <div class="tp__logs__header">
                <h1>{ translate.t("LABEL.LOGS") }</h1>
            </div>
            <div class="tp__logs__body">
                <LogConsole active={props.active} />
            </div>
        </div>
    }
}
