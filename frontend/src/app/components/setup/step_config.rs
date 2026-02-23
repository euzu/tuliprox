use super::setup_helpers::{
    build_setup_app_config, collect_setup_warnings, format_setup_error_message, move_to_next_step,
    move_to_previous_step, prepare_config_and_api_proxy,
};
use crate::{
    app::{
        components::{
            config::{
                ApiConfigView, ConfigForm, ConfigViewContext, HdHomerunConfigView, IpCheckConfigView,
                LibraryConfigView, LogConfigView, MainConfigView, MessagingConfigView, ProxyConfigView,
                ReverseProxyConfigView, SchedulesConfigView, VideoConfigView, WebUiConfigView,
            },
            Card, SetupContext, SetupStep, TextButton,
        },
        ConfigContext,
    },
    hooks::use_service_context,
};
use std::rc::Rc;
use yew::prelude::*;
use yew_i18n::use_translation;

#[derive(Properties, Clone, PartialEq)]
pub struct ConfigStepProps {
    pub step: SetupStep,
}

fn render_config_page(step: SetupStep) -> Html {
    match step {
        SetupStep::Api => html! { <ApiConfigView/> },
        SetupStep::WebUi => html! { <WebUiConfigView/> },
        SetupStep::Main => html! { <MainConfigView/> },
        SetupStep::Log => html! { <LogConfigView/> },
        SetupStep::Messaging => html! { <MessagingConfigView/> },
        SetupStep::ReverseProxy => html! { <ReverseProxyConfigView/> },
        SetupStep::Proxy => html! { <ProxyConfigView/> },
        SetupStep::IpCheck => html! { <IpCheckConfigView/> },
        SetupStep::Video => html! { <VideoConfigView/> },
        SetupStep::HdHomerun => html! { <HdHomerunConfigView/> },
        SetupStep::Library => html! { <LibraryConfigView/> },
        SetupStep::Schedules => html! { <SchedulesConfigView/> },
        _ => html! {},
    }
}

#[function_component]
pub fn ConfigStep(props: &ConfigStepProps) -> Html {
    let step = props.step;
    let translate = use_translation();
    let setup_ctx = use_context::<SetupContext>().expect("Setup context not found");
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let services = use_service_context();
    let edit_mode = use_state(|| true);

    let on_form_change = {
        let setup_ctx = setup_ctx.clone();
        Callback::from(move |form_data: ConfigForm| {
            if !form_data.is_modified() {
                return;
            }

            let current_slot = match &form_data {
                ConfigForm::Main(_, _) => setup_ctx.config_forms.slots.main.as_ref(),
                ConfigForm::Api(_, _) => setup_ctx.config_forms.slots.api.as_ref(),
                ConfigForm::ApiProxy(_, _) => setup_ctx.config_forms.slots.api_proxy.as_ref(),
                ConfigForm::Log(_, _) => setup_ctx.config_forms.slots.log.as_ref(),
                ConfigForm::Schedules(_, _) => setup_ctx.config_forms.slots.schedules.as_ref(),
                ConfigForm::Video(_, _) => setup_ctx.config_forms.slots.video.as_ref(),
                ConfigForm::Messaging(_, _) => setup_ctx.config_forms.slots.messaging.as_ref(),
                ConfigForm::WebUi(_, _) => setup_ctx.config_forms.slots.web_ui.as_ref(),
                ConfigForm::ReverseProxy(_, _) => setup_ctx.config_forms.slots.reverse_proxy.as_ref(),
                ConfigForm::HdHomerun(_, _) => setup_ctx.config_forms.slots.hd_homerun.as_ref(),
                ConfigForm::Proxy(_, _) => setup_ctx.config_forms.slots.proxy.as_ref(),
                ConfigForm::IpCheck(_, _) => setup_ctx.config_forms.slots.ipcheck.as_ref(),
                ConfigForm::Panel(_, _) => setup_ctx.config_forms.slots.panel.as_ref(),
                ConfigForm::Library(_, _) => setup_ctx.config_forms.slots.library.as_ref(),
            };
            if current_slot == Some(&form_data) {
                return;
            }

            let mut next_form_state = (*setup_ctx.config_forms).clone();
            next_form_state.update_form(form_data);
            setup_ctx.config_forms.set(next_form_state);
        })
    };

    let local_app_config = use_memo(
        (config_ctx.clone(), (*setup_ctx.config_forms).clone(), (*setup_ctx.sources).clone()),
        |(config_ctx, form_state, sources)| build_setup_app_config(config_ctx, form_state, sources.clone()),
    );

    let local_config_context = ConfigContext {
        config: Some(Rc::new((*local_app_config).clone())),
        api_proxy: local_app_config.api_proxy.as_ref().map(|api_proxy| Rc::new(api_proxy.clone())),
    };

    let handle_previous = {
        let setup_ctx = setup_ctx.clone();
        Callback::from(move |_| move_to_previous_step(&setup_ctx, step))
    };

    let handle_next = {
        let setup_ctx = setup_ctx.clone();
        let config_ctx = config_ctx.clone();
        let services = services.clone();
        Callback::from(move |_| {
            let mut app_config =
                build_setup_app_config(&config_ctx, &setup_ctx.config_forms, (*setup_ctx.sources).clone());
            for warning in collect_setup_warnings(&app_config) {
                services.toastr.warning(warning);
            }

            if let Err(err) = prepare_config_and_api_proxy(&mut app_config) {
                services.toastr.error(format_setup_error_message(err));
                return;
            }

            setup_ctx.submit_error.set(None);
            move_to_next_step(&setup_ctx, step);
        })
    };

    let context = ConfigViewContext { edit_mode: edit_mode.clone(), show_restart_notice: false, on_form_change };
    let next_title = step.next().map_or_else(
        || translate.t("SETUP.LABEL.NEXT"),
        |next| format!("{}: {}", translate.t("SETUP.LABEL.NEXT"), translate.t(next.title_key())),
    );

    if step.config_page().is_none() {
        html! {}
    } else {
        html! {
            <ContextProvider<ConfigViewContext> context={context}>
                <ContextProvider<ConfigContext> context={local_config_context}>
                    <div class="tp__setup__step tp__setup__step-config">
                        <Card>
                            <div class="tp__config-view__header">
                                <h1>{translate.t(step.title_key())}</h1>
                            </div>
                            <div class="tp__config-view__body">
                                <div class="tp__webui-config-view__info tp__config-view-page__info">
                                    <span class="info">
                                        {format!(
                                            "Step {}/{}: {}",
                                            step.position(),
                                            SetupStep::total(),
                                            translate.t(step.description_key())
                                        )}
                                    </span>
                                </div>
                                {render_config_page(step)}
                            </div>
                            <div class="tp__config-view__toolbar tp__form-page__toolbar">
                                <TextButton
                                    class="secondary"
                                    name="setup_config_previous"
                                    icon="ArrowLeft"
                                    title={translate.t("SETUP.LABEL.BACK")}
                                    onclick={handle_previous}
                                />
                                <TextButton
                                    class="primary"
                                    name="setup_config_next"
                                    icon="ArrowRight"
                                    title={next_title}
                                    onclick={handle_next}
                                />
                            </div>
                        </Card>
                    </div>
                </ContextProvider<ConfigContext>>
            </ContextProvider<ConfigViewContext>>
        }
    }
}
