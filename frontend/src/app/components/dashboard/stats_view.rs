use crate::{
    app::components::{
        use_metrics_history, Card, CollapsePanel, LogConsole, MetricsHistory, PlaylistProgressStatusCard, Sparkline,
        SparklineFormat, SparklineSeries, StatusCard, StatusContext, StreamsView,
    },
    i18n::use_translation,
    utils::format_uptime,
};
use shared::{model::XtreamCluster, utils::human_readable_byte_size};
use std::rc::Rc;
use yew::prelude::*;

#[derive(Clone, PartialEq)]
struct StatsSparklineData {
    memory: Rc<[SparklineSeries]>,
    cpu: Rc<[SparklineSeries]>,
    network: Rc<[SparklineSeries]>,
    users: Rc<[SparklineSeries]>,
    connections: Rc<[SparklineSeries]>,
}

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct StatsViewProps {
    #[prop_or_default]
    pub show_streams: bool,
}

#[component]
pub fn StatsView(props: &StatsViewProps) -> Html {
    let translate = use_translation();
    // Render a fallback instead of panicking when the provider is missing
    let Some(status_ctx) = use_context::<StatusContext>() else {
        log::error!("StatsView rendered without StatusContext provider");
        return html! {};
    };
    let history = use_metrics_history();
    let sparkline_data = use_memo(history.clone(), |history| StatsSparklineData {
        memory: Rc::from([SparklineSeries::new(MetricsHistory::as_vec(&history.memory))]),
        cpu: Rc::from([SparklineSeries::new(MetricsHistory::as_vec(&history.cpu))]),
        network: Rc::from([
            SparklineSeries::new(MetricsHistory::as_vec(&history.net_rx))
                .with_class("tp__sparkline--net-rx")
                .with_label("\u{2193}"),
            SparklineSeries::new(MetricsHistory::as_vec(&history.net_tx))
                .with_class("tp__sparkline--net-tx")
                .with_label("\u{2191}"),
        ]),
        users: Rc::from([SparklineSeries::new(MetricsHistory::as_vec(&history.users))]),
        connections: Rc::from([SparklineSeries::new(MetricsHistory::as_vec(&history.connections))]),
    });

    let logs_expanded = use_state(|| true);
    let on_logs_toggle = {
        let logs_expanded = logs_expanded.clone();
        Callback::from(move |expanded: bool| {
            logs_expanded.set(expanded);
        })
    };

    let loading_label = translate.t("LABEL.LOADING");
    let (mem, cpu, net, disk, net_total) = status_ctx.system_info.as_ref().map_or_else(
        || (loading_label.clone(), loading_label.clone(), loading_label.clone(), loading_label.clone(), String::new()),
        |system| {
            let disk = if system.disk_total_bytes > 0 {
                format!(
                    "{} / {}",
                    human_readable_byte_size(system.disk_total_bytes.saturating_sub(system.disk_free_bytes)),
                    human_readable_byte_size(system.disk_total_bytes),
                )
            } else {
                "n/a".to_string()
            };
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
                disk,
                format!(
                    "\u{2211} \u{2193} {} \u{2191} {}",
                    human_readable_byte_size(system.net_rx_bytes_total),
                    human_readable_byte_size(system.net_tx_bytes_total),
                ),
            )
        },
    );
    let uptime =
        status_ctx.status.as_ref().map_or_else(|| loading_label.clone(), |status| format_uptime(status.uptime_secs));

    let render_system_stats = |cache| {
        html! {
           <div class="tp__stats__body-group">
               <Card class="tp__stats__system"><StatusCard icon="CPU" title={translate.t("LABEL.CPU")} data={cpu.clone()}
                   chart={Some(html! { <Sparkline class="tp__sparkline--cpu" format={SparklineFormat::Percent}
                       series={sparkline_data.cpu.clone()} /> })} /></Card>
               <Card class="tp__stats__system"><StatusCard icon="Memory" title={translate.t("LABEL.MEMORY")} data={mem.clone()}
                   chart={Some(html! { <Sparkline class="tp__sparkline--memory" format={SparklineFormat::Percent}
                       series={sparkline_data.memory.clone()} /> })} /></Card>
               <Card class="tp__stats__system"><StatusCard icon="NetworkSpeed" title={translate.t("LABEL.NETWORK")} data={net.clone()}
                   footer={net_total.clone()}
                   chart={Some(html! { <Sparkline class="tp__sparkline--network" format={SparklineFormat::BytesPerSec}
                       series={sparkline_data.network.clone()} /> })} /></Card>
               <Card class="tp__stats__system"><StatusCard icon="Cache" title={translate.t("LABEL.CACHE")} data={cache} /></Card>
               <Card class="tp__stats__system"><StatusCard icon="Storage" title={translate.t("LABEL.DISK")} data={disk.clone()} /></Card>
               <Card class="tp__stats__system"><StatusCard icon="Clock" title={translate.t("LABEL.UPTIME")} data={uptime.clone()} /></Card>
            </div>
        }
    };

    let render_streams_embedded = || {
        let cache = status_ctx.status.as_ref().map_or_else(
            || loading_label.clone(),
            |status| status.cache.as_ref().map_or_else(|| "n/a".to_string(), std::clone::Clone::clone),
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
            <div class="tp__stats__header">
                <h1>{ translate.t("LABEL.PLAYLIST_UPDATE")}</h1>
            </div>
            <div class="tp__stats__body-group">
                <Card><PlaylistProgressStatusCard /></Card>
            </div>
            <CollapsePanel expanded={*logs_expanded} on_state_change={on_logs_toggle.clone()} title_content={Some(html! {
                <div class="tp__stats__header">
                 <h1>{ translate.t("LABEL.LOGS")}</h1>
                </div>
                })}>
                <div class="tp__stats__body">
                    <LogConsole active={*logs_expanded} />
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
                        if map.is_empty() {
                            empty_card()
                        } else {
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
                        }
                    } else {
                        empty_card()
                    }
                }
                None => empty_card(),
            }
        };

        let (cache, users, connections) = status_ctx.status.as_ref().map_or_else(
            || (loading_label.clone(), loading_label.clone(), loading_label.clone()),
            |status| {
                (
                    status.cache.as_ref().map_or_else(|| "n/a".to_string(), std::clone::Clone::clone),
                    status.active_users.to_string(),
                    status.active_user_connections.to_string(),
                )
            },
        );

        let (stream_count, stream_footer) = status_ctx.status.as_ref().map_or_else(
            || (loading_label.clone(), String::new()),
            |status| {
                let (live, video, series) = status.active_user_streams.iter().fold(
                    (0_usize, 0_usize, 0_usize),
                    |(l, v, s), stream| match stream.channel.cluster {
                        XtreamCluster::Live => (l + 1, v, s),
                        XtreamCluster::Video => (l, v + 1, s),
                        XtreamCluster::Series => (l, v, s + 1),
                    },
                );
                (
                    status.active_user_streams.len().to_string(),
                    format!(
                        "{} {live} \u{b7} {} {video} \u{b7} {} {series}",
                        translate.t("LABEL.LIVE"),
                        translate.t("LABEL.VOD"),
                        translate.t("LABEL.SERIES"),
                    ),
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
                <div class="tp__stats__body-group tp__stats__body-group-provider">
                    <Card><StatusCard title={translate.t("LABEL.ACTIVE_USERS")} data={users}
                        chart={Some(html! { <Sparkline class="tp__sparkline--users" format={SparklineFormat::Count}
                            series={sparkline_data.users.clone()} /> })} /></Card>
                    <Card><StatusCard title={translate.t("LABEL.ACTIVE_USER_CONNECTIONS")} data={connections}
                        chart={Some(html! { <Sparkline class="tp__sparkline--connections" format={SparklineFormat::Count}
                            series={sparkline_data.connections.clone()} /> })} /></Card>
                    <Card><StatusCard icon="PlayArrow" title={translate.t("LABEL.ACTIVE_STREAMS")} data={stream_count}
                        footer={stream_footer} /></Card>
                    { render_active_provider_connections() }
                </div>
                <div class="tp__stats__body-group">
                    <Card><PlaylistProgressStatusCard /></Card>
                </div>
                <CollapsePanel expanded={*logs_expanded} on_state_change={on_logs_toggle.clone()} title_content={Some(html! {
                    <div class="tp__stats__header">
                     <h1>{ translate.t("LABEL.LOGS")}</h1>
                    </div>
                    })}>
                    <div class="tp__stats__body">
                        <LogConsole active={*logs_expanded} />
                    </div>
                </CollapsePanel>
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
