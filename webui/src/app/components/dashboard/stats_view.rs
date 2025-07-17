use yew::prelude::*;
use yew_i18n::use_translation;
use crate::app::components::{Card, StatusCard, StatusContext};

#[function_component]
pub fn StatsView() -> Html {
    let translate = use_translation();
    let status_ctx = use_context::<StatusContext>().expect("Status context not found");

    let render_active_provider_connections = || -> Html {
            match &status_ctx.status {
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
    };

    html! {
      <div class="tp__stats">
        <div class="tp__stats__header">
         <h1>{ translate.t("LABEL.STATS")}</h1>
        </div>
        <div class="tp__stats__body">
            <div class="tp__stats__body-group">
                <Card><StatusCard title={translate.t("LABEL.MEMORY")} data={status_ctx.status.as_ref().map_or_else(|| "n/a".to_string(), |status| status.memory.clone())} /></Card>
                <Card><StatusCard title={translate.t("LABEL.CACHE")} data={status_ctx.status.as_ref().map_or_else(|| "n/a".to_string(), |status| status.cache.as_ref().map_or_else(|| "n/a".to_string(), |c| c.clone()))} /></Card>
            </div>
            <div class="tp__stats__body-group">
                <Card><StatusCard title={translate.t("LABEL.ACTIVE_USERS")} data={status_ctx.status.as_ref().map_or_else(|| "n/a".to_string(), |status| status.active_users.to_string())} /></Card>
                <Card><StatusCard title={translate.t("LABEL.ACTIVE_USER_CONNECTIONS")} data={status_ctx.status.as_ref().map_or_else(|| "n/a".to_string(), |status| status.active_user_connections.to_string())} /></Card>
                { render_active_provider_connections() }
            </div>
        </div>
      </div>
    }
}