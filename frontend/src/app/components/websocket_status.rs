use crate::{app::components::AppIcon, hooks::use_websocket_status};
use yew::prelude::*;

#[derive(Properties, PartialEq)]
pub struct WebsocketStatusProps {}

#[component]
pub fn WebsocketStatus(_props: &WebsocketStatusProps) -> Html {
    let status = use_websocket_status();

    if *status {
        return html! { <></> };
    }

    html! {
        <div class="tp__websocket-status">
            <AppIcon name="WsDisconnected" />
        </div>
    }
}
