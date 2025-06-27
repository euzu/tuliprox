use web_sys::HtmlInputElement;

use crate::app::components::svg_icon::AppIcon;
use crate::hooks::use_service_context;
use yew::prelude::*;
use yew_hooks::use_async;
use yew_i18n::use_translation;
use crate::app::components::input::Input;

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
            let success = result.is_ok();
            authorized_state.set(success);
            result
        })
    };

    let do_login = {
        let authenticator = authenticate.clone();
        Callback::from(move |_| {
            authenticator.run();
        })
    };

    let handle_login = {
        let login = do_login.clone();
        Callback::from(move |e: MouseEvent| {
            e.prevent_default();
            login.emit(());
        })
    };

    let handle_key_down = {
        let login = do_login.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Enter" {
                login.emit(());
            }
        })
    };

    html! {
        <div class="login-view">
           <div class={"login-view__header"}>
                <div class={"login-view__header-logo"}>{app_logo.as_ref().clone()}</div>
                <div class={"login-view__header-title"}>{ format!("Login to {app_title}") }</div>
            </div>
            <form>
                <div class="login-view__form">
                    <Input label={translation.t("LABEL.USERNAME")} input_ref={username_ref} input_type="text" name="username" autocomplete={true} autofocus={true}/>
                    <Input label={translation.t("LABEL.PASSWORD")} input_ref={password_ref} input_type="password" name="password"  autocomplete={true} onkeydown={handle_key_down}/>
                    <div class="login-view__form-action">
                        <button type="button" class="btn" onclick={handle_login}>{"Login"}</button>
                        <span class={if *auth_success { "hidden" }  else { "error-text" }}>{ "Failed to login" }</span>
                    </div>
                </div>
            </form>
        </div>
    }
}
