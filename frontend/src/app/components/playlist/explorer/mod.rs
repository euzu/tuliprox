pub mod bulk_edit;
pub mod detail_panel;
pub mod edit_panel;
pub mod epg_picker;
pub mod identity;
pub mod image_preview;
pub mod live_view;
pub mod orphans_view;
pub mod poster_grid;
pub mod sidebar;
pub mod sort;

use crate::{
    app::{
        components::{
            menu_item::MenuItem,
            popup_menu::PopupMenu,
            recording::{
                ensure_recording_available, target_name_for_id, PaddingBounds, RecordingForm, RecordingFormPrefill,
            },
            AppIcon, DropDownOption, IconButton, NoContent, Search, TextButton,
        },
        context::{ConfigContext, PlaylistExplorerContext},
    },
    hooks::{use_clipboard_copy, use_service_context},
    html_if,
    i18n::use_translation,
    model::{BusyStatus, DialogAction, DialogActions, DialogResult, EventMessage},
    services::{CreateRecordingTaskRequest, DialogService, RecordingService, RecordingSourceInput},
};
pub use bulk_edit::BulkEditPanel;
pub use detail_panel::*;
use identity::{use_identity_revision, Id};
pub use live_view::*;
pub use orphans_view::ExplorerOrphansView;
pub use poster_grid::*;
use shared::{
    error::TuliproxError,
    model::{
        recording::RecordingVisibility, OverrideScopeDto, Permission, PlaylistRequest, PlaylistUrlResolveRequest,
        SearchRequest, SeriesStreamDetailEpisodeProperties, SeriesStreamProperties, UiPlaylistCategories,
        UiPlaylistGroup, UiPlaylistItem, VirtualId, XtreamCluster,
    },
    utils::format_float_localized,
};
pub use sidebar::*;
pub use sort::*;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fmt::Display,
    rc::Rc,
    str::FromStr,
};
use yew::{platform::spawn_local, prelude::*};

const COPY_LINK_TULIPROX_VIRTUAL_ID: &str = "copy_link_tuliprox_virtual_id";
const COPY_LINK_TULIPROX_WEBPLAYER_URL: &str = "copy_link_tuliprox_webplayer_url";
const COPY_LINK_PROVIDER_URL: &str = "copy_link_provider_url";
const RECORD_ITEM: &str = "record_item";
const TP_EXPLORER_SEARCH_FIELDS_KEY: &str = "tp-explorer-search-fields";

#[derive(Clone)]
pub struct ChannelSelection {
    pub virtual_id: VirtualId,
    pub cluster: XtreamCluster,
    pub downloadable: bool,
    pub url: String,
    pub title: String,
    pub input_name: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ExplorerAction {
    CopyLinkTuliproxVirtualId,
    CopyLinkTuliproxWebPlayerUrl,
    CopyLinkProviderUrl,
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
        } else if s.eq(RECORD_ITEM) {
            Ok(Self::Record)
        } else {
            Err(TuliproxError::Config(format!("Unknown ExplorerAction: {s}")))
        }
    }
}

#[must_use]
pub fn can_show_record_action(can_write_recordings: bool, selected_channel: Option<&ChannelSelection>) -> bool {
    can_write_recordings
        && selected_channel.is_some_and(|item| item.cluster == XtreamCluster::Live || item.downloadable)
}

enum ExplorerLevel {
    Browser,
    SeriesEpisodes(Rc<UiPlaylistItem>, Option<Box<SeriesStreamProperties>>),
}

#[component]
pub fn PlaylistExplorer() -> Html {
    let explorer_ctx = use_context::<PlaylistExplorerContext>();
    let cfg_ctx = use_context::<ConfigContext>();
    let dialog_ctx = use_context::<DialogService>();
    let translate = use_translation();

    let (Some(context), Some(config_ctx), Some(dialog)) = (explorer_ctx, cfg_ctx, dialog_ctx) else {
        log::error!("PlaylistExplorer rendered without required context providers");
        return html! { <NoContent text={translate.t("LABEL.NO_CONTENT")} /> };
    };

    let service_ctx = use_service_context();
    let can_write_recordings = service_ctx.auth.has_permission(Permission::RecordingCreate);
    let is_admin_role = service_ctx.auth.is_admin();

    let recording_padding = {
        let rec = config_ctx
            .config
            .as_ref()
            .and_then(|cfg| cfg.config.video.as_ref())
            .and_then(|video| video.recording.as_ref());
        PaddingBounds {
            default_pre_roll_secs: rec.and_then(|c| c.default_pre_roll_secs).unwrap_or(0),
            max_pre_roll_secs: rec.map_or(900, |c| c.max_pre_roll_secs),
            default_post_roll_secs: rec.and_then(|c| c.default_post_roll_secs).unwrap_or(0),
            max_post_roll_secs: rec.map_or(1800, |c| c.max_post_roll_secs),
        }
    };
    let recording_padding = Rc::new(recording_padding);

    let active_cluster = use_state(|| XtreamCluster::Live);
    let current_level = use_state(|| ExplorerLevel::Browser);
    let playlist = use_state(|| (*context.playlist).clone());
    // Revision of the catalog currently shown. Hook dependencies compare this counter (or an
    // `Id`) instead of the playlist value, so a render costs a pointer comparison instead of
    // several full catalog scans.
    let playlist_revision = use_identity_revision(&playlist);
    // Set when this component writes the context playlist itself (item/bulk updates) so the sync
    // effect can tell its own write apart from a freshly loaded playlist.
    let self_written_playlist = use_mut_ref(|| None::<Rc<UiPlaylistCategories>>);

    let (default_sort, default_desc) = default_sort_for_kind(*active_cluster);
    let active_sort = use_state(|| default_sort);
    let active_desc = use_state(|| default_desc);

    let selected_group = use_state(|| SelectedGroup::All);
    let selected_item = use_state(|| None::<Rc<UiPlaylistItem>>);
    let is_vod_list_view = use_state(|| false);

    let selected_ids = use_state(HashSet::<u32>::new);
    let is_bulk_edit_open = use_state(|| false);
    let is_orphans_open = use_state(|| false);

    let is_target_view = matches!(context.playlist_request.as_ref(), Some(PlaylistRequest::Target(_)));
    let initial_bulk_scope = if is_target_view { OverrideScopeDto::Target } else { OverrideScopeDto::Input };
    let bulk_scope = use_state(|| initial_bulk_scope);

    // Filter chip states
    let filter_without_epg = use_state(|| false);
    let filter_edited = use_state(|| false);
    let filter_rating_gte6 = use_state(|| false);

    // Context popup & selection
    let selected_channel = use_state(|| None::<ChannelSelection>);
    let popup_anchor_ref = use_state(|| None::<web_sys::Element>);
    let popup_is_open = use_state(|| false);
    let copy_to_clipboard = use_clipboard_copy();

    // When a different playlist arrives, reset selection. Edits pushed by this component must
    // not reset it: the detail panel stays open after a successful save.
    {
        let set_playlist = playlist.clone();
        let set_level = current_level.clone();
        let set_selected_item = selected_item.clone();
        let set_selected_group = selected_group.clone();
        let set_selected_ids = selected_ids.clone();
        let set_bulk_edit_open = is_bulk_edit_open.clone();
        let set_popup_is_open = popup_is_open.clone();
        let set_selected_channel = selected_channel.clone();
        let set_popup_anchor_ref = popup_anchor_ref.clone();
        let self_written = self_written_playlist.clone();
        let incoming_playlist = (*context.playlist).clone();
        use_effect_with(Id::from_option(incoming_playlist), move |incoming: &Option<Id<UiPlaylistCategories>>| {
            let incoming = incoming.as_ref().map(|id| id.clone().into_inner());
            let is_self_write = {
                let written = self_written.borrow();
                match (incoming.as_ref(), written.as_ref()) {
                    (Some(current), Some(written)) => Rc::ptr_eq(current, written),
                    _ => false,
                }
            };
            self_written.borrow_mut().take();
            if !is_self_write {
                // A self write already updated the local catalog (possibly a filtered view of it).
                set_playlist.set(incoming);
                set_level.set(ExplorerLevel::Browser);
                set_selected_item.set(None);
                set_selected_group.set(SelectedGroup::All);
                set_selected_ids.set(HashSet::new());
                set_bulk_edit_open.set(false);
                set_popup_is_open.set(false);
                set_selected_channel.set(None);
                set_popup_anchor_ref.set(None);
            }
            || {}
        });
    }

    // Cluster change handler
    let handle_cluster_change = {
        let active_cluster = active_cluster.clone();
        let active_sort = active_sort.clone();
        let active_desc = active_desc.clone();
        let selected_group = selected_group.clone();
        let selected_item = selected_item.clone();
        let current_level = current_level.clone();
        let filter_without_epg = filter_without_epg.clone();
        let filter_edited = filter_edited.clone();
        let filter_rating_gte6 = filter_rating_gte6.clone();
        let set_selected_ids = selected_ids.clone();
        let set_bulk_edit_open = is_bulk_edit_open.clone();

        Callback::from(move |cluster: XtreamCluster| {
            active_cluster.set(cluster);
            let (def_sort, def_desc) = default_sort_for_kind(cluster);
            active_sort.set(def_sort);
            active_desc.set(def_desc);
            selected_group.set(SelectedGroup::All);
            selected_item.set(None);
            current_level.set(ExplorerLevel::Browser);
            filter_without_epg.set(false);
            filter_edited.set(false);
            filter_rating_gte6.set(false);
            set_selected_ids.set(HashSet::new());
            set_bulk_edit_open.set(false);
        })
    };

    // Collect all items and groups for the active cluster. The dependency is the playlist
    // revision, never the catalog itself.
    let groups_for_cluster = use_memo((playlist_revision, *active_cluster), |(_, cluster)| {
        playlist.as_ref().and_then(|cat| match cluster {
            XtreamCluster::Live => cat.live.clone().map(Rc::new),
            XtreamCluster::Video => cat.vod.clone().map(Rc::new),
            XtreamCluster::Series => cat.series.clone().map(Rc::new),
        })
    });

    let all_items_for_cluster =
        use_memo(Id::new(groups_for_cluster.clone()), |groups: &Id<Option<Rc<Vec<Rc<UiPlaylistGroup>>>>>| {
            let mut items = Vec::new();
            if let Some(groups) = groups.as_ref() {
                for group in groups.iter() {
                    items.extend(group.channels.iter().cloned());
                }
            }
            items
        });

    // Counts for tabs
    let live_count = playlist
        .as_ref()
        .and_then(|p| p.live.as_ref())
        .map_or(0, |gs| gs.iter().map(|g| g.channels.len()).sum::<usize>());
    let vod_count = playlist
        .as_ref()
        .and_then(|p| p.vod.as_ref())
        .map_or(0, |gs| gs.iter().map(|g| g.channels.len()).sum::<usize>());
    let series_count = playlist
        .as_ref()
        .and_then(|p| p.series.as_ref())
        .map_or(0, |gs| gs.iter().map(|g| g.channels.len()).sum::<usize>());

    let all_items_id = Id::new(all_items_for_cluster.clone());
    let total_items = all_items_id.len();
    let edited_count = use_memo(all_items_id.clone(), |items| count_edited(&items[..]));
    let without_epg_count = use_memo(all_items_id.clone(), |items| count_without_epg(&items[..]));

    // Memoized sort per (items, cluster, sort, desc)
    let sorted_items = use_memo(
        (all_items_id.clone(), *active_cluster, *active_sort, *active_desc),
        |(items, _cluster, sort, desc)| {
            let mut sorted = Vec::with_capacity(items.len());
            sorted.extend(items.iter().cloned());
            sort_items(&mut sorted, *sort, *desc);
            sorted
        },
    );

    // Filter items based on selected group & active chips
    let filtered_items = use_memo(
        (
            Id::new(sorted_items.clone()),
            (*selected_group).clone(),
            *filter_without_epg,
            *filter_edited,
            *filter_rating_gte6,
        ),
        |(sorted, group_filter, without_epg, edited, rating_gte6)| {
            let mut list = Vec::new();
            for item in sorted.iter() {
                // Group match
                let matches_group = match group_filter {
                    SelectedGroup::All => true,
                    SelectedGroup::WithoutEpg => is_without_epg(item),
                    SelectedGroup::Edited => is_edited(item),
                    SelectedGroup::Group(title) => item.group.as_ref() == title.as_ref(),
                };
                if !matches_group {
                    continue;
                }

                // Filter chips match
                if *without_epg && !is_without_epg(item) {
                    continue;
                }
                if *edited && !is_edited(item) {
                    continue;
                }
                if *rating_gte6 && item.rating < 6.0 {
                    continue;
                }

                list.push(Rc::clone(item));
            }
            list
        },
    );

    let filtered_items_id = Id::new(filtered_items.clone());

    // Search options
    let search_fields = use_memo((translate.clone(),), |(translate,)| {
        let persisted: Vec<String> = crate::utils::get_local_storage_item(TP_EXPLORER_SEARCH_FIELDS_KEY)
            .map(|value| value.split(',').filter(|id| !id.is_empty()).map(str::to_string).collect())
            .unwrap_or_default();
        let is_selected = |id: &str| persisted.iter().any(|p| p == id);
        vec![
            DropDownOption::new(
                shared::model::SEARCH_FIELD_GROUP,
                html! { translate.t("LABEL.GROUP") },
                is_selected(shared::model::SEARCH_FIELD_GROUP),
            ),
            DropDownOption::new(
                shared::model::SEARCH_FIELD_TITLE,
                html! { translate.t("LABEL.TITLE") },
                is_selected(shared::model::SEARCH_FIELD_TITLE),
            ),
            DropDownOption::new(
                shared::model::SEARCH_FIELD_NAME,
                html! { translate.t("LABEL.NAME") },
                is_selected(shared::model::SEARCH_FIELD_NAME),
            ),
            DropDownOption::new(
                shared::model::SEARCH_FIELD_URL,
                html! { translate.t("LABEL.URL") },
                is_selected(shared::model::SEARCH_FIELD_URL),
            ),
            DropDownOption::new(
                shared::model::SEARCH_FIELD_PROVIDER_ID,
                html! { translate.t("LABEL.PROVIDER_ID") },
                is_selected(shared::model::SEARCH_FIELD_PROVIDER_ID),
            ),
        ]
    });

    let handle_search_fields_change = Callback::from(move |fields: Option<Rc<Vec<String>>>| {
        let value = fields.as_ref().map(|f| f.join(",")).unwrap_or_default();
        crate::utils::set_local_storage_item(TP_EXPLORER_SEARCH_FIELDS_KEY, &value);
    });

    let handle_search = {
        let services = service_ctx.clone();
        let set_playlist = playlist.clone();
        let context = context.clone();
        Callback::from(move |search_req| match search_req {
            SearchRequest::Clear => set_playlist.set((*context.playlist).clone()),
            SearchRequest::Text(ref _text, ref _search_fields)
            | SearchRequest::Regexp(ref _text, ref _search_fields) => {
                services.event.broadcast(EventMessage::Busy(BusyStatus::Show));
                let set_playlist = set_playlist.clone();
                let context = context.clone();
                let services = services.clone();
                spawn_local(async move {
                    let filtered =
                        context.playlist.as_ref().and_then(|categories| categories.filter(&search_req)).map(Rc::new);
                    set_playlist.set(filtered);
                    services.event.broadcast(EventMessage::Busy(BusyStatus::Hide));
                });
            }
        })
    };

    // Popup actions
    let handle_popup_onclick = {
        let set_selected_channel = selected_channel.clone();
        let set_anchor_ref = popup_anchor_ref.clone();
        let set_is_open = popup_is_open.clone();
        Callback::from(move |(dto, event): (Rc<UiPlaylistItem>, MouseEvent)| {
            event.prevent_default();
            event.stop_propagation();
            if let Some(target) = event.target_dyn_into::<web_sys::Element>() {
                set_selected_channel.set(Some(ChannelSelection {
                    virtual_id: VirtualId::new(dto.virtual_id),
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

    let handle_popup_close = {
        let set_is_open = popup_is_open.clone();
        Callback::from(move |()| {
            set_is_open.set(false);
        })
    };

    let handle_menu_click = {
        let services = service_ctx.clone();
        let dialog = dialog.clone();
        let popup_is_open_state = popup_is_open.clone();
        let selected_channel = selected_channel.clone();
        let playlist_ctx = context.clone();
        let translate_clone = translate.clone();
        let copy_to_clipboard = copy_to_clipboard.clone();
        let config = config_ctx.config.clone();

        Callback::from(move |(name, _): (String, _)| {
            if let Ok(action) = ExplorerAction::from_str(&name) {
                match action {
                    ExplorerAction::CopyLinkTuliproxVirtualId => {
                        if let Some(dto) = &*selected_channel {
                            copy_to_clipboard.emit(dto.virtual_id.to_string());
                        }
                    }
                    ExplorerAction::CopyLinkTuliproxWebPlayerUrl => {
                        if let Some(PlaylistRequest::Target(target_id)) = playlist_ctx.playlist_request.as_ref() {
                            if let Some(dto) = &*selected_channel {
                                let copy_to_clipboard = copy_to_clipboard.clone();
                                let services = services.clone();
                                let virtual_id = dto.virtual_id;
                                let cluster = dto.cluster;
                                let translate_clone = translate_clone.clone();
                                let target_id = *target_id;
                                let services_clone = services.clone();
                                spawn_local(async move {
                                    let request = PlaylistUrlResolveRequest::Webplayer {
                                        target_id,
                                        virtual_id: virtual_id.get(),
                                        cluster,
                                    };
                                    if let Some(url) = services.playlist.resolve_url(request).await {
                                        copy_to_clipboard.emit(url);
                                        services_clone.toastr.success(
                                            translate_clone.t("MESSAGES.PLAYLIST.WEBPLAYER_URL_COPY_TO_CLIPBOARD"),
                                        );
                                    } else {
                                        services_clone
                                            .toastr
                                            .error(translate_clone.t("MESSAGES.FAILED_TO_RETRIEVE_WEBPLAYER_URL"));
                                    }
                                });
                            }
                        }
                    }
                    ExplorerAction::CopyLinkProviderUrl => {
                        if let Some(dto) = &*selected_channel {
                            let url = dto.url.clone();
                            if url.is_empty() {
                                if let Some(playlist_request) = playlist_ctx.playlist_request.as_ref() {
                                    let copy_to_clipboard = copy_to_clipboard.clone();
                                    let services = services.clone();
                                    let virtual_id = dto.virtual_id;
                                    let playlist_request = playlist_request.clone();
                                    spawn_local(async move {
                                        if let Some(pli) =
                                            services.playlist.get_episode(virtual_id.get(), &playlist_request).await
                                        {
                                            let url = pli.url.to_string();
                                            let request = PlaylistUrlResolveRequest::Provider {
                                                playlist_request,
                                                url: url.clone(),
                                            };
                                            let resolved = services.playlist.resolve_url(request).await.unwrap_or(url);
                                            copy_to_clipboard.emit(resolved);
                                        }
                                    });
                                }
                            } else if let Some(playlist_request) = playlist_ctx.playlist_request.as_ref() {
                                let copy_to_clipboard = copy_to_clipboard.clone();
                                let services = services.clone();
                                let playlist_request = playlist_request.clone();
                                spawn_local(async move {
                                    let request =
                                        PlaylistUrlResolveRequest::Provider { playlist_request, url: url.clone() };
                                    let resolved =
                                        services.playlist.resolve_url(request).await.unwrap_or_else(|| url.clone());
                                    copy_to_clipboard.emit(resolved);
                                });
                            } else {
                                copy_to_clipboard.emit(url);
                            }
                        }
                    }
                    ExplorerAction::Record => {
                        if !can_write_recordings {
                            popup_is_open_state.set(false);
                            return;
                        }
                        if let Some(dto) = &*selected_channel {
                            let dialog = dialog.clone();
                            let services = services.clone();
                            let translate_clone = translate_clone.clone();
                            let playlist_request = (*playlist_ctx.playlist_request).clone();
                            let selected = dto.clone();
                            let padding = Rc::clone(&recording_padding);
                            let target_name = match playlist_request.as_ref() {
                                Some(PlaylistRequest::Target(target_id)) => config.as_ref().and_then(|app_config| {
                                    target_name_for_id(&app_config.sources, *target_id, Some(&selected.input_name))
                                }),
                                _ => None,
                            };
                            spawn_local(async move {
                                if !ensure_recording_available(&services, &translate_clone).await {
                                    return;
                                }
                                let target_name = if let Some(name) = target_name {
                                    name
                                } else {
                                    services.toastr.error(translate_clone.t("MESSAGES.RECORDING.NO_TARGET"));
                                    return;
                                };
                                let source = RecordingSourceInput {
                                    target_id: target_name,
                                    virtual_id: selected.virtual_id.to_string(),
                                    cluster: selected.cluster,
                                    input_name: selected.input_name.clone(),
                                };
                                if selected.cluster != XtreamCluster::Live {
                                    let request = CreateRecordingTaskRequest {
                                        source,
                                        program_title: selected.title.clone(),
                                        program_start: None,
                                        program_end: None,
                                        pre_roll_secs: None,
                                        post_roll_secs: None,
                                        visibility: RecordingVisibility::Private,
                                        channel_id: None,
                                        channel_name: None,
                                        epg: None,
                                    };
                                    match RecordingService::new().create_task(request).await {
                                        Ok(()) => {
                                            services.toastr.success(translate_clone.t("MESSAGES.RECORDING.QUEUED"));
                                        }
                                        Err(error) => services.toastr.error(error.to_string()),
                                    }
                                    return;
                                }
                                let now = chrono::Utc::now().timestamp();
                                let program_end = now + 90 * 60;
                                let prefill = RecordingFormPrefill::new(
                                    source,
                                    selected.title.clone(),
                                    now,
                                    program_end,
                                    (*padding).clone(),
                                )
                                .with_channel_name(selected.title.clone());
                                let request_slot: Rc<RefCell<Option<CreateRecordingTaskRequest>>> =
                                    Rc::new(RefCell::new(None));
                                let on_submit = {
                                    let request_slot = Rc::clone(&request_slot);
                                    Callback::from(move |request: CreateRecordingTaskRequest| {
                                        *request_slot.borrow_mut() = Some(request);
                                    })
                                };
                                let on_cancel = Callback::from(|()| {});
                                let body = html! {
                                    <RecordingForm
                                        prefill={prefill}
                                        has_recording_manage={can_write_recordings}
                                        is_admin_role={is_admin_role}
                                        on_submit={on_submit}
                                        on_cancel={on_cancel}
                                    />
                                };
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
                                let result = dialog.content(body, Some(actions), false).await;
                                if result != DialogResult::Ok {
                                    return;
                                }
                                let request = if let Some(r) = request_slot.borrow_mut().take() {
                                    r
                                } else {
                                    services.toastr.error(translate_clone.t("MESSAGES.RECORDING.NO_REQUEST"));
                                    return;
                                };
                                let recording_svc = RecordingService::new();
                                match recording_svc.create_task(request).await {
                                    Ok(()) => {
                                        services.toastr.success(translate_clone.t("MESSAGES.RECORDING.QUEUED"));
                                    }
                                    Err(err) => {
                                        services.toastr.error(err.to_string());
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

    // Series detail loading
    let load_series_info = {
        let current_level = current_level.clone();
        let services = service_ctx.clone();
        let ctx = context.clone();

        Callback::from(move |dto: Rc<UiPlaylistItem>| {
            let current_level = current_level.clone();
            let services = services.clone();
            let ctx = ctx.clone();
            services.event.broadcast(EventMessage::Busy(BusyStatus::Show));
            spawn_local(async move {
                let mut handled = false;
                if let Some(playlist_request) = ctx.playlist_request.as_ref() {
                    if let Some(props) = services.playlist.get_series_info(&dto, playlist_request).await {
                        handled = true;
                        current_level.set(ExplorerLevel::SeriesEpisodes(dto.clone(), Some(Box::new(props))));
                    }
                }
                if !handled {
                    current_level.set(ExplorerLevel::SeriesEpisodes(dto, None));
                }
                services.event.broadcast(EventMessage::Busy(BusyStatus::Hide));
            });
        })
    };

    let handle_back_click = {
        let current_level = current_level.clone();
        Callback::from(move |(_name, _): (String, MouseEvent)| {
            current_level.set(ExplorerLevel::Browser);
        })
    };

    let handle_select_item = {
        let selected_item = selected_item.clone();
        Callback::from(move |item: Rc<UiPlaylistItem>| {
            selected_item.set(Some(item));
        })
    };

    let handle_close_detail = {
        let selected_item = selected_item.clone();
        Callback::from(move |()| {
            selected_item.set(None);
        })
    };

    let handle_item_updated = {
        let selected_item = selected_item.clone();
        let explorer_ctx = context.clone();
        let set_playlist = playlist.clone();
        let self_written = self_written_playlist.clone();
        Callback::from(move |patched_item: Rc<UiPlaylistItem>| {
            selected_item.set(Some(patched_item.clone()));
            if let Some(ref source) = *explorer_ctx.playlist {
                let new_source = Rc::new(edit_panel::replace_item_in_categories(source, (*patched_item).clone()));
                // The visible catalog may be a filtered view of the source; patching it
                // directly keeps an active search or group filter alive across the save.
                let new_visible = (*set_playlist).as_ref().map_or_else(
                    || new_source.clone(),
                    |visible| Rc::new(edit_panel::replace_item_in_categories(visible, (*patched_item).clone())),
                );
                set_playlist.set(Some(new_visible));
                *self_written.borrow_mut() = Some(new_source.clone());
                explorer_ctx.playlist.set(Some(new_source));
            }
        })
    };

    let handle_toggle_select = {
        let selected_ids = selected_ids.clone();
        Callback::from(move |vid: u32| {
            let mut set = (*selected_ids).clone();
            if set.contains(&vid) {
                set.remove(&vid);
            } else {
                set.insert(vid);
            }
            selected_ids.set(set);
        })
    };

    let handle_select_page = {
        let selected_ids = selected_ids.clone();
        Callback::from(move |(vids, select): (Vec<u32>, bool)| {
            let mut set = (*selected_ids).clone();
            for vid in vids {
                if select {
                    set.insert(vid);
                } else {
                    set.remove(&vid);
                }
            }
            selected_ids.set(set);
        })
    };

    let selected_ids_rc = Rc::new((*selected_ids).clone());

    let handle_bulk_updated = {
        let is_bulk_edit_open = is_bulk_edit_open.clone();
        let selected_ids = selected_ids.clone();
        let explorer_ctx = context.clone();
        let set_playlist = playlist.clone();
        let self_written = self_written_playlist.clone();
        Callback::from(move |patched_items: Vec<UiPlaylistItem>| {
            is_bulk_edit_open.set(false);
            selected_ids.set(HashSet::new());
            if let Some(ref cat) = *explorer_ctx.playlist {
                let mut current_cat = (**cat).clone();
                for it in patched_items {
                    current_cat = edit_panel::replace_item_in_categories(&current_cat, it);
                }
                let new_cat = Rc::new(current_cat);
                set_playlist.set(Some(new_cat.clone()));
                *self_written.borrow_mut() = Some(new_cat.clone());
                explorer_ctx.playlist.set(Some(new_cat));
            }
        })
    };

    let (target_id, target_name) = match context.playlist_request.as_ref() {
        Some(PlaylistRequest::Target(id)) => {
            let name =
                config_ctx.config.as_ref().and_then(|app_config| target_name_for_id(&app_config.sources, *id, None));
            (Some(*id), name)
        }
        _ => (None, None),
    };

    let handle_select_group = {
        let selected_group = selected_group.clone();
        Callback::from(move |group: SelectedGroup| {
            selected_group.set(group);
        })
    };

    // Sort options per cluster
    let available_sorts = match *active_cluster {
        XtreamCluster::Live => vec![ExplorerSort::ChannelNo, ExplorerSort::Name, ExplorerSort::Group],
        XtreamCluster::Video => {
            vec![ExplorerSort::Added, ExplorerSort::Title, ExplorerSort::Year, ExplorerSort::Rating]
        }
        XtreamCluster::Series => {
            vec![ExplorerSort::LastModified, ExplorerSort::Title, ExplorerSort::Year, ExplorerSort::Rating]
        }
    };

    let handle_sort_change = {
        let active_sort = active_sort.clone();
        let active_desc = active_desc.clone();
        Callback::from(move |e: Event| {
            let target = e.target_unchecked_into::<web_sys::HtmlSelectElement>();
            let val = target.value();
            match val.as_str() {
                "ChannelNo" => {
                    active_sort.set(ExplorerSort::ChannelNo);
                    active_desc.set(false);
                }
                "Name" => {
                    active_sort.set(ExplorerSort::Name);
                    active_desc.set(false);
                }
                "Group" => {
                    active_sort.set(ExplorerSort::Group);
                    active_desc.set(false);
                }
                "Added" => {
                    active_sort.set(ExplorerSort::Added);
                    active_desc.set(true);
                }
                "Title" => {
                    active_sort.set(ExplorerSort::Title);
                    active_desc.set(false);
                }
                "Year" => {
                    active_sort.set(ExplorerSort::Year);
                    active_desc.set(true);
                }
                "Rating" => {
                    active_sort.set(ExplorerSort::Rating);
                    active_desc.set(true);
                }
                "LastModified" => {
                    active_sort.set(ExplorerSort::LastModified);
                    active_desc.set(true);
                }
                _ => {}
            }
        })
    };

    if playlist.is_none() {
        return html! {
            <div class="tp__playlist-explorer">
                <NoContent
                    text={translate.t("MESSAGES.PLAYLIST_EXPLORER.SELECT_A_PLAYLIST_TO_VIEW_CONTENT")}
                    hint={translate.t("MESSAGES.PLAYLIST_EXPLORER.SELECT_A_PLAYLIST_HINT")}
                />
            </div>
        };
    }

    // Series episodes rendering if in SeriesEpisodes level
    let render_series_episodes_view =
        |series_info: &Rc<UiPlaylistItem>, props: Option<&Box<SeriesStreamProperties>>| {
            let (backdrop, plot, cast, genre, release_date, rating, details) = match props {
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
                        Some(backdrop),
                        series_props.plot.as_deref().map(ToString::to_string).unwrap_or_default(),
                        series_props.cast.to_string(),
                        series_props.genre.as_deref().map(ToString::to_string).unwrap_or_default(),
                        series_props.release_date.as_deref().map(ToString::to_string).unwrap_or_default(),
                        series_props.rating,
                        series_props.details.as_ref(),
                    )
                }
                None => (None, String::new(), String::new(), String::new(), String::new(), 0.0, None),
            };

            let style = backdrop.as_deref().map(background_style).unwrap_or_default();

            let render_ep = |chan: &SeriesStreamDetailEpisodeProperties| {
                let channel_select = ChannelSelection {
                    virtual_id: VirtualId::new(chan.id),
                    cluster: XtreamCluster::Series,
                    downloadable: true,
                    url: chan.direct_source.to_string(),
                    title: chan.title.to_string(),
                    input_name: String::new(),
                };
                let popup_onclick = handle_episode_popup_onclick.clone();
                let ep_rating = chan.rating.unwrap_or_default();
                let logo = if chan.movie_image.is_empty() {
                    "assets/missing-logo.svg".to_string()
                } else {
                    chan.movie_image.to_string()
                };

                html! {
                    <div class="ep" key={format!("ep-{}", chan.id)}>
                        <span class="th">
                            <img alt={chan.title.to_string()} src={logo} loading="lazy" />
                        </span>
                        <span>
                            <b>{ chan.title.clone() }</b>
                            <br />
                            <small>{ if ep_rating > 0.001 { format!("★ {} · ", format_float_localized(ep_rating, 1, false)) } else { String::new() } }{ translate.t("LABEL.EPISODE") }</small>
                            <button type="button" class="tp__icon-button" onclick={Callback::from(move |e: MouseEvent| popup_onclick.emit((channel_select.clone(), e)))}>
                                <AppIcon name="Popup" />
                            </button>
                        </span>
                    </div>
                }
            };

            let episodes_html = if let Some(episodes) = details.as_ref().and_then(|d| d.episodes.as_ref()) {
                let mut grouped: HashMap<u32, Vec<&SeriesStreamDetailEpisodeProperties>> = HashMap::new();
                for item in episodes {
                    grouped.entry(item.season).or_default().push(item);
                }
                let mut grouped_list: Vec<(u32, Vec<&SeriesStreamDetailEpisodeProperties>)> =
                    grouped.into_iter().collect();
                grouped_list.sort_by_key(|(season, _)| *season);

                html! {
                    for (season, season_episodes) in grouped_list.iter() {
                        <div key={format!("season-{season}")}>
                            <div class="seasons">
                                <span class="on">{ translate.t("LABEL.SEASON") } { " " } { season }</span>
                            </div>
                            <div>
                                for episode in season_episodes.iter() {
                                    { render_ep(episode) }
                                }
                            </div>
                        </div>
                    }
                }
            } else {
                html! {}
            };

            html! {
                <div class="tp__playlist-explorer__series-info">
                    <div class="hero" style={style}>
                        <div class="hero-overlay">
                            <b class="title">{ series_info.title.clone() }</b>
                            <div class="meta">
                                { if rating > 0.001 { format!("★ {} · ", format_float_localized(rating, 1, false)) } else { String::new() } }
                                { release_date } { " · " } { genre }
                            </div>
                            <p class="plot">{ plot }</p>
                            <small class="cast">{ cast }</small>
                        </div>
                    </div>
                    <div class="in">
                        { episodes_html }
                    </div>
                </div>
            }
        };

    html! {
        <div class="tp__playlist-explorer tpx">
            // TOP BAR
            <div class="top">
                <div class="tabs">
                    <button
                        type="button"
                        class={if *active_cluster == XtreamCluster::Live { "on" } else { "" }}
                        onclick={
                            let ch = handle_cluster_change.clone();
                            Callback::from(move |_: MouseEvent| ch.emit(XtreamCluster::Live))
                        }
                    >
                        { translate.t("LABEL.LIVE") } { " " } { live_count }
                    </button>
                    <button
                        type="button"
                        class={if *active_cluster == XtreamCluster::Video { "on" } else { "" }}
                        onclick={
                            let ch = handle_cluster_change.clone();
                            Callback::from(move |_: MouseEvent| ch.emit(XtreamCluster::Video))
                        }
                    >
                        { translate.t("LABEL.VOD") } { " " } { vod_count }
                    </button>
                    <button
                        type="button"
                        class={if *active_cluster == XtreamCluster::Series { "on" } else { "" }}
                        onclick={
                            let ch = handle_cluster_change.clone();
                            Callback::from(move |_: MouseEvent| ch.emit(XtreamCluster::Series))
                        }
                    >
                        { translate.t("LABEL.SERIES") } { " " } { series_count }
                    </button>
                </div>

                <div class="search">
                    <Search
                        onsearch={handle_search}
                        options={search_fields.clone()}
                        on_fields_change={handle_search_fields_change}
                    />
                </div>

                <div class="top-actions">
                    <TextButton
                        name="orphan_cleanup"
                        icon="DeleteSweep"
                        title={translate.t("LABEL.EXPLORER_OVERRIDE_CLEANUP")}
                        onclick={let is_orphans_open = is_orphans_open.clone(); Callback::from(move |_: String| is_orphans_open.set(true))}
                    />
                </div>
            </div>

            // BODY: SIDEBAR + MAIN + DETAIL PANEL
            <div class="body">
                // SIDEBAR
                <ExplorerSidebar
                    cluster={*active_cluster}
                    groups={Id::from_option((*groups_for_cluster).clone())}
                    total_items={total_items}
                    edited_count={*edited_count}
                    without_epg_count={*without_epg_count}
                    selected_group={(*selected_group).clone()}
                    on_select_group={handle_select_group}
                />

                // MAIN CONTENT
                <div class="main">
                    // BACK BUTTON IF IN SERIES EPISODES
                    {
                        if matches!(*current_level, ExplorerLevel::SeriesEpisodes(_, _)) {
                            html! {
                                <div class="tp__explorer-back">
                                    <IconButton icon="Back" name="back" onclick={handle_back_click} />
                                </div>
                            }
                        } else {
                            html! {}
                        }
                    }

                    // TOOLBAR / FILTER CHIPS
                    {
                        if matches!(*current_level, ExplorerLevel::Browser) {
                            html! {
                                <div class="bar">
                                    // Live Chips
                                    {
                                        if *active_cluster == XtreamCluster::Live {
                                            let we_chip = filter_without_epg.clone();
                                            let ed_chip = filter_edited.clone();
                                            let all_chip_click = {
                                                let we = filter_without_epg.clone();
                                                let ed = filter_edited.clone();
                                                Callback::from(move |_: MouseEvent| { we.set(false); ed.set(false); })
                                            };
                                            html! {
                                                <>
                                                    <button
                                                        type="button"
                                                        class={format!("chip {}", if !*we_chip && !*ed_chip { "on" } else { "" })}
                                                        onclick={all_chip_click}
                                                    >
                                                        { translate.t("LABEL.EXPLORER_FILTER_ALL") }
                                                    </button>
                                                    <button
                                                        type="button"
                                                        class={format!("chip {}", if *we_chip { "on" } else { "" })}
                                                        onclick={Callback::from(move |_: MouseEvent| we_chip.set(!*we_chip))}
                                                    >
                                                        { translate.t("LABEL.EXPLORER_FILTER_WITHOUT_EPG") }
                                                    </button>
                                                    <button
                                                        type="button"
                                                        class={format!("chip {}", if *ed_chip { "on" } else { "" })}
                                                        onclick={Callback::from(move |_: MouseEvent| ed_chip.set(!*ed_chip))}
                                                    >
                                                        { translate.t("LABEL.EXPLORER_VIRTUAL_GROUP_EDITED") }
                                                    </button>
                                                </>
                                            }
                                        } else {
                                            html! {}
                                        }
                                    }

                                    // VOD / Series Chips
                                    {
                                        if *active_cluster == XtreamCluster::Video || *active_cluster == XtreamCluster::Series {
                                            let ed_chip = filter_edited.clone();
                                            let r_chip = filter_rating_gte6.clone();
                                            html! {
                                                <>
                                                    <button
                                                        type="button"
                                                        class={format!("chip {}", if *r_chip { "on" } else { "" })}
                                                        onclick={Callback::from(move |_: MouseEvent| r_chip.set(!*r_chip))}
                                                    >
                                                        { translate.t("LABEL.EXPLORER_FILTER_RATING_GTE").replace("{rating}", "6.0") }
                                                    </button>
                                                    <button
                                                        type="button"
                                                        class={format!("chip {}", if *ed_chip { "on" } else { "" })}
                                                        onclick={Callback::from(move |_: MouseEvent| ed_chip.set(!*ed_chip))}
                                                    >
                                                        { translate.t("LABEL.EXPLORER_VIRTUAL_GROUP_EDITED") }
                                                    </button>
                                                </>
                                            }
                                        } else {
                                            html! {}
                                        }
                                    }

                                    // Sort dropdown
                                    <select
                                        class="sel"
                                        aria-label={translate.t("LABEL.SORT")}
                                        onchange={handle_sort_change}
                                    >
                                        {
                                            available_sorts.iter().map(|s| {
                                                let is_sel = *s == *active_sort;
                                                let val_str = match s {
                                                    ExplorerSort::ChannelNo => "ChannelNo",
                                                    ExplorerSort::Name => "Name",
                                                    ExplorerSort::Group => "Group",
                                                    ExplorerSort::Added => "Added",
                                                    ExplorerSort::Title => "Title",
                                                    ExplorerSort::Year => "Year",
                                                    ExplorerSort::Rating => "Rating",
                                                    ExplorerSort::LastModified => "LastModified",
                                                };
                                                html! {
                                                    <option value={val_str} selected={is_sel}>
                                                        { translate.t("LABEL.EXPLORER_SORT_PREFIX") }
                                                        { translate.t(s.label_key()) }
                                                    </option>
                                                }
                                            }).collect::<Html>()
                                        }
                                    </select>

                                    // Bulk selection bar
                                    {
                                        if selected_ids.is_empty() {
                                            html! {}
                                        } else {
                                            let is_bulk = is_bulk_edit_open.clone();
                                            let sel = selected_ids.clone();
                                            html! {
                                                <div class="tp__bulk-actions-bar">
                                                    <span class="tp__selection-count">
                                                        { format!("{} {}", selected_ids.len(), translate.t("LABEL.EXPLORER_SELECTED_ITEMS")) }
                                                    </span>
                                                    <TextButton
                                                        class="primary"
                                                        name="bulk_edit"
                                                        icon="Edit"
                                                        title={translate.t("LABEL.EXPLORER_BULK_EDIT")}
                                                        onclick={Callback::from(move |_: String| is_bulk.set(true))}
                                                    />
                                                    <TextButton
                                                        name="clear_selection"
                                                        icon="Clear"
                                                        title={translate.t("LABEL.EXPLORER_CLEAR_SELECTION")}
                                                        onclick={Callback::from(move |_: String| sel.set(HashSet::new()))}
                                                    />
                                                </div>
                                            }
                                        }
                                    }

                                    // View toggle for VOD
                                    {
                                        if *active_cluster == XtreamCluster::Video {
                                            let is_list = is_vod_list_view.clone();
                                            let toggle_grid = {
                                                let is_list = is_list.clone();
                                                Callback::from(move |_: MouseEvent| is_list.set(false))
                                            };
                                            let toggle_list = {
                                                let is_list = is_list.clone();
                                                Callback::from(move |_: MouseEvent| is_list.set(true))
                                            };
                                            html! {
                                                <div class="view-toggle">
                                                    <button
                                                        type="button"
                                                        class={if *is_list { "" } else { "on" }}
                                                        onclick={toggle_grid}
                                                        title={translate.t("LABEL.EXPLORER_GRID_VIEW")}
                                                        aria-label={translate.t("LABEL.EXPLORER_GRID_VIEW")}
                                                        aria-pressed={(!*is_list).to_string()}
                                                    >
                                                        { "▦" }
                                                    </button>
                                                    <button
                                                        type="button"
                                                        class={if *is_list { "on" } else { "" }}
                                                        onclick={toggle_list}
                                                        title={translate.t("LABEL.EXPLORER_LIST_VIEW")}
                                                        aria-label={translate.t("LABEL.EXPLORER_LIST_VIEW")}
                                                        aria-pressed={(*is_list).to_string()}
                                                    >
                                                        { "☰" }
                                                    </button>
                                                </div>
                                            }
                                        } else {
                                            html! {}
                                        }
                                    }

                                    // Count
                                    <span class="cnt">
                                        { format!("{} {}", filtered_items.len(), match *active_cluster {
                                            XtreamCluster::Live => translate.t("LABEL.CHANNELS"),
                                            XtreamCluster::Video => translate.t("LABEL.VOD"),
                                            XtreamCluster::Series => translate.t("LABEL.SERIES"),
                                        }) }
                                    </span>
                                </div>
                            }
                        } else {
                            html! {}
                        }
                    }

                    // MAIN VIEW CONTENT
                    {
                        match *current_level {
                            ExplorerLevel::SeriesEpisodes(ref series_pli, ref props) => {
                                render_series_episodes_view(series_pli, props.as_ref())
                            }
                            ExplorerLevel::Browser => {
                                match *active_cluster {
                                    XtreamCluster::Live => {
                                        html! {
                                            <ExplorerLiveView
                                                items={filtered_items_id.clone()}
                                                selected_item={Id::from_option((*selected_item).clone())}
                                                on_select_item={handle_select_item.clone()}
                                                on_popup_menu={handle_popup_onclick.clone()}
                                                selected_ids={selected_ids_rc.clone()}
                                                on_toggle_select={handle_toggle_select.clone()}
                                                on_select_page={handle_select_page.clone()}
                                            />
                                        }
                                    }
                                    XtreamCluster::Video => {
                                        html! {
                                            <ExplorerPosterGrid
                                                cluster={XtreamCluster::Video}
                                                items={filtered_items_id.clone()}
                                                selected_item={Id::from_option((*selected_item).clone())}
                                                on_select_item={handle_select_item.clone()}
                                                on_popup_menu={handle_popup_onclick.clone()}
                                                is_list_view={*is_vod_list_view}
                                                selected_ids={selected_ids_rc.clone()}
                                                on_toggle_select={handle_toggle_select.clone()}
                                                on_select_page={handle_select_page.clone()}
                                            />
                                        }
                                    }
                                    XtreamCluster::Series => {
                                        html! {
                                            <ExplorerPosterGrid
                                                cluster={XtreamCluster::Series}
                                                items={filtered_items_id.clone()}
                                                selected_item={Id::from_option((*selected_item).clone())}
                                                on_select_item={handle_select_item.clone()}
                                                on_popup_menu={handle_popup_onclick.clone()}
                                                on_open_series={Some(load_series_info.clone())}
                                                is_list_view={false}
                                                selected_ids={selected_ids_rc.clone()}
                                                on_toggle_select={handle_toggle_select.clone()}
                                                on_select_page={handle_select_page.clone()}
                                            />
                                        }
                                    }
                                }
                            }
                        }
                    }
                </div>

                // DETAIL PANEL OR BULK EDIT PANEL
                {
                    if *is_bulk_edit_open && !selected_ids.is_empty() {
                        let selected_items: Rc<Vec<Rc<UiPlaylistItem>>> = Rc::new(
                            all_items_for_cluster
                                .iter()
                                .filter(|it| selected_ids.contains(&it.virtual_id))
                                .cloned()
                                .collect(),
                        );

                        let handle_bulk_close = {
                            let is_bulk_edit_open = is_bulk_edit_open.clone();
                            Callback::from(move |()| is_bulk_edit_open.set(false))
                        };

                        html! {
                            <BulkEditPanel
                                items={Id::new(selected_items)}
                                active_scope={bulk_scope.clone()}
                                target_id={target_id}
                                target_name={target_name.clone()}
                                on_close={handle_bulk_close}
                                on_saved={handle_bulk_updated.clone()}
                            />
                        }
                    } else if selected_item.is_some() {
                        html! {
                            <ExplorerDetailPanel
                                item={Id::from_option((*selected_item).clone())}
                                playlist_revision={playlist_revision}
                                on_close={handle_close_detail}
                                on_item_updated={handle_item_updated}
                            />
                        }
                    } else {
                        html! {}
                    }
                }
            </div>

            // POPUP CONTEXT MENU
            <PopupMenu
                is_open={*popup_is_open}
                anchor_ref={(*popup_anchor_ref).clone()}
                on_close={handle_popup_close}
            >
                {
                    html_if!(context.playlist_request.as_ref().is_some_and(|r| matches!(r, PlaylistRequest::Target(_))), {
                        <>
                            <MenuItem
                                icon="Clipboard"
                                name={ExplorerAction::CopyLinkTuliproxVirtualId.to_string()}
                                label={translate.t("LABEL.COPY_LINK_TULIPROX_VIRTUAL_ID")}
                                onclick={&handle_menu_click}
                            />
                            <MenuItem
                                icon="Clipboard"
                                name={ExplorerAction::CopyLinkTuliproxWebPlayerUrl.to_string()}
                                label={translate.t("LABEL.COPY_LINK_TULIPROX_WEBPLAYER_URL")}
                                onclick={&handle_menu_click}
                            />
                        </>
                    })
                }
                <MenuItem
                    icon="Clipboard"
                    name={ExplorerAction::CopyLinkProviderUrl.to_string()}
                    label={translate.t("LABEL.COPY_LINK_PROVIDER_URL")}
                    onclick={&handle_menu_click}
                />
                {
                    html_if!(can_show_record_action(can_write_recordings, selected_channel.as_ref()), {
                        <MenuItem
                            icon="Record"
                            name={ExplorerAction::Record.to_string()}
                            label={translate.t("LABEL.RECORD")}
                            onclick={&handle_menu_click}
                        />
                    })
                }
            </PopupMenu>

            // ORPHANS CLEANUP DIALOG
            {
                if *is_orphans_open {
                    let close_orphans = {
                        let is_orphans_open = is_orphans_open.clone();
                        Callback::from(move |()| is_orphans_open.set(false))
                    };
                    let initial_input = context.playlist_request.as_ref().and_then(|r| match r {
                        PlaylistRequest::Input(ref inp) => Some(inp.clone()),
                        _ => None,
                    });
                    html! {
                        <ExplorerOrphansView
                            input={initial_input}
                            on_close={close_orphans}
                        />
                    }
                } else {
                    html! {}
                }
            }
        </div>
    }
}

/// Builds a `background-image` declaration for an untrusted backdrop URL.
///
/// The URL comes from provider metadata, so it must not be able to break out of the
/// declaration: quotes, parentheses, backslashes and whitespace/control characters would allow
/// a second declaration to be injected. Any such URL yields an empty style instead.
#[must_use]
pub fn background_style(url: &str) -> String {
    let trimmed = url.trim();
    let is_safe = !trimmed.is_empty()
        && !trimmed
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '"' | '\'' | '(' | ')' | '\\' | ';'));
    if is_safe {
        format!("background-image: url(\"{trimmed}\");")
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::background_style;

    #[test]
    fn plain_urls_are_accepted() {
        assert_eq!(
            background_style("https://cdn.example.com/a.jpg"),
            "background-image: url(\"https://cdn.example.com/a.jpg\");"
        );
        assert_eq!(
            background_style("/api/v1/resource?token=abc"),
            "background-image: url(\"/api/v1/resource?token=abc\");"
        );
    }

    #[test]
    fn declaration_breakout_is_refused() {
        assert_eq!(background_style("https://x/a.jpg\");background:red"), "");
        assert_eq!(background_style("javascript:alert(1)"), "");
        assert_eq!(background_style("https://x/a b.jpg"), "");
        assert_eq!(background_style("   "), "");
    }

    #[test]
    fn every_explorer_i18n_key_exists_in_en_json() {
        const SOURCES: &[(&str, &str)] = &[
            ("bulk_edit.rs", include_str!("bulk_edit.rs")),
            ("detail_panel.rs", include_str!("detail_panel.rs")),
            ("edit_panel.rs", include_str!("edit_panel.rs")),
            ("epg_picker.rs", include_str!("epg_picker.rs")),
            ("identity.rs", include_str!("identity.rs")),
            ("image_preview.rs", include_str!("image_preview.rs")),
            ("live_view.rs", include_str!("live_view.rs")),
            ("mod.rs", include_str!("mod.rs")),
            ("orphans_view.rs", include_str!("orphans_view.rs")),
            ("poster_grid.rs", include_str!("poster_grid.rs")),
            ("sidebar.rs", include_str!("sidebar.rs")),
            ("sort.rs", include_str!("sort.rs")),
        ];

        let en: serde_json::Value =
            serde_json::from_str(include_str!("../../../../../public/assets/i18n/en.json")).expect("en.json parses");

        fn collect_dotted(prefix: &str, v: &serde_json::Value, out: &mut std::collections::BTreeSet<String>) {
            match v {
                serde_json::Value::Object(map) => {
                    for (k, val) in map {
                        let next = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                        if val.is_object() {
                            collect_dotted(&next, val, out);
                        } else {
                            out.insert(next);
                        }
                    }
                }
                _ => {
                    if !prefix.is_empty() {
                        out.insert(prefix.to_string());
                    }
                }
            }
        }

        let mut en_keys = std::collections::BTreeSet::new();
        collect_dotted("", &en, &mut en_keys);

        let mut missing = Vec::new();
        for (file, source) in SOURCES {
            let mut rest = *source;
            while let Some(open) = rest.find('"') {
                let after_open = &rest[open + 1..];
                let Some(close) = after_open.find('"') else { break };
                let literal = &after_open[..close];
                let is_i18n_key = (literal.starts_with("LABEL.") && literal.len() > "LABEL.".len()
                    || literal.starts_with("MESSAGES.") && literal.len() > "MESSAGES.".len()
                    || literal.starts_with("EXPLANATION.") && literal.len() > "EXPLANATION.".len())
                    && !literal.contains("{}");
                if is_i18n_key && !en_keys.contains(literal) {
                    missing.push(format!("{file}: key \"{literal}\" not found in en.json"));
                }
                rest = &after_open[close + 1..];
            }
        }

        assert!(missing.is_empty(), "Found missing i18n keys in explorer sources:\n{}", missing.join("\n"));
    }
}
