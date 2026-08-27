use crate::{hooks::use_service_context, model::EventMessage};
use yew::prelude::*;

#[hook]
pub fn use_websocket_status() -> UseStateHandle<bool> {
    let services = use_service_context();
    let status = use_state(|| services.websocket.is_connected());

    {
        let services = services.clone();
        let status = status.clone();
        use_effect_with((), move |()| {
            let services_ctx = services.clone();
            let status = status.clone();
            let subid = services_ctx.event.subscribe(move |msg| {
                if let EventMessage::WebSocketStatus(active) = msg {
                    status.set(active);
                }
            });
            move || services_ctx.event.unsubscribe(subid)
        });
    }

    status
}
