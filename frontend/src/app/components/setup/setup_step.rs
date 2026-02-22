use super::{
    step_api_users::ApiUsersStep, step_config::ConfigStep, step_finish::FinishStep, step_sources::SourcesStep,
    step_welcome::WelcomeStep, SetupConfigFormState, SetupContext, SetupStep,
};
use crate::app::{components::Panel, ConfigContext};
use yew::prelude::*;

#[function_component]
pub fn Setup() -> Html {
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

    let active_step_value = (*active_step).to_string();
    let active_panel = match *active_step {
        SetupStep::Welcome => html! {
            <Panel value={SetupStep::Welcome.to_string()} active={active_step_value.clone()}>
                <WelcomeStep/>
            </Panel>
        },
        SetupStep::Api => html! {
            <Panel value={SetupStep::Api.to_string()} active={active_step_value.clone()}>
                <ConfigStep step={SetupStep::Api}/>
            </Panel>
        },
        SetupStep::WebUi => html! {
            <Panel value={SetupStep::WebUi.to_string()} active={active_step_value.clone()}>
                <ConfigStep step={SetupStep::WebUi}/>
            </Panel>
        },
        SetupStep::Main => html! {
            <Panel value={SetupStep::Main.to_string()} active={active_step_value.clone()}>
                <ConfigStep step={SetupStep::Main}/>
            </Panel>
        },
        SetupStep::Log => html! {
            <Panel value={SetupStep::Log.to_string()} active={active_step_value.clone()}>
                <ConfigStep step={SetupStep::Log}/>
            </Panel>
        },
        SetupStep::Messaging => html! {
            <Panel value={SetupStep::Messaging.to_string()} active={active_step_value.clone()}>
                <ConfigStep step={SetupStep::Messaging}/>
            </Panel>
        },
        SetupStep::ReverseProxy => html! {
            <Panel value={SetupStep::ReverseProxy.to_string()} active={active_step_value.clone()}>
                <ConfigStep step={SetupStep::ReverseProxy}/>
            </Panel>
        },
        SetupStep::Proxy => html! {
            <Panel value={SetupStep::Proxy.to_string()} active={active_step_value.clone()}>
                <ConfigStep step={SetupStep::Proxy}/>
            </Panel>
        },
        SetupStep::IpCheck => html! {
            <Panel value={SetupStep::IpCheck.to_string()} active={active_step_value.clone()}>
                <ConfigStep step={SetupStep::IpCheck}/>
            </Panel>
        },
        SetupStep::Video => html! {
            <Panel value={SetupStep::Video.to_string()} active={active_step_value.clone()}>
                <ConfigStep step={SetupStep::Video}/>
            </Panel>
        },
        SetupStep::HdHomerun => html! {
            <Panel value={SetupStep::HdHomerun.to_string()} active={active_step_value.clone()}>
                <ConfigStep step={SetupStep::HdHomerun}/>
            </Panel>
        },
        SetupStep::Library => html! {
            <Panel value={SetupStep::Library.to_string()} active={active_step_value.clone()}>
                <ConfigStep step={SetupStep::Library}/>
            </Panel>
        },
        SetupStep::Sources => html! {
            <Panel value={SetupStep::Sources.to_string()} active={active_step_value.clone()}>
                <SourcesStep/>
            </Panel>
        },
        SetupStep::ApiUsers => html! {
            <Panel value={SetupStep::ApiUsers.to_string()} active={active_step_value.clone()}>
                <ApiUsersStep/>
            </Panel>
        },
        SetupStep::Schedules => html! {
            <Panel value={SetupStep::Schedules.to_string()} active={active_step_value.clone()}>
                <ConfigStep step={SetupStep::Schedules}/>
            </Panel>
        },
        SetupStep::Finish => html! {
            <Panel value={SetupStep::Finish.to_string()} active={active_step_value}>
                <FinishStep/>
            </Panel>
        },
    };

    html! {
        <ContextProvider<SetupContext> context={context}>
            <div class="tp__setup-assistant">
                <aside class="tp__setup-sidebar">
                    <div class="tp__setup-sidebar__title">{"Setup Steps"}</div>
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
                                            <span class="tp__setup-sidebar__label">{step.title()}</span>
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
