use gloo_utils::window;
use crate::app::components::{ActionCard, TextButton};
use crate::hooks::use_service_context;
use yew::prelude::*;
use yew_i18n::use_translation;


#[function_component]
pub fn DocumentationActionCard() -> Html {
    let services = use_service_context();
    let translate = use_translation();

    let handle_url = {
        let docu_link = services.config.ui_config.documentation.to_string();
        Callback::from(move |_| {
            let _ = window().open_with_url_and_target(
                &docu_link,
                "_blank",
            );
        })
    };

    html! {
        <ActionCard icon="Book" classname="tp__documentation" title={translate.t("LABEL.DOCUMENTATION")}
        subtitle={translate.t("LABEL.DOCUMENTATION_CONTENT")}>
          <TextButton name="documentation" title={translate.t("LABEL.OPEN_DOCUMENTATION")} icon="Link" onclick={handle_url} />
        </ActionCard>
    }
}