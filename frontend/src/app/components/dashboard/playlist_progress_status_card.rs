use crate::{
    app::components::StatusCard,
    hooks::{use_service_context, use_websocket_status},
    i18n::use_translation,
    model::EventMessage,
};
use yew::{component, html, use_effect_with, use_state, Html};

#[component]
pub fn PlaylistProgressStatusCard() -> Html {
    let services = use_service_context();
    let translate = use_translation();
    let data = use_state(|| None::<String>);
    let ws_connected = use_websocket_status();

    {
        let services_ctx = services.clone();
        let data_clone = data.clone();
        use_effect_with((), move |_| {
            let services_ctx = services_ctx.clone();
            let data_clone = data_clone.clone();
            let subid = services_ctx.event.subscribe(move |msg| {
                if let EventMessage::PlaylistUpdateProgress(progress) = msg {
                    data_clone.set(Some(format!(
                        "[{}] {}",
                        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                        progress.message
                    )));
                }
            });
            move || services_ctx.event.unsubscribe(subid)
        });
    }

    // Distinguish "no updates yet" from "realtime connection lost"
    let display_data = (*data).clone().unwrap_or_else(|| {
        if *ws_connected {
            translate.t("LABEL.IDLE")
        } else {
            translate.t("LABEL.HEALTH_DISCONNECTED")
        }
    });
    let footer = if *ws_connected { String::new() } else { translate.t("LABEL.HEALTH_DISCONNECTED") };

    html! {
        <StatusCard
            title={translate.t("LABEL.PLAYLIST_UPDATE")}
            data={display_data}
            footer={footer}
                />
    }
}
