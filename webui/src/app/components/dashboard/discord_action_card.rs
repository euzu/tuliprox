use gloo_utils::window;
use crate::app::components::{ActionCard, TextButton};
use crate::hooks::use_service_context;
use yew::prelude::*;
use yew_i18n::use_translation;


#[function_component]
pub fn DiscordActionCard() -> Html {
    let services = use_service_context();
    let translate = use_translation();


    let handle_url = {
        let discord_link = services.config.ui_config.discord.to_string();
        Callback::from(move |_| {
            let _ = window().open_with_url_and_target(
                &discord_link,
                "_blank",
            );
        })
    };

    html! {
        <ActionCard icon="Discord" classname="discord" title={translate.t("LABEL.DISCORD")}
        subtitle={translate.t("LABEL.JOIN_ON_DISCORD")}>
          <TextButton name="discord" title={translate.t("LABEL.OPEN_DISCORD")} icon="Link" onclick={handle_url} />
        </ActionCard>
    }
}