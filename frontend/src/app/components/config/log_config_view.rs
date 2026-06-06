use crate::{
    app::{
        components::{
            config::{
                config_page::{ConfigForm, LABEL_LOG_CONFIG},
                config_view_context::ConfigViewContext,
                use_emit_config_form,
            },
            Card, Chip, RadioButtonGroup,
        },
        context::ConfigContext,
    },
    config_field_bool, config_field_child, edit_field_bool, generate_form_reducer,
    i18n::use_translation,
    use_default_form_reducer,
};
use shared::model::{LogConfigDto, RuntimeConfigReportFormat};
use std::{rc::Rc, str::FromStr};
use strum::IntoEnumIterator;
use yew::prelude::*;

const LABEL_LOG_LEVEL: &str = "LABEL.LOG_LEVEL";
const LABEL_LOG_ACTIVE_USER: &str = "LABEL.LOG_ACTIVE_USER";
const LABEL_LOG_SANITIZE_SENSITIVE_INFO: &str = "LABEL.SANITIZE_SENSITIVE_INFO";
const LABEL_RUNTIME_CONFIG_REPORT: &str = "LABEL.RUNTIME_CONFIG_REPORT";
const LABEL_RUNTIME_CONFIG_REPORT_FORMAT: &str = "LABEL.RUNTIME_CONFIG_REPORT_FORMAT";

const LOG_LEVELS: [&str; 5] = ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"];

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

#[component]
pub fn LogConfigView() -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let config_view_ctx = use_context::<ConfigViewContext>().expect("ConfigViewContext not found");

    let log_level_options = use_memo((), |_| LOG_LEVELS.iter().map(ToString::to_string).collect::<Vec<String>>());
    let runtime_report_format_options = use_memo((), |_| {
        RuntimeConfigReportFormat::iter().collect::<Vec<_>>().iter().map(ToString::to_string).collect::<Vec<String>>()
    });

    let form_state: UseReducerHandle<LogConfigFormState> =
        use_default_form_reducer!(LogConfigFormState { form: LogConfigDto::default() });

    {
        use_emit_config_form(&form_state, config_view_ctx.on_form_change.clone(), ConfigForm::Log);
    }

    {
        let form_state = form_state.clone();
        let log_config = config_ctx.config.as_ref().and_then(|c| c.config.log.clone()); // clone()  Option<LogConfigDto>

        use_effect_with((log_config, *config_view_ctx.edit_mode), move |(log_cfg, _mode)| {
            if let Some(log) = log_cfg {
                form_state.dispatch(LogConfigFormAction::SetAll((*log).clone()));
            } else {
                form_state.dispatch(LogConfigFormAction::SetAll(LogConfigDto::default()));
            }
            || ()
        });
    }

    let render_view_mode = || {
        let log_state = form_state.clone();
        html! {
          <>
            <Card class="tp__config-view__card">
            { config_field_bool!(log_state.form, translate.t(LABEL_LOG_ACTIVE_USER),  log_active_user) }
            { config_field_bool!(log_state.form, translate.t(LABEL_LOG_SANITIZE_SENSITIVE_INFO),  sanitize_sensitive_info) }
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
                        Some(level) => html! { <div><Chip label={level.to_string()} /></div> },
                        None => html! { <div><Chip class="tp__text-button" label={"INFO".to_string()} /></div> },
                    }
                })}
            </div>
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
            { edit_field_bool!(form_state, translate.t(LABEL_LOG_SANITIZE_SENSITIVE_INFO),  sanitize_sensitive_info, LogConfigFormAction::SanitizeSensitiveInfo) }
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
            </>
        }
    };

    html! {
      <div class="tp__log-config-view tp__config-view-page">
        <div class="tp__config-view-page__title">{translate.t(LABEL_LOG_CONFIG)}</div>
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
