use gloo_utils::window;
use yew::prelude::*;
use yew_i18n::use_translation;
use crate::app::components::{ActionCard, TextButton};

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct VersionActionProps {
    pub version: String,
    pub build_time: String,
}

#[function_component]
pub fn VersionActionCard(props: &VersionActionProps) -> Html {
    let translate = use_translation();

    let handle_url = {
        Callback::from(move |_| {
            let _ = window().open_with_url_and_target(
                "https://github.com/euzu/tuliprox/releases",
                "_blank",
            );
        })
    };


    html! {
        <ActionCard icon="/assets/tuliprox-logo.svg" title={props.version.clone()}
        subtitle={props.build_time.clone()}>
          <TextButton name="realeases" title={translate.t("LABEL.RELEASES")} icon="Link" onclick={handle_url} />
        </ActionCard>
    }
}