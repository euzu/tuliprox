use crate::app::components::{AppIcon, Card, Chip};
use crate::app::context::ConfigContext;
use crate::app::components::config::config_view_context::ConfigViewContext;
use crate::app::components::config::config_page::{ConfigForm, LABEL_WEB_UI_CONFIG};
use crate::{config_field, config_field_bool, config_field_child, config_field_hide, config_field_optional, edit_field_bool, edit_field_list_option, edit_field_number, edit_field_number_u64, edit_field_text, edit_field_text_option, generate_form_reducer, html_if};
use yew::prelude::*;
use yew_i18n::use_translation;
use shared::model::{WebUiConfigDto, ContentSecurityPolicyConfigDto, WebAuthConfigDto};

// Labels
const LABEL_AUTH: &str = "LABEL.AUTH";
const LABEL_ENABLED: &str = "LABEL.ENABLED";
const LABEL_ISSUER: &str = "LABEL.ISSUER";
const LABEL_SECRET: &str = "LABEL.SECRET";
const LABEL_TOKEN_TTL_MINS: &str = "LABEL.TOKEN_TTL_MINS";
const LABEL_USERFILE: &str = "LABEL.USERFILE";
const LABEL_PLAYER_SERVER: &str = "LABEL.PLAYER_SERVER";
const LABEL_KICK_DURATION: &str = "LABEL.KICK_DURATION";
const LABEL_USER_UI_ENABLED: &str = "LABEL.USER_UI_ENABLED";
const LABEL_CONTENT_SECURITY_POLICY: &str = "LABEL.CONTENT_SECURITY_POLICY";
const LABEL_CONTENT_SECURITY_POLICY_CUSTOM_ATTRIBUTES: &str = "LABEL.CUSTOM_ATTRIBUTES";
const LABEL_PATH: &str = "LABEL.PATH";

// Reducers for form states
generate_form_reducer!(
    state: WebUiConfigFormState { form: WebUiConfigDto },
    action_name: WebUiConfigFormAction,
    fields {
        Enabled => enabled: bool,
        UserUiEnabled => user_ui_enabled: bool,
        Path => path: Option<String>,
        PlayerServer => player_server: Option<String>,
        KickSecs => kick_secs: u64,
    }
);

generate_form_reducer!(
    state: WebUiAuthConfigFormState { form: WebAuthConfigDto },
    action_name: WebUiAuthConfigFormAction,
    fields {
        Enabled => enabled: bool,
        Issuer => issuer: String,
        Secret => secret: String,
        TokenTtlMins => token_ttl_mins: u32,
        Userfile => userfile: Option<String>,
    }
);

generate_form_reducer!(
    state: CspConfigFormState { form: ContentSecurityPolicyConfigDto },
    action_name: CspConfigFormAction,
    fields {
        Enabled => enabled: bool,
        CustomAttributes =>  custom_attributes: Option<Vec<String>>
    }
);

#[function_component]
pub fn WebUiConfigView() -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let config_view_ctx = use_context::<ConfigViewContext>().expect("ConfigViewContext not found");

    // Local form states
    let webui_state: UseReducerHandle<WebUiConfigFormState> = use_reducer(|| {
        WebUiConfigFormState { form: WebUiConfigDto::default(), modified: false }
    });
    let auth_state: UseReducerHandle<WebUiAuthConfigFormState> = use_reducer(|| {
        WebUiAuthConfigFormState { form: WebAuthConfigDto::default(), modified: false }
    });
    let csp_state: UseReducerHandle<CspConfigFormState> = use_reducer(|| {
        CspConfigFormState { form: ContentSecurityPolicyConfigDto::default(), modified: false }
    });

    // Notify parent when form changes
    {
        let on_form_change = config_view_ctx.on_form_change.clone();
        let webui_state = webui_state.clone();
        let auth_state = auth_state.clone();
        let csp_state = csp_state.clone();
        let deps = (webui_state.modified, auth_state.modified, csp_state.modified, webui_state, auth_state, csp_state);
        use_effect_with(deps,
                        move |(wm, am, cm, w, a, c)| {
            let mut form = w.form.clone();
            form.auth = Some(a.form.clone());
            form.content_security_policy = Some(c.form.clone());

            let modified = *wm || *am || *cm;
            on_form_change.emit(ConfigForm::WebUi(modified, form));
        });
    }

    // Sync from context when config or edit mode changes
    {
        let webui_state = webui_state.clone();
        let auth_state = auth_state.clone();
        let csp_state = csp_state.clone();

        let webui_cfg = config_ctx.config.as_ref().and_then(|c| c.config.web_ui.clone());
        use_effect_with((webui_cfg, config_view_ctx.edit_mode.clone()), move |(cfg, _mode)| {
            if let Some(webui) = cfg {
                webui_state.dispatch(WebUiConfigFormAction::SetAll((*webui).clone()));
                if let Some(auth) = &webui.auth {
                    auth_state.dispatch(WebUiAuthConfigFormAction::SetAll(auth.clone()));
                } else {
                    auth_state.dispatch(WebUiAuthConfigFormAction::SetAll(WebAuthConfigDto::default()));
                }
                if let Some(csp) = &webui.content_security_policy {
                    csp_state.dispatch(CspConfigFormAction::SetAll(csp.clone()));
                } else {
                    csp_state.dispatch(CspConfigFormAction::SetAll(ContentSecurityPolicyConfigDto::default()));
                }

            } else {
                webui_state.dispatch(WebUiConfigFormAction::SetAll(WebUiConfigDto::default()));
                auth_state.dispatch(WebUiAuthConfigFormAction::SetAll(WebAuthConfigDto::default()));
                csp_state.dispatch(CspConfigFormAction::SetAll(ContentSecurityPolicyConfigDto::default()));
            }
            || ()
        });
    }

    // View mode
    let render_view_mode = || {
        html! {
        <>
            <Card class="tp__config-view__card">
            { config_field_bool!(webui_state.form, translate.t(LABEL_ENABLED), enabled) }
            { config_field_bool!(webui_state.form, translate.t(LABEL_USER_UI_ENABLED), user_ui_enabled) }
            { config_field_optional!(webui_state.form, translate.t(LABEL_PATH), path) }
            { config_field_optional!(webui_state.form, translate.t(LABEL_PLAYER_SERVER), player_server) }
            { config_field!(webui_state.form, translate.t(LABEL_KICK_DURATION), kick_secs) }
            </Card>

            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_CONTENT_SECURITY_POLICY)}</h1>
                { config_field_bool!(csp_state.form, translate.t(LABEL_ENABLED), enabled) }
                { config_field_child!(translate.t(LABEL_CONTENT_SECURITY_POLICY_CUSTOM_ATTRIBUTES), {
                    html! {
                        <div class="tp__config-view__tags">
                            {
                                if let Some(custom) = &csp_state.form.custom_attributes {
                                    html! { for custom.iter().map(|a| html! { <Chip label={a.clone()} /> }) }
                                } else {
                                    html! {}
                                }
                            }
                        </div>
                    }
                }) }
            </Card>
           <Card class="tp__config-view__card">
            <h1>{translate.t(LABEL_AUTH)}</h1>
            { config_field_bool!(auth_state.form, translate.t(LABEL_ENABLED), enabled) }
            { config_field!(auth_state.form, translate.t(LABEL_ISSUER), issuer) }
            { config_field_hide!(auth_state.form, translate.t(LABEL_SECRET), secret) }
            { config_field!(auth_state.form, translate.t(LABEL_TOKEN_TTL_MINS), token_ttl_mins) }
            { config_field_optional!(auth_state.form, translate.t(LABEL_USERFILE), userfile) }
            </Card>
        </>
        }
    };

    // Edit mode
    let render_edit_mode = || {
        html! {
            <>
            <Card class="tp__config-view__card">
                { edit_field_bool!(webui_state, translate.t(LABEL_ENABLED), enabled, WebUiConfigFormAction::Enabled) }
                { edit_field_bool!(webui_state, translate.t(LABEL_USER_UI_ENABLED), user_ui_enabled, WebUiConfigFormAction::UserUiEnabled) }
                { edit_field_text_option!(webui_state, translate.t(LABEL_PATH), path, WebUiConfigFormAction::Path) }
                { edit_field_text_option!(webui_state, translate.t(LABEL_PLAYER_SERVER), player_server, WebUiConfigFormAction::PlayerServer) }
                { edit_field_number_u64!(webui_state, translate.t(LABEL_KICK_DURATION), kick_secs, WebUiConfigFormAction::KickSecs) }
            </Card>
            <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_CONTENT_SECURITY_POLICY)}</h1>
                    { edit_field_bool!(csp_state, translate.t(LABEL_ENABLED), enabled, CspConfigFormAction::Enabled) }
                    { edit_field_list_option!(csp_state, translate.t(LABEL_CONTENT_SECURITY_POLICY_CUSTOM_ATTRIBUTES), custom_attributes, CspConfigFormAction::CustomAttributes, translate.t("LABEL.ADD_ATTRIBUTE")) }
                </Card>
                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_AUTH)}</h1>
                    { edit_field_bool!(auth_state, translate.t(LABEL_ENABLED), enabled, WebUiAuthConfigFormAction::Enabled) }
                    { edit_field_text!(auth_state, translate.t(LABEL_ISSUER), issuer, WebUiAuthConfigFormAction::Issuer) }
                    { edit_field_text!(auth_state, translate.t(LABEL_SECRET), secret, WebUiAuthConfigFormAction::Secret, true) }
                    { edit_field_number!(auth_state, translate.t(LABEL_TOKEN_TTL_MINS), token_ttl_mins, WebUiAuthConfigFormAction::TokenTtlMins) }
                    { edit_field_text_option!(auth_state, translate.t(LABEL_USERFILE), userfile, WebUiAuthConfigFormAction::Userfile) }
                </Card>
            </>
        }
    };

    html! {
        <div class="tp__webui-config-view tp__config-view-page">
            <div class="tp__config-view-page__title">{translate.t(LABEL_WEB_UI_CONFIG)}</div>
            {
             html_if!(*config_view_ctx.edit_mode, {
                  <div class="tp__webui-config-view__info tp__config-view-page__info">
                    <AppIcon name="Warn"/> <span class="info">{translate.t("INFO.RESTART_TO_APPLY_CHANGES")}</span>
                  </div>
            })}
            <div class="tp__webui-config-view__body tp__config-view-page__body">
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
