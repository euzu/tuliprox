mod components;
mod context;

pub use crate::app::components::{ConfirmDialog, ContentDialog};
use crate::{
    app::components::{Authentication, Home, LoadingScreen, Login, RoleBasedContent},
    error::Error,
    hooks::IconDefinition,
    i18n::{I18nProvider, LanguageInfo, LanguageManifest, LanguageState},
    model::WebConfig,
    provider::{IconContextProvider, ServiceContextProvider},
    services::request_get,
    utils::{get_local_storage_item, set_local_storage_item},
};
pub use context::*;
use futures::future::join_all;
use log::error;
use serde_json::Value;
use std::{collections::HashMap, rc::Rc};
use web_sys::window;
use yew::prelude::*;
use yew_hooks::{use_async_with_options, UseAsyncOptions};
use yew_router::prelude::*;

const STATIC_ASSET_VERSION: &str = env!("CARGO_PKG_VERSION");

/// App routes
#[derive(Routable, Debug, Clone, PartialEq, Eq)]
pub enum AppRoute {
    #[at("/login")]
    Login,
    #[at("/")]
    Home,
    #[not_found]
    #[at("/404")]
    NotFound,
}

pub fn switch(route: AppRoute) -> Html {
    match route {
        AppRoute::Login => html! {<Login />},
        AppRoute::Home => html! {<Home />},
        AppRoute::NotFound => html! { "Page not found" },
    }
}

fn versioned_static_asset_url(path: &str) -> String {
    let separator = if path.contains('?') { '&' } else { '?' };
    format!("{path}{separator}v={STATIC_ASSET_VERSION}")
}

fn versioned_config_url() -> String { versioned_static_asset_url("config.json") }

fn router_basename(config: &WebConfig) -> Option<String> {
    config
        .web_path
        .as_ref()
        .map(|path| path.trim())
        .filter(|path| !path.is_empty() && *path != "/")
        .map(ToOwned::to_owned)
}

fn resolve_effective_language(languages: &[LanguageInfo], active_language: &str) -> String {
    if languages.iter().any(|language| language.code == active_language) {
        active_language.to_string()
    } else {
        languages.first().map_or_else(|| "en".to_string(), |language| language.code.clone())
    }
}

#[component]
pub fn App() -> Html {
    let translations_state = use_state(|| None::<HashMap<String, Value>>);
    let languages_state = use_state(|| None::<Rc<Vec<LanguageInfo>>>);
    let configuration_state = use_state(|| None);
    let icon_state = use_state(|| None);
    let active_language = use_state(|| get_local_storage_item("tp_language").unwrap_or_else(|| "en".to_string()));

    {
        let trans_state = translations_state.clone();
        let langs_state = languages_state.clone();
        use_async_with_options::<_, (), Error>(
            async move {
                let manifest_url = versioned_static_asset_url("assets/i18n/index.json");
                let mut languages = match request_get::<LanguageManifest>(&manifest_url, None, None).await {
                    Ok(Some(manifest)) => manifest.languages,
                    _ => Vec::new(),
                };
                if languages.is_empty() {
                    languages.push(LanguageInfo {
                        code: "en".to_string(),
                        label: "English".to_string(),
                        dir: "ltr".to_string(),
                    });
                }

                let futures = languages
                    .iter()
                    .map(|lang| {
                        let code = lang.code.clone();
                        async move {
                            let url = versioned_static_asset_url(&format!("assets/i18n/{code}.json"));
                            let result: Result<Option<Value>, Error> = request_get(&url, None, None).await;
                            (code, result)
                        }
                    })
                    .collect::<Vec<_>>();
                let results = join_all(futures).await;
                let mut translations = HashMap::<String, serde_json::Value>::new();
                for (lang, result) in results {
                    if let Ok(i18n) = result {
                        translations.insert(lang, i18n.unwrap_or_else(|| Value::Object(serde_json::Map::new())));
                    }
                }
                trans_state.set(Some(translations));
                langs_state.set(Some(Rc::new(languages)));
                Ok(())
            },
            UseAsyncOptions::enable_auto(),
        );
    }

    {
        let active = (*active_language).clone();
        let langs = (*languages_state).clone();
        use_effect_with((active, langs), move |(active, langs)| {
            if let Some(languages) = langs.as_ref() {
                let effective_language = resolve_effective_language(languages, active);
                let dir = languages
                    .iter()
                    .find(|l| l.code == effective_language)
                    .map_or_else(|| "ltr".to_string(), |l| l.dir.clone());
                if let Some(root) = window().and_then(|w| w.document()).and_then(|d| d.document_element()) {
                    let _ = root.set_attribute("dir", &dir);
                    let _ = root.set_attribute("lang", &effective_language);
                }
            }
            || ()
        });
    }

    {
        let config_state = configuration_state.clone();
        use_async_with_options::<_, (), Error>(
            async move {
                let config_url = versioned_config_url();
                match request_get::<WebConfig>(&config_url, None, None).await {
                    Ok(Some(cfg)) => {
                        if let Some(tab_title) = cfg.tab_title.as_deref() {
                            if let Some(win) = window() {
                                if let Some(doc) = win.document() {
                                    doc.set_title(tab_title);
                                }
                            }
                        }
                        config_state.set(Some(cfg));
                    }
                    Ok(None) => config_state.set(Some(WebConfig::default())),
                    Err(err) => {
                        error!("Failed to load config {err}");
                        // Fallback: render app with defaults instead of spinning forever
                        #[allow(clippy::default_trait_access)]
                        config_state.set(Some(WebConfig::default()));
                    }
                }
                Ok(())
            },
            UseAsyncOptions::enable_auto(),
        );
    }

    {
        let icon_state = icon_state.clone();
        use_async_with_options::<_, (), Error>(
            async move {
                let icons_url = versioned_static_asset_url("assets/icons.json");
                match request_get(&icons_url, None, None).await {
                    Ok(Some(icons)) => icon_state.set(Some(icons)),
                    Ok(None) => icon_state.set(Some(Vec::new())),
                    Err(err) => {
                        // Fallback: proceed with an empty icon set
                        icon_state.set(Some(Vec::new()));
                        error!("Failed to load icons {err}");
                    }
                }
                Ok(())
            },
            UseAsyncOptions::enable_auto(),
        );
    }

    if translations_state.as_ref().is_none()
        || languages_state.as_ref().is_none()
        || configuration_state.as_ref().is_none()
        || icon_state.as_ref().is_none()
    {
        return html! { <LoadingScreen/> };
    }
    let transl = translations_state.as_ref().unwrap();
    let languages = languages_state.as_ref().unwrap().clone();
    let config: &WebConfig = configuration_state.as_ref().unwrap();
    let icons: &Vec<Rc<IconDefinition>> = icon_state.as_ref().unwrap();

    let effective_language = resolve_effective_language(&languages, &active_language);

    let supported_languages: Vec<String> = languages.iter().map(|l| l.code.clone()).collect();

    let on_language_change = {
        let active_language = active_language.clone();
        Callback::from(move |code: String| {
            set_local_storage_item("tp_language", &code);
            active_language.set(code);
        })
    };

    let language_state = LanguageState {
        languages: languages.clone(),
        active: effective_language.clone(),
        on_change: on_language_change,
    };
    let basename = router_basename(config);

    html! {
        <BrowserRouter basename={basename}>
            <ServiceContextProvider config={config.clone()}>
                <IconContextProvider icons={icons.clone()}>
                    <ContextProvider<LanguageState> context={language_state}>
                        <I18nProvider
                            supported_languages={supported_languages}
                            active_language={effective_language}
                            translations={transl.clone()}
                        >
                            <Authentication>
                                <RoleBasedContent />
                            </Authentication>
                        </I18nProvider>
                    </ContextProvider<LanguageState>>
                </IconContextProvider>
            </ServiceContextProvider>
        </BrowserRouter>
    }
}

#[derive(Clone, PartialEq)]
pub(in crate::app) struct CardContext {
    pub custom_class: UseStateHandle<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versioned_static_asset_url_appends_release_version() {
        assert_eq!(
            versioned_static_asset_url("assets/i18n/en.json"),
            format!("assets/i18n/en.json?v={}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn versioned_static_asset_url_keeps_existing_query_params() {
        assert_eq!(
            versioned_static_asset_url("assets/i18n/en.json?lang=en"),
            format!("assets/i18n/en.json?lang=en&v={}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn router_basename_uses_non_root_web_path() {
        let config = WebConfig { web_path: Some("/tuli".to_string()), ..WebConfig::default() };
        assert_eq!(router_basename(&config), Some("/tuli".to_string()));
    }

    #[test]
    fn router_basename_ignores_root_path() {
        let config = WebConfig { web_path: Some("/".to_string()), ..WebConfig::default() };
        assert_eq!(router_basename(&config), None);
    }

    #[test]
    fn versioned_config_url_uses_release_version() {
        assert_eq!(versioned_config_url(), format!("config.json?v={}", env!("CARGO_PKG_VERSION")));
    }
}
