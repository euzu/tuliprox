use crate::{
    app::components::{ActionCard, TextButton},
    hooks::use_service_context,
    i18n::use_translation,
};
use gloo_utils::window;
use shared::utils::concat_path_leading_slash;
use yew::prelude::*;

fn asset_url(web_path: Option<&str>, asset: &str) -> String {
    web_path.map_or_else(|| asset.to_string(), |path| concat_path_leading_slash(path, asset))
}

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct VersionActionProps {
    pub version: String,
    pub build_time: String,
}

#[component]
pub fn VersionActionCard(props: &VersionActionProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();

    let handle_url = {
        let services = services.clone();
        Callback::from(move |_| {
            let releases_link = services.config.ui_config.releases.clone();
            let _ = window().open_with_url_and_target(releases_link.as_ref(), "_blank");
        })
    };

    let web_path = services.config.ui_config.web_path.as_deref();
    let logo_url = asset_url(web_path, "/assets/tuliprox-logo.svg");

    html! {
        <ActionCard icon={logo_url} title={props.version.clone()}
        subtitle={props.build_time.clone()}>
          <TextButton name="realeases" title={translate.t("LABEL.RELEASES")} icon="Link" onclick={handle_url} />
        </ActionCard>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_assets_respect_the_configured_web_path() {
        assert_eq!(asset_url(None, "/assets/tuliprox-logo.svg"), "/assets/tuliprox-logo.svg");
        assert_eq!(asset_url(Some("/gateway"), "/assets/tuliprox-logo.svg"), "/gateway/assets/tuliprox-logo.svg");
    }
}
