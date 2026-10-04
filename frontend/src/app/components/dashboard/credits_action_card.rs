use crate::{
    app::components::{ActionCard, TextButton},
    hooks::use_service_context,
    i18n::use_translation,
    services::DialogService,
};
use shared::utils::concat_path_leading_slash;
use yew::{platform::spawn_local, prelude::*};

const TMDB_API_NOTICE: &str = "This product uses the TMDB API but is not endorsed or certified by TMDB.";

fn asset_url(web_path: Option<&str>, asset: &str) -> String {
    web_path.map_or_else(|| asset.to_string(), |path| concat_path_leading_slash(path, asset))
}

fn credits_content(title: String, app_logo: String, tmdb_logo: String) -> Html {
    html! {
        <section class="tp__credits" aria-labelledby="tp-credits-title">
            <img class="tp__credits__app-logo" src={app_logo} alt="Tuliprox" />
            <h2 id="tp-credits-title">{title}</h2>
            <a href="https://www.themoviedb.org" target="_blank" rel="noopener noreferrer">
                <img class="tp__credits__tmdb-logo" src={tmdb_logo} alt="The Movie Database (TMDB)" />
            </a>
            <p lang="en" dir="ltr">{TMDB_API_NOTICE}</p>
        </section>
    }
}

#[component]
pub fn CreditsActionCard() -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let dialog = use_context::<DialogService>();

    let web_path = services.config.ui_config.web_path.as_deref();
    let app_logo = asset_url(web_path, "/assets/tuliprox-logo.svg");
    let tmdb_logo = asset_url(web_path, "/assets/tmdb-logo.svg");
    let handle_credits = {
        let app_logo = app_logo.clone();
        let tmdb_logo = tmdb_logo.clone();
        let title = translate.t("LABEL.CREDITS");
        let dialog = dialog.clone();
        Callback::from(move |_| {
            if let Some(dialog) = dialog.as_ref() {
                let content = credits_content(title.clone(), app_logo.clone(), tmdb_logo.clone());
                let dialog = dialog.clone();
                spawn_local(async move {
                    let _ = dialog.content(content, None, true).await;
                });
            }
        })
    };

    html! {
        <ActionCard
            icon="Credits"
            classname="tp__credits"
            title={translate.t("LABEL.CREDITS")}
            subtitle={translate.t("LABEL.CREDITS_CONTENT")}
        >
          <TextButton name="credits" title={translate.t("LABEL.CREDITS")} icon="Credits" onclick={handle_credits} />
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
            "tp__credits",
            "tp__credits__app-logo",
            "tp__credits__tmdb-logo",
        ] {
            assert!(tree.contains(required), "missing credit content: {required}");
        }
    }
}
