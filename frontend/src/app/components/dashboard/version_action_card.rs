use crate::app::components::{ActionCard, TextButton};
use crate::hooks::use_service_context;
use gloo_utils::window;
use shared::utils::concat_path_leading_slash;
use yew::prelude::*;
use yew_i18n::use_translation;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct VersionActionProps {
    pub version: String,
    pub build_time: String,
}

#[function_component]
pub fn VersionActionCard(props: &VersionActionProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();

    let handle_url = {
        Callback::from(move |_| {
            let _ = window()
                .open_with_url_and_target("https://github.com/euzu/tuliprox/releases", "_blank");
        })
    };

    let logo_url = {
        let mut url = "/assets/tuliprox-logo.svg".to_owned();
        if let Some(web_path) = services.config.ui_config.web_path.as_ref() {
            url = concat_path_leading_slash(web_path, &url);
        }
        url
    };

    html! {
        <ActionCard icon={logo_url} title={props.version.clone()}
        subtitle={props.build_time.clone()}>
          <TextButton name="realeases" title={translate.t("LABEL.RELEASES")} icon="Link" onclick={handle_url} />
        </ActionCard>
    }
}
