use crate::{app::components::login::Login, hooks::use_service_context};
use shared::model::permission::Permission;
use std::future;
use yew::{prelude::*, suspense::use_future};
use yew_hooks::{use_async_with_options, UseAsyncOptions};

fn should_connect_websocket(success: bool, setup_mode: bool, can_read_system: bool) -> bool {
    success && !setup_mode && can_read_system
}

#[derive(Properties, Clone, PartialEq)]
pub struct AuthenticationProps {
    pub children: Children,
}

#[component]
pub fn Authentication(props: &AuthenticationProps) -> Html {
    let services = use_service_context();
    let loading = use_state(|| true);
    let authenticated = use_state(|| false);

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
