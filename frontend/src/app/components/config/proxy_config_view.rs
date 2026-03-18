use crate::{
    app::{
        components::config::{
            config_page::{ConfigForm, LABEL_PROXY_CONFIG},
            config_view_context::ConfigViewContext,
            use_emit_config_form,
        },
        context::ConfigContext,
    },
    config_field, config_field_optional, config_field_optional_hide, edit_field_text, edit_field_text_option,
    generate_form_reducer,
    i18n::use_translation,
};
use shared::model::ProxyConfigDto;
use yew::prelude::*;

const LABEL_URL: &str = "LABEL.URL";
const LABEL_USERNAME: &str = "LABEL.USERNAME";
const LABEL_PASSWORD: &str = "LABEL.PASSWORD";

generate_form_reducer!(
    state: ProxyConfigFormState { form: ProxyConfigDto },
    action_name: ProxyConfigFormAction,
    fields {
        Url => url: String,
        Username => username: Option<String>,
        Password => password: Option<String>,
    }
);

#[component]
pub fn ProxyConfigView() -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let config_view_ctx = use_context::<ConfigViewContext>().expect("ConfigViewContext not found");

    let form_state: UseReducerHandle<ProxyConfigFormState> =
        use_reducer(|| ProxyConfigFormState { form: ProxyConfigDto::default(), modified: false });

    {
        use_emit_config_form(&form_state, config_view_ctx.on_form_change.clone(), ConfigForm::Proxy);
    }

    {
        let form_state = form_state.clone();
        let proxy_config = config_ctx.config.as_ref().and_then(|c| c.config.proxy.clone());
        use_effect_with((proxy_config, *config_view_ctx.edit_mode), move |(proxy_cfg, _mode)| {
            if let Some(proxy) = proxy_cfg {
                form_state.dispatch(ProxyConfigFormAction::SetAll((*proxy).clone()));
            } else {
                form_state.dispatch(ProxyConfigFormAction::SetAll(ProxyConfigDto::default()));
            }
            || ()
        });
    }

    let render_view_mode = || {
        html! {
            <div class="tp__proxy-config-config-view__body tp__config-view-page__body">
                { config_field!(form_state.form, translate.t(LABEL_URL), url) }
                { config_field_optional!(form_state.form, translate.t(LABEL_USERNAME), username) }
                { config_field_optional_hide!(form_state.form, translate.t(LABEL_PASSWORD), password) }
            </div>
        }
    };

    let render_edit_mode = || {
        html! {
            <div class="tp__proxy-config-config-view__body tp__config-view-page__body">
                { edit_field_text!(form_state, translate.t(LABEL_URL), url, ProxyConfigFormAction::Url) }
                { edit_field_text_option!(form_state, translate.t(LABEL_USERNAME), username, ProxyConfigFormAction::Username) }
                { edit_field_text_option!(form_state, translate.t(LABEL_PASSWORD), password, ProxyConfigFormAction::Password) }
            </div>
        }
    };

    html! {
        <div class="tp__proxy-config-view tp__config-view-page">
            <div class="tp__config-view-page__title">{translate.t(LABEL_PROXY_CONFIG)}</div>
            {
                if *config_view_ctx.edit_mode {
                    render_edit_mode()
                } else {
                    render_view_mode()
                }
            }
        </div>
    }
}
