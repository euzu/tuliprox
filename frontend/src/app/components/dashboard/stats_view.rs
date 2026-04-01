use crate::{
    app::components::{Card, CollapsePanel, PlaylistProgressStatusCard, StatusCard, StatusContext, StreamsView},
    i18n::use_translation,
};
use shared::utils::human_readable_byte_size;
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct StatsViewProps {
    #[prop_or_default]
    pub show_streams: bool,
}

#[component]
pub fn StatsView(props: &StatsViewProps) -> Html {
    let translate = use_translation();
    let status_ctx = use_context::<StatusContext>().expect("Status context not found");

    let (mem, cpu, net) = status_ctx.system_info.as_ref().map_or_else(
        || ("n/a".to_string(), "n/a".to_string(), "n/a".to_string()),
        |system| {
            (
                format!(
                    "{} / {}",
                    human_readable_byte_size(system.memory_usage),
                    human_readable_byte_size(system.memory_total)
                ),
                format!("{:.2}%", system.cpu_usage),
                format!(
                    "\u{2193} {}/s \u{2191} {}/s",
                    human_readable_byte_size(system.net_rx_bytes_per_sec as u64),
                    human_readable_byte_size(system.net_tx_bytes_per_sec as u64),
                ),
            )
        },
    );

    let render_system_stats = |cache| {
        html! {
           <div class="tp__stats__body-group">
               <Card class="tp__stats__system"><StatusCard title={translate.t("LABEL.MEMORY")} data={mem.clone()} /></Card>
               <Card class="tp__stats__system"><StatusCard title={translate.t("LABEL.CACHE")} data={cache} /></Card>
               <Card class="tp__stats__system"><StatusCard title={translate.t("LABEL.CPU")} data={cpu.clone()} /></Card>
               <Card class="tp__stats__system"><StatusCard title={translate.t("LABEL.NETWORK")} data={net.clone()} /></Card>
            </div>
        }
    };

    let render_streams_embedded = || {
        let cache = status_ctx.status.as_ref().map_or_else(
            || "n/a".to_string(),
            |status| status.cache.as_ref().map_or_else(|| "n/a".to_string(), |c| c.clone()),
        );

        html! {
        <div class="tp__stats">
            <CollapsePanel expanded={true} title_content={Some(html! {
                <div class="tp__stats__header">
                 <h1>{ translate.t("LABEL.STATS")}</h1>
                </div>
                })}>
                <div class="tp__stats__body">
                  { render_system_stats(cache) }
                  <div class="tp__stats__body-group">
                    <Card><PlaylistProgressStatusCard /></Card>
                  </div>
                </div>
            </CollapsePanel>
            <CollapsePanel expanded={true} title_content={Some(html! {
                <div class="tp__stats__header">
                 <h1>{ translate.t("LABEL.STREAMS")}</h1>
                </div>
                })}>
                <div class="tp__stats__body">
                  <div class="tp__stats__body-group">
                     <StreamsView embedded={true} />
                  </div>
                </div>
            </CollapsePanel>
            </div>
            }
    };

    let render_stats_only = || {
        let render_active_provider_connections = || -> Html {
            let empty_card = || {
                html! {
                    <Card>
                        <StatusCard
                            title={translate.t("LABEL.ACTIVE_PROVIDER_CONNECTIONS")}
                            data={"-"}
                        />
                    </Card>
                }
            };
            match &status_ctx.status {
                Some(stats) => {
                    if let Some(map) = &stats.active_provider_connections {
                        if !map.is_empty() {
                            let cards = map
                                .iter()
                                .filter(|(_provider, connections)| **connections > 0)
                                .map(|(provider, connections)| {
                                    html! {
                                        <Card>
                                            <StatusCard
                                                title={provider.to_string()}
                                                data={connections.to_string()}
                                                footer={translate.t("LABEL.ACTIVE_PROVIDER_CONNECTIONS")}
                                            />
                                        </Card>
                                    }
                                })
                                .collect::<Html>();

                            cards
                        } else {
                            empty_card()
                        }
                    } else {
                        empty_card()
                    }
                }
                None => empty_card(),
            }
        };

        let (cache, users, connections) = status_ctx.status.as_ref().map_or_else(
            || ("n/a".to_string(), "n/a".to_string(), "n/a".to_string()),
            |status| {
                (
                    status.cache.as_ref().map_or_else(|| "n/a".to_string(), |c| c.clone()),
                    status.active_users.to_string(),
                    status.active_user_connections.to_string(),
                )
            },
        );

        html! {
          <div class="tp__stats">
            <div class="tp__stats__header">
             <h1>{ translate.t("LABEL.STATS")}</h1>
            </div>
            <div class="tp__stats__body">
                { render_system_stats(cache) }
                <div class="tp__stats__body-group">
                    <Card><PlaylistProgressStatusCard /></Card>
                </div>
                <div class="tp__stats__body-group tp__stats__body-group-provider">
                    <Card><StatusCard title={translate.t("LABEL.ACTIVE_USERS")} data={users} /></Card>
                    <Card><StatusCard title={translate.t("LABEL.ACTIVE_USER_CONNECTIONS")} data={connections} /></Card>
                    { render_active_provider_connections() }
                </div>
            </div>
          </div>
        }
    };

    if props.show_streams {
        render_streams_embedded()
    } else {
        render_stats_only()
    }
}
