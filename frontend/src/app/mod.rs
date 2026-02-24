mod components;
mod context;

pub use crate::app::components::{ConfirmDialog, ContentDialog};
use crate::{
    app::components::{Authentication, Home, LoadingScreen, Login, RoleBasedContent},
    error::Error,
    hooks::IconDefinition,
    i18n::I18nProvider,
    model::WebConfig,
    provider::{IconContextProvider, ServiceContextProvider},
    services::request_get,
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

#[component]
pub fn App() -> Html {
    let supported_languages = vec!["en"];
    let translations_state = use_state(|| None);
    let configuration_state = use_state(|| None);
    let icon_state = use_state(|| None);

    {
        let trans_state = translations_state.clone();
        let languages = supported_languages.clone();
        use_async_with_options::<_, (), Error>(
            async move {
                let futures = languages
                    .iter()
                    .map(|lang| async move {
                        let url = format!("assets/i18n/{lang}.json");
                        let result: Result<Option<Value>, Error> = request_get(&url, None, None).await;
                        (lang.to_string(), result)
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
                Ok(())
            },
            UseAsyncOptions::enable_auto(),
        );
    }

    {
        let config_state = configuration_state.clone();
        use_async_with_options::<_, (), Error>(
            async move {
                match request_get::<WebConfig>("config.json", None, None).await {
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
                match request_get("assets/icons.json", None, None).await {
                    Ok(Some(icons)) => icon_state.set(Some(icons)),
                    Ok(None) => icon_state.set(Some(Vec::new())),
                    Err(err) => {
                        // Fallback: proceed with an empty icon set
                        icon_state.set(Some(Vec::new()));
                        error!("Failed to load icons {err}")
                    }
                }
                Ok(())
            },
            UseAsyncOptions::enable_auto(),
        );
    }

    if translations_state.as_ref().is_none() || configuration_state.as_ref().is_none() || icon_state.as_ref().is_none()
    {
        return html! { <LoadingScreen/> };
    }
    let transl = translations_state.as_ref().unwrap();
    let config: &WebConfig = configuration_state.as_ref().unwrap();
    let icons: &Vec<Rc<IconDefinition>> = icon_state.as_ref().unwrap();

    html! {
        <BrowserRouter>
            <ServiceContextProvider config={config.clone()}>
                <IconContextProvider icons={icons.clone()}>
                    <I18nProvider supported_languages={supported_languages} translations={transl.clone()}>
                        <Authentication>
                            <RoleBasedContent />
                        </Authentication>
                    </I18nProvider>
                </IconContextProvider>
            </ServiceContextProvider>
        </BrowserRouter>
    }
}

#[derive(Clone, PartialEq)]
pub(in crate::app) struct CardContext {
    pub custom_class: UseStateHandle<String>,
}
