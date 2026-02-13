use crate::app::components::StatusCard;
use crate::hooks::use_service_context;
use crate::model::EventMessage;
use yew::{function_component, html, use_effect_with, use_state, Html};
use yew_i18n::use_translation;

#[function_component]
pub fn PlaylistProgressStatusCard() -> Html {
    let services = use_service_context();
    let translate = use_translation();
    let data = use_state(|| "-".to_owned());

    {
        let services_ctx = services.clone();
        let data_clone = data.clone();
        use_effect_with((), move |_| {
            let services_ctx = services_ctx.clone();
            let data_clone = data_clone.clone();
            let subid = services_ctx.event.subscribe(move |msg| {
                if let EventMessage::PlaylistUpdateProgress(_target, msg) = msg {
                    data_clone.set(format!(
                        "[{}] {msg}",
                        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S")
                    ));
                }
            });
            move || services_ctx.event.unsubscribe(subid)
        });
    }

    html! {
        <StatusCard
            title={translate.t("LABEL.PLAYLIST_UPDATE")}
            data={(*data).clone()}
                />
    }
}
