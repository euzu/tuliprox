mod components;
mod context;

use crate::app::components::{Authentication, Home, LoadingScreen, Login, RoleBasedContent};
pub use crate::app::components::{ConfirmDialog, ContentDialog};
use crate::error::Error;
use crate::hooks::IconDefinition;
use crate::model::WebConfig;
use crate::provider::IconContextProvider;
use crate::provider::ServiceContextProvider;
use crate::services::request_get;
pub use context::*;
use futures::future::join_all;
use log::error;
use serde_json::Value;
use std::collections::HashMap;
use std::rc::Rc;
use web_sys::window;
use yew::prelude::*;
use yew_hooks::{use_async_with_options, UseAsyncOptions};
use yew_i18n::I18nProvider;
use yew_router::prelude::*;

fn flatten_json(value: &Value, prefix: String, map: &mut HashMap<String, serde_json::Value>) {
    match value {
        Value::Object(obj) => {
            for (key, val) in obj {
                let new_prefix = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten_json(val, new_prefix, map);
            }
        }
        Value::Array(arr) => {
            for (i, val) in arr.iter().enumerate() {
                let new_prefix = format!("{prefix}[{i}]");
                flatten_json(val, new_prefix, map);
            }
        }
        other => {
            map.insert(prefix, other.clone());
        }
    }
}

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

#[function_component]
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
                        let result: Result<Option<Value>, Error> =
                            request_get(&url, None, None).await;
                        (lang.to_string(), result)
                    })
                    .collect::<Vec<_>>();
                let results = join_all(futures).await;
                let mut translations = HashMap::<String, serde_json::Value>::new();
                for (lang, result) in results {
                    if let Ok(i18n) = result {
                        let mut lang_translations = HashMap::<String, serde_json::Value>::new();
                        if let Some(i18n) = i18n {
                            flatten_json(&i18n, String::new(), &mut lang_translations);
                        }
                        let map: serde_json::Map<String, Value> =
                            lang_translations.into_iter().collect();
                        translations.insert(lang, Value::Object(map));
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

    if translations_state.as_ref().is_none()
        || configuration_state.as_ref().is_none()
        || icon_state.as_ref().is_none()
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
