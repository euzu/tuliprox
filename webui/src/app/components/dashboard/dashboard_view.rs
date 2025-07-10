use gloo_timers::callback::Interval;
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;
use yew_i18n::use_translation;
use crate::app::components::{Card, DiscordActionCard, UserActionCard, VersionActionCard, DocumentationActionCard, StatusCard, IpinfoActionCard};
use crate::hooks::use_service_context;

#[function_component]
pub fn DashboardView() -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let status =  use_state(|| None);

    {
        let services_ctx = services.clone();
        let status_signal = status.clone();

        use_effect_with((), move |_| {
            let fetch_status = {
                let status = status_signal.clone();
                let services_ctx = services_ctx.clone();
                move || {
                    let status = status.clone();
                    let services_ctx = services_ctx.clone();
                    spawn_local(async move {
                        status.set(services_ctx.status.get_server_status().await.ok());
                    });
                }
            };

            fetch_status();
            // all 5 seconds
            let interval = Interval::new(5000, move || {
                fetch_status();
            });

            // Cleanup function
            || drop(interval)
        });
    }

    html! {
      <div class="tp__dashboard">
        <div class="tp__dashboard__header">
         <h1>{ translate.t("LABEL.DASHBOARD")}</h1>
        </div>
        <div class="tp__dashboard__body">
            <div class="tp__dashboard__body-actions">
              <Card>
                 <VersionActionCard version={(*status).as_ref().map_or_else(String::new,  |s| s.version.clone())}
                     build_time={(*status).as_ref().map_or_else(String::new,  |s| s.build_time.as_ref().map_or_else(String::new, |v| v.clone()))}/>
              </Card>
              <Card><UserActionCard /></Card>
              <Card><DocumentationActionCard /></Card>
              <Card><DiscordActionCard /></Card>
              <Card><IpinfoActionCard /></Card>
            </div>
            <div class="tp__dashboard__body-stats">
                <Card><StatusCard title={translate.t("LABEL.MEMORY")} data={status.as_ref().map_or_else(|| "n/a".to_string(), |status| status.memory.clone())} /></Card>
                <Card><StatusCard title={translate.t("LABEL.CACHE")} data={status.as_ref().map_or_else(|| "n/a".to_string(), |status| status.cache.as_ref().map_or_else(|| "n/a".to_string(), |c| c.clone()))} /></Card>
                <Card><StatusCard title={translate.t("LABEL.ACTIVE_USERS")} data={status.as_ref().map_or_else(|| "n/a".to_string(), |status| status.active_users.to_string())} /></Card>
                <Card><StatusCard title={translate.t("LABEL.ACTIVE_USER_CONNECTIONS")} data={status.as_ref().map_or_else(|| "n/a".to_string(), |status| status.active_user_connections.to_string())} /></Card>
        {
                match &*status {
                    Some(stats) => {
                        if let Some(map) = &stats.active_provider_connections {
                            let cards = map.iter().map(|(provider, connections)| {
                                html! {
                                    <Card>
                                        <StatusCard
                                            title={provider.clone()}
                                            data={connections.to_string()}
                                            footer={translate.t("LABEL.ACTIVE_PROVIDER_CONNECTIONS")}
                                        />
                                    </Card>
                                }
                            }).collect::<Html>();

                            cards
                        } else {
                            html! {
                                <Card>
                                    <StatusCard
                                        title={translate.t("LABEL.ACTIVE_PROVIDER_CONNECTIONS")}
                                        data={"n/a"}
                                    />
                                </Card>
                            }
                        }
                    }
                    None => html! {
                        <Card>
                            <StatusCard
                                title={translate.t("LABEL.ACTIVE_PROVIDER_CONNECTIONS")}
                                data={"n/a"}
                            />
                        </Card>
                    }
                }
            }
            </div>
        </div>
      </div>
    }
}