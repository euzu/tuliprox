use crate::{
    app::{
        components::{
            config::{
                config_page::{ConfigForm, ConfigPage},
                config_view_context::ConfigViewContext,
                use_emit_mapped,
            },
            dto_field_id,
            number_input::NumberInput,
            selection_parse_first, AppIcon, Card, Chip, DropDownOption, DropDownSelection, RadioButtonGroup, Select,
        },
        context::ConfigContext,
    },
    config_field, config_field_bool, config_field_child, config_field_optional, edit_field_bool, edit_field_text,
    edit_field_text_option, generate_form_reducer,
    i18n::use_translation,
    use_default_form_reducer,
};
use shared::model::{LogConfigDto, LogLevel, RuntimeConfigReportFormat, SyslogConfigDto, SyslogFacility, SyslogFormat};
use std::{rc::Rc, str::FromStr};
use strum::IntoEnumIterator;
use yew::prelude::*;

const LABEL_LOG_LEVEL: &str = "LABEL.LOG_LEVEL";
const LABEL_LOG_ACTIVE_USER: &str = "LABEL.LOG_ACTIVE_USER";
const LABEL_LOG_SANITIZE_SENSITIVE_INFO: &str = "LABEL.SANITIZE_SENSITIVE_INFO";
const LABEL_RUNTIME_CONFIG_REPORT: &str = "LABEL.RUNTIME_CONFIG_REPORT";
const LABEL_RUNTIME_CONFIG_REPORT_FORMAT: &str = "LABEL.RUNTIME_CONFIG_REPORT_FORMAT";

const LABEL_SYSLOG: &str = "LABEL.SYSLOG";
const LABEL_SYSLOG_ENABLED: &str = "LABEL.SYSLOG_ENABLED";
const LABEL_SYSLOG_HOST: &str = "LABEL.SYSLOG_HOST";
const LABEL_SYSLOG_PORT: &str = "LABEL.SYSLOG_PORT";
const LABEL_SYSLOG_FORMAT: &str = "LABEL.SYSLOG_FORMAT";
const LABEL_SYSLOG_FACILITY: &str = "LABEL.SYSLOG_FACILITY";
const LABEL_SYSLOG_APP_NAME: &str = "LABEL.SYSLOG_APP_NAME";
const LABEL_SYSLOG_HOSTNAME: &str = "LABEL.SYSLOG_HOSTNAME";
const LABEL_SYSLOG_LOG_LEVEL: &str = "LABEL.SYSLOG_LOG_LEVEL";
const LABEL_SYSLOG_QUEUE_CAPACITY: &str = "LABEL.SYSLOG_QUEUE_CAPACITY";
const LABEL_SYSLOG_MAX_MESSAGE_LEN: &str = "LABEL.SYSLOG_MAX_MESSAGE_LEN";
const LABEL_SYSLOG_INHERIT_ROOT: &str = "LABEL.SYSLOG_INHERIT_ROOT";
const LABEL_SYSLOG_INFO: &str = "LABEL.SYSLOG_INFO";

const LOG_LEVELS: [&str; 5] = ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"];
const SYSLOG_LOG_LEVELS: [&str; 4] = ["DEBUG", "INFO", "WARN", "ERROR"];

generate_form_reducer!(
    state: LogConfigFormState { form: LogConfigDto },
    action_name: LogConfigFormAction,
    fields {
        LogLevel => log_level: Option<String>,
        SanitizeSensitiveInfo => sanitize_sensitive_info: bool,
        LogActiveUser => log_active_user: bool,
        RuntimeConfigReportEnabled => runtime_config_report_enabled: bool,
        RuntimeConfigReportFormat => runtime_config_report_format: RuntimeConfigReportFormat,
    }
);

generate_form_reducer!(
    state: SyslogConfigFormState { form: SyslogConfigDto },
    action_name: SyslogConfigFormAction,
    fields {
        Enabled => enabled: bool,
        Host => host: String,
        Port => port: u16,
        Format => format: SyslogFormat,
        Facility => facility: SyslogFacility,
        AppName => app_name: String,
        Hostname => hostname: Option<String>,
        LogLevel => log_level: Option<LogLevel>,
        QueueCapacity => queue_capacity: u32,
        MaxMessageLen => max_message_len: u16,
    }
);

#[component]
pub fn LogConfigView() -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let config_view_ctx = use_context::<ConfigViewContext>().expect("ConfigViewContext not found");

    let log_level_options = use_memo((), |()| LOG_LEVELS.iter().map(ToString::to_string).collect::<Vec<String>>());
    let runtime_report_format_options = use_memo((), |()| {
        RuntimeConfigReportFormat::iter().collect::<Vec<_>>().iter().map(ToString::to_string).collect::<Vec<String>>()
    });

    let syslog_format_options = use_memo((), |()| SyslogFormat::iter().map(|f| f.to_string()).collect::<Vec<String>>());
    let syslog_log_level_options =
        use_memo((), |()| SYSLOG_LOG_LEVELS.iter().map(ToString::to_string).collect::<Vec<String>>());

    let form_state: UseReducerHandle<LogConfigFormState> =
        use_default_form_reducer!(LogConfigFormState { form: LogConfigDto::default() });
    let syslog_state: UseReducerHandle<SyslogConfigFormState> =
        use_default_form_reducer!(SyslogConfigFormState { form: SyslogConfigDto::default() });

    // All 24 RFC 5424 facilities are offered (exactly one can be selected), which is too many for a radio group,
    // so this field is a select. `local0` is the default and the only value that really fits this process; the rest
    // exist for interop with existing collector rules.
    // The options depend on the current value, otherwise the dropdown would keep showing the value it was built with.
    let syslog_facility_options = use_memo(syslog_state.form.facility, |facility| {
        SyslogFacility::iter()
            .map(|item| {
                let id = item.as_ref();
                DropDownOption::new(id, html! { <span>{id}</span> }, item == *facility)
            })
            .collect::<Vec<DropDownOption>>()
    });

    {
        use_emit_mapped(
            (form_state.form.clone(), syslog_state.form.clone(), form_state.modified(), syslog_state.modified()),
            config_view_ctx.on_form_change.clone(),
            |(mut form, syslog, form_modified, syslog_modified)| {
                form.syslog = syslog;
                ConfigForm::Log(form_modified || syslog_modified, form)
            },
        );
    }

    {
        let form_state = form_state.clone();
        let syslog_state = syslog_state.clone();
        let log_config = config_ctx.config.as_ref().and_then(|c| c.config.log.clone());

        use_effect_with((log_config, *config_view_ctx.edit_mode), move |(log_cfg, _mode)| {
            if let Some(log) = log_cfg {
                form_state.dispatch(LogConfigFormAction::SetAll((*log).clone()));
                syslog_state.dispatch(SyslogConfigFormAction::SetAll(log.syslog.clone()));
            } else {
                form_state.dispatch(LogConfigFormAction::SetAll(LogConfigDto::default()));
                syslog_state.dispatch(SyslogConfigFormAction::SetAll(SyslogConfigDto::default()));
            }
            || ()
        });
    }

    let render_view_mode = || {
        let log_state = form_state.clone();
        let syslog_view_state = syslog_state.clone();
        html! {
          <>
            <Card class="tp__config-view__card">
            { config_field_bool!(log_state.form, translate.t(LABEL_LOG_ACTIVE_USER), log_active_user) }
            { config_field_bool!(log_state.form, translate.t(LABEL_LOG_SANITIZE_SENSITIVE_INFO), sanitize_sensitive_info) }
            { config_field_bool!(log_state.form, translate.t(LABEL_RUNTIME_CONFIG_REPORT), runtime_config_report_enabled) }
            <div class="tp__log-config-view__header tp__config-view-page__header">
              { config_field_child!(translate.t(LABEL_RUNTIME_CONFIG_REPORT_FORMAT), "LOG_CONFIG.RUNTIME_CONFIG_REPORT_FORMAT", {
                html! {
                    <div><Chip label={log_state.form.runtime_config_report_format.to_string()} /></div>
                }
              })}
            </div>
           </Card>
            <Card class="tp__config-view__card">
            <div class="tp__log-config-view__header tp__config-view-page__header">
                { config_field_child!(translate.t(LABEL_LOG_LEVEL), "LOG_CONFIG.LOG_LEVEL", {
                    match log_state.form.log_level.as_ref() {
                        Some(level) => html! { <div><Chip label={level.clone()} /></div> },
                        None => html! { <div><Chip class="tp__text-button" label={"INFO".to_string()} /></div> },
                    }
                })}
            </div>
           </Card>
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_SYSLOG)}</h1>
                { config_field_bool!(syslog_view_state.form, translate.t(LABEL_SYSLOG_ENABLED), enabled) }
                {
                    if syslog_view_state.form.enabled {
                        html! {
                            <>
                                { config_field!(syslog_view_state.form, translate.t(LABEL_SYSLOG_HOST), host, "LOG_CONFIG.SYSLOG_HOST") }
                                { config_field!(syslog_view_state.form, translate.t(LABEL_SYSLOG_PORT), port, "LOG_CONFIG.SYSLOG_PORT") }
                                { config_field!(syslog_view_state.form, translate.t(LABEL_SYSLOG_FORMAT), format, "LOG_CONFIG.SYSLOG_FORMAT") }
                                { config_field!(syslog_view_state.form, translate.t(LABEL_SYSLOG_FACILITY), facility, "LOG_CONFIG.SYSLOG_FACILITY") }
                                { config_field!(syslog_view_state.form, translate.t(LABEL_SYSLOG_APP_NAME), app_name, "LOG_CONFIG.SYSLOG_APP_NAME") }
                                { config_field_optional!(syslog_view_state.form, translate.t(LABEL_SYSLOG_HOSTNAME), hostname, "LOG_CONFIG.SYSLOG_HOSTNAME") }
                                <div class="tp__log-config-view__header tp__config-view-page__header">
                                    { config_field_child!(translate.t(LABEL_SYSLOG_LOG_LEVEL), dto_field_id(&syslog_view_state.form, "log_level"), "LOG_CONFIG.SYSLOG_LOG_LEVEL", {
                                        match syslog_view_state.form.log_level.as_ref() {
                                            Some(level) => html! { <div><Chip label={level.to_string().to_uppercase()} /></div> },
                                            None => html! { <div><Chip class="tp__text-button" label={translate.t(LABEL_SYSLOG_INHERIT_ROOT)} /></div> },
                                        }
                                    })}
                                </div>
                                { config_field!(syslog_view_state.form, translate.t(LABEL_SYSLOG_QUEUE_CAPACITY), queue_capacity, "LOG_CONFIG.SYSLOG_QUEUE_CAPACITY") }
                                { config_field!(syslog_view_state.form, translate.t(LABEL_SYSLOG_MAX_MESSAGE_LEN), max_message_len, "LOG_CONFIG.SYSLOG_MAX_MESSAGE_LEN") }
                            </>
                        }
                    } else {
                        html! {
                            <div class="tp__mcp-config-view__info tp__config-view-page__info">
                                <AppIcon name="Warn"/>
                                <span class="info">{translate.t(LABEL_SYSLOG_INFO)}</span>
                            </div>
                        }
                    }
                }
            </Card>
          </>
        }
    };

    let render_edit_mode = || {
        let forms = form_state.clone();
        let forms_clone = form_state.clone();
        let log_level_selection =
            Rc::new(forms.form.log_level.as_ref().map_or_else(Vec::new, |l| vec![l.to_uppercase()]));
        let runtime_report_format_selection = Rc::new(vec![forms.form.runtime_config_report_format.to_string()]);
        html! {
            <>
            <Card class="tp__config-view__card">
            { edit_field_bool!(form_state, translate.t(LABEL_LOG_ACTIVE_USER), log_active_user, LogConfigFormAction::LogActiveUser) }
            { edit_field_bool!(form_state, translate.t(LABEL_LOG_SANITIZE_SENSITIVE_INFO), sanitize_sensitive_info, LogConfigFormAction::SanitizeSensitiveInfo) }
            { edit_field_bool!(form_state, translate.t(LABEL_RUNTIME_CONFIG_REPORT), runtime_config_report_enabled, LogConfigFormAction::RuntimeConfigReportEnabled) }
                { config_field_child!(translate.t(LABEL_RUNTIME_CONFIG_REPORT_FORMAT), "LOG_CONFIG.RUNTIME_CONFIG_REPORT_FORMAT", {
                   html! { <RadioButtonGroup
                        multi_select={false} none_allowed={false}
                        on_select={Callback::from(move |selections: Rc<Vec<String>>| {
                            if let Some(frmt) = selections.first() {
                               if let Ok(format) = RuntimeConfigReportFormat::from_str(frmt) {
                                    forms_clone.dispatch(LogConfigFormAction::RuntimeConfigReportFormat(format));
                               }
                            }
                        })}
                        options={runtime_report_format_options.clone()}
                        selected={runtime_report_format_selection}
                    />
            }})}
            </Card>
            <Card class="tp__config-view__card">
            { config_field_child!(translate.t(LABEL_LOG_LEVEL), "LOG_CONFIG.LOG_LEVEL", {
               html! { <RadioButtonGroup
                    multi_select={false} none_allowed={true}
                    on_select={Callback::from(move |selections: Rc<Vec<String>>| {
                        let level: Option<String> = selections.first().map(ToString::to_string);
                        forms.dispatch(LogConfigFormAction::LogLevel(level));
                    })}
                    options={log_level_options.clone()}
                    selected={log_level_selection}
                />
            }})}
            </Card>
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_SYSLOG)}</h1>
                { edit_field_bool!(syslog_state, translate.t(LABEL_SYSLOG_ENABLED), enabled, SyslogConfigFormAction::Enabled) }
                {
                    if syslog_state.form.enabled {
                        let syslog_format_state = syslog_state.clone();
                        let syslog_facility_state = syslog_state.clone();
                        let syslog_level_state = syslog_state.clone();
                        let syslog_port_state = syslog_state.clone();
                        let syslog_queue_state = syslog_state.clone();
                        let syslog_msg_len_state = syslog_state.clone();

                        let format_selection = Rc::new(vec![syslog_state.form.format.to_string()]);
                        let syslog_level_selection = Rc::new(
                            syslog_state.form.log_level.as_ref().map_or_else(Vec::new, |l| vec![l.to_string().to_uppercase()])
                        );
                        let min_len = if syslog_state.form.format == SyslogFormat::Json { 1024 } else { 480 };

                        html! {
                            <>
                                { edit_field_text!(syslog_state, translate.t(LABEL_SYSLOG_HOST), host, SyslogConfigFormAction::Host, false, "LOG_CONFIG.SYSLOG_HOST") }
                                { config_field_child!(translate.t(LABEL_SYSLOG_PORT), dto_field_id(&syslog_state.form, "port"), "LOG_CONFIG.SYSLOG_PORT", {
                                    html! {
                                        <NumberInput
                                            name="port"
                                            value={Some(i64::from(syslog_state.form.port))}
                                            min_i64={Some(1)}
                                            max_i64={Some(65535)}
                                            on_change={Callback::from(move |v: Option<i64>| {
                                                if let Some(port) = v.and_then(|v| u16::try_from(v).ok()) {
                                                    syslog_port_state.dispatch(SyslogConfigFormAction::Port(port));
                                                }
                                            })}
                                        />
                                    }
                                })}
                                { config_field_child!(translate.t(LABEL_SYSLOG_FORMAT), dto_field_id(&syslog_state.form, "format"), "LOG_CONFIG.SYSLOG_FORMAT", {
                                    html! {
                                        <RadioButtonGroup
                                            multi_select={false}
                                            none_allowed={false}
                                            options={syslog_format_options.clone()}
                                            selected={format_selection}
                                            on_select={Callback::from(move |selections: Rc<Vec<String>>| {
                                                if let Some(fmt_str) = selections.first() {
                                                    if let Ok(format) = SyslogFormat::from_str(fmt_str) {
                                                        // `max_message_len` below 1024 is valid for the RFC formats but
                                                        // cannot hold the JSON envelope. Raise it with the format switch
                                                        // instead of leaving a value the server rejects on save.
                                                        if format == SyslogFormat::Json
                                                            && syslog_format_state.form.max_message_len < 1024
                                                        {
                                                            syslog_format_state
                                                                .dispatch(SyslogConfigFormAction::MaxMessageLen(1024));
                                                        }
                                                        syslog_format_state.dispatch(SyslogConfigFormAction::Format(format));
                                                    }
                                                }
                                            })}
                                        />
                                    }
                                })}
                                { config_field_child!(translate.t(LABEL_SYSLOG_FACILITY), dto_field_id(&syslog_state.form, "facility"), "LOG_CONFIG.SYSLOG_FACILITY", {
                                    html! {
                                        <Select
                                            name="facility"
                                            multi_select={false}
                                            required={true}
                                            options={syslog_facility_options.clone()}
                                            on_select={Callback::from(move |(_name, selections): (String, DropDownSelection)| {
                                                if let Some(facility) = selection_parse_first::<SyslogFacility>(&selections) {
                                                    syslog_facility_state.dispatch(SyslogConfigFormAction::Facility(facility));
                                                }
                                            })}
                                        />
                                    }
                                })}
                                { edit_field_text!(syslog_state, translate.t(LABEL_SYSLOG_APP_NAME), app_name, SyslogConfigFormAction::AppName, false, "LOG_CONFIG.SYSLOG_APP_NAME") }
                                { edit_field_text_option!(syslog_state, translate.t(LABEL_SYSLOG_HOSTNAME), hostname, SyslogConfigFormAction::Hostname, false, "LOG_CONFIG.SYSLOG_HOSTNAME") }
                                { config_field_child!(translate.t(LABEL_SYSLOG_LOG_LEVEL), dto_field_id(&syslog_state.form, "log_level"), "LOG_CONFIG.SYSLOG_LOG_LEVEL", {
                                    html! {
                                        <RadioButtonGroup
                                            multi_select={false}
                                            none_allowed={true}
                                            options={syslog_log_level_options.clone()}
                                            selected={syslog_level_selection}
                                            on_select={Callback::from(move |selections: Rc<Vec<String>>| {
                                                let level = selections.first().and_then(|s| LogLevel::from_str(s).ok());
                                                syslog_level_state.dispatch(SyslogConfigFormAction::LogLevel(level));
                                            })}
                                        />
                                    }
                                })}
                                { config_field_child!(translate.t(LABEL_SYSLOG_QUEUE_CAPACITY), dto_field_id(&syslog_state.form, "queue_capacity"), "LOG_CONFIG.SYSLOG_QUEUE_CAPACITY", {
                                    html! {
                                        <NumberInput
                                            name="queue_capacity"
                                            value={Some(i64::from(syslog_state.form.queue_capacity))}
                                            min_i64={Some(1)}
                                            max_i64={Some(65536)}
                                            on_change={Callback::from(move |v: Option<i64>| {
                                                if let Some(cap) = v.and_then(|v| u32::try_from(v).ok()) {
                                                    syslog_queue_state.dispatch(SyslogConfigFormAction::QueueCapacity(cap));
                                                }
                                            })}
                                        />
                                    }
                                })}
                                { config_field_child!(translate.t(LABEL_SYSLOG_MAX_MESSAGE_LEN), dto_field_id(&syslog_state.form, "max_message_len"), "LOG_CONFIG.SYSLOG_MAX_MESSAGE_LEN", {
                                    html! {
                                        <NumberInput
                                            name="max_message_len"
                                            value={Some(i64::from(syslog_state.form.max_message_len))}
                                            min_i64={Some(min_len)}
                                            max_i64={Some(65507)}
                                            on_change={Callback::from(move |v: Option<i64>| {
                                                if let Some(len) = v.and_then(|v| u16::try_from(v).ok()) {
                                                    syslog_msg_len_state.dispatch(SyslogConfigFormAction::MaxMessageLen(len));
                                                }
                                            })}
                                        />
                                    }
                                })}
                            </>
                        }
                    } else {
                        html! {}
                    }
                }
            </Card>
            </>
        }
    };

    html! {
      <div class="tp__log-config-view tp__config-view-page">
        <div class="tp__config-view-page__title">{translate.t(ConfigPage::Log.title_key())}</div>
        <div class="tp__log-config-view__body tp__config-view-page__body">
        {
           if *config_view_ctx.edit_mode {
              render_edit_mode()
           } else {
              render_view_mode()
           }
        }
        </div>
      </div>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod browser_tests {
    use super::*;
    use crate::{
        app::{components::config::ConfigForm, ConfigContext},
        i18n::I18nProvider,
        provider::{DialogProvider, IconContextProvider},
    };
    use gloo_timers::future::TimeoutFuture;
    use shared::model::{AppConfigDto, ConfigDto, SourcesConfigDto};
    use std::{cell::RefCell, rc::Rc};
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};
    use web_sys::{Element, HtmlElement};
    use yew::{AppHandle, Renderer};

    wasm_bindgen_test_configure!(run_in_browser);

    #[derive(Properties, Clone, PartialEq)]
    struct HarnessProps {
        config: Rc<AppConfigDto>,
        edit_mode: bool,
        emissions: Rc<RefCell<Vec<ConfigForm>>>,
    }

    #[component]
    fn Harness(props: &HarnessProps) -> Html {
        let edit_mode = use_state(|| props.edit_mode);
        let emissions = Rc::clone(&props.emissions);
        let view_ctx = ConfigViewContext {
            edit_mode,
            show_restart_notice: false,
            on_form_change: Callback::from(move |form| emissions.borrow_mut().push(form)),
        };
        let config_ctx = ConfigContext { config: Some(Rc::clone(&props.config)), api_proxy: None };
        html! {
            <ContextProvider<ConfigContext> context={config_ctx}>
                <ContextProvider<ConfigViewContext> context={view_ctx}>
                    <I18nProvider>
                        <IconContextProvider icons={vec![]}>
                            <DialogProvider>
                                <LogConfigView />
                            </DialogProvider>
                        </IconContextProvider>
                    </I18nProvider>
                </ContextProvider<ConfigViewContext>>
            </ContextProvider<ConfigContext>>
        }
    }

    fn syslog_enabled_config() -> AppConfigDto {
        let mut log = LogConfigDto::default();
        log.log_level = Some("info".to_string());
        log.syslog.enabled = true;
        log.syslog.host = "127.0.0.1".to_string();
        log.syslog.log_level = Some(LogLevel::Debug);
        let mut config = ConfigDto::default();
        config.log = Some(log);
        AppConfigDto {
            config,
            sources: SourcesConfigDto { templates: None, provider: None, inputs: vec![], sources: vec![] },
            mappings: None,
            templates: None,
            api_proxy: None,
        }
    }

    async fn settle() { TimeoutFuture::new(50).await }

    async fn render(
        edit_mode: bool,
    ) -> Result<(Element, AppHandle<Harness>, Rc<RefCell<Vec<ConfigForm>>>), wasm_bindgen::JsValue> {
        let document = gloo_utils::document();
        let body = document.body().ok_or_else(|| wasm_bindgen::JsValue::from_str("test document has no body"))?;
        let root = document.create_element("div")?;
        body.append_child(&root)?;
        let emissions = Rc::new(RefCell::new(Vec::new()));
        let props = HarnessProps { config: Rc::new(syslog_enabled_config()), edit_mode, emissions: emissions.clone() };
        let handle = Renderer::<Harness>::with_root_and_props(root.clone(), props).render();
        settle().await;
        Ok((root, handle, emissions))
    }

    /// 24 facilities are a list, not a row of toggles: the editor must render the facility field as a select while
    /// the 3-value format and the 4-value log level stay radio groups.
    #[wasm_bindgen_test]
    async fn facility_renders_as_a_select_and_format_stays_a_radio_group() -> Result<(), wasm_bindgen::JsValue> {
        let (root, handle, _emissions) = render(true).await?;

        let selects = root.query_selector_all(".tp__select")?;
        assert_eq!(selects.length(), 1, "exactly one select is expected (facility)");

        let selected = selects
            .item(0)
            .and_then(|node| node.dyn_into::<Element>().ok())
            .and_then(|element| element.query_selector(".tp__select__selected").ok().flatten())
            .and_then(|element| element.text_content())
            .unwrap_or_default();
        assert_eq!(selected.trim(), "local0", "the select must show the current facility");

        // Scope to the card that holds the select: the other cards have their own radio group (runtime report
        // format), so only this card's count says anything about the syslog fields.
        let card = selects
            .item(0)
            .and_then(|node| node.dyn_into::<Element>().ok())
            .and_then(|element| element.closest(".tp__config-view__card").ok().flatten())
            .ok_or_else(|| wasm_bindgen::JsValue::from_str("facility select is not inside a config card"))?;
        let radio_groups = card.query_selector_all(".tp__radio-button-group")?;
        assert_eq!(radio_groups.length(), 2, "format (3 values) and log level (4 values) stay radio groups");

        handle.destroy();
        root.remove();
        Ok(())
    }

    #[wasm_bindgen_test]
    async fn selecting_a_facility_emits_the_new_value() -> Result<(), wasm_bindgen::JsValue> {
        let (root, handle, emissions) = render(true).await?;

        let combobox = root
            .query_selector(".tp__select button[role=\"combobox\"]")?
            .ok_or_else(|| wasm_bindgen::JsValue::from_str("facility select has no combobox button"))?;
        combobox.dyn_into::<HtmlElement>()?.click();
        settle().await;

        // The popup renders into a portal on the document body, so the options are not inside `root`.
        let document = gloo_utils::document();
        let options = document.query_selector_all("button[role=\"option\"]")?;
        assert_eq!(options.length(), 24, "every RFC 5424 facility must be selectable");

        let mut target: Option<Element> = None;
        for index in 0..options.length() {
            let Some(node) = options.item(index) else { continue };
            let element =
                node.dyn_into::<Element>().map_err(|_| wasm_bindgen::JsValue::from_str("option is not an element"))?;
            let label = element.text_content().unwrap_or_default();
            if label.trim() == "daemon" {
                target = Some(element);
                break;
            }
        }
        let target = target.ok_or_else(|| wasm_bindgen::JsValue::from_str("daemon option not offered"))?;
        target.dyn_into::<HtmlElement>()?.click();
        settle().await;

        let selected_after = root
            .query_selector(".tp__select .tp__select__selected")?
            .and_then(|element| element.text_content())
            .unwrap_or_default();
        assert_eq!(selected_after.trim(), "daemon", "the select must show the chosen facility");

        let emitted = emissions.borrow();
        let last = emitted.last().expect("a form change must be emitted");
        match last {
            ConfigForm::Log(modified, dto) => {
                assert!(*modified, "a facility change marks the form as modified");
                assert_eq!(dto.syslog.facility, SyslogFacility::Daemon);
                assert!(dto.syslog.enabled, "the rest of the block must survive the edit");
                assert_eq!(dto.syslog.host, "127.0.0.1");
            }
            other => panic!("expected ConfigForm::Log, got {other:?}"),
        }

        drop(emitted);
        handle.destroy();
        root.remove();
        Ok(())
    }
}
