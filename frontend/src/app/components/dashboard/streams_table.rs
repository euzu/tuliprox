use crate::{
    app::{
        components::{
            menu_item::MenuItem, popup_menu::PopupMenu, AppIcon, Chip, RevealContent, Table, TableDefinition,
            ToggleSwitch,
        },
        ConfigContext,
    },
    hooks::use_service_context,
    i18n::use_translation,
    model::EventMessage,
    services::DialogService,
    utils::t_safe,
};
use gloo_timers::callback::Interval;
use gloo_utils::window;
use shared::{
    concat_string,
    error::{info_err_res, TuliproxError},
    model::{
        PlaylistItemType, ProtocolMessage, SortOrder, StreamChannel, StreamInfo, StreamTechnicalInfo, UserCommand,
    },
    utils::{current_time_secs, default_kick_secs, strip_port},
};
use std::{fmt::Display, rc::Rc, str::FromStr};
use wasm_bindgen::JsCast;
use web_sys::Element;
use yew::{platform::spawn_local, prelude::*};
use yew_hooks::use_clipboard;

const LIVE: &str = "Live";
const MOVIE: &str = "Movie";
const SERIES: &str = "Series";
const CATCHUP: &str = "Archive";
const HLS: &str = "HLS";
const DASH: &str = "DASH";

const KICK: &str = "kick";
const COPY_LINK_TULIPROX_VIRTUAL_ID: &str = "copy_link_tuliprox_virtual_id";
const COPY_LINK_TULIPROX_WEBPLAYER_URL: &str = "copy_link_tuliprox_webplayer_url";
const COPY_LINK_PROVIDER_URL: &str = "copy_link_provider_url";

const HEADERS: [&str; 15] = [
    "EMPTY",
    "USERNAME",
    "STREAM_ID",
    "CLUSTER",
    "CHANNEL",
    "GROUP",
    "TECH",
    "CLIENT_IP",
    "COUNTRY",
    "PROVIDER",
    "SHARED",
    "USER_AGENT",
    "DURATION",
    "BANDWIDTH",
    "TRANSFERRED",
];

fn is_stream_metrics_enabled(config_ctx: &ConfigContext) -> bool {
    config_ctx
        .config
        .as_ref()
        .and_then(|cfg| cfg.config.reverse_proxy.as_ref())
        .and_then(|reverse_proxy| reverse_proxy.stream.as_ref())
        .is_some_and(|stream| stream.metrics_enabled)
}

fn format_duration(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

fn format_bandwidth(rate_kbps: u32) -> String {
    if rate_kbps == 0 {
        return "-".to_string();
    }
    if rate_kbps >= 1_048_576 {
        format!("{:.1} GB/s", f64::from(rate_kbps) / 1_048_576.0)
    } else if rate_kbps >= 1024 {
        format!("{:.1} MB/s", f64::from(rate_kbps) / 1024.0)
    } else {
        format!("{rate_kbps} KB/s")
    }
}

fn format_transferred(total_kb: u32) -> String {
    if total_kb == 0 {
        return "-".to_string();
    }
    if total_kb >= 1_048_576 {
        format!("{:.2} GB", f64::from(total_kb) / 1_048_576.0)
    } else if total_kb >= 1024 {
        format!("{:.1} MB", f64::from(total_kb) / 1024.0)
    } else {
        format!("{total_kb} KB")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MeterDisplayKind {
    Bandwidth,
    Transferred,
}

#[derive(Properties, PartialEq)]
struct StreamMeterCellProps {
    pub uid: u32,
    pub meter_uid: u32,
    pub kind: MeterDisplayKind,
}

#[component]
fn StreamMeterCell(props: &StreamMeterCellProps) -> Html {
    let services = use_service_context();
    let meter_value = use_state(|| (0_u32, 0_u32));

    {
        let meter_value = meter_value.clone();
        let reset_key = (props.uid, props.meter_uid);
        use_effect_with(reset_key, move |_| {
            meter_value.set((0, 0));
            || ()
        });
    }

    {
        let services = services.clone();
        let meter_value = meter_value.clone();
        let uid = props.uid;
        use_effect_with(uid, move |_| {
            let subid = services.event.subscribe(move |msg| {
                if let EventMessage::StreamMeterBatch(entries) = msg {
                    if let Some(entry) = entries.iter().find(|entry| entry.uids.contains(&uid)) {
                        meter_value.set((entry.rate_kbps, entry.total_kb));
                    }
                }
            });
            move || services.event.unsubscribe(subid)
        });
    }

    let (rate_kbps, total_kb) = *meter_value;
    match props.kind {
        MeterDisplayKind::Bandwidth => html! { format_bandwidth(rate_kbps) },
        MeterDisplayKind::Transferred => html! { format_transferred(total_kb) },
    }
}

fn build_technical_chips(technical: Option<&StreamTechnicalInfo>) -> Vec<(String, &'static str)> {
    let mut chips = Vec::new();
    let Some(tech) = technical else {
        return chips;
    };

    if !tech.container.is_empty() {
        chips.push((tech.container.to_ascii_uppercase(), "tp__streams-table__tech-chip--container"));
    }
    if !tech.video_codec.is_empty() {
        chips.push((tech.video_codec.clone(), "tp__streams-table__tech-chip--video-codec"));
    }
    if !tech.resolution.is_empty() {
        chips.push((tech.resolution.clone(), "tp__streams-table__tech-chip--resolution"));
    }
    if !tech.fps.is_empty() {
        chips.push((format!("{} fps", tech.fps), "tp__streams-table__tech-chip--fps"));
    }
    if !tech.audio_codec.is_empty() {
        chips.push((tech.audio_codec.clone(), "tp__streams-table__tech-chip--audio-codec"));
    }
    if !tech.audio_channels.is_empty() {
        chips.push((tech.audio_channels.clone(), "tp__streams-table__tech-chip--audio-channels"));
    }

    chips
}

fn update_timestamps() {
    let window = window();
    let document = window.document().unwrap();
    let spans = document.query_selector_all("span[data-ts]").unwrap();
    for i in 0..spans.length() {
        if let Some(node) = spans.item(i) {
            let el: Element = node.dyn_into().unwrap();
            if let Some(ts_str) = el.get_attribute("data-ts") {
                if let Ok(ts) = ts_str.parse::<u64>() {
                    el.set_inner_html(&format_duration(current_time_secs() - ts));
                }
            }
        }
    }
}

#[derive(Properties, PartialEq, Clone)]
pub struct StreamsTableProps {
    pub streams: Option<Vec<Rc<StreamInfo>>>,
}

#[component]
pub fn StreamsTable(props: &StreamsTableProps) -> Html {
    let translate = use_translation();
    let service_ctx = use_service_context();
    let dialog = use_context::<DialogService>().expect("Dialog service not found");
    let clipboard = use_clipboard();
    let config_ctx = use_context::<ConfigContext>().expect("Config context not found");
    let popup_anchor_ref = use_state(|| None::<web_sys::Element>);
    let popup_is_open = use_state(|| false);
    let selected_dto = use_state(|| None::<Rc<StreamInfo>>);

    let headers = use_memo(config_ctx.clone(), |cfg| {
        let include_country = if let Some(app_cfg) = &cfg.config { app_cfg.config.is_geoip_enabled() } else { false };
        let metrics_enabled = is_stream_metrics_enabled(cfg);

        let visible_headers: Vec<&str> = if include_country {
            HEADERS
                .iter()
                .filter(|h| metrics_enabled || (**h != "BANDWIDTH" && **h != "TRANSFERRED"))
                .copied()
                .collect()
        } else {
            HEADERS
                .iter()
                .filter(|h| **h != "COUNTRY")
                .filter(|h| metrics_enabled || (**h != "BANDWIDTH" && **h != "TRANSFERRED"))
                .copied()
                .collect()
        };
        visible_headers
    });

    use_effect_with((), move |_| {
        let interval = Interval::new(1000, update_timestamps);
        move || drop(interval)
    });

    {
        let metrics_enabled = is_stream_metrics_enabled(&config_ctx);
        let websocket = service_ctx.websocket.clone();
        let event_service = service_ctx.event.clone();
        use_effect_with(metrics_enabled, move |metrics_enabled| {
            let subid = if *metrics_enabled {
                websocket.send_message(ProtocolMessage::StreamMeterSubscribe);
                let websocket_for_events = websocket.clone();
                Some(event_service.subscribe(move |msg| {
                    if let EventMessage::WebSocketStatus(true) = msg {
                        websocket_for_events.send_message(ProtocolMessage::StreamMeterSubscribe);
                    }
                }))
            } else {
                None
            };

            move || {
                if let Some(subid) = subid {
                    event_service.unsubscribe(subid);
                    websocket.send_message(ProtocolMessage::StreamMeterUnsubscribe);
                }
            }
        });
    }

    let handle_popup_close = {
        let set_is_open = popup_is_open.clone();
        Callback::from(move |()| {
            set_is_open.set(false);
        })
    };

    let handle_popup_onclick = {
        let set_selected_dto = selected_dto.clone();
        let set_anchor_ref = popup_anchor_ref.clone();
        let set_is_open = popup_is_open.clone();
        Callback::from(move |(dto, event): (Rc<StreamInfo>, MouseEvent)| {
            if let Some(streams) = event.target_dyn_into::<web_sys::Element>() {
                set_selected_dto.set(Some(dto.clone()));
                set_anchor_ref.set(Some(streams));
                set_is_open.set(true);
            }
        })
    };

    let render_header_cell = {
        let translator = translate.clone();
        let headers = headers.clone();
        Callback::<usize, Html>::from(move |col| {
            html! {
                {
                    if col < headers.len() {
                       translator.t(&concat_string!("LABEL.", headers[col]))
                    } else {
                      String::new()
                    }
               }
            }
        })
    };

    let render_cluster = |channel: &StreamChannel| -> &str {
        match channel.item_type {
            PlaylistItemType::LiveUnknown | PlaylistItemType::Live => LIVE,
            PlaylistItemType::Video | PlaylistItemType::LocalVideo => MOVIE,
            PlaylistItemType::Series
            | PlaylistItemType::SeriesInfo
            | PlaylistItemType::LocalSeries
            | PlaylistItemType::LocalSeriesInfo => SERIES,
            PlaylistItemType::Catchup => CATCHUP,
            PlaylistItemType::LiveHls => HLS,
            PlaylistItemType::LiveDash => DASH,
        }
    };

    let render_data_cell = {
        let popup_onclick = handle_popup_onclick.clone();
        let headers = headers.clone();
        let translate = translate.clone();
        Callback::<(usize, usize, Rc<StreamInfo>), Html>::from(
            move |(row, col, dto): (usize, usize, Rc<StreamInfo>)| match headers[col] {
                "EMPTY" => {
                    let popup_onclick = popup_onclick.clone();
                    html! {
                        <button class="tp__icon-button"
                            onclick={Callback::from(move |event: MouseEvent| popup_onclick.emit((dto.clone(), event)))}
                            data-row={row.to_string()}>
                            <AppIcon name="Popup"></AppIcon>
                        </button>
                    }
                }
                "USERNAME" => html! {&dto.username},
                "STREAM_ID" => html! { <>
                    { dto.channel.virtual_id.to_string() }
                    {" ("}
                    { dto.channel.provider_id.to_string() }
                    {")"}
                </>},
                "CLUSTER" => html! { render_cluster(&dto.channel) },
                "CHANNEL" => html! {&dto.channel.title},
                "GROUP" => html! {&*dto.channel.group},
                "TECH" => {
                    let chips = build_technical_chips(dto.channel.technical.as_ref());
                    if chips.is_empty() {
                        html! {}
                    } else {
                        html! {
                            <div class="tp__streams-table__tech-chips">
                                { for chips.into_iter().map(|(label, chip_class)| html! {
                                    <Chip
                                        label={label}
                                        class={Some(format!("tp__streams-table__tech-chip {chip_class}"))}
                                    />
                                })}
                            </div>
                        }
                    }
                }
                "CLIENT_IP" => html! { strip_port(&dto.client_ip)},
                "COUNTRY" => {
                    html! { dto.country.as_ref().map_or_else(String::new, |c| t_safe(&translate, &format!("COUNTRY.{c}")).unwrap_or_else(||c.to_string())) }
                }
                "PROVIDER" => html! {&dto.provider},
                "SHARED" => html! { <ToggleSwitch value={dto.channel.shared} readonly={true} /> },
                "USER_AGENT" => {
                    html! { <RevealContent preview={Some(html! { &dto.user_agent })}>{&dto.user_agent}</RevealContent> }
                }
                "DURATION" => {
                    html! { <span class="tp__stream-table__duration" data-ts={dto.ts.to_string()}>{format_duration(current_time_secs() - dto.ts)}</span> }
                }
                "BANDWIDTH" => {
                    html! { <StreamMeterCell uid={dto.uid} meter_uid={dto.meter_uid} kind={MeterDisplayKind::Bandwidth} /> }
                }
                "TRANSFERRED" => {
                    html! { <StreamMeterCell uid={dto.uid} meter_uid={dto.meter_uid} kind={MeterDisplayKind::Transferred} /> }
                }
                _ => html! {""},
            },
        )
    };

    let is_sortable = Callback::<usize, bool>::from(move |_col| false);

    let on_sort = Callback::<Option<(usize, SortOrder)>, ()>::from(move |_args| {});

    let table_definition = {
        // first register for config update
        let render_header_cell_cb = render_header_cell.clone();
        let render_data_cell_cb = render_data_cell.clone();
        let is_sortable = is_sortable.clone();
        let on_sort = on_sort.clone();
        let num_cols = headers.len();
        use_memo((props.streams.clone(), (*headers).clone()), move |(streams, _)| {
            streams.as_ref().map(|list| {
                Rc::new(TableDefinition::<StreamInfo> {
                    items: if list.is_empty() { None } else { Some(Rc::new(list.clone())) },
                    num_cols,
                    is_sortable,
                    on_sort,
                    render_header_cell: render_header_cell_cb,
                    render_data_cell: render_data_cell_cb,
                })
            })
        })
    };

    let copy_to_clipboard: Callback<String> = {
        let clipboard = clipboard.clone();
        let dialog = dialog.clone();
        Callback::from(move |text: String| {
            if *clipboard.is_supported {
                clipboard.write_text(text);
            } else {
                let dlg = dialog.clone();
                spawn_local(async move {
                    let _result = dlg
                        .content(html! {<input value={text} readonly={true} class="tp__copy-input"/>}, None, false)
                        .await;
                });
            }
        })
    };

    let handle_menu_click = {
        let popup_is_open_state = popup_is_open.clone();
        let translate = translate.clone();
        let services = service_ctx.clone();
        let selected_dto = selected_dto.clone();
        let copy_to_clipboard = copy_to_clipboard.clone();
        let kick_secs = config_ctx
            .config
            .as_ref()
            .and_then(|app_cfg| app_cfg.config.web_ui.as_ref())
            .map(|web_ui| web_ui.kick_secs)
            .unwrap_or_else(default_kick_secs);
        Callback::from(move |(name, _): (String, _)| {
            if let Ok(action) = StreamsTableAction::from_str(&name) {
                match action {
                    StreamsTableAction::Kick => {
                        if let Some(dto) = (*selected_dto).as_ref() {
                            if !services.websocket.send_message(ProtocolMessage::UserAction(UserCommand::Kick(
                                dto.addr,
                                dto.channel.virtual_id,
                                kick_secs,
                            ))) {
                                services.toastr.error(translate.t("MESSAGES.FAILED_TO_KICK_USER_STREAM"));
                            }
                        }
                    }
                    StreamsTableAction::CopyLinkTuliproxVirtualId => {
                        if let Some(dto) = &*selected_dto {
                            copy_to_clipboard.emit(dto.channel.virtual_id.to_string());
                        }
                    }
                    StreamsTableAction::CopyLinkProviderUrl => {
                        if let Some(dto) = &*selected_dto {
                            copy_to_clipboard.emit(dto.channel.url.to_string());
                        }
                    }
                    StreamsTableAction::CopyLinkTuliproxWebPlayerUrl => {
                        if let Some(dto) = &*selected_dto {
                            let target_id = dto.channel.target_id;
                            let virtual_id = dto.channel.virtual_id;
                            let cluster = dto.channel.cluster;
                            let services = services.clone();
                            let translate = translate.clone();
                            let copy_to_clipboard = copy_to_clipboard.clone();
                            spawn_local(async move {
                                if let Some(url) =
                                    services.playlist.get_playlist_webplayer_url(target_id, virtual_id, cluster).await
                                {
                                    copy_to_clipboard.emit(url);
                                } else {
                                    services.toastr.error(translate.t("MESSAGES.FAILED_TO_RETRIEVE_WEBPLAYER_URL"));
                                }
                            });
                        }
                    }
                }
            }
            popup_is_open_state.set(false);
        })
    };

    html! {
        <div class="tp__streams-table">
          {
            if let Some(definition) = table_definition.as_ref() {
                html! {
                  <>
                   <Table::<StreamInfo> definition={definition.clone()} />
                    <PopupMenu is_open={*popup_is_open} anchor_ref={(*popup_anchor_ref).clone()} on_close={handle_popup_close}>
                        <MenuItem icon="Disconnect" name={StreamsTableAction::Kick.to_string()} label={translate.t("LABEL.KICK")} onclick={&handle_menu_click} class="tp__delete_action"></MenuItem>
                        <MenuItem icon="Clipboard" name={StreamsTableAction::CopyLinkTuliproxVirtualId.to_string()} label={translate.t("LABEL.COPY_LINK_TULIPROX_VIRTUAL_ID")} onclick={&handle_menu_click}></MenuItem>
                        <MenuItem icon="Clipboard" name={StreamsTableAction::CopyLinkTuliproxWebPlayerUrl.to_string()} label={translate.t("LABEL.COPY_LINK_TULIPROX_WEBPLAYER_URL")} onclick={&handle_menu_click}></MenuItem>
                        <MenuItem icon="Clipboard" name={StreamsTableAction::CopyLinkProviderUrl.to_string()} label={translate.t("LABEL.COPY_LINK_PROVIDER_URL")} onclick={&handle_menu_click}></MenuItem>
                    </PopupMenu>
                </>
                  }
            } else {
              html! {}
            }
          }
        </div>
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum StreamsTableAction {
    Kick,
    CopyLinkTuliproxVirtualId,
    CopyLinkTuliproxWebPlayerUrl,
    CopyLinkProviderUrl,
}

impl Display for StreamsTableAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Kick => KICK,
                Self::CopyLinkTuliproxVirtualId => COPY_LINK_TULIPROX_VIRTUAL_ID,
                Self::CopyLinkTuliproxWebPlayerUrl => COPY_LINK_TULIPROX_WEBPLAYER_URL,
                Self::CopyLinkProviderUrl => COPY_LINK_PROVIDER_URL,
            }
        )
    }
}

impl FromStr for StreamsTableAction {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        if s.eq(KICK) {
            Ok(Self::Kick)
        } else if s.eq(COPY_LINK_TULIPROX_VIRTUAL_ID) {
            Ok(Self::CopyLinkTuliproxVirtualId)
        } else if s.eq(COPY_LINK_TULIPROX_WEBPLAYER_URL) {
            Ok(Self::CopyLinkTuliproxWebPlayerUrl)
        } else if s.eq(COPY_LINK_PROVIDER_URL) {
            Ok(Self::CopyLinkProviderUrl)
        } else {
            info_err_res!("Unknown Stream Action: {}", s)
        }
    }
}
