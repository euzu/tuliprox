use web_sys::HtmlInputElement;

use crate::app::components::svg_icon::AppIcon;
use crate::hooks::use_service_context;
use yew::prelude::*;
use yew_hooks::use_async;
use yew_i18n::use_translation;
use crate::app::components::floating_background::FloatingBackground;
use crate::app::components::input::Input;
use crate::app::components::TextButton;

#[function_component]
pub fn Login() -> Html {
    let services = use_service_context();
    let username_ref = use_node_ref();
    let password_ref = use_node_ref();
    let auth_success = use_state(|| true);
    let translation = use_translation();

    let app_title = services.config.ui_config.app_title.as_ref().map_or("tuliprox", |v| v.as_str());

    let services_ctx = services.clone();
    let app_logo = use_memo(services_ctx,|service| {
        match service.config.ui_config.app_logo.as_ref() {
            Some(logo) => html! { <img src={logo.to_string()} alt="logo"/> },
            None => html! { <AppIcon name="Logo"  width={"48"} height={"48"}/> },
        }
    });

    let authenticate = {
        let services_ctx = services.clone();
        let authorized_state = auth_success.clone();
        let u_ref = username_ref.clone();
        let p_ref = password_ref.clone();
        use_async(async move {
            let username_input: HtmlInputElement = u_ref.cast::<HtmlInputElement>().unwrap();
            let password_input: HtmlInputElement = p_ref.cast::<HtmlInputElement>().unwrap();
            let username = username_input.value();
            let password = password_input.value();
            let result = services_ctx.auth.authenticate(username, password).await;
            match &result  {
                Ok(_token) => {
                    authorized_state.set(true)
                }
                Err(_) => {authorized_state.set(false);}
            }
            result
        })
    };


    let handle_login = {
        let authenticator = authenticate.clone();
        Callback::from(move |_: String| {
            authenticator.run();
        })
    };

    let handle_key_down = {
        let authenticator = authenticate.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Enter" {
                e.prevent_default();
                e.stop_propagation();
                authenticator.run();
            }
        })
    };

    {
        let input_ref = username_ref.clone();
        use_effect(move || {
            if let Some(input) = input_ref.cast::<HtmlInputElement>() {
                input.focus().unwrap();
            }
            || ()
        });
    }

    html! {
        <>
        <FloatingBackground />
        <div class="tp__login-view">
           <div class={"tp__login-view__header"}>
                <div class={"tp__login-view__header-logo"}>{app_logo.as_ref().clone()}</div>
                <div class={"tp__login-view__header-title"}>{ format!("{app_title}") }</div>
            </div>
            <div class="tp__login-view__message">{translation.t("MESSAGES.LOGIN.MESSAGE")}</div>
            <form>
                <div class="tp__login-view__form">
                    <Input placeholder={translation.t("LABEL.USERNAME")} input_ref={username_ref} name="username" autocomplete={true} icon="User"/>
                    <Input placeholder={translation.t("LABEL.PASSWORD")} input_ref={password_ref} name="password" hidden={true}  autocomplete={false} onkeydown={handle_key_down} icon="Lock"/>
                    <div class="tp__login-view__form-action">
                        <TextButton class="primary" name="login" title={ translation.t("LABEL.LOGIN")} onclick={handle_login}></TextButton>
                        <span class={if *auth_success { "tp__hidden" }  else { "tp__error-text" }}>{ "Failed to login" }</span>
                    </div>
                </div>
            </form>
        </div>
        </>
    }
}
