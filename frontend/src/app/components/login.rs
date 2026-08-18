use crate::{
    app::components::{
        input::Input, svg_icon::AppIcon, theme::Theme, LanguagePicker, ParticleFlowBackground, TextButton, ThemePicker,
    },
    hooks::use_service_context,
    i18n::use_translation,
};
use web_sys::HtmlInputElement;
use yew::prelude::*;
use yew_hooks::use_async;

#[component]
pub fn Login() -> Html {
    let services = use_service_context();
    let username_ref = use_node_ref();
    let password_ref = use_node_ref();
    let auth_success = use_state(|| true);
    let translation = use_translation();
    let theme = use_state(Theme::get_current_theme);

    let app_title = services.config.ui_config.app_title.as_ref().map_or("tuliprox", |v| v.as_str());

    let services_ctx = services.clone();
    let app_logo = use_memo(services_ctx, |service| {
        let alt = format!("{} logo", service.config.ui_config.app_title.as_deref().unwrap_or("tuliprox"));
        match service.config.ui_config.app_logo.as_ref() {
            Some(logo) => html! { <img src={logo.to_string()} alt={alt}/> },
            None => html! { <AppIcon name="Logo"  width={"48"} height={"48"}/> },
        }
    });

    let authenticate = {
        let services_ctx = services.clone();
        let authorized_state = auth_success.clone();
        let u_ref = username_ref.clone();
        let p_ref = password_ref.clone();
        use_async(async move {
            let username = u_ref.cast::<HtmlInputElement>().map(|input| input.value()).unwrap_or_default();
            let password = p_ref.cast::<HtmlInputElement>().map(|input| input.value()).unwrap_or_default();
            let result = services_ctx.auth.authenticate(username, password).await;
            match &result {
                Ok(_token) => authorized_state.set(true),
                Err(_) => {
                    authorized_state.set(false);
                }
            }
            result
        })
    };

    let handle_login = {
        let authenticator = authenticate.clone();
        Callback::from(move |_: String| {
            if !authenticator.loading {
                authenticator.run();
            }
        })
    };

    let handle_key_down = {
        let authenticator = authenticate.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Enter" {
                e.prevent_default();
                e.stop_propagation();
                if !authenticator.loading {
                    authenticator.run();
                }
            }
        })
    };

    let handle_theme_select = {
        let theme = theme.clone();
        Callback::from(move |new_theme: Theme| {
            if new_theme == *theme {
                return;
            }
            new_theme.switch_theme();
            theme.set(new_theme);
        })
    };

    {
        let input_ref = username_ref.clone();
        use_effect(move || {
            if let Some(input) = input_ref.cast::<HtmlInputElement>() {
                let _ = input.focus();
            }
        });
    }

    html! {
        <>
        <ParticleFlowBackground />
        <div class="tp__login-view">
           <div class="tp__login-view__toolbar">
                <LanguagePicker />
                <ThemePicker theme={*theme} on_select={handle_theme_select} />
           </div>
           <div class={"tp__login-view__header"}>
                <div class={"tp__login-view__header-logo"}>{app_logo.as_ref().clone()}</div>
                <div class={"tp__login-view__header-title"}>{ app_title.to_string() }</div>
            </div>
            <div class="tp__login-view__message">{translation.t("MESSAGES.LOGIN.MESSAGE")}</div>
            <form>
                <div class="tp__login-view__form">
                    <Input placeholder={translation.t("LABEL.USERNAME")} input_ref={username_ref} name="username" autocomplete={true} onkeydown={handle_key_down.clone()} icon="User"/>
                    <Input placeholder={translation.t("LABEL.PASSWORD")} input_ref={password_ref} name="password" hidden={true}  autocomplete={false} onkeydown={handle_key_down} icon="Lock"/>
                    <div class="tp__login-view__form-action">
                        <TextButton class="primary" name="login" disabled={authenticate.loading} title={ translation.t("LABEL.LOGIN")} onclick={handle_login}></TextButton>
                        <span role="alert" class={if *auth_success { "tp__hidden" }  else { "tp__error-text" }}>{ translation.t("MESSAGES.LOGIN.FAILED") }</span>
                    </div>
                </div>
            </form>
        </div>
        </>
    }
}
