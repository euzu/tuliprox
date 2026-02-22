use super::setup_helpers::{move_to_next_step, validate_credentials};
use crate::{
    app::components::{input::Input, Card, SetupContext, SetupStep, TextButton},
    hooks::use_service_context,
};
use yew::prelude::*;

#[function_component]
pub fn WelcomeStep() -> Html {
    let setup_ctx = use_context::<SetupContext>().expect("Setup context not found");
    let services = use_service_context();

    let handle_next = {
        let setup_ctx = setup_ctx.clone();
        let services = services.clone();
        Callback::from(move |_| {
            let username = setup_ctx.setup_username.trim().to_string();
            let password = (*setup_ctx.setup_password).clone();
            let password_repeat = (*setup_ctx.setup_password_repeat).clone();

            if let Err(err) = validate_credentials(&username, &password, Some(&password_repeat)) {
                services.toastr.error(err);
                return;
            }

            setup_ctx.setup_username.set(username);
            setup_ctx.submit_error.set(None);
            move_to_next_step(&setup_ctx, SetupStep::Welcome);
        })
    };
    let step_message = format!(
        "Step {}/{}: create the first WebUI user. These credentials will be written to user.txt.",
        SetupStep::Welcome.position(),
        SetupStep::total()
    );
    let next_title = SetupStep::Welcome
        .next()
        .map(|next_step| format!("Next: {}", next_step.title()))
        .unwrap_or_else(|| "Next".to_string());

    html! {
        <div class="tp__setup__step tp__setup__step-welcome">
            <Card>
                <div class="tp__config-view__header">
                    <h1>{"Initial Setup"}</h1>
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
                            label={Some("WebUI Username".to_string())}
                            value={(*setup_ctx.setup_username).clone()}
                            on_change={Some({
                                let setup_ctx = setup_ctx.clone();
                                Callback::from(move |value: String| setup_ctx.setup_username.set(value))
                            })}
                        />
                        <Input
                            name="setup_password"
                            label={Some("WebUI Password".to_string())}
                            hidden={true}
                            value={(*setup_ctx.setup_password).clone()}
                            on_change={Some({
                                let setup_ctx = setup_ctx.clone();
                                Callback::from(move |value: String| setup_ctx.setup_password.set(value))
                            })}
                        />
                        <Input
                            name="setup_password_repeat"
                            label={Some("Repeat WebUI Password".to_string())}
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
