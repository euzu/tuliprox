mod components;

use std::collections::HashMap;
use std::rc::Rc;
use futures::future::join_all;
use log::error;
use serde_json::Value;
use crate::provider::icon_context_provider::IconContextProvider;
use crate::provider::service_context_provider::ServiceContextProvider;
use yew_i18n::I18nProvider;
use yew::prelude::*;
use yew_hooks::{use_async_with_options, UseAsyncOptions};
use yew_router::prelude::*;
use crate::app::components::{Authentication, Home, Login, Preferences};
use crate::error::Error;
use crate::hooks::IconDefinition;
use crate::model::WebConfig;
use crate::services::request_get;

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
    #[at("/preferecnes")]
    Preferences,
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
        AppRoute::Preferences => html! {<Preferences />},
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
        use_async_with_options::<_, (), Error>(async move {
            let futures = languages.iter()
                .map(|lang| async move {
                    let url = format!("assets/i18n/{lang}.json");
                    let result: Result<Value, Error> = request_get(&url).await;
                    (lang.to_string(), result)
                })
                .collect::<Vec<_>>();
            let results = join_all(futures).await;
            let mut translations = HashMap::<String, serde_json::Value>::new();
            for (lang, result) in results {
                if let Ok(i18n) = result {
                    let mut lang_translations = HashMap::<String, serde_json::Value>::new();
                    flatten_json(&i18n, String::new(), &mut lang_translations);
                    let map: serde_json::Map<String, Value> = lang_translations.into_iter().collect();
                    translations.insert(lang, Value::Object(map));
                }
            }
            trans_state.set(Some(translations));
            Ok(())
        }, UseAsyncOptions::enable_auto());
    }

    {
        let config_state = configuration_state.clone();
        use_async_with_options::<_, (), Error>(async move {
            match request_get("config.json").await {
                Ok(cfg) => config_state.set(Some(cfg)),
                Err(err) => error!("Failed to load config {err}"),
            }
            Ok(())
        }, UseAsyncOptions::enable_auto());
    }

    {
        let icon_state = icon_state.clone();
        use_async_with_options::<_, (), Error>(async move {
            match request_get("assets/icons.json").await {
                Ok(icons) => icon_state.set(Some(icons)),
                Err(err) => error!("Failed to load icons {err}"),
            }
            Ok(())
        }, UseAsyncOptions::enable_auto());
    }

    if translations_state.as_ref().is_none() || configuration_state.as_ref().is_none()
    || icon_state.as_ref().is_none(){
        return html! { <div>{ "Loading..." }</div> };
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
                            <Switch<AppRoute> render={switch} />
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