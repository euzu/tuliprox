use crate::{
    app::components::{ActionCard, TextButton},
    hooks::use_service_context,
    i18n::use_translation,
    services::DialogService,
};
use gloo_utils::window;
use shared::utils::concat_path_leading_slash;
use yew::{platform::spawn_local, prelude::*};

const TMDB_API_NOTICE: &str = "This product uses the TMDB API but is not endorsed or certified by TMDB.";

fn asset_url(web_path: Option<&str>, asset: &str) -> String {
    web_path.map_or_else(|| asset.to_string(), |path| concat_path_leading_slash(path, asset))
}

fn credits_content(title: String, app_logo: String, tmdb_logo: String) -> Html {
    html! {
        <section class="tp__version-credits" aria-labelledby="tp-credits-title">
            <img class="tp__version-credits__app-logo" src={app_logo} alt="Tuliprox" />
            <h2 id="tp-credits-title">{title}</h2>
            <a href="https://www.themoviedb.org" target="_blank" rel="noopener noreferrer">
                <img class="tp__version-credits__tmdb-logo" src={tmdb_logo} alt="The Movie Database (TMDB)" />
            </a>
            <p lang="en" dir="ltr">{TMDB_API_NOTICE}</p>
        </section>
    }
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
    let dialog = use_context::<DialogService>().expect("Dialog service not found");

    let handle_url = {
        let services = services.clone();
        Callback::from(move |_| {
            let releases_link = services.config.ui_config.releases.clone();
            let _ = window().open_with_url_and_target(releases_link.as_ref(), "_blank");
        })
    };

    let web_path = services.config.ui_config.web_path.as_deref();
    let logo_url = asset_url(web_path, "/assets/tuliprox-logo.svg");
    let handle_credits = {
        let app_logo = logo_url.clone();
        let tmdb_logo = asset_url(web_path, "/assets/tmdb-logo.svg");
        let title = translate.t("LABEL.CREDITS");
        Callback::from(move |_| {
            let content = credits_content(title.clone(), app_logo.clone(), tmdb_logo.clone());
            let dialog = dialog.clone();
            spawn_local(async move {
                let _ = dialog.content(content, None, true).await;
            });
        })
    };

    html! {
        <ActionCard icon={logo_url} title={props.version.clone()}
        subtitle={props.build_time.clone()}>
          <TextButton name="realeases" title={translate.t("LABEL.RELEASES")} icon="Link" onclick={handle_url} />
          <TextButton name="credits" title={translate.t("LABEL.CREDITS")} icon="QuestionMark" onclick={handle_credits} />
        </ActionCard>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credits_assets_respect_the_configured_web_path() {
        assert_eq!(asset_url(None, "/assets/tmdb-logo.svg"), "/assets/tmdb-logo.svg");
        assert_eq!(asset_url(Some("/gateway"), "/assets/tmdb-logo.svg"), "/gateway/assets/tmdb-logo.svg");
        assert_eq!(asset_url(Some("/gateway"), "/assets/tuliprox-logo.svg"), "/gateway/assets/tuliprox-logo.svg");
    }

    #[test]
    fn credits_content_includes_official_logo_link_and_exact_notice() {
        let content =
            credits_content("Credits".into(), "/assets/tuliprox-logo.svg".into(), "/assets/tmdb-logo.svg".into());
        let tree = format!("{content:?}");
        for required in [
            TMDB_API_NOTICE,
            "/assets/tmdb-logo.svg",
            "/assets/tuliprox-logo.svg",
            "https://www.themoviedb.org",
            "tp-credits-title",
        ] {
            assert!(tree.contains(required), "missing credit content: {required}");
        }
    }
}
