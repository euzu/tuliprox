use crate::{
    app::{
        components::{
            config::{
                config_page::{ConfigForm, LABEL_WEB_UI_CONFIG},
                config_view_context::ConfigViewContext,
                use_emit_mapped, HasFormData,
            },
            AppIcon, Card, Chip, DropDownOption, DropDownSelection, Select,
        },
        context::ConfigContext,
    },
    config_field, config_field_bool, config_field_child, config_field_custom, config_field_hide, config_field_optional,
    edit_field_bool, edit_field_list_option, edit_field_number, edit_field_number_u64, edit_field_text,
    edit_field_text_option, generate_form_reducer, html_if,
    i18n::use_translation,
};
use shared::model::{
    view_type::ViewType, ContentSecurityPolicyConfigDto, StreamInfoConfigDto, WebAuthConfigDto, WebUiConfigDto,
};
use strum::IntoEnumIterator;
use yew::prelude::*;

// Labels
const LABEL_AUTH: &str = "LABEL.AUTH";
const LABEL_ENABLED: &str = "LABEL.ENABLED";
const LABEL_ISSUER: &str = "LABEL.ISSUER";
const LABEL_SECRET: &str = "LABEL.SECRET";
const LABEL_TOKEN_TTL_MINS: &str = "LABEL.TOKEN_TTL_MINS";
const LABEL_USERFILE: &str = "LABEL.USERFILE";
const LABEL_GROUPFILE: &str = "LABEL.GROUPFILE";
const LABEL_PLAYER_SERVER: &str = "LABEL.PLAYER_SERVER";
const LABEL_KICK_DURATION: &str = "LABEL.KICK_DURATION";
const LABEL_USER_UI_ENABLED: &str = "LABEL.USER_UI_ENABLED";
const LABEL_CONTENT_SECURITY_POLICY: &str = "LABEL.CONTENT_SECURITY_POLICY";
const LABEL_CONTENT_SECURITY_POLICY_CUSTOM_ATTRIBUTES: &str = "LABEL.CUSTOM_ATTRIBUTES";
const LABEL_PATH: &str = "LABEL.PATH";
const LABEL_COMBINE_VIEWS_STATS_STREAMS: &str = "LABEL.COMBINE_VIEWS_STATS_STREAMS";
const LABEL_LANDING_PAGE: &str = "LABEL.LANDING_PAGE";
const LABEL_STREAM_INFO: &str = "LABEL.STREAM_INFO";
const LABEL_HIDE_GROUP: &str = "LABEL.HIDE_GROUP";
const LABEL_HIDE_IP: &str = "LABEL.HIDE_IP";
const LABEL_HIDE_COUNTRY: &str = "LABEL.HIDE_COUNTRY";
const LABEL_HIDE_SHARED: &str = "LABEL.HIDE_SHARED";
const LABEL_HIDE_DURATION: &str = "LABEL.HIDE_DURATION";
const LABEL_HIDE_BANDWIDTH: &str = "LABEL.HIDE_BANDWIDTH";
const LABEL_HIDE_TRANSFERRED: &str = "LABEL.HIDE_TRANSFERRED";
const LABEL_HIDE_PLAYER: &str = "LABEL.HIDE_PLAYER";
const LABEL_HIDE_USER_COMMENT: &str = "LABEL.HIDE_USER_COMMENT";
const LABEL_HIDE_EPG: &str = "LABEL.HIDE_EPG";

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
        CombineViewsStatsStreams => combine_views_stats_streams: bool,
        LandingPage => landing_page: shared::model::view_type::ViewType,
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
        Groupfile => groupfile: Option<String>,
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

generate_form_reducer!(
    state: StreamInfoConfigFormState { form: StreamInfoConfigDto },
    action_name: StreamInfoConfigFormAction,
    fields {
        HideGroup => hide_group: bool,
        HideIp => hide_ip: bool,
        HideCountry => hide_country: bool,
        HideShared => hide_shared: bool,
        HideDuration => hide_duration: bool,
        HideBandwidth => hide_bandwidth: bool,
        HideTransferred => hide_transferred: bool,
        HidePlayer => hide_player: bool,
        HideUserComment => hide_user_comment: bool,
        HideEpg => hide_epg: bool,
    }
);

#[component]
pub fn WebUiConfigView() -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let config_view_ctx = use_context::<ConfigViewContext>().expect("ConfigViewContext not found");

    // Local form states
    let webui_state: UseReducerHandle<WebUiConfigFormState> =
        use_reducer(|| WebUiConfigFormState { form: WebUiConfigDto::default(), modified: false });
    let auth_state: UseReducerHandle<WebUiAuthConfigFormState> =
        use_reducer(|| WebUiAuthConfigFormState { form: WebAuthConfigDto::default(), modified: false });
    let csp_state: UseReducerHandle<CspConfigFormState> =
        use_reducer(|| CspConfigFormState { form: ContentSecurityPolicyConfigDto::default(), modified: false });
    let stream_info_state: UseReducerHandle<StreamInfoConfigFormState> =
        use_reducer(|| StreamInfoConfigFormState { form: StreamInfoConfigDto::default(), modified: false });

    let view_types = use_memo(webui_state.data().landing_page, |landing_page| {
        ViewType::iter()
            .collect::<Vec<_>>()
            .iter()
            .map(|view_type| DropDownOption {
                id: view_type.to_string(),
                label: html! { translate.t(&format!("LABEL.VIEW_TYPE_{}", view_type.to_string().to_uppercase()))},
                selected: landing_page == view_type,
            })
            .collect::<Vec<DropDownOption>>()
    });

    // Notify parent when form changes
    {
        let deps = (
            webui_state.form.clone(),
            auth_state.form.clone(),
            csp_state.form.clone(),
            stream_info_state.form.clone(),
            webui_state.modified,
            auth_state.modified,
            csp_state.modified,
            stream_info_state.modified,
        );
        use_emit_mapped(
            deps,
            config_view_ctx.on_form_change.clone(),
            |(
                webui_form,
                auth_form,
                csp_form,
                stream_info_form,
                webui_modified,
                auth_modified,
                csp_modified,
                stream_info_modified,
            )| {
                let mut form = webui_form;
                form.auth = Some(auth_form);
                form.content_security_policy = Some(csp_form);
                form.stream_info = if stream_info_form.is_empty() { None } else { Some(stream_info_form) };

                let modified = webui_modified || auth_modified || csp_modified || stream_info_modified;
                ConfigForm::WebUi(modified, form)
            },
        );
    }

    // Sync from context when config or edit mode changes
    {
        let webui_state = webui_state.clone();
        let auth_state = auth_state.clone();
        let csp_state = csp_state.clone();
        let stream_info_state = stream_info_state.clone();

        let webui_cfg = config_ctx.config.as_ref().and_then(|c| c.config.web_ui.clone());
        use_effect_with((webui_cfg, *config_view_ctx.edit_mode), move |(cfg, _mode)| {
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
                if let Some(stream_info) = &webui.stream_info {
                    stream_info_state.dispatch(StreamInfoConfigFormAction::SetAll(stream_info.clone()));
                } else {
                    stream_info_state.dispatch(StreamInfoConfigFormAction::SetAll(StreamInfoConfigDto::default()));
                }
            } else {
                webui_state.dispatch(WebUiConfigFormAction::SetAll(WebUiConfigDto::default()));
                auth_state.dispatch(WebUiAuthConfigFormAction::SetAll(WebAuthConfigDto::default()));
                csp_state.dispatch(CspConfigFormAction::SetAll(ContentSecurityPolicyConfigDto::default()));
                stream_info_state.dispatch(StreamInfoConfigFormAction::SetAll(StreamInfoConfigDto::default()));
            }
        });
    }

    // View mode
    let render_view_mode = || {
        html! {
        <>
            <Card class="tp__config-view__card">
            { config_field_bool!(webui_state.form, translate.t(LABEL_ENABLED), enabled) }
            { config_field_custom!(translate.t(LABEL_LANDING_PAGE),  translate.t(&format!("LABEL.VIEW_TYPE_{}", webui_state.form.landing_page.to_string().to_uppercase()))) }
            { config_field_bool!(webui_state.form, translate.t(LABEL_USER_UI_ENABLED), user_ui_enabled) }
            { config_field_bool!(webui_state.form, translate.t(LABEL_COMBINE_VIEWS_STATS_STREAMS), combine_views_stats_streams) }
            { config_field_optional!(webui_state.form, translate.t(LABEL_PATH), path) }
            { config_field_optional!(webui_state.form, translate.t(LABEL_PLAYER_SERVER), player_server) }
            { config_field!(webui_state.form, translate.t(LABEL_KICK_DURATION), kick_secs) }
            </Card>

            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_CONTENT_SECURITY_POLICY)}</h1>
                { config_field_bool!(csp_state.form, translate.t(LABEL_ENABLED), enabled) }
                { config_field_child!(translate.t(LABEL_CONTENT_SECURITY_POLICY_CUSTOM_ATTRIBUTES), "WEBUI_CONFIG.CONTENT_SECURITY_POLICY_CUSTOM_ATTRIBUTES", {
                    html! {
                        <div class="tp__config-view__tags">
                            {
                                if let Some(custom) = &csp_state.form.custom_attributes {
                                    html! { for a in custom.iter() { <Chip label={a.clone()} /> } }
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
            { config_field_optional!(auth_state.form, translate.t(LABEL_GROUPFILE), groupfile) }
           </Card>
           <Card class="tp__config-view__card">
            <h1>{translate.t(LABEL_STREAM_INFO)}</h1>
            { config_field_bool!(stream_info_state.form, translate.t(LABEL_HIDE_GROUP), hide_group) }
            { config_field_bool!(stream_info_state.form, translate.t(LABEL_HIDE_IP), hide_ip) }
            { config_field_bool!(stream_info_state.form, translate.t(LABEL_HIDE_COUNTRY), hide_country) }
            { config_field_bool!(stream_info_state.form, translate.t(LABEL_HIDE_SHARED), hide_shared) }
            { config_field_bool!(stream_info_state.form, translate.t(LABEL_HIDE_DURATION), hide_duration) }
            { config_field_bool!(stream_info_state.form, translate.t(LABEL_HIDE_BANDWIDTH), hide_bandwidth) }
            { config_field_bool!(stream_info_state.form, translate.t(LABEL_HIDE_TRANSFERRED), hide_transferred) }
            { config_field_bool!(stream_info_state.form, translate.t(LABEL_HIDE_PLAYER), hide_player) }
            { config_field_bool!(stream_info_state.form, translate.t(LABEL_HIDE_USER_COMMENT), hide_user_comment) }
            { config_field_bool!(stream_info_state.form, translate.t(LABEL_HIDE_EPG), hide_epg) }
           </Card>
        </>
        }
    };

    // Edit mode
    let render_edit_mode = || {
        let webui_state_clone = webui_state.clone();
        html! {
            <>
            <Card class="tp__config-view__card">
                { edit_field_bool!(webui_state, translate.t(LABEL_ENABLED), enabled, WebUiConfigFormAction::Enabled) }
                { config_field_child!(translate.t(LABEL_LANDING_PAGE), "WEB_UI_CONFIG.LANDING_PAGE", {
                   html! { <Select name="landing_page"
                    multi_select={false}
                    on_select={Callback::from(move |(_name, selections):(String, DropDownSelection)| {
                        let view_type = match selections {
                            DropDownSelection::Empty => None,
                            DropDownSelection::Single(option) => option.parse::<ViewType>().ok(),
                            DropDownSelection::Multi(options) => options.first().as_ref().and_then(|f| f.parse::<ViewType>().ok())
                           };
                        webui_state_clone.dispatch(WebUiConfigFormAction::LandingPage(view_type.unwrap_or_else(ViewType::default)));
                    })}
                    options={view_types.clone()}
                    />
                }})}
                { edit_field_bool!(webui_state, translate.t(LABEL_USER_UI_ENABLED), user_ui_enabled, WebUiConfigFormAction::UserUiEnabled) }
                { edit_field_bool!(webui_state, translate.t(LABEL_COMBINE_VIEWS_STATS_STREAMS), combine_views_stats_streams, WebUiConfigFormAction::CombineViewsStatsStreams) }
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
                    { edit_field_text_option!(auth_state, translate.t(LABEL_GROUPFILE), groupfile, WebUiAuthConfigFormAction::Groupfile) }
                </Card>
                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_STREAM_INFO)}</h1>
                    { edit_field_bool!(stream_info_state, translate.t(LABEL_HIDE_GROUP), hide_group, StreamInfoConfigFormAction::HideGroup) }
                    { edit_field_bool!(stream_info_state, translate.t(LABEL_HIDE_IP), hide_ip, StreamInfoConfigFormAction::HideIp) }
                    { edit_field_bool!(stream_info_state, translate.t(LABEL_HIDE_COUNTRY), hide_country, StreamInfoConfigFormAction::HideCountry) }
                    { edit_field_bool!(stream_info_state, translate.t(LABEL_HIDE_SHARED), hide_shared, StreamInfoConfigFormAction::HideShared) }
                    { edit_field_bool!(stream_info_state, translate.t(LABEL_HIDE_DURATION), hide_duration, StreamInfoConfigFormAction::HideDuration) }
                    { edit_field_bool!(stream_info_state, translate.t(LABEL_HIDE_BANDWIDTH), hide_bandwidth, StreamInfoConfigFormAction::HideBandwidth) }
                    { edit_field_bool!(stream_info_state, translate.t(LABEL_HIDE_TRANSFERRED), hide_transferred, StreamInfoConfigFormAction::HideTransferred) }
                    { edit_field_bool!(stream_info_state, translate.t(LABEL_HIDE_PLAYER), hide_player, StreamInfoConfigFormAction::HidePlayer) }
                    { edit_field_bool!(stream_info_state, translate.t(LABEL_HIDE_USER_COMMENT), hide_user_comment, StreamInfoConfigFormAction::HideUserComment) }
                    { edit_field_bool!(stream_info_state, translate.t(LABEL_HIDE_EPG), hide_epg, StreamInfoConfigFormAction::HideEpg) }
                </Card>
            </>
        }
    };

    html! {
        <div class="tp__webui-config-view tp__config-view-page">
            <div class="tp__config-view-page__title">{translate.t(LABEL_WEB_UI_CONFIG)}</div>
            {
             html_if!(*config_view_ctx.edit_mode && config_view_ctx.show_restart_notice, {
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
