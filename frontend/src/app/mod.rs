mod components;
mod context;

pub use crate::app::components::{ConfirmDialog, ContentDialog};
use crate::{
    app::components::{Authentication, Home, LoadingScreen, Login, RoleBasedContent},
    error::Error,
    hooks::IconDefinition,
    i18n::{I18nProvider, I18nProviderProps, LanguageInfo, LanguageManifest, LanguageState},
    model::WebConfig,
    provider::{IconContextProvider, ServiceContextProvider, UserSettingsProvider},
    services::request_get,
    utils::{get_local_storage_item, set_local_storage_item},
};
pub(crate) use components::{map_sources_to_playlist_rows, InputRow, TableColumnsPanel, TablePanelSpec};
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

fn app_providers(
    config: WebConfig,
    icons: Vec<Rc<IconDefinition>>,
    language: LanguageState,
    translations: I18nProviderProps,
    children: Html,
) -> Html {
    html! {
        <ServiceContextProvider config={config}>
            <IconContextProvider icons={icons}>
                <ContextProvider<LanguageState> context={language}>
                    <I18nProvider supported_languages={translations.supported_languages}
                        active_language={translations.active_language} translations={translations.translations}>
                        <UserSettingsProvider>{children}</UserSettingsProvider>
                    </I18nProvider>
                </ContextProvider<LanguageState>>
            </IconContextProvider>
        </ServiceContextProvider>
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
            {app_providers(
                config.clone(), icons.clone(), language_state,
                I18nProviderProps { supported_languages, active_language: effective_language,
                    translations: transl.clone(), children: Children::default() },
                html! { <Authentication><RoleBasedContent /></Authentication> },
            )}
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

#[cfg(all(test, target_arch = "wasm32"))]
mod browser_tests {
    use super::*;
    use crate::{
        app::components::{TableColumn, TableShell},
        hooks::use_service_context,
        model::ApiConfig,
        services::{get_token, set_token},
    };
    use gloo_timers::future::TimeoutFuture;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};
    use web_sys::{Element, Event};
    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen(inline_js = r#"
    let originalContextFetch;
    export function mockContextSettings(token) {
      originalContextFetch = window.fetch;
      window.fetch = async request => {
        let body;
        if (request.url.endsWith('/auth/refresh')) body = {username:'no_auth',token};
        else if (request.url.endsWith('/me/settings')) body = {shared:true,preferences:{web_ui:{tables:{}}},section_etags:{users:'"'+'1'.repeat(64)+'"'}};
        else if (request.url.endsWith('/me/settings/tables/users')) body = {column_order:[],column_visibility:{}};
        else throw new Error('Unexpected request: '+request.url);
        return new Response(JSON.stringify(body),{status:200,headers:{'Content-Type':'application/json',ETag:'"'+'1'.repeat(64)+'"'}});
      };
    }
    export function restoreContextSettings() { window.fetch = originalContextFetch; }
    "#)]
    extern "C" {
        #[wasm_bindgen(js_name=mockContextSettings)]
        fn start_mock(token: &str);
        #[wasm_bindgen(js_name=restoreContextSettings)]
        fn stop_mock();
    }
    struct MockGuard {
        token: Option<String>,
        language: Option<String>,
    }
    impl Drop for MockGuard {
        fn drop(&mut self) {
            stop_mock();
            set_token(self.token.as_deref());
            if let Some(language) = &self.language {
                set_local_storage_item("tp_language", language);
            } else {
                crate::utils::remove_local_storage_item("tp_language");
            }
        }
    }

    #[component]
    fn SettingsTable() -> Html {
        let services = use_service_context();
        {
            let auth = services.auth.clone();
            use_effect_with((), move |()| {
                let request_auth = auth.clone();
                yew::platform::spawn_local(async move {
                    let _ = request_auth.refresh().await;
                });
                move || auth.logout()
            });
        }
        html! {<TableShell table_id="users" columns={Rc::new(vec![TableColumn::translated("username","LABEL.USERNAME"),TableColumn::translated("password","LABEL.PASSWORD")])}>
            <table><thead style="height: 42px"><tr><th>{"Username"}</th><th>{"Password"}</th></tr></thead><tbody><tr><td>{"Alice"}</td><td>{"secret"}</td></tr></tbody></table>
        </TableShell>}
    }

    #[derive(Properties, PartialEq)]
    struct SettingsConsumerProps {
        renders: Rc<std::cell::Cell<usize>>,
    }

    #[component]
    fn SettingsConsumer(props: &SettingsConsumerProps) -> Html {
        let _context = crate::provider::use_user_settings();
        props.renders.set(props.renders.get() + 1);
        html! {}
    }

    #[derive(Properties, PartialEq)]
    struct HarnessProps {
        renders: Rc<std::cell::Cell<usize>>,
    }

    #[component]
    fn Harness(props: &HarnessProps) -> Html {
        let active = use_state(|| "en".to_owned());
        let assets = use_memo((), |()| -> Result<_, serde_json::Error> {
            let icons: Vec<Rc<IconDefinition>> = serde_json::from_str(include_str!("../../public/assets/icons.json"))?;
            let translations = HashMap::from([
                ("en".into(), serde_json::from_str(include_str!("../../public/assets/i18n/en.json"))?),
                ("es".into(), serde_json::from_str(include_str!("../../public/assets/i18n/es.json"))?),
            ]);
            Ok((icons, translations))
        });
        let Ok((icons, translations)) = &*assets else {
            return html! {<p>{"Invalid test assets"}</p>};
        };
        let change = {
            let active = active.clone();
            Callback::from(move |_| active.set("es".into()))
        };
        let language = LanguageState {
            languages: Rc::new(vec![
                LanguageInfo { code: "en".into(), label: "English".into(), dir: "ltr".into() },
                LanguageInfo { code: "es".into(), label: "Español".into(), dir: "ltr".into() },
            ]),
            active: (*active).clone(),
            on_change: Callback::noop(),
        };
        html! {<>
            <button id="context-test-language" onclick={change}>{"Español"}</button>
            {app_providers(
                WebConfig {api:ApiConfig {auth_url:"/context-test/auth".into(),api_url:String::new()},..Default::default()},
                icons.clone(),language,I18nProviderProps {supported_languages:vec!["en".into(),"es".into()],active_language:(*active).clone(),translations:translations.clone(),children:Children::default()},
                html!{<><SettingsConsumer renders={props.renders.clone()}/><SettingsTable/></>},
            )}
        </>}
    }
    async fn settle() {
        TimeoutFuture::new(0).await;
        TimeoutFuture::new(0).await;
    }
    fn find(selector: &str) -> Result<Element, JsValue> {
        gloo_utils::document().query_selector(selector)?.ok_or_else(|| JsValue::from_str(selector))
    }
    fn click(element: &Element) -> Result<(), JsValue> {
        element.dispatch_event(&Event::new("click")?)?;
        Ok(())
    }

    #[wasm_bindgen_test(async)]
    async fn app_provider_tree_opens_localized_panel_with_icons_and_keeps_language_context() -> Result<(), JsValue> {
        let _guard = MockGuard { token: get_token(), language: get_local_storage_item("tp_language") };
        set_token(None);
        start_mock(shared::model::TOKEN_NO_AUTH);
        let document = gloo_utils::document();
        let root = document.create_element("div")?;
        document.body().ok_or_else(|| JsValue::from_str("missing body"))?.append_child(&root)?;
        let renders = Rc::new(std::cell::Cell::new(0));
        let handle =
            yew::Renderer::<Harness>::with_root_and_props(root.clone(), HarnessProps { renders: renders.clone() })
                .render();
        for _ in 0..20 {
            TimeoutFuture::new(5).await;
            if root.query_selector(".tp__table-shell__columns:not([disabled])")?.is_some() {
                break;
            }
        }
        let initial_renders = renders.get();
        web_sys::window().ok_or_else(|| JsValue::from_str("missing window"))?.dispatch_event(&Event::new("focus")?)?;
        TimeoutFuture::new(40).await;
        assert_eq!(renders.get(), initial_renders, "unchanged settings must not notify consumers");
        let button = find(".tp__table-shell__columns:not([disabled])")?;
        assert_eq!(button.get_attribute("aria-label").as_deref(), Some("Customize table columns"));
        let icon = button
            .query_selector("svg[data-testid='Columns']")?
            .ok_or_else(|| JsValue::from_str("missing columns icon"))?;
        assert_eq!(icon.next_element_sibling().and_then(|label| label.text_content()).as_deref(), Some("Columns"));
        let rail = find(".tp__table-shell__rail")?.dyn_into::<web_sys::HtmlElement>()?;
        let offset = rail
            .style()
            .get_property_value("--table-header-size")?
            .trim_end_matches("px")
            .parse::<f64>()
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        assert!(offset >= 42.0);
        let button_element = button.clone().dyn_into::<web_sys::HtmlElement>()?;
        button_element.style().set_property("position", "fixed")?;
        button_element.style().set_property("top", "180px")?;
        button_element.style().set_property("left", "400px")?;
        button_element.style().set_property("width", "28px")?;
        click(&button)?;
        settle().await;
        assert_eq!(renders.get(), initial_renders, "opening the panel must not notify settings consumers");
        let popup = find(".tp__table-columns")?.dyn_into::<web_sys::HtmlElement>()?;
        for (property, value) in [
            ("position", "fixed"),
            ("top", "var(--table-columns-top)"),
            ("left", "var(--table-columns-left)"),
            ("right", "auto"),
            ("width", "280px"),
            ("height", "160px"),
            ("max-height", "var(--table-columns-max-height)"),
        ] {
            popup.style().set_property(property, value)?;
        }
        TimeoutFuture::new(40).await;
        assert!((popup.get_bounding_client_rect().top() - 180.0).abs() < 1.0);
        assert!(popup.get_bounding_client_rect().right() <= button.get_bounding_client_rect().left());
        let window = web_sys::window().ok_or_else(|| JsValue::from_str("missing window"))?;
        let viewport_height =
            window.inner_height()?.as_f64().ok_or_else(|| JsValue::from_str("missing viewport height"))?;
        button_element.style().set_property("top", &format!("{}px", viewport_height - 30.0))?;
        window.dispatch_event(&Event::new("resize")?)?;
        TimeoutFuture::new(40).await;
        assert!(popup.get_bounding_client_rect().top() < button.get_bounding_client_rect().top());
        assert!(popup.get_bounding_client_rect().bottom() <= viewport_height - 7.0);
        button_element.style().set_property("top", "180px")?;
        window.dispatch_event(&Event::new("resize")?)?;
        TimeoutFuture::new(40).await;
        assert!((popup.get_bounding_client_rect().top() - 180.0).abs() < 1.0);
        assert_eq!(find(".tp__table-columns header label")?.text_content().as_deref(), Some("Customize columns"));
        assert!(find(".tp__table-columns")?.query_selector("svg[data-testid='QuestionMark']")?.is_some());
        assert!(find(".tp__table-columns__cancel")?.class_list().contains("secondary"));
        assert_eq!(find(".tp__table-columns input")?.get_attribute("role").as_deref(), Some("switch"));
        assert!(!find(".tp__table-columns")?.text_content().unwrap_or_default().contains("These settings are shared"));
        click(&find(".tp__table-columns .tp__field-label__help")?)?;
        settle().await;
        let help = find(".tp__field-explanation-dialog__body")?.text_content().unwrap_or_default();
        assert!(help.contains("These settings are shared"));
        assert!(help.contains("Space or Enter"));
        click(&find(".tp__content-dialog .tp__text-button")?)?;
        settle().await;
        click(&find("#context-test-language")?)?;
        settle().await;
        assert_eq!(find(".tp__table-columns header label")?.text_content().as_deref(), Some("Personalizar columnas"));
        let before_close = renders.get();
        click(&find(".tp__table-columns .tp__table-columns__cancel")?)?;
        settle().await;
        assert_eq!(renders.get(), before_close, "closing the panel must not notify settings consumers");
        assert!(document.query_selector(".tp__table-columns")?.is_none());
        handle.destroy();
        root.remove();
        settle().await;
        Ok(())
    }
}
