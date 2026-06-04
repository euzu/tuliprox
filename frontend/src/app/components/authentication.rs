use crate::{
    app::{components::login::Login, AppRoute},
    hooks::use_service_context,
    i18n::use_translation,
};
use gloo_timers::callback::Timeout;
use shared::model::permission::Permission;
use std::future;
use yew::{prelude::*, suspense::use_future};
use yew_hooks::{use_async_with_options, UseAsyncOptions};
use yew_router::prelude::use_navigator;

fn should_connect_websocket(success: bool, setup_mode: bool, can_read_system: bool) -> bool {
    success && !setup_mode && can_read_system
}

const SESSION_EXPIRY_SKEW_SECS: i64 = 30;

#[derive(Properties, Clone, PartialEq)]
pub struct AuthenticationProps {
    pub children: Children,
}

#[component]
pub fn Authentication(props: &AuthenticationProps) -> Html {
    let services = use_service_context();
    let loading = use_state(|| true);
    let authenticated = use_state(|| false);
    let navigator = use_navigator();
    let translate = use_translation();

    {
        let services_ctx = services.clone();
        let authenticated_state = authenticated.clone();
        let _ = use_future(|| async move {
            services_ctx
                .auth
                .auth_subscribe(&mut |success| {
                    authenticated_state.set(success);
                    if should_connect_websocket(
                        success,
                        services_ctx.config.ui_config.setup_mode,
                        services_ctx.auth.has_permission(Permission::SystemRead),
                    ) {
                        services_ctx.websocket.connect_ws_with_backoff();
                    }
                    future::ready(())
                })
                .await
        });
    }

    {
        let services_ctx = services.clone();
        let authenticated_state = authenticated.clone();
        let loading_state = loading.clone();
        use_async_with_options(
            async move {
                let result = services_ctx.auth.refresh().await;
                let success = result.is_ok();
                authenticated_state.set(success);
                loading_state.set(false);
                result
            },
            UseAsyncOptions::enable_auto(),
        );
    }

    {
        let navigator = navigator.clone();
        use_effect_with((*loading, *authenticated), move |(loading, authenticated)| {
            if !*loading && !*authenticated {
                if let Some(navigator) = navigator.clone() {
                    navigator.replace(&AppRoute::Home);
                }
            }
            || ()
        });
    }

    {
        let services_ctx = services.clone();
        let translate = translate.clone();
        let token_exp = services.auth.token_exp_timestamp();
        use_effect_with((*authenticated, token_exp), move |(authenticated, token_exp)| {
            let mut timeout: Option<Timeout> = None;
            if *authenticated {
                if let Some(exp) = *token_exp {
                    let now_secs = js_sys::Date::now() / 1000.0;
                    let remaining_secs = exp - SESSION_EXPIRY_SKEW_SECS - now_secs as i64;
                    let delay_ms = u32::try_from(remaining_secs.max(0) * 1000).unwrap_or(u32::MAX);
                    timeout = Some(Timeout::new(delay_ms, move || {
                        services_ctx.auth.logout();
                        services_ctx.toastr.warning(translate.t("MESSAGES.SESSION.EXPIRED"));
                    }));
                }
            }
            move || {
                drop(timeout);
            }
        });
    }

    if *loading {
        html! {}
    } else if *authenticated {
        html! {
            { for props.children.iter() }
        }
    } else {
        html! {<Login/>}
    }
}

#[cfg(test)]
mod tests {
    use super::should_connect_websocket;

    #[test]
    fn websocket_connects_only_for_authenticated_non_setup_users_with_system_read() {
        assert!(should_connect_websocket(true, false, true));
        assert!(!should_connect_websocket(false, false, true));
        assert!(!should_connect_websocket(true, true, true));
        assert!(!should_connect_websocket(true, false, false));
    }
}
