use crate::app::components::{Card, StatusCard, StreamsTable};
use crate::app::StatusContext;
use crate::hooks::use_service_context;
use crate::model::EventMessage;
use std::rc::Rc;
use yew::prelude::*;
use yew_i18n::use_translation;

#[function_component]
pub fn StreamsView() -> Html {
    let translate = use_translation();
    let service_ctx = use_service_context();
    let status_ctx = use_context::<StatusContext>().expect("Status context not found");
    let provider_connections = use_state(|| 0);

    let memo_streams = {
        let status = status_ctx.status.clone();
        use_memo(status, |s| {
            s.as_ref().map(|st| {
                st.active_user_streams
                    .iter()
                    .cloned()
                    .map(Rc::new)
                    .collect::<Vec<_>>()
            })
        })
    };

    {
        let services = service_ctx.clone();
        let provider_connections = provider_connections.clone();
        use_effect_with((), move |_| {
            let services = services.clone();
            let subid = services.event.subscribe(move |msg| {
                if let EventMessage::ActiveProviderCount(count) = msg {
                    provider_connections.set(count);
                }
            });
            move || services.event.unsubscribe(subid)
        });
    }

    {
        let status_ctx = status_ctx.clone();
        let provider_connections = provider_connections.clone();
        use_effect_with(status_ctx.status, move |status| {
            let count = status.as_ref().map_or(0, |status| {
                status
                    .active_provider_connections
                    .as_ref()
                    .map(|map| map.values().sum::<usize>())
                    .unwrap_or(0)
            });
            provider_connections.set(count);
        })
    }

    html! {
      <div class="tp__streams">
        <div class="tp__streams__header">
         <h1>{ translate.t("LABEL.STREAMS")}</h1>
        </div>
        <div class="tp__streams__body">
           <div class="tp__stats__body-group">
                <Card><StatusCard title={translate.t("LABEL.ACTIVE_USERS")} data={status_ctx.status.as_ref().map_or_else(|| "n/a".to_string(), |status| status.active_users.to_string())} /></Card>
                <Card><StatusCard title={translate.t("LABEL.ACTIVE_USER_CONNECTIONS")} data={status_ctx.status.as_ref().map_or_else(|| "n/a".to_string(), |status| status.active_user_connections.to_string())} /></Card>
                <Card>
                    <StatusCard
                        title={translate.t("LABEL.ACTIVE_PROVIDER_CONNECTIONS")}
                        data={(*provider_connections).to_string()}
                    />
                 </Card>
            </div>
            <StreamsTable streams={ (*memo_streams).clone() } />
        </div>
      </div>
    }
}
