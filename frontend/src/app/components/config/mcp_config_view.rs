use crate::{
    app::{
        components::{
            config::{
                config_page::{ConfigForm, LABEL_MCP_CONFIG},
                config_view_context::ConfigViewContext,
                use_emit_config_form,
            },
            AppIcon, Card,
        },
        context::ConfigContext,
    },
    config_field, config_field_bool, config_field_tags, edit_field_bool, edit_field_list,
    edit_field_number_usize_strict, generate_form_reducer,
    i18n::use_translation,
    use_default_form_reducer,
};
use shared::model::McpConfigDto;
use yew::prelude::*;

const LABEL_ENABLED: &str = "LABEL.ENABLED";
const LABEL_READ_ONLY: &str = "LABEL.READ_ONLY";
const LABEL_ALLOWED_ORIGINS: &str = "LABEL.ALLOWED_ORIGINS";
const LABEL_ADD_ORIGIN: &str = "LABEL.ADD_ORIGIN";
const LABEL_MAX_RESPONSE_BYTES: &str = "LABEL.MAX_RESPONSE_BYTES";
const INFO_MCP_RESTART_REQUIRED: &str = "INFO.MCP_RESTART_REQUIRED";
const INFO_MCP_READ_ONLY_FORCED: &str = "INFO.MCP_READ_ONLY_FORCED";

generate_form_reducer!(
    state: McpConfigFormState { form: McpConfigDto },
    action_name: McpConfigFormAction,
    fields {
        Enabled => enabled: bool,
        ReadOnly => read_only: bool,
        AllowedOrigins => allowed_origins: Vec<String>,
        MaxResponseBytes => max_response_bytes: usize,
    }
);

#[component]
pub fn McpConfigView() -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let config_view_ctx = use_context::<ConfigViewContext>().expect("ConfigViewContext not found");

    let form_state: UseReducerHandle<McpConfigFormState> =
        use_default_form_reducer!(McpConfigFormState { form: McpConfigDto::default() });

    {
        use_emit_config_form(&form_state, config_view_ctx.on_form_change.clone(), ConfigForm::Mcp);
    }

    {
        let form_state = form_state.clone();
        let mcp_config = config_ctx.config.as_ref().and_then(|c| c.config.mcp.clone());

        use_effect_with((mcp_config, *config_view_ctx.edit_mode), move |(mcp_cfg, _mode)| {
            form_state.dispatch(McpConfigFormAction::SetAll(mcp_cfg.clone().unwrap_or_default()));
            || ()
        });
    }

    let web_auth_enabled = config_ctx
        .config
        .as_ref()
        .and_then(|c| c.config.web_ui.as_ref())
        .and_then(|web_ui| web_ui.auth.as_ref())
        .is_some_and(|auth| auth.enabled);

    let render_hint = |key: &str| {
        html! {
            <div class="tp__mcp-config-view__info tp__config-view-page__info">
                <AppIcon name="Warn"/>
                <span class="info">{translate.t(key)}</span>
            </div>
        }
    };

    let render_view_mode = || {
        let mcp_state = form_state.clone();
        html! {
            <Card class="tp__config-view__card">
                { config_field_bool!(mcp_state.form, translate.t(LABEL_ENABLED), enabled) }
                { config_field_bool!(mcp_state.form, translate.t(LABEL_READ_ONLY), read_only) }
                { config_field_tags!(mcp_state.form, translate.t(LABEL_ALLOWED_ORIGINS), allowed_origins, "MCP_CONFIG.ALLOWED_ORIGINS") }
                { config_field!(mcp_state.form, translate.t(LABEL_MAX_RESPONSE_BYTES), max_response_bytes) }
            </Card>
        }
    };

    let render_edit_mode = || {
        html! {
            <Card class="tp__config-view__card">
                { edit_field_bool!(form_state, translate.t(LABEL_ENABLED), enabled, McpConfigFormAction::Enabled) }
                { edit_field_bool!(form_state, translate.t(LABEL_READ_ONLY), read_only, McpConfigFormAction::ReadOnly) }
                { edit_field_list!(form_state, translate.t(LABEL_ALLOWED_ORIGINS), allowed_origins, McpConfigFormAction::AllowedOrigins, translate.t(LABEL_ADD_ORIGIN)) }
                { edit_field_number_usize_strict!(form_state, translate.t(LABEL_MAX_RESPONSE_BYTES), max_response_bytes, McpConfigFormAction::MaxResponseBytes) }
            </Card>
        }
    };

    html! {
      <div class="tp__mcp-config-view tp__config-view-page">
        <div class="tp__config-view-page__title">{translate.t(LABEL_MCP_CONFIG)}</div>
        { if config_view_ctx.show_restart_notice { render_hint(INFO_MCP_RESTART_REQUIRED) } else { html! {} } }
        { if config_view_ctx.show_restart_notice && !web_auth_enabled && !form_state.form.read_only { render_hint(INFO_MCP_READ_ONLY_FORCED) } else { html! {} } }
        <div class="tp__mcp-config-view__body tp__config-view-page__body">
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
