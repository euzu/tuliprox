use super::setup_helpers::{
    apply_setup_api_users, build_setup_app_config, collect_setup_warnings, format_setup_error_message,
    move_to_previous_step, prepare_config_and_api_proxy, prepare_sources, validate_credentials,
};
use crate::{
    app::{
        components::{Card, SetupContext, SetupStep, TextButton},
        ConfigContext,
    },
    hooks::use_service_context,
    i18n::use_translation,
    services::{SetupCompleteRequestDto, SetupWebUserCredentialDto},
};
use yew::{platform::spawn_local, prelude::*};

const LABEL_SETUP_FINISH: &str = "SETUP.LABEL.FINISH_SETUP";
const LABEL_SETUP_BACK: &str = "SETUP.LABEL.BACK";
const LABEL_SETUP_STEP: &str = "SETUP.LABEL.STEP";
const LABEL_SETUP_WEBUI_USER: &str = "SETUP.LABEL.WEBUI_USER";
const LABEL_SETUP_API_USERS: &str = "SETUP.LABEL.API_USERS";
const LABEL_SETUP_SUBMITTING: &str = "SETUP.LABEL.SUBMITTING";
const LABEL_SETUP_SUBMITTED: &str = "SETUP.LABEL.SUBMITTED";
const MSG_SETUP_COMPLETE_SUCCESS: &str = "SETUP.MSG.COMPLETE_SUCCESS";
const MSG_SETUP_RESTART_TO_CONTINUE: &str = "SETUP.MSG.RESTART_TO_CONTINUE";
const MSG_SETUP_COMPLETE_RESTART_HINT: &str = "SETUP.MSG.COMPLETE_RESTART_HINT";

#[component]
pub fn FinishStep() -> Html {
    let setup_ctx = use_context::<SetupContext>().expect("Setup context not found");
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let services = use_service_context();
    let translate = use_translation();

    let handle_previous = {
        let setup_ctx = setup_ctx.clone();
        Callback::from(move |_| {
            if !*setup_ctx.is_submitting && !*setup_ctx.is_completed {
                move_to_previous_step(&setup_ctx, SetupStep::Finish);
            }
        })
    };

    let handle_submit = {
        let setup_ctx = setup_ctx.clone();
        let config_ctx = config_ctx.clone();
        let services = services.clone();
        let translate = translate.clone();
        Callback::from(move |_| {
            if *setup_ctx.is_submitting || *setup_ctx.is_completed {
                return;
            }

            let username = setup_ctx.setup_username.trim().to_string();
            let password = (*setup_ctx.setup_password).clone();
            let password_repeat = (*setup_ctx.setup_password_repeat).clone();
            if let Err(err) = validate_credentials(&username, &password, Some(&password_repeat)) {
                services.toastr.error(translate.t(err.i18n_key()));
                return;
            }

            let mut app_config =
                build_setup_app_config(&config_ctx, &setup_ctx.config_forms, (*setup_ctx.sources).clone());
            apply_setup_api_users(&mut app_config, setup_ctx.api_users.as_ref());
            for warning in collect_setup_warnings(&app_config) {
                services.toastr.warning(warning);
            }

            if let Err(err) = prepare_config_and_api_proxy(&mut app_config) {
                let message = format_setup_error_message(err);
                services.toastr.error(message.clone());
                setup_ctx.submit_error.set(Some(message));
                return;
            }

            if let Err(err) = prepare_sources(&mut app_config) {
                let message = format_setup_error_message(err);
                services.toastr.error(message.clone());
                setup_ctx.submit_error.set(Some(message));
                return;
            }

            setup_ctx.submit_error.set(None);
            setup_ctx.is_submitting.set(true);

            let services = services.clone();
            let setup_ctx = setup_ctx.clone();
            let translate_for_submit = translate.clone();
            spawn_local(async move {
                let payload = SetupCompleteRequestDto {
                    app_config,
                    web_users: vec![SetupWebUserCredentialDto { username, password }],
                };

                match services.config.complete_setup(payload).await {
                    Ok(()) => {
                        setup_ctx.is_submitting.set(false);
                        setup_ctx.is_completed.set(true);
                        services.toastr.success(translate_for_submit.t(MSG_SETUP_COMPLETE_SUCCESS));
                        services.toastr.warning(translate_for_submit.t(MSG_SETUP_RESTART_TO_CONTINUE));
                    }
                    Err(err) => {
                        let message = format_setup_error_message(err.to_string());
                        setup_ctx.is_submitting.set(false);
                        setup_ctx.submit_error.set(Some(message.clone()));
                        services.toastr.error(message);
                    }
                }
            });
        })
    };
    let step = SetupStep::Finish;
    let step_message = format!(
        "{} {}/{}: {}",
        translate.t(LABEL_SETUP_STEP),
        step.position(),
        SetupStep::total(),
        translate.t(step.description_key())
    );

    html! {
        <div class="tp__setup__step tp__setup__step-finish">
            <Card>
                <div class="tp__config-view__header">
                    <h1>{translate.t(LABEL_SETUP_FINISH)}</h1>
                </div>
                <div class="tp__config-view__body">
                    <div class="tp__webui-config-view__info tp__config-view-page__info">
                        <span class="info">{step_message}</span>
                    </div>
                    <div class="tp__config-view-page__body">
                        <div>
                            <strong>{format!("{}: ", translate.t(LABEL_SETUP_WEBUI_USER))}</strong>
                            {setup_ctx.setup_username.as_str()}
                        </div>
                        <div>
                            <strong>{format!("{}: ", translate.t("LABEL.INPUTS"))}</strong>
                            {setup_ctx.sources.inputs.len()}
                        </div>
                        <div>
                            <strong>{format!("{}: ", translate.t("LABEL.SOURCES"))}</strong>
                            {setup_ctx.sources.sources.len()}
                        </div>
                        <div>
                            <strong>{format!("{}: ", translate.t(LABEL_SETUP_API_USERS))}</strong>
                            {setup_ctx.api_users.iter().map(|target_user| target_user.credentials.len()).sum::<usize>()}
                        </div>
                        {
                            if let Some(err) = setup_ctx.submit_error.as_ref() {
                                html! {
                                    <div class="tp__webui-config-view__info tp__config-view-page__info">
                                        <span class="error">{err.clone()}</span>
                                    </div>
                                }
                            } else {
                                html! {}
                            }
                        }
                        {
                            if *setup_ctx.is_completed {
                                html! {
                                    <div class="tp__webui-config-view__info tp__config-view-page__info">
                                        <span class="info">{translate.t(MSG_SETUP_COMPLETE_RESTART_HINT)}</span>
                                    </div>
                                }
                            } else {
                                html! {}
                            }
                        }
                    </div>
                </div>
                <div class="tp__config-view__toolbar tp__form-page__toolbar">
                    <TextButton
                        class="secondary"
                        name="setup_finish_previous"
                        icon="ArrowLeft"
                        title={translate.t(LABEL_SETUP_BACK)}
                        onclick={handle_previous}
                    />
                    <TextButton
                        class="primary"
                        name="setup_finish_submit"
                        icon="Save"
                        title={
                            if *setup_ctx.is_submitting {
                                translate.t(LABEL_SETUP_SUBMITTING)
                            } else if *setup_ctx.is_completed {
                                translate.t(LABEL_SETUP_SUBMITTED)
                            } else {
                                translate.t(LABEL_SETUP_FINISH)
                            }
                        }
                        disabled={*setup_ctx.is_submitting || *setup_ctx.is_completed}
                        onclick={handle_submit}
                    />
                </div>
            </Card>
        </div>
    }
}
