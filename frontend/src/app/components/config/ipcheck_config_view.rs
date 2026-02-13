use crate::app::components::config::config_page::{ConfigForm, LABEL_IP_CHECK_CONFIG};
use crate::app::components::config::config_view_context::ConfigViewContext;
use crate::app::context::ConfigContext;
use crate::{
    config_field_empty, config_field_optional, edit_field_text_option, generate_form_reducer,
};
use shared::model::IpCheckConfigDto;
use yew::prelude::*;
use yew_i18n::use_translation;
const LABEL_URL: &str = "LABEL.URL";
const LABEL_URL_IPV4: &str = "LABEL.URL_IPV4";
const LABEL_URL_IPV6: &str = "LABEL.URL_IPV6";
const LABEL_PATTERN_IPV4: &str = "LABEL.PATTERN_IPV4";
const LABEL_PATTERN_IPV6: &str = "LABEL.PATTERN_IPV6";

generate_form_reducer!(
    state: IpCheckConfigFormState { form: IpCheckConfigDto },
    action_name: IpCheckConfigFormAction,
    fields {
        Url => url: Option<String>,
        UrlIpV4 => url_ipv4: Option<String>,
        UrlIpV6 => url_ipv6: Option<String>,
        PatternIpV4 => pattern_ipv4: Option<String>,
        PatternIpV6 => pattern_ipv6: Option<String>,
    }
);

#[function_component]
pub fn IpCheckConfigView() -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let config_view_ctx = use_context::<ConfigViewContext>().expect("ConfigViewContext not found");

    let form_state: UseReducerHandle<IpCheckConfigFormState> =
        use_reducer(|| IpCheckConfigFormState {
            form: IpCheckConfigDto::default(),
            modified: false,
        });

    {
        let on_form_change = config_view_ctx.on_form_change.clone();
        let deps = (form_state.clone(), form_state.modified);
        use_effect_with(deps, move |(state, modified)| {
            on_form_change.emit(ConfigForm::IpCheck(*modified, state.form.clone()));
        });
    }

    {
        let form_state = form_state.clone();
        let ipcheck_config = config_ctx
            .config
            .as_ref()
            .and_then(|c| c.config.ipcheck.clone());

        use_effect_with(
            (ipcheck_config, config_view_ctx.edit_mode.clone()),
            move |(ipcheck_cfg, _mode)| {
                if let Some(ipcheck) = ipcheck_cfg {
                    form_state.dispatch(IpCheckConfigFormAction::SetAll((*ipcheck).clone()));
                } else {
                    form_state
                        .dispatch(IpCheckConfigFormAction::SetAll(IpCheckConfigDto::default()));
                }
                || ()
            },
        );
    }

    let render_empty = || {
        html! {
          <>
            { config_field_empty!(translate.t(LABEL_URL)) }
            { config_field_empty!(translate.t(LABEL_URL_IPV4)) }
            { config_field_empty!(translate.t(LABEL_URL_IPV6)) }
            { config_field_empty!(translate.t(LABEL_PATTERN_IPV4)) }
            { config_field_empty!(translate.t(LABEL_PATTERN_IPV6)) }
          </>
        }
    };

    let render_view_mode = || {
        if let Some(config) = &config_ctx.config {
            if let Some(ipcheck) = &config.config.ipcheck {
                html! {
                  <>
                    { config_field_optional!(ipcheck, translate.t(LABEL_URL),  url) }
                    { config_field_optional!(ipcheck, translate.t(LABEL_URL_IPV4),  url_ipv4) }
                    { config_field_optional!(ipcheck, translate.t(LABEL_URL_IPV6),  url_ipv6) }
                    { config_field_optional!(ipcheck, translate.t(LABEL_PATTERN_IPV4),  pattern_ipv4) }
                    { config_field_optional!(ipcheck, translate.t(LABEL_PATTERN_IPV6),  pattern_ipv6) }
                  </>
                }
            } else {
                render_empty()
            }
        } else {
            render_empty()
        }
    };

    let render_edit_mode = || {
        html! {
            <>
            { edit_field_text_option!(form_state, translate.t(LABEL_URL),  url, IpCheckConfigFormAction::Url) }
            { edit_field_text_option!(form_state, translate.t(LABEL_URL_IPV4), url_ipv4, IpCheckConfigFormAction::UrlIpV4) }
            { edit_field_text_option!(form_state, translate.t(LABEL_URL_IPV6), url_ipv6, IpCheckConfigFormAction::UrlIpV6) }
            { edit_field_text_option!(form_state, translate.t(LABEL_PATTERN_IPV4), pattern_ipv4, IpCheckConfigFormAction::PatternIpV4) }
            { edit_field_text_option!(form_state, translate.t(LABEL_PATTERN_IPV6), pattern_ipv6, IpCheckConfigFormAction::PatternIpV6) }
            </>
        }
    };

    html! {
      <div class="tp__ipcheck-config-view tp__config-view-page">
        <div class="tp__config-view-page__title">{translate.t(LABEL_IP_CHECK_CONFIG)}</div>
        <div class="tp__ipcheck-config-view__body tp__config-view-page__body">
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
