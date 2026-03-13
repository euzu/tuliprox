use crate::{
    app::{
        components::{
            config::{
                config_page::{
                    ConfigForm, ConfigFormSlots, ConfigPage, LABEL_API_CONFIG, LABEL_HDHOMERUN_CONFIG,
                    LABEL_IP_CHECK_CONFIG, LABEL_LIBRARY_CONFIG, LABEL_LOG_CONFIG, LABEL_MAIN_CONFIG,
                    LABEL_MESSAGING_CONFIG, LABEL_METADATA_UPDATE_CONFIG, LABEL_PANEL_CONFIG, LABEL_PROXY_CONFIG,
                    LABEL_REVERSE_PROXY_CONFIG, LABEL_SCHEDULES_CONFIG, LABEL_VIDEO_CONFIG, LABEL_WEB_UI_CONFIG,
                },
                config_update::update_config,
                config_view_context::ConfigViewContext,
                ApiConfigView, HdHomerunConfigView, IpCheckConfigView, LibraryConfigView, LogConfigView,
                MainConfigView, MessagingConfigView, MetadataUpdateConfigView, PanelConfigView, ProxyConfigView,
                ReverseProxyConfigView, SchedulesConfigView, VideoConfigView, WebUiConfigView,
            },
            input::Input,
            validate_credentials, Card, TabItem, TabSet, TextButton,
        },
        ConfigContext,
    },
    hooks::use_service_context,
    html_if,
    i18n::use_translation,
    services::{get_base_href, SetupCompleteRequestDto, SetupWebUserCredentialDto},
    utils::set_timeout,
};
use log::warn;
use shared::model::{ApiProxyConfigDto, AppConfigDto, ConfigDto, SourcesConfigDto};
use std::str::FromStr;
use yew::{platform::spawn_local, prelude::*};

const LABEL_CONFIG: &str = "LABEL.CONFIG";
const LABEL_EDIT: &str = "LABEL.EDIT";
const LABEL_VIEW: &str = "LABEL.VIEW";
const LABEL_SAVE: &str = "LABEL.SAVE";
const LABEL_UPDATE_GEOIP: &str = "LABEL.UPDATE_GEOIP_DB";
const LABEL_SETUP_WELCOME: &str = "SETUP.MSG.WELCOME";
const LABEL_SETUP_FINISH: &str = "SETUP.LABEL.FINISH_SETUP";
const LABEL_SETUP_WEBUI_USERNAME: &str = "SETUP.LABEL.WEBUI_USERNAME";
const LABEL_SETUP_WEBUI_PASSWORD: &str = "SETUP.LABEL.WEBUI_PASSWORD";
const LABEL_SETUP_WEBUI_PASSWORD_REPEAT: &str = "SETUP.LABEL.WEBUI_PASSWORD_REPEAT";

const ACTION_UPDATE_GEO_IP: &str = "update_geo_ip";
fn config_form_to_config_page(form: &ConfigForm) -> ConfigPage {
    match form {
        ConfigForm::Main(_, _) => ConfigPage::Main,
        ConfigForm::Api(_, _) => ConfigPage::Api,
        ConfigForm::ApiProxy(_, _) => ConfigPage::Api,
        ConfigForm::Log(_, _) => ConfigPage::Log,
        ConfigForm::Schedules(_, _) => ConfigPage::Schedules,
        ConfigForm::Video(_, _) => ConfigPage::Video,
        ConfigForm::MetadataUpdate(_, _) => ConfigPage::MetadataUpdate,
        ConfigForm::Messaging(_, _) => ConfigPage::Messaging,
        ConfigForm::WebUi(_, _) => ConfigPage::WebUi,
        ConfigForm::ReverseProxy(_, _) => ConfigPage::ReverseProxy,
        ConfigForm::HdHomerun(_, _) => ConfigPage::HdHomerun,
        ConfigForm::Proxy(_, _) => ConfigPage::Proxy,
        ConfigForm::IpCheck(_, _) => ConfigPage::IpCheck,
        ConfigForm::Panel(_, _) => ConfigPage::Panel,
        ConfigForm::Library(_, _) => ConfigPage::Library,
    }
}

#[derive(Default, Debug, Clone, PartialEq)]
struct ConfigFormState {
    pub slots: ConfigFormSlots,
}

#[component]
pub fn ConfigView() -> Html {
    let translate = use_translation();
    let services_ctx = use_service_context();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let setup_mode = services_ctx.config.ui_config.setup_mode;

    let active_tab = use_state(|| ConfigPage::Main);
    let edit_mode = use_state(|| setup_mode);
    let form_state = use_state(ConfigFormState::default);
    let setup_username = use_state(|| "admin".to_string());
    let setup_password = use_state(String::new);
    let setup_password_repeat = use_state(String::new);

    let handle_tab_change = {
        let active_tab = active_tab.clone();
        Callback::from(move |tab_id: String| {
            if let Ok(page) = ConfigPage::from_str(&tab_id) {
                active_tab.set(page);
            }
        })
    };

    let tabs = {
        let form_state_value = (*form_state).clone();
        let translate = translate.clone();
        let edit_value = *edit_mode;

        use_memo((form_state_value, edit_value, translate.clone()), move |(forms, editing, translate)| {
            let forms: &ConfigFormState = forms;
            let modified_pages = forms
                .slots
                .collect_modified_forms()
                .into_iter()
                .filter(|form| !matches!(form, ConfigForm::ApiProxy(_, _)))
                .map(|form| config_form_to_config_page(&form))
                .collect::<Vec<ConfigPage>>();

            let tab_configs = vec![
                (ConfigPage::Main, LABEL_MAIN_CONFIG, html! { <MainConfigView/> }, "MainConfig"),
                (ConfigPage::Api, LABEL_API_CONFIG, html! { <ApiConfigView/> }, "ApiConfig"),
                (ConfigPage::Log, LABEL_LOG_CONFIG, html! { <LogConfigView/> }, "Log"),
                (ConfigPage::Schedules, LABEL_SCHEDULES_CONFIG, html! { <SchedulesConfigView/> }, "SchedulesConfig"),
                (ConfigPage::Messaging, LABEL_MESSAGING_CONFIG, html! { <MessagingConfigView/> }, "MessagingConfig"),
                (ConfigPage::WebUi, LABEL_WEB_UI_CONFIG, html! { <WebUiConfigView/> }, "WebUiConfig"),
                (
                    ConfigPage::ReverseProxy,
                    LABEL_REVERSE_PROXY_CONFIG,
                    html! { <ReverseProxyConfigView/> },
                    "ReverseProxyConfig",
                ),
                (ConfigPage::HdHomerun, LABEL_HDHOMERUN_CONFIG, html! { <HdHomerunConfigView/> }, "HdHomerunConfig"),
                (ConfigPage::Proxy, LABEL_PROXY_CONFIG, html! { <ProxyConfigView/> }, "ProxyConfig"),
                (ConfigPage::IpCheck, LABEL_IP_CHECK_CONFIG, html! { <IpCheckConfigView/> }, "IpCheckConfig"),
                (ConfigPage::Panel, LABEL_PANEL_CONFIG, html! { <PanelConfigView/> }, "Settings"),
                (ConfigPage::Video, LABEL_VIDEO_CONFIG, html! { <VideoConfigView/> }, "VideoConfig"),
                (
                    ConfigPage::MetadataUpdate,
                    LABEL_METADATA_UPDATE_CONFIG,
                    html! { <MetadataUpdateConfigView/> },
                    "Metadata",
                ),
                (ConfigPage::Library, LABEL_LIBRARY_CONFIG, html! { <LibraryConfigView/> }, "VideoLibrary"),
            ];

            let editing = *editing;
            tab_configs
                .into_iter()
                .map(|(page, label, children, icon)| {
                    let is_modified = editing && modified_pages.contains(&page);
                    TabItem {
                        id: page.to_string(),
                        title: translate.t(label),
                        icon: icon.to_string(),
                        children,
                        active_class: if is_modified { Some("tp__tab__modified__active".to_string()) } else { None },
                        inactive_class: if is_modified {
                            Some("tp__tab__modified__inactive".to_string())
                        } else {
                            None
                        },
                    }
                })
                .collect::<Vec<TabItem>>()
        })
    };

    let handle_config_edit = {
        let set_edit_mode = edit_mode.clone();
        Callback::from(move |_| {
            if setup_mode {
                return;
            }
            set_edit_mode.set(!*set_edit_mode);
        })
    };

    let handle_save_config = {
        let config_ctx = config_ctx.clone();
        let translate = translate.clone();
        let services = services_ctx.clone();
        let get_form_state = form_state.clone();
        let set_edit_mode = edit_mode.clone();
        let setup_username = setup_username.clone();
        let setup_password = setup_password.clone();
        let setup_password_repeat = setup_password_repeat.clone();

        Callback::from(move |_| {
            let modified_forms: Vec<ConfigForm> = get_form_state.slots.collect_modified_forms();

            if !setup_mode && modified_forms.is_empty() {
                set_edit_mode.set(false);
                services.toastr.info(translate.t("MESSAGES.SAVE.NO_CHANGES"));
                return;
            }

            if setup_mode {
                let username = setup_username.trim().to_string();
                let password = setup_password.to_string();
                let password_repeat = setup_password_repeat.to_string();

                if let Err(err) = validate_credentials(&username, &password, Some(&password_repeat)) {
                    services.toastr.error(translate.t(err.i18n_key()));
                    return;
                }

                let mut app_config =
                    config_ctx.config.as_ref().map_or_else(AppConfigDto::default, |cfg| cfg.as_ref().clone());
                if app_config.api_proxy.is_none() {
                    app_config.api_proxy = Some(
                        config_ctx
                            .api_proxy
                            .as_ref()
                            .map_or_else(ApiProxyConfigDto::default, |api| api.as_ref().clone()),
                    );
                }

                let mut modified_main_forms = Vec::new();
                for form in modified_forms {
                    match form {
                        ConfigForm::Panel(_, sources) => app_config.sources = sources,
                        ConfigForm::ApiProxy(_, api_proxy) => app_config.api_proxy = Some(api_proxy),
                        other => modified_main_forms.push(other),
                    }
                }
                if !modified_main_forms.is_empty() {
                    update_config(&mut app_config.config, modified_main_forms);
                }

                if let Err(err) = app_config.config.prepare(false) {
                    services.toastr.error(err.to_string());
                    return;
                }
                if let Err(err) = app_config.sources.prepare(
                    false,
                    app_config.config.get_hdhr_device_overview().as_ref(),
                    app_config.templates.as_ref().map(|defs| defs.templates.as_slice()),
                ) {
                    services.toastr.error(err.to_string());
                    return;
                }
                if let Some(api_proxy) = app_config.api_proxy.as_mut() {
                    if let Err(err) = api_proxy.prepare() {
                        services.toastr.error(err.to_string());
                        return;
                    }
                }

                let payload = SetupCompleteRequestDto {
                    app_config,
                    web_users: vec![SetupWebUserCredentialDto { username, password }],
                };

                let services = services.clone();
                let translate = translate.clone();
                let set_edit_mode = set_edit_mode.clone();
                spawn_local(async move {
                    match services.config.complete_setup(payload).await {
                        Ok(()) => {
                            set_edit_mode.set(false);
                            services.toastr.success(translate.t("MESSAGES.SAVE.MAIN_CONFIG.SUCCESS"));
                            set_timeout(
                                move || {
                                    if let Some(window) = web_sys::window() {
                                        let _ = window.open_with_url_and_target(&get_base_href(), "_self");
                                    }
                                },
                                500,
                            );
                        }
                        Err(err) => {
                            services.toastr.error(format!(
                                "{}: {}",
                                translate.t("MESSAGES.SAVE.MAIN_CONFIG.FAIL"),
                                err
                            ));
                        }
                    }
                });
                return;
            }

            let mut modified_main_forms = Vec::new();
            let mut modified_sources: Option<SourcesConfigDto> = None;
            let mut modified_api_proxy: Option<ApiProxyConfigDto> = None;
            for form in modified_forms {
                match form {
                    ConfigForm::Panel(_, sources) => modified_sources = Some(sources),
                    ConfigForm::ApiProxy(_, api_proxy) => modified_api_proxy = Some(api_proxy),
                    other => modified_main_forms.push(other),
                }
            }

            let mut modified_main_dto: Option<ConfigDto> = None;
            if !modified_main_forms.is_empty() {
                let mut config_dto =
                    config_ctx.config.as_ref().map_or_else(ConfigDto::default, |app_cfg| app_cfg.config.clone());
                update_config(&mut config_dto, modified_main_forms);
                if let Err(err) = config_dto.prepare(false) {
                    services.toastr.error(err.to_string());
                    return;
                }
                modified_main_dto = Some(config_dto);
            }

            if let Some(api_proxy) = modified_api_proxy.as_mut() {
                if let Err(err) = api_proxy.prepare() {
                    services.toastr.error(err.to_string());
                    return;
                }
            }

            if let Some(sources) = modified_sources.as_mut() {
                let global_templates = config_ctx
                    .config
                    .as_ref()
                    .and_then(|cfg| cfg.templates.as_ref().map(|defs| defs.templates.as_slice()));
                if let Err(err) = sources.prepare(false, None, global_templates) {
                    services.toastr.error(err.to_string());
                    return;
                }
            }

            let services = services.clone();
            let translate = translate.clone();
            let set_edit_mode = set_edit_mode.clone();
            spawn_local(async move {
                let mut ok = true;

                if let Some(config_dto) = modified_main_dto {
                    match services.config.save_config(config_dto).await {
                        Ok(()) => {
                            services.toastr.success(translate.t("MESSAGES.SAVE.MAIN_CONFIG.SUCCESS"));
                        }
                        Err(err) => {
                            ok = false;
                            services.toastr.error(translate.t("MESSAGES.SAVE.MAIN_CONFIG.FAIL"));
                            services.toastr.error(err.to_string());
                        }
                    }
                }

                if let Some(api_proxy_dto) = modified_api_proxy {
                    match services.config.save_api_proxy_config(api_proxy_dto).await {
                        Ok(()) => {
                            services.toastr.success(translate.t("MESSAGES.SAVE.API_PROXY_CONFIG.SUCCESS"));
                        }
                        Err(err) => {
                            ok = false;
                            services.toastr.error(translate.t("MESSAGES.SAVE.API_PROXY_CONFIG.FAIL"));
                            services.toastr.error(err.to_string());
                        }
                    }
                }

                if let Some(sources_dto) = modified_sources {
                    match services.config.save_sources(sources_dto).await {
                        Ok(()) => {
                            services.toastr.success(translate.t("MESSAGES.SAVE.SOURCES_CONFIG.SUCCESS"));
                        }
                        Err(err) => {
                            ok = false;
                            services.toastr.error(translate.t("MESSAGES.SAVE.SOURCES_CONFIG.FAIL"));
                            services.toastr.error(err.to_string());
                        }
                    }
                }

                if ok {
                    set_edit_mode.set(false);
                    let (app_config, api_proxy_config) = services.config.get_server_config().await;
                    if app_config.is_none() {
                        // Log but don't fail - save succeeded; refresh is best-effort
                        warn!("Config refresh failed");
                    }
                    if api_proxy_config.is_none() {
                        // Log but don't fail - save succeeded; refresh is best-effort
                        warn!("ApiProxy Config refresh failed");
                    }
                }
            });
        })
    };

    let on_form_change = {
        let set_form_state = form_state.clone();
        Callback::from(move |form_data: ConfigForm| {
            let mut new_state = (*set_form_state).clone();
            new_state.slots.update_form(form_data);
            set_form_state.set(new_state);
        })
    };

    let handle_update_content = {
        let services = services_ctx.clone();
        let translate = translate.clone();
        Callback::from(move |name: String| {
            let services = services.clone();
            let translate = translate.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if name.as_str() == ACTION_UPDATE_GEO_IP {
                    match services.config.update_geoip().await {
                        Ok(_) => services.toastr.success(translate.t("MESSAGES.DOWNLOAD.GEOIP.SUCCESS")),
                        Err(_err) => services.toastr.error(translate.t("MESSAGES.DOWNLOAD.GEOIP.FAIL")),
                    }
                }
            });
        })
    };

    let context = ConfigViewContext {
        edit_mode: edit_mode.clone(),
        show_restart_notice: !setup_mode,
        on_form_change: on_form_change.clone(),
    };

    let geo_ip_enabled = !setup_mode && config_ctx.config.as_ref().is_some_and(|c| c.config.is_geoip_enabled());

    html! {
        <ContextProvider<ConfigViewContext> context={context}>
        <div class="tp__config-view">
           { html_if!(setup_mode, {
                    <div class="tp__webui-config-view__info tp__config-view-page__info">
                        <span class="info">{translate.t(LABEL_SETUP_WELCOME)}</span>
                    </div>
           })}

            <div class="tp__config-view__header">
                <h1>{ translate.t(LABEL_CONFIG) } </h1>
                <div class="tp__config-view__header-tools">
                {html_if!(geo_ip_enabled, {
                    <TextButton class="tertiary" name={ACTION_UPDATE_GEO_IP}
                        icon="Refresh"
                        title={ translate.t(LABEL_UPDATE_GEOIP)}
                        onclick={handle_update_content.clone()}></TextButton>
                })}
                </div>
               { html_if!(!setup_mode, {
                   <TextButton name="config_edit"
                        class={ if *edit_mode { "secondary" } else { "primary" }}
                        icon={ if *edit_mode { "Unlocked" } else { "Locked" }}
                        title={ if *edit_mode { translate.t(LABEL_EDIT) } else { translate.t(LABEL_VIEW) }}
                        onclick={handle_config_edit}></TextButton>
               })}

            </div>
            <div class="tp__config-view__body">
            <Card>
                { html_if!(setup_mode, {
                    <div class="tp__form-page__toolbar">
                        <Input
                            name="setup_username"
                            label={Some(translate.t(LABEL_SETUP_WEBUI_USERNAME).to_string())}
                            value={(*setup_username).clone()}
                            on_change={Some({
                                let setup_username = setup_username.clone();
                                Callback::from(move |value: String| setup_username.set(value))
                            })}
                        />
                        <Input
                            name="setup_password"
                            label={Some(translate.t(LABEL_SETUP_WEBUI_PASSWORD).to_string())}
                            hidden={true}
                            value={(*setup_password).clone()}
                            on_change={Some({
                                let setup_password = setup_password.clone();
                                Callback::from(move |value: String| setup_password.set(value))
                            })}
                        />
                        <Input
                            name="setup_password_repeat"
                            label={Some(translate.t(LABEL_SETUP_WEBUI_PASSWORD_REPEAT).to_string())}
                            hidden={true}
                            value={(*setup_password_repeat).clone()}
                            on_change={Some({
                                let setup_password_repeat = setup_password_repeat.clone();
                                Callback::from(move |value: String| setup_password_repeat.set(value))
                            })}
                        />
                    </div>
                })}
                 <TabSet tabs={tabs.clone()} active_tab={Some((*active_tab).to_string())}
                     on_tab_change={Some(handle_tab_change)}
                     class="tp__config-view__tabset"/>

                { html_if!(*edit_mode || setup_mode, {
                    <div class="tp__config-view__toolbar tp__form-page__toolbar">
                     <TextButton class="secondary" name="save_config"
                        icon="Save"
                        title={ if setup_mode { translate.t(LABEL_SETUP_FINISH) } else { translate.t(LABEL_SAVE) }}
                        onclick={handle_save_config}></TextButton>
                    </div>
                })}
            </Card>
            </div>
        </div>
        </ContextProvider<ConfigViewContext>>
    }
}
