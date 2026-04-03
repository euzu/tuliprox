use crate::{
    app::{
        components::{menu_item::MenuItem, popup_menu::PopupMenu, AppIcon, Chip, IconButton, NoContent, Panel, Search},
        context::{ConfigContext, PlaylistExplorerContext},
    },
    hooks::use_service_context,
    html_if,
    i18n::use_translation,
    model::{BusyStatus, DialogAction, DialogActions, DialogResult, EventMessage},
    services::DialogService,
};
use shared::{
    error::{info_err_res, TuliproxError},
    model::{
        Permission, PlaylistRequest, PlaylistUrlResolveRequest, SearchRequest, SeriesStreamDetailEpisodeProperties,
        SeriesStreamProperties, UiPlaylistGroup, UiPlaylistItem, VirtualId, XtreamCluster,
    },
    utils::format_float_localized,
};
use std::{cell::RefCell, collections::HashMap, fmt::Display, rc::Rc, str::FromStr};
use wasm_bindgen::JsCast;
use web_sys::HtmlInputElement;
use yew::{platform::spawn_local, prelude::*};
use yew_hooks::use_clipboard;

const COPY_LINK_TULIPROX_VIRTUAL_ID: &str = "copy_link_tuliprox_virtual_id";
const COPY_LINK_TULIPROX_WEBPLAYER_URL: &str = "copy_link_tuliprox_webplayer_url";
const COPY_LINK_PROVIDER_URL: &str = "copy_link_provider_url";
const DOWNLOAD_ITEM: &str = "download_item";
const RECORD_ITEM: &str = "record_item";

#[derive(Clone)]
struct ChannelSelection {
    virtual_id: VirtualId,
    cluster: XtreamCluster,
    downloadable: bool,
    url: String,
    title: String,
    input_name: String,
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Eq, PartialEq)]
enum ExplorerAction {
    CopyLinkTuliproxVirtualId,
    CopyLinkTuliproxWebPlayerUrl,
    CopyLinkProviderUrl,
    Download,
    Record,
}

impl Display for ExplorerAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::CopyLinkTuliproxVirtualId => COPY_LINK_TULIPROX_VIRTUAL_ID,
                Self::CopyLinkTuliproxWebPlayerUrl => COPY_LINK_TULIPROX_WEBPLAYER_URL,
                Self::CopyLinkProviderUrl => COPY_LINK_PROVIDER_URL,
                Self::Download => DOWNLOAD_ITEM,
                Self::Record => RECORD_ITEM,
            }
        )
    }
}

impl FromStr for ExplorerAction {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        if s.eq(COPY_LINK_TULIPROX_VIRTUAL_ID) {
            Ok(Self::CopyLinkTuliproxVirtualId)
        } else if s.eq(COPY_LINK_TULIPROX_WEBPLAYER_URL) {
            Ok(Self::CopyLinkTuliproxWebPlayerUrl)
        } else if s.eq(COPY_LINK_PROVIDER_URL) {
            Ok(Self::CopyLinkProviderUrl)
        } else if s.eq(DOWNLOAD_ITEM) {
            Ok(Self::Download)
        } else if s.eq(RECORD_ITEM) {
            Ok(Self::Record)
        } else {
            info_err_res!("Unknown ExplorerAction: {}", s)
        }
    }
}

fn build_download_filename(title: &str, url: &str) -> String {
    let sanitized = title
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => c,
            _ => '_',
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string();
    let base = if sanitized.is_empty() { "download".to_string() } else { sanitized };
    let ext = url
        .split('?')
        .next()
        .and_then(|base| base.rsplit('/').next())
        .and_then(|name| name.rsplit_once('.').map(|(_, ext)| ext))
        .filter(|ext| !ext.is_empty())
        .map(|ext| format!(".{ext}"))
        .unwrap_or_else(|| ".mp4".to_string());
    if base.ends_with(&ext) {
        base
    } else {
        format!("{base}{ext}")
    }
}

fn default_record_start_value() -> String { chrono::Local::now().format("%Y-%m-%dT%H:%M").to_string() }

fn parse_record_start_value(start_value: &str) -> Option<i64> {
    use chrono::TimeZone;

    let start_value = start_value.trim();
    let naive = ["%Y-%m-%dT%H:%M", "%Y-%m-%dT%H:%M:%S"]
        .into_iter()
        .find_map(|format| chrono::NaiveDateTime::parse_from_str(start_value, format).ok())?;
    chrono::Local.from_local_datetime(&naive).earliest().map(|dt| dt.timestamp())
}

fn parse_record_duration_minutes(duration_value: &str) -> Option<u64> {
    let minutes = duration_value.trim().parse::<u64>().ok()?;
    (minutes > 0).then_some(minutes)
}

fn build_record_filename(title: &str, start_at: &str) -> String {
    let sanitized = title
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => c,
            _ => '_',
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string();
    let base = if sanitized.is_empty() { "recording".to_string() } else { sanitized };
    let time_part = start_at.replace([':', 'T'], "-");
    format!("{base}_{time_part}.ts")
}

fn parse_optional_priority_input(priority_value: Option<String>) -> Result<Option<i8>, String> {
    let Some(raw) = priority_value.as_deref() else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed.parse::<i8>().map(Some).map_err(|_| "Priority must be a whole number between -128 and 127".to_string())
}

fn normalize_input_name(input_name: &str) -> Option<String> {
    let trimmed = input_name.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn can_show_download_action(can_write_downloads: bool, selected_channel: Option<&ChannelSelection>) -> bool {
    can_write_downloads && selected_channel.is_some_and(|item| item.cluster != XtreamCluster::Live && item.downloadable)
}

fn can_show_record_action(can_write_downloads: bool, selected_channel: Option<&ChannelSelection>) -> bool {
    can_write_downloads && selected_channel.is_some_and(|item| item.cluster == XtreamCluster::Live)
}

enum ExplorerLevel {
    Categories,
    Group(Rc<UiPlaylistGroup>),
    SeriesInfo(Rc<UiPlaylistGroup>, Rc<UiPlaylistItem>, Option<Box<SeriesStreamProperties>>),
}

#[component]
pub fn PlaylistExplorer() -> Html {
    let context = use_context::<PlaylistExplorerContext>().expect("PlaylistExplorer context not found");
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let dialog = use_context::<DialogService>().expect("Dialog service not found");
    let translate = use_translation();
    let service_ctx = use_service_context();
    let can_write_downloads = service_ctx.auth.has_permission(Permission::DownloadWrite);
    let default_download_priority = config_ctx
        .config
        .as_ref()
        .and_then(|cfg| cfg.config.video.as_ref())
        .and_then(|video| video.download.as_ref())
        .map(|download| download.download_priority);
    let default_recording_priority = config_ctx
        .config
        .as_ref()
        .and_then(|cfg| cfg.config.video.as_ref())
        .and_then(|video| video.download.as_ref())
        .map(|download| download.recording_priority);
    let current_item = use_state(|| ExplorerLevel::Categories);
    let playlist = use_state(|| (*context.playlist).clone());
    let selected_channel = use_state(|| None::<ChannelSelection>);
    let popup_anchor_ref = use_state(|| None::<web_sys::Element>);
    let popup_is_open = use_state(|| false);
    let clipboard = use_clipboard();
    let cluster_visible = use_state(|| XtreamCluster::Live);

    let handle_cluster_change = {
        let cluster_vis = cluster_visible.clone();
        Callback::from(move |(name, _event): (String, MouseEvent)| {
            if let Ok(xc) = XtreamCluster::from_str(name.as_str()) {
                cluster_vis.set(xc);
            }
        })
    };

    let handle_popup_close = {
        let set_is_open = popup_is_open.clone();
        Callback::from(move |()| {
            set_is_open.set(false);
        })
    };

    let handle_popup_onclick = {
        let set_selected_channel = selected_channel.clone();
        let set_anchor_ref = popup_anchor_ref.clone();
        let set_is_open = popup_is_open.clone();
        Callback::from(move |(dto, event): (Rc<UiPlaylistItem>, MouseEvent)| {
            event.prevent_default();
            event.stop_propagation();
            if let Some(target) = event.target_dyn_into::<web_sys::Element>() {
                set_selected_channel.set(Some(ChannelSelection {
                    virtual_id: dto.virtual_id,
                    cluster: dto.xtream_cluster,
                    downloadable: dto.xtream_cluster == XtreamCluster::Video,
                    url: dto.url.to_string(),
                    title: dto.title.to_string(),
                    input_name: dto.input_name.to_string(),
                }));
                set_anchor_ref.set(Some(target));
                set_is_open.set(true);
            }
        })
    };

    let handle_episode_popup_onclick = {
        let set_selected_channel = selected_channel.clone();
        let set_anchor_ref = popup_anchor_ref.clone();
        let set_is_open = popup_is_open.clone();
        Callback::from(move |(dto, event): (ChannelSelection, MouseEvent)| {
            event.prevent_default();
            event.stop_propagation();
            if let Some(target) = event.target_dyn_into::<web_sys::Element>() {
                set_selected_channel.set(Some(dto));
                set_anchor_ref.set(Some(target));
                set_is_open.set(true);
            }
        })
    };

    let load_series_info = {
        let set_current_item = current_item.clone();
        let services = service_ctx.clone();
        let ctx = context.clone();

        move |group: Rc<UiPlaylistGroup>, dto: Rc<UiPlaylistItem>| {
            // UiPlaylistItem has no additional_properties - always load from server
            let set_current_item = set_current_item.clone();
            let services = services.clone();
            let ctx = ctx.clone();
            services.event.broadcast(EventMessage::Busy(BusyStatus::Show));
            spawn_local(async move {
                let mut handled = false;
                if let Some(playlist_request) = ctx.playlist_request.as_ref() {
                    if let Some(props) = services.playlist.get_series_info(&dto, playlist_request).await {
                        handled = true;
                        set_current_item.set(ExplorerLevel::SeriesInfo(
                            group.clone(),
                            dto.clone(),
                            Some(Box::new(props)),
                        ));
                    }
                }
                if !handled {
                    set_current_item.set(ExplorerLevel::SeriesInfo(group, dto, None));
                }
                services.event.broadcast(EventMessage::Busy(BusyStatus::Hide));
            });
        }
    };

    let handle_series_onclick = {
        let set_current_item = current_item.clone();
        Callback::from(move |(dto, event): (Rc<UiPlaylistItem>, MouseEvent)| {
            event.prevent_default();
            event.stop_propagation();
            if let ExplorerLevel::Group(ref group) = *set_current_item {
                load_series_info(group.clone(), dto.clone());
            }
        })
    };

    {
        let set_playlist = playlist.clone();
        let set_current_item = current_item.clone();
        let set_selected_channel = selected_channel.clone();
        let set_popup_is_open = popup_is_open.clone();
        let set_anchor_ref = popup_anchor_ref.clone();
        use_effect_with((*context.playlist).clone(), move |new_playlist| {
            set_current_item.set(ExplorerLevel::Categories);
            set_playlist.set(new_playlist.clone());
            // Reset popup state and selection when the underlying data changes
            set_selected_channel.set(None);
            set_popup_is_open.set(false);
            set_anchor_ref.set(None);
            || {}
        });
    }

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
        let services = service_ctx.clone();
        let dialog = dialog.clone();
        let popup_is_open_state = popup_is_open.clone();
        let selected_channel = selected_channel.clone();
        let playlist_ctx = context.clone();
        let translate_clone = translate.clone();
        let can_queue_downloads = can_write_downloads;
        let copy_to_clipboard = copy_to_clipboard.clone();
        Callback::from(move |(name, _): (String, _)| {
            if let Ok(action) = ExplorerAction::from_str(&name) {
                match action {
                    ExplorerAction::CopyLinkTuliproxVirtualId => {
                        if let Some(dto) = &*selected_channel {
                            copy_to_clipboard.emit(dto.virtual_id.to_string());
                        }
                    }
                    ExplorerAction::CopyLinkTuliproxWebPlayerUrl => {
                        if let Some(playlist_request) = playlist_ctx.playlist_request.as_ref() {
                            match playlist_request {
                                PlaylistRequest::Target(target_id) => {
                                    if let Some(dto) = &*selected_channel {
                                        let copy_to_clipboard = copy_to_clipboard.clone();
                                        let services = services.clone();
                                        let virtual_id = dto.virtual_id;
                                        let cluster = dto.cluster;
                                        let translate_clone = translate_clone.clone();
                                        let target_id = *target_id;
                                        let services_clone = services.clone();
                                        spawn_local(async move {
                                            let request =
                                                PlaylistUrlResolveRequest::Webplayer { target_id, virtual_id, cluster };
                                            if let Some(url) = services.playlist.resolve_url(request).await {
                                                copy_to_clipboard.emit(url);
                                                services_clone.toastr.success(
                                                    translate_clone
                                                        .t("MESSAGES.PLAYLIST.WEBPLAYER_URL_COPY_TO_CLIPBOARD"),
                                                );
                                            } else {
                                                services_clone.toastr.error(
                                                    translate_clone.t("MESSAGES.FAILED_TO_RETRIEVE_WEBPLAYER_URL"),
                                                );
                                            }
                                        });
                                    }
                                }
                                PlaylistRequest::Input(_) => {}
                                PlaylistRequest::CustomXtream(_) => {}
                                PlaylistRequest::CustomM3u(_) => {}
                            }
                        }
                    }
                    ExplorerAction::CopyLinkProviderUrl => {
                        if let Some(dto) = &*selected_channel {
                            let url = dto.url.clone();
                            if !url.is_empty() {
                                if let Some(playlist_request) = playlist_ctx.playlist_request.as_ref() {
                                    let copy_to_clipboard = copy_to_clipboard.clone();
                                    let services = services.clone();
                                    let playlist_request = playlist_request.clone();
                                    spawn_local(async move {
                                        let request = PlaylistUrlResolveRequest::Provider {
                                            playlist_request,
                                            url: url.to_string(),
                                        };
                                        let resolved = services
                                            .playlist
                                            .resolve_url(request)
                                            .await
                                            .unwrap_or_else(|| url.to_string());
                                        copy_to_clipboard.emit(resolved);
                                    });
                                } else {
                                    copy_to_clipboard.emit(url);
                                }
                            } else {
                                // Try to fetch episode
                                if let Some(playlist_request) = playlist_ctx.playlist_request.as_ref() {
                                    let copy_to_clipboard = copy_to_clipboard.clone();
                                    let services = services.clone();
                                    let virtual_id = dto.virtual_id;
                                    let playlist_request = playlist_request.clone();
                                    spawn_local(async move {
                                        if let Some(pli) =
                                            services.playlist.get_episode(virtual_id, &playlist_request).await
                                        {
                                            let url = pli.url.to_string();
                                            let request = PlaylistUrlResolveRequest::Provider {
                                                playlist_request,
                                                url: url.to_string(),
                                            };
                                            let resolved = services.playlist.resolve_url(request).await.unwrap_or(url);
                                            copy_to_clipboard.emit(resolved);
                                        }
                                    });
                                }
                            }
                        }
                    }
                    ExplorerAction::Download => {
                        if !can_queue_downloads {
                            popup_is_open_state.set(false);
                            return;
                        }
                        if let Some(dto) = &*selected_channel {
                            let dialog = dialog.clone();
                            let services = services.clone();
                            let translate_clone = translate_clone.clone();
                            let playlist_request = (*playlist_ctx.playlist_request).clone();
                            let default_download_priority = default_download_priority;
                            let selected = dto.clone();
                            spawn_local(async move {
                                let resolved_url = if !selected.url.is_empty() {
                                    if let Some(playlist_request) = playlist_request.clone() {
                                        let request = PlaylistUrlResolveRequest::Provider {
                                            playlist_request,
                                            url: selected.url.clone(),
                                        };
                                        services.playlist.resolve_url(request).await.unwrap_or(selected.url.clone())
                                    } else {
                                        selected.url.clone()
                                    }
                                } else if selected.cluster == XtreamCluster::Series {
                                    if let Some(playlist_request) = playlist_request.as_ref() {
                                        if let Some(pli) =
                                            services.playlist.get_episode(selected.virtual_id, playlist_request).await
                                        {
                                            let episode_url = pli.url.to_string();
                                            let request = PlaylistUrlResolveRequest::Provider {
                                                playlist_request: playlist_request.clone(),
                                                url: episode_url.clone(),
                                            };
                                            services.playlist.resolve_url(request).await.unwrap_or(episode_url)
                                        } else {
                                            String::new()
                                        }
                                    } else {
                                        String::new()
                                    }
                                } else {
                                    String::new()
                                };

                                if resolved_url.is_empty() {
                                    services.toastr.error(translate_clone.t("MESSAGES.DOWNLOAD.FAIL"));
                                    return;
                                }

                                let default_filename = build_download_filename(&selected.title, &resolved_url);
                                let filename_value = Rc::new(RefCell::new(default_filename.clone()));
                                let default_download_priority_value =
                                    default_download_priority.map_or_else(String::new, |priority| priority.to_string());
                                let priority_value = Rc::new(RefCell::new(default_download_priority_value.clone()));
                                let actions = DialogActions {
                                    left: Some(vec![DialogAction::new(
                                        "cancel",
                                        "LABEL.CANCEL",
                                        DialogResult::Cancel,
                                        Some("Close".to_owned()),
                                        None,
                                    )]),
                                    right: vec![DialogAction::new_focused(
                                        "download",
                                        "LABEL.DOWNLOAD",
                                        DialogResult::Ok,
                                        Some("Download".to_owned()),
                                        Some("primary".to_string()),
                                    )],
                                };
                                let filename_value_input = Rc::clone(&filename_value);
                                let priority_value_input = Rc::clone(&priority_value);
                                let result = dialog
                                    .content(
                                        html! {
                                            <div class="tp__record-dialog">
                                                <div class="tp__input">
                                                    <label class="tp__label">{translate_clone.t("LABEL.FILENAME")}</label>
                                                    <div class="tp__input-wrapper">
                                                        <input
                                                            type="text"
                                                            value={default_filename.clone()}
                                                            oninput={Callback::from(move |event: InputEvent| {
                                                                let input: HtmlInputElement = event.target_unchecked_into();
                                                                *filename_value_input.borrow_mut() = input.value();
                                                            })}
                                                        />
                                                    </div>
                                                </div>
                                                <div class="tp__input">
                                                    <label class="tp__label">{translate_clone.t("LABEL.PRIORITY")}</label>
                                                    <div class="tp__input-wrapper">
                                                        <input
                                                            type="number"
                                                            min="-127"
                                                            max="127"
                                                            step="1"
                                                            value={default_download_priority_value.clone()}
                                                            oninput={Callback::from(move |event: InputEvent| {
                                                                let input: HtmlInputElement = event.target_unchecked_into();
                                                                *priority_value_input.borrow_mut() = input.value();
                                                            })}
                                                        />
                                                    </div>
                                                </div>
                                                <div class="tp__field-explanation">
                                                    {selected.title.clone()}
                                                </div>
                                            </div>
                                        },
                                        Some(actions),
                                        false,
                                    )
                                    .await;

                                if result != DialogResult::Ok {
                                    return;
                                }

                                let filename = filename_value.borrow().clone().trim().to_string();
                                let priority =
                                    match parse_optional_priority_input(Some(priority_value.borrow().clone())) {
                                        Ok(priority) => priority,
                                        Err(err) => {
                                            services.toastr.error(err);
                                            return;
                                        }
                                    };

                                if filename.is_empty() {
                                    services.toastr.error(translate_clone.t("MESSAGES.DOWNLOAD.FAIL"));
                                    return;
                                }

                                let input_name = normalize_input_name(&selected.input_name);
                                match services
                                    .downloads
                                    .queue_download(resolved_url, filename, input_name, priority)
                                    .await
                                {
                                    Ok(_) => {
                                        services.toastr.success(translate_clone.t("MESSAGES.DOWNLOAD.DOWNLOAD_QUEUED"))
                                    }
                                    Err(_) => services.toastr.error(translate_clone.t("MESSAGES.DOWNLOAD.FAIL")),
                                }
                            });
                        }
                    }
                    ExplorerAction::Record => {
                        if !can_queue_downloads {
                            popup_is_open_state.set(false);
                            return;
                        }
                        if let Some(dto) = &*selected_channel {
                            let dialog = dialog.clone();
                            let services = services.clone();
                            let translate_clone = translate_clone.clone();
                            let playlist_request = (*playlist_ctx.playlist_request).clone();
                            let default_recording_priority = default_recording_priority;
                            let selected = dto.clone();
                            spawn_local(async move {
                                let default_start_value = default_record_start_value();
                                let start_value = Rc::new(RefCell::new(default_start_value.clone()));
                                let duration_value = Rc::new(RefCell::new("90".to_string()));
                                let priority_value = Rc::new(RefCell::new(
                                    default_recording_priority
                                        .map_or_else(String::new, |priority| priority.to_string()),
                                ));
                                let actions = DialogActions {
                                    left: Some(vec![DialogAction::new(
                                        "cancel",
                                        "LABEL.CANCEL",
                                        DialogResult::Cancel,
                                        Some("Close".to_owned()),
                                        None,
                                    )]),
                                    right: vec![DialogAction::new_focused(
                                        "record",
                                        "LABEL.RECORD",
                                        DialogResult::Ok,
                                        Some("Record".to_owned()),
                                        Some("primary".to_string()),
                                    )],
                                };
                                let start_value_input = Rc::clone(&start_value);
                                let duration_value_input = Rc::clone(&duration_value);
                                let priority_value_input = Rc::clone(&priority_value);
                                let default_recording_priority_value = default_recording_priority
                                    .map_or_else(String::new, |priority| priority.to_string());
                                let result = dialog
                                    .content(
                                        html! {
                                            <div class="tp__record-dialog">
                                                <div class="tp__input">
                                                    <label class="tp__label">{translate_clone.t("LABEL.START")}</label>
                                                    <div class="tp__input-wrapper">
                                                        <input
                                                            type="datetime-local"
                                                            value={default_start_value.clone()}
                                                            oninput={Callback::from(move |event: InputEvent| {
                                                                let input: HtmlInputElement = event.target_unchecked_into();
                                                                *start_value_input.borrow_mut() = input.value();
                                                            })}
                                                        />
                                                    </div>
                                                </div>
                                                <div class="tp__input">
                                                    <label class="tp__label">{translate_clone.t("LABEL.DURATION")}{" (min)"}</label>
                                                    <div class="tp__input-wrapper">
                                                        <input
                                                            type="number"
                                                            min="1"
                                                            step="1"
                                                            value="90"
                                                            oninput={Callback::from(move |event: InputEvent| {
                                                                let input: HtmlInputElement = event.target_unchecked_into();
                                                                *duration_value_input.borrow_mut() = input.value();
                                                            })}
                                                        />
                                                    </div>
                                                </div>
                                                <div class="tp__input">
                                                    <label class="tp__label">{translate_clone.t("LABEL.PRIORITY")}</label>
                                                    <div class="tp__input-wrapper">
                                                        <input
                                                            type="number"
                                                            min="-127"
                                                            max="127"
                                                            step="1"
                                                            value={default_recording_priority_value.clone()}
                                                            oninput={Callback::from(move |event: InputEvent| {
                                                                let input: HtmlInputElement = event.target_unchecked_into();
                                                                *priority_value_input.borrow_mut() = input.value();
                                                            })}
                                                        />
                                                    </div>
                                                </div>
                                                <div class="tp__field-explanation">
                                                    {selected.title.clone()}
                                                </div>
                                            </div>
                                        },
                                        Some(actions),
                                        false,
                                    )
                                    .await;

                                if result == DialogResult::Ok {
                                    let start_value = start_value.borrow().clone();
                                    let duration_value = duration_value.borrow().clone();
                                    let priority =
                                        match parse_optional_priority_input(Some(priority_value.borrow().clone())) {
                                            Ok(priority) => priority,
                                            Err(err) => {
                                                services.toastr.error(err);
                                                return;
                                            }
                                        };
                                    let start_ts = parse_record_start_value(&start_value);
                                    let duration_mins = parse_record_duration_minutes(&duration_value);

                                    match (start_ts, duration_mins) {
                                        (Some(start_at), Some(minutes)) => {
                                            let filename = build_record_filename(&selected.title, &start_value);
                                            let input_name = normalize_input_name(&selected.input_name);
                                            let resolved_url = if let Some(playlist_request) = playlist_request.clone()
                                            {
                                                let request = PlaylistUrlResolveRequest::Provider {
                                                    playlist_request,
                                                    url: selected.url.clone(),
                                                };
                                                services
                                                    .playlist
                                                    .resolve_url(request)
                                                    .await
                                                    .unwrap_or(selected.url.clone())
                                            } else {
                                                selected.url.clone()
                                            };
                                            match services
                                                .downloads
                                                .queue_recording(
                                                    resolved_url,
                                                    filename,
                                                    start_at,
                                                    minutes.saturating_mul(60),
                                                    input_name,
                                                    priority,
                                                )
                                                .await
                                            {
                                                Ok(_) => {
                                                    services.toastr.success(
                                                        translate_clone.t("MESSAGES.DOWNLOAD.RECORDING_QUEUED"),
                                                    );
                                                }
                                                Err(_) => {
                                                    services.toastr.error(translate_clone.t("MESSAGES.DOWNLOAD.FAIL"));
                                                }
                                            }
                                        }
                                        (None, _) => services
                                            .toastr
                                            .error(translate_clone.t("MESSAGES.DOWNLOAD.INVALID_RECORD_START")),
                                        (_, None) => services
                                            .toastr
                                            .error(translate_clone.t("MESSAGES.DOWNLOAD.INVALID_RECORD_DURATION")),
                                    }
                                }
                            });
                        }
                    }
                }
            }
            popup_is_open_state.set(false);
        })
    };

    let handle_back_click = {
        let current_item = current_item.clone();
        Callback::from(move |_| match *current_item {
            ExplorerLevel::Categories => {}
            ExplorerLevel::Group(_) => {
                current_item.set(ExplorerLevel::Categories);
            }
            ExplorerLevel::SeriesInfo(ref group, _, _) => {
                current_item.set(ExplorerLevel::Group(group.clone()));
            }
        })
    };

    let handle_search = {
        let services = service_ctx.clone();
        let set_playlist = playlist.clone();
        let set_current_item = current_item.clone();
        let context = context.clone();
        Callback::from(move |search_req| match search_req {
            SearchRequest::Clear => set_playlist.set((*context.playlist).clone()),
            SearchRequest::Text(ref _text, ref _search_fields)
            | SearchRequest::Regexp(ref _text, ref _search_fields) => {
                services.event.broadcast(EventMessage::Busy(BusyStatus::Show));
                let set_playlist = set_playlist.clone();
                let set_current_item = set_current_item.clone();
                let context = context.clone();
                let services = services.clone();
                spawn_local(async move {
                    let filtered =
                        context.playlist.as_ref().and_then(|categories| categories.filter(&search_req)).map(Rc::new);
                    set_playlist.set(filtered);
                    set_current_item.set(ExplorerLevel::Categories);
                    services.event.broadcast(EventMessage::Busy(BusyStatus::Hide));
                });
            }
        })
    };

    let handle_category_select = {
        let set_current_item = current_item.clone();
        Callback::from(move |(group, _event): (Rc<UiPlaylistGroup>, MouseEvent)| {
            set_current_item.set(ExplorerLevel::Group(group));
        })
    };

    let render_cluster = |cluster: XtreamCluster, list: &Vec<Rc<UiPlaylistGroup>>| {
        list.iter()
            .map(|group| {
                let group_clone = group.clone();
                let on_click = {
                    let category_select = handle_category_select.clone();
                    Callback::from(move |event: MouseEvent| {
                        category_select.emit((group_clone.clone(), event));
                    })
                };
                html! {
                <span class={format!("tp__playlist-explorer__item tp__playlist-explorer__item-{}", cluster.to_string().to_lowercase())} onclick={on_click}>
                    { group.title.clone() }
                </span>
            }
            })
            .collect::<Html>()
    };

    let render_categories = || {
        if playlist.is_none() {
            html! {
                <NoContent/>
            }
        } else {
            html! {
            <div class="tp__playlist-explorer__categories">
                <div class="tp__playlist-explorer__categories-sidebar tp__app-sidebar__content">
                    <IconButton class={format!("tp__app-sidebar-menu--{}{}", XtreamCluster::Live, if *cluster_visible == XtreamCluster::Live { " active" } else {""})}  icon="Live" name={XtreamCluster::Live.to_string()} onclick={&handle_cluster_change}></IconButton>
                    <IconButton class={format!("tp__app-sidebar-menu--{}{}", XtreamCluster::Video, if *cluster_visible == XtreamCluster::Video { " active" } else {""})} icon="Video" name={XtreamCluster::Video.to_string()} onclick={&handle_cluster_change}></IconButton>
                    <IconButton class={format!("tp__app-sidebar-menu--{}{}", XtreamCluster::Series, if *cluster_visible == XtreamCluster::Series { " active" } else {""})} icon="Series" name={XtreamCluster::Series.to_string()} onclick={&handle_cluster_change}></IconButton>
                </div>
                <div class="tp__playlist-explorer__categories-content">
                    <Panel class="tp__full-width" value={XtreamCluster::Live.to_string()} active={cluster_visible.to_string()}>
                        <div class="tp__playlist-explorer__categories-list">
                            { playlist.as_ref()
                                .and_then(|response| response.live.as_ref())
                                .map(|list| render_cluster(XtreamCluster::Live, list))
                                .unwrap_or_default()
                            }
                            </div>
                    </Panel>
                    <Panel class="tp__full-width" value={XtreamCluster::Video.to_string()} active={cluster_visible.to_string()}>
                        <div class="tp__playlist-explorer__categories-list">
                            { playlist.as_ref()
                                .and_then(|response| response.vod.as_ref())
                                .map(|list| render_cluster(XtreamCluster::Video, list))
                                .unwrap_or_default()
                            }
                            </div>
                    </Panel>
                    <Panel class="tp__full-width" value={XtreamCluster::Series.to_string()} active={cluster_visible.to_string()}>
                        <div class="tp__playlist-explorer__categories-list">
                            { playlist.as_ref()
                                .and_then(|response| response.series.as_ref())
                                .map(|list| render_cluster(XtreamCluster::Series, list))
                                .unwrap_or_default()
                            }
                        </div>
                    </Panel>
                </div>
            </div>
            }
        }
    };

    let render_channel_logo = |logo: &str| {
        let logo = if logo.is_empty() { "assets/missing-logo.svg".to_string() } else { logo.to_string() };
        html! {
            <span  class="tp__playlist-explorer__channel-logo">
                <img  alt={"n/a"} src={logo} loading="lazy"
                onerror={Callback::from(move |e: web_sys::Event| {
                if let Some(target)  = e.target() {
                    if let Ok(img) = target.dyn_into::<web_sys::HtmlImageElement>() {
                        img.set_src("assets/missing-logo.svg");
                    }
                }
                })}/>
            </span>
        }
    };

    let render_live = |chan: &Rc<UiPlaylistItem>| {
        let popup_onclick = handle_popup_onclick.clone();
        let chan_clone = Rc::clone(chan);
        html! {
        <span class="tp__playlist-explorer__channel tp__playlist-explorer__channel-live">
            <button class="tp__icon-button" onclick={Callback::from(move |event: MouseEvent| popup_onclick.emit((chan_clone.clone(), event)))}>
                <AppIcon name="Popup"></AppIcon>
            </button>
            {render_channel_logo(&chan.logo)}
            <span class="tp__playlist-explorer__channel-title">{chan.title.clone()}</span>
            </span>
        }
    };

    let render_movie = |chan: &Rc<UiPlaylistItem>| {
        let popup_onclick = handle_popup_onclick.clone();
        let chan_clone = Rc::clone(chan);
        html! {
            <span class="tp__playlist-explorer__channel tp__playlist-explorer__channel-video">
                {render_channel_logo(&chan.logo)}
                {
                    html_if!(chan.rating > 0.001, {
                        <Chip class="tp__playlist-explorer__channel-video-rating" label={format_float_localized(chan.rating, 1, false)} />
                    })
                }
                <span class="tp__playlist-explorer__channel-video-info">
                    <button class="tp__icon-button" onclick={Callback::from(move |event: MouseEvent| popup_onclick.emit((chan_clone.clone(), event)))}>
                        <AppIcon name="Popup"></AppIcon>
                    </button>
                    <span class="tp__playlist-explorer__channel-video-title">{chan.title.clone()}</span>
                </span>
            </span>
        }
    };

    let render_series = |chan: &Rc<UiPlaylistItem>| {
        let popup_onclick = handle_popup_onclick.clone();
        let chan_clone = Rc::clone(chan);
        let chan_click = {
            let chan_clone = chan.clone();
            let series_click = handle_series_onclick.clone();
            Callback::from(move |event: MouseEvent| series_click.emit((chan_clone.clone(), event)))
        };
        html! {
            <span onclick={chan_click} class="tp__playlist-explorer__channel tp__playlist-explorer__channel-series">
                {render_channel_logo(&chan.logo)}
                {
                    html_if!(chan.rating > 0.001, {
                        <Chip class="tp__playlist-explorer__channel-series-rating" label={format_float_localized(chan.rating, 1, false)} />
                    })
                }
                <span class="tp__playlist-explorer__channel-series-info">
                    <button class="tp__icon-button" onclick={Callback::from(move |event: MouseEvent| popup_onclick.emit((chan_clone.clone(), event)))}>
                        <AppIcon name="Popup"></AppIcon>
                    </button>
                    <span class="tp__playlist-explorer__channel-series-title">{chan.title.clone()}</span>
                </span>
            </span>
        }
    };

    let render_episode = |chan: &SeriesStreamDetailEpisodeProperties| {
        let channel_select = ChannelSelection {
            virtual_id: chan.id,
            cluster: XtreamCluster::Series,
            downloadable: true,
            url: String::new(), // TODO provider url
            title: chan.title.to_string(),
            input_name: String::new(),
        };
        let popup_onclick = handle_episode_popup_onclick.clone();
        let rating = chan.rating.unwrap_or_default();
        html! {
            <span class="tp__playlist-explorer__channel tp__playlist-explorer__channel-episode">
                {render_channel_logo(&chan.movie_image)}
                {
                    html_if!(rating > 0.001, {
                        <Chip class="tp__playlist-explorer__channel-episode-rating" label={format_float_localized(rating, 1, false)} />
                    })
                }
                <span class="tp__playlist-explorer__channel-episode-info">
                    <button class="tp__icon-button" onclick={Callback::from(move |event: MouseEvent| popup_onclick.emit((channel_select.clone(), event)))}>
                        <AppIcon name="Popup"></AppIcon>
                    </button>
                    <span class="tp__playlist-explorer__channel-episode-title">{chan.title.clone()}</span>
                </span>
            </span>
        }
    };

    let render_channel = |chan: &Rc<UiPlaylistItem>| match chan.xtream_cluster {
        XtreamCluster::Live => render_live(chan),
        XtreamCluster::Video => render_movie(chan),
        XtreamCluster::Series => render_series(chan),
    };

    let render_group = |group: &Rc<UiPlaylistGroup>| {
        html! {
            <div class="tp__playlist-explorer__group">
              <div class={format!("tp__playlist-explorer__group-list tp__playlist-explorer__group-list-{}", group.xtream_cluster.to_string().to_lowercase())}>
              {
                  group.channels.iter().map(render_channel).collect::<Html>()
              }
              </div>
            </div>
        }
    };

    let render_series_info = |series_info: &Rc<UiPlaylistItem>, props: Option<&Box<SeriesStreamProperties>>| {
        // UiPlaylistItem has no additional_properties - props are passed in or None
        let series_info_props = props;
        let (mut backdrop, plot, cast, genre, release_date, rating, details) = match series_info_props {
            Some(series_props) => {
                let backdrop = series_props.backdrop_path.as_ref().and_then(|l| l.first()).map_or_else(
                    || {
                        if series_props.cover.is_empty() {
                            series_info.logo.to_string()
                        } else {
                            series_props.cover.to_string()
                        }
                    },
                    ToString::to_string,
                );
                (
                    Some(backdrop.to_string()),
                    series_props.plot.as_deref().map(ToString::to_string).unwrap_or_default(),
                    series_props.cast.to_string(),
                    series_props.genre.as_deref().map(ToString::to_string).unwrap_or_default(),
                    series_props.release_date.as_deref().map(ToString::to_string).unwrap_or_default(),
                    series_props.rating,
                    series_props.details.as_ref(),
                )
            }
            _ => (None, String::new(), String::new(), String::new(), String::new(), 0.0, None),
        };

        if !series_info.logo.is_empty() && backdrop.as_ref().is_none_or(|v| v.is_empty()) {
            backdrop = Some(series_info.logo.to_string());
        };

        let style = backdrop.as_ref().map(|b| format!("background-image: url(\"{b}\");")).unwrap_or_default();

        let series_html = html! {
            <div class="tp__playlist-explorer__series-info__body-top" style={style}>
                <div class="tp__playlist-explorer__series-info__body-top-backdrop"></div>
                <div class="tp__playlist-explorer__series-info__body-top-content">
                    <span class="tp__playlist-explorer__series-info__title">{series_info.title.clone()}</span>
                    <span class="tp__playlist-explorer__series-info__infos">
                        {
                            html_if!(rating > 0.001, {
                            <>
                             <span class="tp__playlist-explorer__series-info__nowrap">
                                 <Chip class="tp__playlist-explorer__series-info__rating" label={format_float_localized(rating, 1, false)} />
                            </span>
                            {"◦"}
                            </>
                        })}
                        <span class="tp__playlist-explorer__series-info__nowrap">{release_date}</span>
                        {"◦"}
                        <span>{genre}</span>
                    </span>
                    <span class="tp__playlist-explorer__series-info__plot">{plot}</span>
                    <span class="tp__playlist-explorer__series-info__cast">{cast}</span>
                </div>
            </div>
        };

        let episodes_html = if let Some(episodes) = details.as_ref().and_then(|d| d.episodes.as_ref()) {
            let mut grouped: HashMap<u32, Vec<&SeriesStreamDetailEpisodeProperties>> = HashMap::new();
            for item in episodes {
                grouped.entry(item.season).or_default().push(item);
            }
            let mut grouped_list: Vec<(u32, Vec<&SeriesStreamDetailEpisodeProperties>)> = grouped.into_iter().collect();
            grouped_list.sort_by_key(|(season, _)| *season);

            html! {
                for (season, season_episodes) in grouped_list.iter() {
                    <div key={format!("season-{season}")}>
                        <div class={"tp__playlist-explorer__series-info__season"}>
                            <span class={"tp__playlist-explorer__series-info__season-title"}>{translate.t("LABEL.SEASON")} {" - "} {season}</span>
                        </div>
                        <div class={"tp__playlist-explorer__group-list tp__playlist-explorer__group-list-episodes"}>
                            for episode in season_episodes.iter() {
                                { render_episode(episode) }
                            }
                        </div>
                    </div>
                }
            }
        } else {
            Html::default()
        };

        html! {
        <div class="tp__playlist-explorer__series-info">
            <div class="tp__playlist-explorer__series-info__header">
                { series_html }
            </div>
             <div class="tp__playlist-explorer__series-info__body">
                 {episodes_html}
            </div>
        </div>
        }
    };

    html! {
      <div class="tp__playlist-explorer">
        <div class="tp__playlist-explorer__header">
            <div class="tp__playlist-explorer__header-toolbar">
                <div class="tp__playlist-explorer__header-toolbar-actions">
                   <IconButton class={if matches!(*current_item, ExplorerLevel::Categories) { "disabled" } else {""}} name="back" icon="Back" onclick={handle_back_click} />
                  {
                    match *current_item {
                        ExplorerLevel::Categories => html!{} ,
                        ExplorerLevel::Group(ref group) => html!{ <span>{group.title.to_string()}</span> },
                        ExplorerLevel::SeriesInfo(_, ref pli, _) => html!{ <span>{pli.title.to_string()}</span> },
                    }
                  }
                </div>
                <div class="tp__playlist-explorer__header-toolbar-search">
                  <Search onsearch={handle_search}/>
                </div>
            </div>
        </div>
        <div class="tp__playlist-explorer__body">
          {
            match *current_item {
                ExplorerLevel::Categories => html!{render_categories()} ,
                ExplorerLevel::Group(ref group) => html!{ render_group(group) },
                ExplorerLevel::SeriesInfo(_, ref pli, ref props) => html!{ render_series_info(pli, props.as_ref()) },
            }
          }
        </div>

        <PopupMenu is_open={*popup_is_open} anchor_ref={(*popup_anchor_ref).clone()} on_close={handle_popup_close}>
            { html_if!(context.playlist_request.as_ref().is_some_and(|r| matches!(r, PlaylistRequest::Target(_))), {
                <>
                 <MenuItem icon="Clipboard" name={ExplorerAction::CopyLinkTuliproxVirtualId.to_string()} label={translate.t("LABEL.COPY_LINK_TULIPROX_VIRTUAL_ID")} onclick={&handle_menu_click}></MenuItem>
                 <MenuItem icon="Clipboard" name={ExplorerAction::CopyLinkTuliproxWebPlayerUrl.to_string()} label={translate.t("LABEL.COPY_LINK_TULIPROX_WEBPLAYER_URL")} onclick={&handle_menu_click}></MenuItem>
                </>
             })
            }
            <MenuItem icon="Clipboard" name={ExplorerAction::CopyLinkProviderUrl.to_string()} label={translate.t("LABEL.COPY_LINK_PROVIDER_URL")} onclick={&handle_menu_click}></MenuItem>
            { html_if!(
                can_show_record_action(can_write_downloads, selected_channel.as_ref()),
                {
                <MenuItem icon="Record" name={ExplorerAction::Record.to_string()} label={translate.t("LABEL.RECORD")} onclick={&handle_menu_click}></MenuItem>
            })}
            { html_if!(
                can_show_download_action(can_write_downloads, selected_channel.as_ref()),
                {
                <MenuItem icon="Download" name={ExplorerAction::Download.to_string()} label={translate.t("LABEL.DOWNLOAD")} onclick={&handle_menu_click}></MenuItem>
            })}
        </PopupMenu>
      </div>
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_download_filename, can_show_download_action, can_show_record_action, normalize_input_name,
        parse_optional_priority_input, parse_record_duration_minutes, parse_record_start_value, ChannelSelection,
    };
    use shared::model::{VirtualId, XtreamCluster};

    #[test]
    fn parse_optional_priority_input_treats_blank_as_none() {
        assert_eq!(parse_optional_priority_input(None), Ok(None));
        assert_eq!(parse_optional_priority_input(Some(String::new())), Ok(None));
        assert_eq!(parse_optional_priority_input(Some("   ".to_string())), Ok(None));
    }

    #[test]
    fn parse_optional_priority_input_parses_valid_i8_values() {
        assert_eq!(parse_optional_priority_input(Some("-1".to_string())), Ok(Some(-1)));
        assert_eq!(parse_optional_priority_input(Some("12".to_string())), Ok(Some(12)));
        assert_eq!(parse_optional_priority_input(Some(" 0 ".to_string())), Ok(Some(0)));
    }

    #[test]
    fn parse_optional_priority_input_rejects_invalid_non_empty_values() {
        assert!(parse_optional_priority_input(Some("abc".to_string())).is_err());
    }

    #[test]
    fn normalize_input_name_treats_blank_as_none() {
        assert_eq!(normalize_input_name(""), None);
        assert_eq!(normalize_input_name("   "), None);
        assert_eq!(normalize_input_name(" provider-a "), Some("provider-a".to_string()));
    }

    #[test]
    fn popup_actions_require_download_write_permission() {
        let live = ChannelSelection {
            virtual_id: VirtualId::default(),
            cluster: XtreamCluster::Live,
            downloadable: false,
            url: String::new(),
            title: "Live".to_string(),
            input_name: String::new(),
        };
        let vod = ChannelSelection {
            virtual_id: VirtualId::default(),
            cluster: XtreamCluster::Video,
            downloadable: true,
            url: String::new(),
            title: "VOD".to_string(),
            input_name: String::new(),
        };
        let series_container = ChannelSelection {
            virtual_id: VirtualId::default(),
            cluster: XtreamCluster::Series,
            downloadable: false,
            url: String::new(),
            title: "Series".to_string(),
            input_name: String::new(),
        };
        let episode = ChannelSelection {
            virtual_id: VirtualId::default(),
            cluster: XtreamCluster::Series,
            downloadable: true,
            url: String::new(),
            title: "Episode".to_string(),
            input_name: String::new(),
        };

        assert!(!can_show_record_action(false, Some(&live)));
        assert!(!can_show_download_action(false, Some(&vod)));
        assert!(can_show_record_action(true, Some(&live)));
        assert!(can_show_download_action(true, Some(&vod)));
        assert!(!can_show_download_action(true, Some(&live)));
        assert!(!can_show_download_action(true, Some(&series_container)));
        assert!(can_show_download_action(true, Some(&episode)));
        assert!(!can_show_record_action(true, Some(&vod)));
    }

    #[test]
    fn parse_record_duration_minutes_rejects_zero_and_invalid_values() {
        assert_eq!(parse_record_duration_minutes("0"), None);
        assert_eq!(parse_record_duration_minutes("abc"), None);
        assert_eq!(parse_record_duration_minutes("15"), Some(15));
    }

    #[test]
    fn parse_record_start_value_accepts_default_format() {
        let default_value = super::default_record_start_value();
        assert!(parse_record_start_value(&default_value).is_some());
        assert!(parse_record_start_value("2026-04-03T12:34:00").is_some());
        assert!(parse_record_start_value("not-a-date").is_none());
    }

    #[test]
    fn build_download_filename_keeps_url_extension() {
        let filename = build_download_filename("My Movie", "https://example.com/video.mkv?token=1");
        assert_eq!(filename, "My_Movie.mkv");
    }

    #[test]
    fn build_download_filename_falls_back_to_mp4() {
        let filename = build_download_filename("Episode 01", "https://example.com/stream");
        assert_eq!(filename, "Episode_01.mp4");
    }
}
