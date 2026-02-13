use crate::app::components::AppIcon;
use crate::hooks::use_service_context;
use crate::model::EventMessage;
use yew::prelude::*;

#[derive(Properties, PartialEq)]
pub struct WebsocketStatusProps {}

#[function_component]
pub fn WebsocketStatus(_props: &WebsocketStatusProps) -> Html {
    let status = use_state(|| true);
    let services = use_service_context();

    {
        let status_clone = status.clone();
        use_effect_with((), move |_| {
            let services_ctx = services.clone();
            let status_clone = status_clone.clone();
            let subid = services_ctx.event.subscribe(move |msg| {
                if let EventMessage::WebSocketStatus(active) = msg {
                    status_clone.set(active);
                }
            });
            move || services_ctx.event.unsubscribe(subid)
        });
    }

    if *status {
        return html! { <></> };
    }

    html! {
        <div class="tp__websocket-status">
            <AppIcon name="WsDisconnected" />
        </div>
    }
}
