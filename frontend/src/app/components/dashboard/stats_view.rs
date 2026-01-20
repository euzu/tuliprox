use yew::prelude::*;
use yew_i18n::use_translation;
use shared::utils::human_readable_byte_size;
use crate::app::components::{Card, PlaylistProgressStatusCard, StatusCard, StatusContext};

#[function_component]
pub fn StatsView() -> Html {
    let translate = use_translation();
    let status_ctx = use_context::<StatusContext>().expect("Status context not found");

    let render_active_provider_connections = || -> Html {
       let empty_card = || html! {
                    <Card>
                        <StatusCard
                            title={translate.t("LABEL.ACTIVE_PROVIDER_CONNECTIONS")}
                            data={"-"}
                        />
                    </Card>
                };
        match &status_ctx.status {
            Some(stats) => {
                if let Some(map) = &stats.active_provider_connections {
                    if !map.is_empty() {
                        let cards = map.iter().map(|(provider, connections)| {
                            html! {
                                    <Card>
                                        <StatusCard
                                            title={provider.to_string()}
                                            data={connections.to_string()}
                                            footer={translate.t("LABEL.ACTIVE_PROVIDER_CONNECTIONS")}
                                        />
                                    </Card>
                                }
                        }).collect::<Html>();

                        cards
                    } else {
                        empty_card()
                    }
                } else {
                    empty_card()
                }
            }
            None => empty_card()
        }
    };

    let (mem, cpu) = status_ctx.system_info.as_ref().map_or_else(|| ("n/a".to_string(), "n/a".to_string()),
        |system| (format!("{} / {}", human_readable_byte_size(system.memory_usage), human_readable_byte_size(system.memory_total)), format!("{:.2}%", system.cpu_usage)));


    let (cache, users, connections) = status_ctx.status.as_ref().map_or_else(|| ("n/a".to_string(),"n/a".to_string(),"n/a".to_string()),
         |status| (status.cache.as_ref().map_or_else(|| "n/a".to_string(), |c| c.clone()), status.active_users.to_string(), status.active_user_connections.to_string()));

    html! {
      <div class="tp__stats">
        <div class="tp__stats__header">
         <h1>{ translate.t("LABEL.STATS")}</h1>
        </div>
        <div class="tp__stats__body">
            <div class="tp__stats__body-group">
                <Card><StatusCard title={translate.t("LABEL.MEMORY")} data={mem} /></Card>
                <Card><StatusCard title={translate.t("LABEL.CPU")} data={cpu} /></Card>
                <Card><StatusCard title={translate.t("LABEL.CACHE")} data={cache} /></Card>
            </div>
            <div class="tp__stats__body-group">
                <Card><PlaylistProgressStatusCard /></Card>
            </div>
            <div class="tp__stats__body-group">
                <Card><StatusCard title={translate.t("LABEL.ACTIVE_USERS")} data={users} /></Card>
                <Card><StatusCard title={translate.t("LABEL.ACTIVE_USER_CONNECTIONS")} data={connections} /></Card>
                { render_active_provider_connections() }
            </div>
        </div>
      </div>
    }
}