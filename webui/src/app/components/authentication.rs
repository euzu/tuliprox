use crate::app::components::login::Login;
use crate::hooks::use_service_context;
use std::future;
use yew::prelude::*;
use yew::suspense::use_future;
use yew_hooks::{use_async_with_options, UseAsyncOptions};

#[derive(Properties, Clone, PartialEq)]
pub struct AuthenticationProps {
    pub children: Children,
}

#[function_component]
pub fn Authentication(props: &AuthenticationProps) -> Html {
    let services = use_service_context();
    let loading = use_state(|| true);
    let authenticated = use_state(|| false);

    {
        let services_ctx = services.clone();
        let authenticated_state = authenticated.clone();
        let _ = use_future(|| async move {
            services_ctx.auth.auth_subscribe(
                &mut |success| {
                    authenticated_state.set(success);
                    future::ready(())
                }
            ).await
        });
    }

    {
        let services_ctx = services.clone();
        let authenticated_state = authenticated.clone();
        let loading_state = loading.clone();
        use_async_with_options(async move {
            let result = services_ctx.auth.refresh().await;
            let success = result.is_ok();
            authenticated_state.set(success);
            loading_state.set(false);
            result
        }, UseAsyncOptions::enable_auto());
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