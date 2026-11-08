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
            popup_menu::{PopupMenu, PopupMenuPlacement},
            recording::{
                ensure_recording_available, target_name_for_id, PaddingBounds, RecordingForm, RecordingFormPrefill,
            },
            DropDownOption, DropDownSelection, HorizontalShrinkPanel, IconButton, NoContent, PluginActionDialog,
            PluginActionMenuItems, PluginActionRequest, PluginSlot, Search, Select, SplitButton, TextButton,
        },
        context::{ConfigContext, PlaylistExplorerContext},
    },
    hooks::{use_clipboard_copy, use_service_context},
    html_if,
    i18n::use_translation,
    model::{BusyStatus, DialogAction, DialogActions, DialogResult, EventMessage},
    services::{CreateRecordingTaskRequest, DialogService, RecordingService, RecordingSourceInput},
    utils::is_mobile_viewport,
};
/// Minimum star rating the VOD/series rating filter starts with.
const DEFAULT_MIN_RATING: u8 = 6;
/// Gap between a poster card's action button and its menu (same as `Select`).
const CARD_POPUP_GAP_PX: f64 = 2.0;
/// Thresholds offered by the rating filter dropdown.
const RATING_FILTER_STEPS: [u8; 9] = [1, 2, 3, 4, 5, 6, 7, 8, 9];

pub use bulk_edit::BulkEditPanel;
pub use detail_panel::*;
use identity::{use_identity_revision, Id};
pub use live_view::*;
pub use orphans_view::ExplorerOrphansView;
pub use poster_grid::*;
use serde_json::{json, Value};
use shared::{
    error::TuliproxError,
    model::{
        recording::RecordingVisibility, OverrideScopeDto, Permission, PlaylistRequest, PlaylistUrlResolveRequest,
        PluginChannelContextDto, PluginSlotDto, SearchRequest, SeriesStreamDetailEpisodeProperties,
        SeriesStreamProperties, UiPlaylistCategories, UiPlaylistGroup, UiPlaylistItem, VirtualId, XtreamCluster,
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
use wasm_bindgen::{prelude::Closure, JsCast};
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
    /// What a plugin may see of the channel; never carries a URL.
    pub plugin_context: PluginChannelContextDto,
}

/// The slot context of a playlist item. Stream and catch-up URLs embed provider
/// credentials, so none of them is copied.
#[must_use]
pub fn plugin_channel_context(item: &UiPlaylistItem) -> PluginChannelContextDto {
    PluginChannelContextDto {
        virtual_id: item.virtual_id,
        name: item.name.to_string(),
        title: item.title.to_string(),
        group: item.group.to_string(),
        item_type: item.item_type.as_str().to_string(),
        epg_channel_id: item.epg_channel_id.as_ref().map(ToString::to_string),
        input_name: (!item.input_name.is_empty()).then(|| item.input_name.to_string()),
    }
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

#[derive(Clone, PartialEq, Debug)]
pub enum ExplorerLevel {
    Browser,
    SeriesEpisodes(Rc<UiPlaylistItem>, Option<Box<SeriesStreamProperties>>),
}

#[must_use]
pub fn selected_group_from_id(id: &str) -> SelectedGroup {
    match id {
        "__all__" => SelectedGroup::All,
        "__without_epg__" => SelectedGroup::WithoutEpg,
        "__edited__" => SelectedGroup::Edited,
        s if s.starts_with("grp:") => SelectedGroup::Group(s[4..].into()),
        _ => SelectedGroup::All,
    }
}

/// Determines the next `ExplorerLevel` when a group is selected in the sidebar.
///
/// If the user was viewing series episodes, selecting any group returns them to
/// the catalog browser so the selected group is displayed.
#[must_use]
pub fn level_after_group_select(current: &ExplorerLevel) -> ExplorerLevel {
    match current {
        ExplorerLevel::SeriesEpisodes(_, _) => ExplorerLevel::Browser,
        ExplorerLevel::Browser => ExplorerLevel::Browser,
    }
}

/// Determines the next `ExplorerLevel` when a search is executed or cleared.
///
/// Running a search while viewing episodes returns to the catalog browser to show
/// matching items.
#[must_use]
pub fn level_after_search(current: &ExplorerLevel) -> ExplorerLevel {
    match current {
        ExplorerLevel::SeriesEpisodes(_, _) => ExplorerLevel::Browser,
        ExplorerLevel::Browser => ExplorerLevel::Browser,
    }
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
    let can_write = service_ctx.auth.has_permission(Permission::PlaylistWrite);
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
    let filter_rating = use_state(|| false);
    let filter_rating_min = use_state(|| DEFAULT_MIN_RATING);

    // Context popup & selection
    let selected_channel = use_state(|| None::<ChannelSelection>);
    // The plugin action being shown in its dialog, with the context it was opened with.
    let plugin_action = use_state(|| None::<(PluginActionRequest, Value)>);
    let popup_anchor_ref = use_state(|| None::<web_sys::Element>);
    let popup_is_open = use_state(|| false);
    let copy_to_clipboard = use_clipboard_copy();
    let is_mobile = use_state(is_mobile_viewport);

    // Keep mobile state in sync on resize, and close context/dropdown menus on viewport change.
    // The listener is registered once, so it reads the current values through refs that every render
    // refreshes; reading the state handles captured at registration would see the first render only.
    let mobile_now = use_mut_ref(|| *is_mobile);
    let popup_open_now = use_mut_ref(|| false);
    *mobile_now.borrow_mut() = *is_mobile;
    *popup_open_now.borrow_mut() = *popup_is_open;
    {
        let is_mobile = is_mobile.clone();
        let popup_is_open = popup_is_open.clone();
        let selected_channel = selected_channel.clone();
        let popup_anchor_ref = popup_anchor_ref.clone();
        let mobile_now = mobile_now.clone();
        let popup_open_now = popup_open_now.clone();
        use_effect_with((), move |()| {
            let onresize = Closure::<dyn FnMut(web_sys::Event)>::wrap(Box::new(move |_| {
                let mobile = is_mobile_viewport();
                if *mobile_now.borrow() != mobile {
                    *mobile_now.borrow_mut() = mobile;
                    is_mobile.set(mobile);
                }
                if *popup_open_now.borrow() {
                    *popup_open_now.borrow_mut() = false;
                    popup_is_open.set(false);
                    selected_channel.set(None);
                    popup_anchor_ref.set(None);
                }
            }));
            if let Some(win) = web_sys::window() {
                let _ = win.add_event_listener_with_callback("resize", onresize.as_ref().unchecked_ref());
            }
            move || {
                if let Some(win) = web_sys::window() {
                    let _ = win.remove_event_listener_with_callback("resize", onresize.as_ref().unchecked_ref());
                }
                drop(onresize);
            }
        });
    }

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
        let filter_rating = filter_rating.clone();
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
            filter_rating.set(false);
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
            *filter_rating,
            *filter_rating_min,
        ),
        |(sorted, group_filter, without_epg, edited, rating_on, rating_min)| {
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
                if !passes_rating_filter(item, *rating_on, *rating_min) {
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
        let current_level = current_level.clone();
        let context = context.clone();
        Callback::from(move |search_req| {
            current_level.set(level_after_search(&current_level));
            match search_req {
                SearchRequest::Clear => set_playlist.set((*context.playlist).clone()),
                SearchRequest::Text(ref _text, ref _search_fields)
                | SearchRequest::Regexp(ref _text, ref _search_fields) => {
                    services.event.broadcast(EventMessage::Busy(BusyStatus::Show));
                    let set_playlist = set_playlist.clone();
                    let context = context.clone();
                    let services = services.clone();
                    spawn_local(async move {
                        let filtered = context
                            .playlist
                            .as_ref()
                            .and_then(|categories| categories.filter(&search_req))
                            .map(Rc::new);
                        set_playlist.set(filtered);
                        services.event.broadcast(EventMessage::Busy(BusyStatus::Hide));
                    });
                }
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
                    plugin_context: plugin_channel_context(&dto),
                }));
                set_anchor_ref.set(Some(target));
                set_is_open.set(true);
            }
        })
    };

    let handle_plugin_action = {
        let plugin_action = plugin_action.clone();
        let selected_channel = selected_channel.clone();
        let set_is_open = popup_is_open.clone();
        Callback::from(move |request: PluginActionRequest| {
            if let Some(selection) = selected_channel.as_ref() {
                let context = if request.slot == PluginSlotDto::ChannelDetailPanel {
                    json!({ "channel": selection.plugin_context.clone() })
                } else {
                    json!({ "channels": [selection.plugin_context.clone()] })
                };
                plugin_action.set(Some((request, context)));
            }
            set_is_open.set(false);
        })
    };

    let handle_plugin_action_close = {
        let plugin_action = plugin_action.clone();
        Callback::from(move |()| plugin_action.set(None))
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
        let explorer_ctx = context.clone();
        let set_playlist = playlist.clone();
        let self_written = self_written_playlist.clone();
        let selected_item = selected_item.clone();
        Callback::from(move |patched_items: Vec<UiPlaylistItem>| {
            if patched_items.is_empty() {
                return;
            }
            let patch_map = edit_panel::bulk_patch_map(patched_items);

            if let Some(ref sel) = *selected_item {
                if let Some(patched) = patch_map.get(&(sel.xtream_cluster, sel.virtual_id)) {
                    selected_item.set(Some(Rc::new(patched.clone())));
                }
            }

            if let Some(ref cat) = *explorer_ctx.playlist {
                let new_source = Rc::new(edit_panel::apply_bulk_patches_to_categories(cat, &patch_map));
                let new_visible = (*set_playlist).as_ref().map_or_else(
                    || new_source.clone(),
                    |visible| Rc::new(edit_panel::apply_bulk_patches_to_categories(visible, &patch_map)),
                );
                set_playlist.set(Some(new_visible));
                *self_written.borrow_mut() = Some(new_source.clone());
                explorer_ctx.playlist.set(Some(new_source));
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
        let current_level = current_level.clone();
        Callback::from(move |group: SelectedGroup| {
            current_level.set(level_after_group_select(&current_level));
            selected_group.set(group);
        })
    };

    // Built only on mobile, and only when its inputs change: thousands of groups must not be walked
    // on every click.
    let mobile_group_options = use_memo(
        (
            *is_mobile,
            playlist_revision,
            *active_cluster,
            (*selected_group).clone(),
            *edited_count,
            *without_epg_count,
            total_items,
            Id::new(groups_for_cluster.clone()),
        ),
        {
            let translate = translate.clone();
            let active_cluster = *active_cluster;
            let groups_for_cluster = groups_for_cluster.clone();
            move |(mobile, _, _, selected_group, edited_count, without_epg_count, total_items, _)| {
                if !*mobile {
                    return Rc::new(Vec::new());
                }
                let total_items = *total_items;
                let mut opts = Vec::new();

                // 1. All groups
                opts.push(DropDownOption::new(
                    "__all__",
                    html! { <>{ translate.t("LABEL.EXPLORER_VIRTUAL_GROUP_ALL") }{ format!(" ({total_items})") }</> },
                    *selected_group == SelectedGroup::All,
                ));

                // 2. Without EPG (live only)
                if active_cluster == XtreamCluster::Live {
                    let without_epg_count = *without_epg_count;
                    opts.push(DropDownOption::new(
                        "__without_epg__",
                        html! { <>{ "⚠ " }{ translate.t("LABEL.EXPLORER_VIRTUAL_GROUP_WITHOUT_EPG") }{ format!(" ({without_epg_count})") }</> },
                        *selected_group == SelectedGroup::WithoutEpg,
                    ));
                }

                // 3. Edited
                let edited_count = *edited_count;
                opts.push(DropDownOption::new(
                    "__edited__",
                    html! { <>{ "✎ " }{ translate.t("LABEL.EXPLORER_VIRTUAL_GROUP_EDITED") }{ format!(" ({edited_count})") }</> },
                    *selected_group == SelectedGroup::Edited,
                ));

                // 4. Every group with its count
                if let Some(ref groups) = *groups_for_cluster {
                    for g in groups.iter() {
                        let is_sel = match selected_group {
                            SelectedGroup::Group(title) => title == &g.title,
                            _ => false,
                        };
                        opts.push(DropDownOption::new(
                            &format!("grp:{}", g.title),
                            html! { <>{ g.title.clone() }{ format!(" ({})", g.channels.len()) }</> },
                            is_sel,
                        ));
                    }
                }
                Rc::new(opts)
            }
        },
    );

    let handle_mobile_group_select = {
        let handle_select_group = handle_select_group.clone();
        Callback::from(move |(_name, sel): (String, DropDownSelection)| {
            if let DropDownSelection::Single(id) = sel {
                handle_select_group.emit(selected_group_from_id(&id));
            }
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
        Callback::from(move |(_name, selection): (String, DropDownSelection)| {
            if let DropDownSelection::Single(id) = selection {
                if let Some(sort) = ExplorerSort::from_id(&id) {
                    active_sort.set(sort);
                    active_desc.set(sort.default_desc());
                }
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
                    plugin_context: PluginChannelContextDto {
                        virtual_id: chan.id,
                        name: chan.title.to_string(),
                        title: chan.title.to_string(),
                        group: series_info.group.to_string(),
                        item_type: shared::model::PlaylistItemType::Series.as_str().to_string(),
                        epg_channel_id: None,
                        input_name: None,
                    },
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
                            <IconButton
                                name="episode_actions"
                                icon="Popup"
                                aria_label={Some(translate.t("LABEL.ACTIONS"))}
                                onclick={Callback::from(move |(_, e): (String, MouseEvent)| popup_onclick.emit((channel_select.clone(), e)))}
                            />
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
                    <PluginSlot
                        slot={PluginSlotDto::ChannelDetailPanel}
                        context={Some(json!({ "channel": plugin_channel_context(series_info) }))}
                    />
                </div>
            }
        };

    // A poster card has its action button on the right: open its menu right-aligned below the button,
    // attached like a select list. Row menus (button in the first column) keep the adaptive placement.
    let popup_in_card =
        (*popup_anchor_ref).as_ref().and_then(|anchor| anchor.closest(".post").ok().flatten()).is_some();
    let popup_placement = if popup_in_card { PopupMenuPlacement::BottomEndFlip } else { PopupMenuPlacement::default() };

    html! {
        <div class="tp__playlist-explorer tpx">
            // TOP BAR
            <div class="top">
                <div class="tabs">
                    <TextButton
                        name={"live"}
                        class={if *active_cluster == XtreamCluster::Live { "active" } else { "" }}
                        aria_pressed={Some((*active_cluster == XtreamCluster::Live).to_string())}
                        title={translate.t("LABEL.LIVE")}
                        badge={Some(live_count.to_string())}
                        onclick={
                            let ch = handle_cluster_change.clone();
                            Callback::from(move |_: String| ch.emit(XtreamCluster::Live))
                        }
                    />
                    <TextButton
                        name={"video"}
                        class={if *active_cluster == XtreamCluster::Video { "active" } else { "" }}
                        aria_pressed={Some((*active_cluster == XtreamCluster::Video).to_string())}
                        title={translate.t("LABEL.VOD")}
                        badge={Some(vod_count.to_string())}
                        onclick={
                            let ch = handle_cluster_change.clone();
                            Callback::from(move |_: String| ch.emit(XtreamCluster::Video))
                        }
                    />
                    <TextButton
                        name={"series"}
                        class={if *active_cluster == XtreamCluster::Series { "active" } else { "" }}
                        aria_pressed={Some((*active_cluster == XtreamCluster::Series).to_string())}
                        title={translate.t("LABEL.SERIES")}
                        badge={Some(series_count.to_string())}
                        onclick={
                            let ch = handle_cluster_change.clone();
                            Callback::from(move |_: String| ch.emit(XtreamCluster::Series))
                        }
                    />
                </div>

                <div class="search">
                    <Search
                        onsearch={handle_search}
                        options={search_fields.clone()}
                        on_fields_change={handle_search_fields_change}
                    />
                </div>

                <div class="top-actions">
                    { html_if!(can_write, {
                        <TextButton
                            name="orphan_cleanup"
                            icon="DeleteSweep"
                            title={translate.t("LABEL.EXPLORER_OVERRIDE_CLEANUP")}
                            aria_label={Some(translate.t("LABEL.EXPLORER_OVERRIDE_CLEANUP"))}
                            onclick={let is_orphans_open = is_orphans_open.clone(); Callback::from(move |_: String| is_orphans_open.set(true))}
                        />
                    })}
                </div>
            </div>

            // BODY: SIDEBAR + MAIN + DETAIL PANEL
            <div class="body">
                // SIDEBAR
                { html_if!(!*is_mobile, {
                    <HorizontalShrinkPanel
                        class="tp__explorer-group-panel"
                        shrink_width="8rem"
                        default_width="14rem"
                        max_width="28rem"
                        storage_key="tp-explorer-group-panel-width"
                    >
                        <ExplorerSidebar
                            cluster={*active_cluster}
                            groups={Id::from_option((*groups_for_cluster).clone())}
                            total_items={total_items}
                            edited_count={*edited_count}
                            without_epg_count={*without_epg_count}
                            selected_group={(*selected_group).clone()}
                            on_select_group={handle_select_group}
                        />
                    </HorizontalShrinkPanel>
                })}

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
                                    // Mobile Group Select
                                    { html_if!(*is_mobile, {
                                        <Select
                                            name="explorer_mobile_group"
                                            class="tp__explorer-group-select sel"
                                            options={(*mobile_group_options).clone()}
                                            on_select={handle_mobile_group_select}
                                            close_on_resize=true
                                        />
                                    })}
                                    // Live Chips
                                    {
                                        if *active_cluster == XtreamCluster::Live {
                                            let we_chip = filter_without_epg.clone();
                                            let ed_chip = filter_edited.clone();
                                            let all_chip_click = {
                                                let we = filter_without_epg.clone();
                                                let ed = filter_edited.clone();
                                                Callback::from(move |_: String| { we.set(false); ed.set(false); })
                                            };
                                            html! {
                                                <>
                                                    <TextButton
                                                        name="filter_all"
                                                        class={if !*we_chip && !*ed_chip { "active" } else { "" }}
                                                        aria_pressed={Some((!*we_chip && !*ed_chip).to_string())}
                                                        title={translate.t("LABEL.EXPLORER_FILTER_ALL")}
                                                        onclick={all_chip_click}
                                                    />
                                                    <TextButton
                                                        name="filter_without_epg"
                                                        class={if *we_chip { "active" } else { "" }}
                                                        aria_pressed={Some((*we_chip).to_string())}
                                                        title={translate.t("LABEL.EXPLORER_FILTER_WITHOUT_EPG")}
                                                        onclick={Callback::from(move |_: String| we_chip.set(!*we_chip))}
                                                    />
                                                    <TextButton
                                                        name="filter_edited"
                                                        class={if *ed_chip { "active" } else { "" }}
                                                        aria_pressed={Some((*ed_chip).to_string())}
                                                        title={translate.t("LABEL.EXPLORER_VIRTUAL_GROUP_EDITED")}
                                                        onclick={Callback::from(move |_: String| ed_chip.set(!*ed_chip))}
                                                    />
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
                                            let r_chip = filter_rating.clone();
                                            let r_min = filter_rating_min.clone();
                                            let on_rating_min = {
                                                let r_chip = filter_rating.clone();
                                                let r_min = filter_rating_min.clone();
                                                Callback::from(move |(_, sel): (String, DropDownSelection)| {
                                                    if let DropDownSelection::Single(v) = sel {
                                                        if let Ok(min) = v.parse::<u8>() {
                                                            r_min.set(min);
                                                            r_chip.set(true);
                                                        }
                                                    }
                                                })
                                            };
                                            html! {
                                                <>
                                                    <SplitButton
                                                        name="filter_rating"
                                                        active={*r_chip}
                                                        title={translate.t("LABEL.EXPLORER_FILTER_RATING_GTE").replace("{rating}", &r_min.to_string())}
                                                        menu_label={Some(translate.t("LABEL.EXPLORER_FILTER_RATING_MIN"))}
                                                        options={Rc::new(RATING_FILTER_STEPS.iter().map(|n| DropDownOption::new(
                                                            &n.to_string(),
                                                            html! { { translate.t("LABEL.EXPLORER_FILTER_RATING_GTE").replace("{rating}", &n.to_string()) } },
                                                            *n == *r_min,
                                                        )).collect::<Vec<_>>())}
                                                        onclick={Callback::from(move |_: String| r_chip.set(!*r_chip))}
                                                        on_select={on_rating_min}
                                                    />
                                                    <TextButton
                                                        name="filter_edited"
                                                        class={if *ed_chip { "active" } else { "" }}
                                                        aria_pressed={Some((*ed_chip).to_string())}
                                                        title={translate.t("LABEL.EXPLORER_VIRTUAL_GROUP_EDITED")}
                                                        onclick={Callback::from(move |_: String| ed_chip.set(!*ed_chip))}
                                                    />
                                                </>
                                            }
                                        } else {
                                            html! {}
                                        }
                                    }

                                    // Sort dropdown
                                    <Select
                                        name="explorer_sort"
                                        class="sel"
                                        options={Rc::new(available_sorts.iter().map(|s| DropDownOption::new(
                                            s.id(),
                                            html! { <>{ translate.t("LABEL.EXPLORER_SORT_PREFIX") }{ translate.t(s.label_key()) }</> },
                                            *s == *active_sort,
                                        )).collect::<Vec<_>>())}
                                        close_on_resize=true
                                        on_select={handle_sort_change}
                                    />

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
                                                    { html_if!(can_write, {
                                                        <TextButton
                                                            class="primary"
                                                            name="bulk_edit"
                                                            icon="Edit"
                                                            title={translate.t("LABEL.EXPLORER_BULK_EDIT")}
                                                            onclick={Callback::from(move |_: String| is_bulk.set(true))}
                                                        />
                                                    })}
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
                                                Callback::from(move |_: (String, MouseEvent)| is_list.set(false))
                                            };
                                            let toggle_list = {
                                                let is_list = is_list.clone();
                                                Callback::from(move |_: (String, MouseEvent)| is_list.set(true))
                                            };
                                            html! {
                                                <div class="view-toggle">
                                                    <IconButton
                                                        name="grid_view"
                                                        icon="VideoLibrary"
                                                        class={if *is_list { "" } else { "active" }}
                                                        hint={translate.t("LABEL.EXPLORER_GRID_VIEW")}
                                                        aria_label={Some(translate.t("LABEL.EXPLORER_GRID_VIEW"))}
                                                        onclick={toggle_grid}
                                                    />
                                                    <IconButton
                                                        name="list_view"
                                                        icon="Log"
                                                        class={if *is_list { "active" } else { "" }}
                                                        hint={translate.t("LABEL.EXPLORER_LIST_VIEW")}
                                                        aria_label={Some(translate.t("LABEL.EXPLORER_LIST_VIEW"))}
                                                        onclick={toggle_list}
                                                    />
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
                                                is_mobile={*is_mobile}
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
                    if can_write && *is_bulk_edit_open && !selected_ids.is_empty() {
                        let selected_items: Rc<Vec<Rc<UiPlaylistItem>>> = Rc::new(
                            all_items_for_cluster
                                .iter()
                                .filter(|it| selected_ids.contains(&it.virtual_id))
                                .cloned()
                                .collect(),
                        );

                        let handle_bulk_close = {
                            let is_bulk_edit_open = is_bulk_edit_open.clone();
                            let selected_ids = selected_ids.clone();
                            Callback::from(move |()| {
                                is_bulk_edit_open.set(false);
                                selected_ids.set(HashSet::new());
                            })
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
                                on_open_series={Some(load_series_info.clone())}
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
                placement={popup_placement}
                gap={popup_in_card.then_some(CARD_POPUP_GAP_PX)}
                class={classes!(popup_in_card.then_some("tp__popup-menu--select"))}
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
                // Live and VOD channels have no detail view: their `channel-detail-panel`
                // contributions open from the menu (series show theirs in the series view).
                {
                    html_if!(
                        selected_channel.as_ref().is_some_and(|selection| selection.cluster != XtreamCluster::Series),
                        { <PluginActionMenuItems slot={PluginSlotDto::ChannelDetailPanel} onselect={handle_plugin_action.clone()} /> }
                    )
                }
                <PluginActionMenuItems onselect={handle_plugin_action} />
            </PopupMenu>

            // PLUGIN ACTION DIALOG
            {
                if let Some((request, context)) = (*plugin_action).as_ref() {
                    html! {
                        <PluginActionDialog
                            request={request.clone()}
                            context={Some(context.clone())}
                            on_close={handle_plugin_action_close}
                        />
                    }
                } else {
                    html! {}
                }
            }

            // ORPHANS CLEANUP DIALOG
            {
                if can_write && *is_orphans_open {
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
    use super::*;
    use std::rc::Rc;

    #[test]
    fn selected_group_from_id_maps_ids_correctly() {
        assert_eq!(selected_group_from_id("__all__"), SelectedGroup::All);
        assert_eq!(selected_group_from_id("__without_epg__"), SelectedGroup::WithoutEpg);
        assert_eq!(selected_group_from_id("__edited__"), SelectedGroup::Edited);
        assert_eq!(selected_group_from_id("grp:Sports"), SelectedGroup::Group("Sports".into()));
        assert_eq!(selected_group_from_id("unknown"), SelectedGroup::All);
    }

    #[test]
    fn search_by_provider_id_matches_override_id_and_provider_id() {
        use shared::model::{PlaylistItemType, UiItemExplorerMeta, UiPlaylistItem};
        use std::sync::Arc;

        let item_with_override = Rc::new(UiPlaylistItem {
            virtual_id: 1,
            provider_id: Arc::from("m3u-stream-id"),
            name: Arc::from("Channel 1"),
            title: Arc::from("Channel 1"),
            group: Arc::from("Group"),
            logo: Arc::from(""),
            url: Arc::from(""),
            item_type: PlaylistItemType::Live,
            xtream_cluster: shared::model::XtreamCluster::Live,
            category_id: 1,
            rating: 0.0,
            input_name: Arc::from("m3u-input"),
            epg_channel_id: None,
            explorer_meta: Some(UiItemExplorerMeta {
                override_id: Some(Arc::from("urlhash-v1:blake3hash123")),
                ..Default::default()
            }),
        });

        let item_plain = Rc::new(UiPlaylistItem {
            virtual_id: 2,
            provider_id: Arc::from("xtream-12345"),
            name: Arc::from("Channel 2"),
            title: Arc::from("Channel 2"),
            group: Arc::from("Group"),
            logo: Arc::from(""),
            url: Arc::from(""),
            item_type: PlaylistItemType::Live,
            xtream_cluster: shared::model::XtreamCluster::Live,
            category_id: 1,
            rating: 0.0,
            input_name: Arc::from("xtream-input"),
            epg_channel_id: None,
            explorer_meta: None,
        });

        let group = Rc::new(shared::model::UiPlaylistGroup {
            id: 1,
            title: Arc::from("Group"),
            channels: vec![item_with_override.clone(), item_plain.clone()],
            xtream_cluster: shared::model::XtreamCluster::Live,
        });

        let categories = shared::model::UiPlaylistCategories { live: Some(vec![group]), vod: None, series: None };

        // 1. Search by override_id exact
        let req_override_id = shared::model::SearchRequest::Text(
            "urlhash-v1:blake3hash123".to_string(),
            Some(Rc::new(vec![shared::model::SEARCH_FIELD_PROVIDER_ID.to_string()])),
        );
        let res1 = categories.filter(&req_override_id);
        assert!(res1.is_some());
        let live1 = res1.unwrap().live.unwrap();
        assert_eq!(live1[0].channels.len(), 1);
        assert_eq!(live1[0].channels[0].virtual_id, 1);

        // 2. Search by provider_id exact
        let req_provider_id = shared::model::SearchRequest::Text(
            "xtream-12345".to_string(),
            Some(Rc::new(vec![shared::model::SEARCH_FIELD_PROVIDER_ID.to_string()])),
        );
        let res2 = categories.filter(&req_provider_id);
        assert!(res2.is_some());
        let live2 = res2.unwrap().live.unwrap();
        assert_eq!(live2[0].channels.len(), 1);
        assert_eq!(live2[0].channels[0].virtual_id, 2);
    }

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

    fn make_test_item(virtual_id: u32, cluster: shared::model::XtreamCluster) -> shared::model::UiPlaylistItem {
        shared::model::UiPlaylistItem {
            virtual_id,
            provider_id: std::sync::Arc::from("prov-1"),
            name: std::sync::Arc::from("Series 1"),
            title: std::sync::Arc::from("Series 1"),
            group: std::sync::Arc::from("Group"),
            logo: std::sync::Arc::from(""),
            url: std::sync::Arc::from(""),
            item_type: shared::model::PlaylistItemType::Series,
            xtream_cluster: cluster,
            category_id: 1,
            rating: 8.0,
            input_name: std::sync::Arc::from("input-1"),
            epg_channel_id: None,
            explorer_meta: None,
        }
    }

    #[test]
    fn group_selection_leaves_series_episodes_level() {
        let dummy = Rc::new(make_test_item(10, XtreamCluster::Series));
        let level = ExplorerLevel::SeriesEpisodes(dummy, None);
        assert_eq!(level_after_group_select(&level), ExplorerLevel::Browser);
        assert_eq!(level_after_group_select(&ExplorerLevel::Browser), ExplorerLevel::Browser);
    }

    #[test]
    fn search_leaves_series_episodes_level() {
        let dummy = Rc::new(make_test_item(10, XtreamCluster::Series));
        let level = ExplorerLevel::SeriesEpisodes(dummy, None);
        assert_eq!(level_after_search(&level), ExplorerLevel::Browser);
        assert_eq!(level_after_search(&ExplorerLevel::Browser), ExplorerLevel::Browser);
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
