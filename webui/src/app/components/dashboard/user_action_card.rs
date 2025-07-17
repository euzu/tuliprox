use std::future;
use yew::prelude::*;
use yew::suspense::use_future;
use yew_i18n::use_translation;
use crate::app::components::{ActionCard, TextButton};
use crate::hooks::use_service_context;


#[function_component]
pub fn UserActionCard() -> Html {
    let services = use_service_context();
    let translate = use_translation();
    let username = use_state(String::new);

    let handle_logout = {
        let services_ctx = services.clone();
        Callback::from(move |_| services_ctx.auth.logout())
    };

    {
        let services_ctx = services.clone();
        let authenticated_user  = username.clone();
        let _ = use_future(|| async move {
            services_ctx.auth.auth_subscribe(
                &mut |_success| {
                    authenticated_user.set(services_ctx.auth.get_username());
                    future::ready(())
                }
            ).await
        });
    }

    html! {
        <ActionCard icon="User" title={translate.t("LABEL.WELCOME")}
        subtitle={(*username).clone()}>
          <TextButton name="logout" title={translate.t("LABEL.LOGOUT")} icon="Logout" onclick={handle_logout} />
        </ActionCard>
    }
}