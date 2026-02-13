use crate::app::components::{ActionCard, TextButton};
use crate::hooks::use_service_context;
use gloo_utils::window;
use yew::prelude::*;
use yew_i18n::use_translation;

#[function_component]
pub fn GithubActionCard() -> Html {
    let services = use_service_context();
    let translate = use_translation();

    let handle_url = {
        let mut github_link = services.config.ui_config.github.to_string();
        if github_link.is_empty() {
            github_link = String::from("https://github.com/euzu/tuliprox");
        }
        Callback::from(move |_| {
            let _ = window().open_with_url_and_target(&github_link, "_blank");
        })
    };

    html! {
        <ActionCard icon="Github" classname="tp__github" title={translate.t("LABEL.GITHUB")}
        subtitle={translate.t("LABEL.STAR_ON_GITHUB")}>
          <TextButton name="github" title={translate.t("LABEL.OPEN_GITHUB")} icon="Link" onclick={handle_url} />
        </ActionCard>
    }
}
