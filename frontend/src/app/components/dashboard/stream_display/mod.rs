mod helpers;
mod item;
mod meter;

use self::{
    helpers::{
        filter_visible_streams, get_adaptive_session_ttl_secs, is_stream_metrics_enabled, refresh_adaptive_last_seen,
        update_timestamps, ADAPTIVE_STREAM_CLEANUP_INTERVAL_MILLIS,
    },
    item::StreamDisplayItem,
};
use crate::{
    app::{
        components::{menu_item::MenuItem, popup_menu::PopupMenu, NoContent},
        ConfigContext,
    },
    hooks::use_service_context,
    i18n::use_translation,
    model::EventMessage,
    services::DialogService,
};
use gloo_timers::callback::Interval;
use shared::{
    error::{info_err_res, TuliproxError},
    model::{PlaylistRequest, PlaylistUrlResolveRequest, ProtocolMessage, StreamInfo, UserCommand},
    utils::default_kick_secs,
};
use std::{collections::HashMap, fmt::Display, rc::Rc, str::FromStr};
use yew::{platform::spawn_local, prelude::*};
use yew_hooks::use_clipboard;

const KICK: &str = "kick";
const COPY_LINK_TULIPROX_VIRTUAL_ID: &str = "copy_link_tuliprox_virtual_id";
const COPY_LINK_TULIPROX_WEBPLAYER_URL: &str = "copy_link_tuliprox_webplayer_url";
const COPY_LINK_PROVIDER_URL: &str = "copy_link_provider_url";

#[derive(Properties, PartialEq, Clone)]
pub struct StreamDisplayProps {
    pub streams: Option<Vec<Rc<StreamInfo>>>,
}

#[component]
pub fn StreamDisplay(props: &StreamDisplayProps) -> Html {
    let translate = use_translation();
    let service_ctx = use_service_context();
    let dialog = use_context::<DialogService>().expect("Dialog service not found");
    let clipboard = use_clipboard();
    let config_ctx = use_context::<ConfigContext>().expect("Config context not found");
    let popup_anchor_ref = use_state(|| None::<web_sys::Element>);
    let popup_is_open = use_state(|| false);
    let selected_dto = use_state(|| None::<Rc<StreamInfo>>);
    let adaptive_last_seen = use_state(HashMap::<u32, u64>::new);
    let cleanup_now_secs = use_state(shared::utils::current_time_secs);
    let adaptive_session_ttl_secs = get_adaptive_session_ttl_secs(&config_ctx);
    let metrics_enabled = is_stream_metrics_enabled(&config_ctx);

    use_effect_with((), move |_| {
        let interval = Interval::new(1000, update_timestamps);
        move || drop(interval)
    });

    {
        let adaptive_last_seen = adaptive_last_seen.clone();
        let streams = props.streams.clone();
        use_effect_with(streams, move |streams| {
            refresh_adaptive_last_seen(&adaptive_last_seen, streams);
            || ()
        });
    }

    {
        let cleanup_now_secs = cleanup_now_secs.clone();
        use_effect_with((), move |_| {
            let interval = Interval::new(ADAPTIVE_STREAM_CLEANUP_INTERVAL_MILLIS, move || {
                cleanup_now_secs.set(shared::utils::current_time_secs());
            });
            move || drop(interval)
        });
    }

    {
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

    let visible_streams = use_memo(
        (props.streams.clone(), (*adaptive_last_seen).clone(), *cleanup_now_secs, adaptive_session_ttl_secs),
        |(streams, adaptive_last_seen, cleanup_now_secs, adaptive_session_ttl_secs)| {
            filter_visible_streams(streams.clone(), adaptive_last_seen, *cleanup_now_secs, *adaptive_session_ttl_secs)
        },
    );

    let handle_popup_close = {
        let set_is_open = popup_is_open.clone();
        Callback::from(move |()| set_is_open.set(false))
    };

    let handle_popup_onclick = {
        let set_selected_dto = selected_dto.clone();
        let set_anchor_ref = popup_anchor_ref.clone();
        let set_is_open = popup_is_open.clone();
        Callback::from(move |(dto, event): (Rc<StreamInfo>, MouseEvent)| {
            if let Some(streams) = event.target_dyn_into::<web_sys::Element>() {
                set_selected_dto.set(Some(dto));
                set_anchor_ref.set(Some(streams));
                set_is_open.set(true);
            }
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
                    let _ = dlg
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
            if let Ok(action) = StreamDisplayAction::from_str(&name) {
                match action {
                    StreamDisplayAction::Kick => {
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
                    StreamDisplayAction::CopyLinkTuliproxVirtualId => {
                        if let Some(dto) = &*selected_dto {
                            copy_to_clipboard.emit(dto.channel.virtual_id.to_string());
                        }
                    }
                    StreamDisplayAction::CopyLinkProviderUrl => {
                        if let Some(dto) = &*selected_dto {
                            let url = dto.channel.url.to_string();
                            let playlist_request = PlaylistRequest::Target(dto.channel.target_id);
                            let copy_to_clipboard = copy_to_clipboard.clone();
                            let services = services.clone();
                            spawn_local(async move {
                                let request =
                                    PlaylistUrlResolveRequest::Provider { playlist_request, url: url.to_string() };
                                let resolved = services.playlist.resolve_url(request).await.unwrap_or(url);
                                copy_to_clipboard.emit(resolved);
                            });
                        }
                    }
                    StreamDisplayAction::CopyLinkTuliproxWebPlayerUrl => {
                        if let Some(dto) = &*selected_dto {
                            let target_id = dto.channel.target_id;
                            let virtual_id = dto.channel.virtual_id;
                            let cluster = dto.channel.cluster;
                            let services = services.clone();
                            let translate = translate.clone();
                            let copy_to_clipboard = copy_to_clipboard.clone();
                            spawn_local(async move {
                                let request = PlaylistUrlResolveRequest::Webplayer { target_id, virtual_id, cluster };
                                if let Some(url) = services.playlist.resolve_url(request).await {
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
        <div class="tp__stream-display">
            <div class="tp__stream-display__header">
                <label>{translate.t("LABEL.ACTIVE_STREAMS")}</label>
            </div>
            <div class="tp__stream-display__body">
            {
                if let Some(streams) = visible_streams.as_ref() {
                    if streams.is_empty() {
                        html! { <NoContent /> }
                    } else {
                        html! {
                            <>
                                <div class="tp__stream-display__list">
                                    { for streams.iter().cloned().map(|stream| html! {
                                        <StreamDisplayItem
                                            stream={stream}
                                            metrics_enabled={metrics_enabled}
                                            on_popup_click={handle_popup_onclick.clone()}
                                        />
                                    })}
                                </div>
                                <PopupMenu is_open={*popup_is_open} anchor_ref={(*popup_anchor_ref).clone()} on_close={handle_popup_close}>
                                    <MenuItem icon="Disconnect" name={StreamDisplayAction::Kick.to_string()} label={translate.t("LABEL.KICK")} onclick={&handle_menu_click} class="tp__delete_action"></MenuItem>
                                    <MenuItem icon="Clipboard" name={StreamDisplayAction::CopyLinkTuliproxVirtualId.to_string()} label={translate.t("LABEL.COPY_LINK_TULIPROX_VIRTUAL_ID")} onclick={&handle_menu_click}></MenuItem>
                                    <MenuItem icon="Clipboard" name={StreamDisplayAction::CopyLinkTuliproxWebPlayerUrl.to_string()} label={translate.t("LABEL.COPY_LINK_TULIPROX_WEBPLAYER_URL")} onclick={&handle_menu_click}></MenuItem>
                                    <MenuItem icon="Clipboard" name={StreamDisplayAction::CopyLinkProviderUrl.to_string()} label={translate.t("LABEL.COPY_LINK_PROVIDER_URL")} onclick={&handle_menu_click}></MenuItem>
                                </PopupMenu>
                            </>
                        }
                    }
                } else {
                    html! { <NoContent /> }
                }
            }
           </div>
        </div>
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum StreamDisplayAction {
    Kick,
    CopyLinkTuliproxVirtualId,
    CopyLinkTuliproxWebPlayerUrl,
    CopyLinkProviderUrl,
}

impl Display for StreamDisplayAction {
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

impl FromStr for StreamDisplayAction {
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
