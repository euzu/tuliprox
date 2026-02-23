use super::{
    step_api_users::ApiUsersStep, step_config::ConfigStep, step_finish::FinishStep, step_sources::SourcesStep,
    step_welcome::WelcomeStep, SetupConfigFormState, SetupContext, SetupStep,
};
use crate::app::{components::Panel, ConfigContext};
use yew::prelude::*;
use yew_i18n::use_translation;

#[function_component]
pub fn Setup() -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");

    let active_step = use_state(|| SetupStep::Welcome);
    let max_unlocked_step = use_state(|| SetupStep::Welcome);
    let setup_username = use_state(|| "admin".to_string());
    let setup_password = use_state(String::new);
    let setup_password_repeat = use_state(String::new);
    let config_forms = use_state(SetupConfigFormState::default);
    let sources = use_state(|| config_ctx.config.as_ref().map_or_else(Default::default, |cfg| cfg.sources.clone()));
    let api_users = use_state(|| config_ctx.api_proxy.as_ref().map_or_else(Vec::new, |cfg| cfg.user.clone()));
    let is_submitting = use_state(|| false);
    let is_completed = use_state(|| false);
    let submit_error = use_state(|| None::<String>);

    {
        let sources = sources.clone();
        use_effect_with(config_ctx.config.clone(), move |cfg| {
            if let Some(app_cfg) = cfg {
                if sources.inputs.is_empty() && sources.sources.is_empty() {
                    sources.set(app_cfg.sources.clone());
                }
            }
            || ()
        });
    }

    {
        let api_users = api_users.clone();
        use_effect_with(config_ctx.api_proxy.clone(), move |api_proxy_cfg| {
            if let Some(api_proxy) = api_proxy_cfg {
                if api_users.is_empty() {
                    api_users.set(api_proxy.user.clone());
                }
            }
            || ()
        });
    }

    let handle_sidebar_nav = {
        let active_step = active_step.clone();
        let max_unlocked_step = max_unlocked_step.clone();
        Callback::from(move |target_step: SetupStep| {
            if target_step.index() <= max_unlocked_step.index() {
                active_step.set(target_step);
            }
        })
    };

    let context = SetupContext {
        active_step: active_step.clone(),
        max_unlocked_step: max_unlocked_step.clone(),
        setup_username: setup_username.clone(),
        setup_password: setup_password.clone(),
        setup_password_repeat: setup_password_repeat.clone(),
        config_forms: config_forms.clone(),
        sources: sources.clone(),
        api_users: api_users.clone(),
        is_submitting: is_submitting.clone(),
        is_completed: is_completed.clone(),
        submit_error: submit_error.clone(),
    };

    let step = *active_step;
    let value = step.to_string();
    let active_panel = match step {
        SetupStep::Welcome => html! {
            <Panel value={value.clone()} active={value.clone()}>
                <WelcomeStep/>
            </Panel>
        },
        SetupStep::Sources => html! {
            <Panel value={value.clone()} active={value.clone()}>
                <SourcesStep/>
            </Panel>
        },
        SetupStep::ApiUsers => html! {
            <Panel value={value.clone()} active={value.clone()}>
                <ApiUsersStep/>
            </Panel>
        },
        SetupStep::Finish => html! {
            <Panel value={value.clone()} active={value.clone()}>
                <FinishStep/>
            </Panel>
        },
        config_step => html! {
            <Panel value={value.clone()} active={value.clone()}>
                <ConfigStep step={config_step}/>
            </Panel>
        },
    };

    html! {
        <ContextProvider<SetupContext> context={context}>
            <div class="tp__setup-assistant">
                <aside class="tp__setup-sidebar">
                    <div class="tp__setup-sidebar__title">{translate.t("SETUP.LABEL.STEPS")}</div>
                    <ol class="tp__setup-sidebar__list">
                        {
                            for SetupStep::all().iter().copied().map(|step| {
                                let is_active = step == *active_step;
                                let is_unlocked = step.index() <= max_unlocked_step.index();
                                let onclick = {
                                    let handle_sidebar_nav = handle_sidebar_nav.clone();
                                    Callback::from(move |_| handle_sidebar_nav.emit(step))
                                };
                                html! {
                                    <li class="tp__setup-sidebar__item-wrap">
                                        <button
                                            class={classes!(
                                                "tp__setup-sidebar__item",
                                                if is_active { Some("active") } else { None },
                                                if is_unlocked { Some("unlocked") } else { Some("locked") },
                                            )}
                                            disabled={!is_unlocked}
                                            onclick={onclick}
                                        >
                                            <span class="tp__setup-sidebar__index">{step.position()}</span>
                                            <span class="tp__setup-sidebar__label">{translate.t(step.title_key())}</span>
                                        </button>
                                    </li>
                                }
                            })
                        }
                    </ol>
                </aside>
                <div class="tp__setup-content">
                    {active_panel}
                </div>
            </div>
        </ContextProvider<SetupContext>>
    }
}
