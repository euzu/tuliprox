use super::setup_helpers::{move_to_next_step, validate_credentials};
use crate::{
    app::components::{input::Input, Card, SetupContext, SetupStep, TextButton},
    hooks::use_service_context,
    i18n::use_translation,
};
use yew::prelude::*;

const LABEL_SETUP_WELCOME: &str = "SETUP.LABEL.WELCOME";
const LABEL_SETUP_STEP: &str = "SETUP.LABEL.STEP";
const LABEL_SETUP_WEBUI_USERNAME: &str = "SETUP.LABEL.WEBUI_USERNAME";
const LABEL_SETUP_WEBUI_PASSWORD: &str = "SETUP.LABEL.WEBUI_PASSWORD";
const LABEL_SETUP_WEBUI_PASSWORD_REPEAT: &str = "SETUP.LABEL.WEBUI_PASSWORD_REPEAT";

#[component]
pub fn WelcomeStep() -> Html {
    let setup_ctx = use_context::<SetupContext>().expect("Setup context not found");
    let services = use_service_context();
    let translate = use_translation();

    let handle_next = {
        let setup_ctx = setup_ctx.clone();
        let services = services.clone();
        let translate = translate.clone();
        Callback::from(move |_| {
            let username = setup_ctx.setup_username.trim().to_string();
            let password = (*setup_ctx.setup_password).clone();
            let password_repeat = (*setup_ctx.setup_password_repeat).clone();

            if let Err(err) = validate_credentials(&username, &password, Some(&password_repeat)) {
                services.toastr.error(translate.t(err.i18n_key()));
                return;
            }

            setup_ctx.setup_username.set(username);
            setup_ctx.submit_error.set(None);
            move_to_next_step(&setup_ctx, SetupStep::Welcome);
        })
    };
    let step_message = format!(
        "{} {}/{}: {}",
        translate.t(LABEL_SETUP_STEP),
        SetupStep::Welcome.position(),
        SetupStep::total(),
        translate.t(SetupStep::Welcome.description_key())
    );
    let next_title = SetupStep::Welcome
        .next()
        .map(|next_step| format!("{}: {}", translate.t("SETUP.LABEL.NEXT"), translate.t(next_step.title_key())))
        .unwrap_or_else(|| translate.t("SETUP.LABEL.NEXT"));

    html! {
        <div class="tp__setup__step tp__setup__step-welcome">
            <Card>
                <div class="tp__config-view__header">
                    <h1>{translate.t(LABEL_SETUP_WELCOME)}</h1>
                </div>
                <div class="tp__config-view__body">
                    <div class="tp__webui-config-view__info tp__config-view-page__info">
                        <span class="info">
                            {step_message}
                        </span>
                    </div>
                    <div class="tp__config-view-page">
                        <Input
                            name="setup_username"
                            label={Some(translate.t(LABEL_SETUP_WEBUI_USERNAME).to_string())}
                            value={(*setup_ctx.setup_username).clone()}
                            on_change={Some({
                                let setup_ctx = setup_ctx.clone();
                                Callback::from(move |value: String| setup_ctx.setup_username.set(value))
                            })}
                        />
                        <Input
                            name="setup_password"
                            label={Some(translate.t(LABEL_SETUP_WEBUI_PASSWORD).to_string())}
                            hidden={true}
                            value={(*setup_ctx.setup_password).clone()}
                            on_change={Some({
                                let setup_ctx = setup_ctx.clone();
                                Callback::from(move |value: String| setup_ctx.setup_password.set(value))
                            })}
                        />
                        <Input
                            name="setup_password_repeat"
                            label={Some(translate.t(LABEL_SETUP_WEBUI_PASSWORD_REPEAT).to_string())}
                            hidden={true}
                            value={(*setup_ctx.setup_password_repeat).clone()}
                            on_change={Some({
                                let setup_ctx = setup_ctx.clone();
                                Callback::from(move |value: String| setup_ctx.setup_password_repeat.set(value))
                            })}
                        />
                    </div>
                </div>
                <div class="tp__config-view__toolbar tp__form-page__toolbar">
                    <TextButton
                            class="primary"
                            name="setup_welcome_next"
                            icon="ArrowRight"
                            title={next_title}
                            onclick={handle_next}
                        />
                    </div>
            </Card>
        </div>
    }
}
