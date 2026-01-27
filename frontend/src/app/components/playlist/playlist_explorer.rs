use crate::app::components::menu_item::MenuItem;
use crate::app::components::popup_menu::PopupMenu;
use crate::app::components::{AppIcon, Chip, IconButton, NoContent, Panel, Search};
use crate::app::context::PlaylistExplorerContext;
use crate::hooks::use_service_context;
use crate::html_if;
use crate::model::{BusyStatus, EventMessage};
use crate::services::DialogService;
use shared::error::{info_err_res, TuliproxError};
use shared::model::{PlaylistRequest, SearchRequest, SeriesStreamDetailEpisodeProperties, SeriesStreamProperties, UiPlaylistGroup, UiPlaylistItem, VirtualId, XtreamCluster};
use shared::utils::format_float_localized;
use std::collections::HashMap;
use std::fmt::Display;
use std::rc::Rc;
use std::str::FromStr;
use wasm_bindgen::JsCast;
use yew::platform::spawn_local;
use yew::prelude::*;
use yew_hooks::use_clipboard;
use yew_i18n::use_translation;

const COPY_LINK_TULIPROX_VIRTUAL_ID: &str = "copy_link_tuliprox_virtual_id";
const COPY_LINK_TULIPROX_WEBPLAYER_URL: &str = "copy_link_tuliprox_webplayer_url";
const COPY_LINK_PROVIDER_URL: &str = "copy_link_provider_url";

#[derive(Clone)]
struct ChannelSelection {
    virtual_id: VirtualId,
    cluster: XtreamCluster,
    url: String,
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Eq, PartialEq)]
enum ExplorerAction {
    CopyLinkTuliproxVirtualId,
    CopyLinkTuliproxWebPlayerUrl,
    CopyLinkProviderUrl,
}

impl Display for ExplorerAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", match self {
            Self::CopyLinkTuliproxVirtualId => COPY_LINK_TULIPROX_VIRTUAL_ID,
            Self::CopyLinkTuliproxWebPlayerUrl => COPY_LINK_TULIPROX_WEBPLAYER_URL,
            Self::CopyLinkProviderUrl => COPY_LINK_PROVIDER_URL,
        })
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
        } else {
            info_err_res!("Unknown ExplorerAction: {}", s)
        }
    }
}

enum ExplorerLevel {
    Categories,
    Group(Rc<UiPlaylistGroup>),
    SeriesInfo(Rc<UiPlaylistGroup>, Rc<UiPlaylistItem>, Option<Box<SeriesStreamProperties>>),
}

#[function_component]
pub fn PlaylistExplorer() -> Html {
    let context = use_context::<PlaylistExplorerContext>().expect("PlaylistExplorer context not found");
    let dialog = use_context::<DialogService>().expect("Dialog service not found");
    let translate = use_translation();
    let service_ctx = use_service_context();
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
                    url: dto.url.to_string(),
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
                        set_current_item.set(ExplorerLevel::SeriesInfo(group.clone(), dto.clone(), Some(Box::new(props))));
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
        use_effect_with(context.playlist.clone(), move |new_playlist| {
            set_current_item.set(ExplorerLevel::Categories);
            set_playlist.set((**new_playlist).clone());
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
                    let _result = dlg.content(html! {<input value={text} readonly={true} class="tp__copy-input"/>}, None, false).await;
                });
            }
        })
    };

    let handle_menu_click = {
        let services = service_ctx.clone();
        let popup_is_open_state = popup_is_open.clone();
        let selected_channel = selected_channel.clone();
        let playlist_ctx = context.clone();
        let translate_clone = translate.clone();
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
                                        spawn_local(async move {
                                            if let Some(url) = services.playlist.get_playlist_webplayer_url(target_id, virtual_id, cluster).await {
                                                copy_to_clipboard.emit(url);
                                                services.toastr.success(translate_clone.t("MESSAGES.PLAYLIST.WEBPLAYER_URL_COPY_TO_CLIPBOARD"));
                                            } else {
                                                services.toastr.error(translate_clone.t("MESSAGES.FAILED_TO_RETRIEVE_WEBPLAYER_URL"));
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
                            copy_to_clipboard.emit(dto.url.to_string());
                        }
                    }
                }
            }
            popup_is_open_state.set(false);
        })
    };

    let handle_back_click = {
        let current_item = current_item.clone();
        Callback::from(move |_| {
            match *current_item {
                ExplorerLevel::Categories => {}
                ExplorerLevel::Group(_) => {
                    current_item.set(ExplorerLevel::Categories);
                }
                ExplorerLevel::SeriesInfo(ref group, _, _) => {
                    current_item.set(ExplorerLevel::Group(group.clone()));
                }
            }
        })
    };

    let handle_search = {
        let services = service_ctx.clone();
        let set_playlist = playlist.clone();
        let set_current_item = current_item.clone();
        let context = context.clone();
        Callback::from(move |search_req| {
            match search_req {
                SearchRequest::Clear => set_playlist.set((*context.playlist).clone()),
                SearchRequest::Text(ref _text, ref _search_fields)
                | SearchRequest::Regexp(ref _text, ref _search_fields) => {
                    services.event.broadcast(EventMessage::Busy(BusyStatus::Show));
                    let set_playlist = set_playlist.clone();
                    let set_current_item = set_current_item.clone();
                    let context = context.clone();
                    let services = services.clone();
                    spawn_local(async move {
                        let filtered = context
                            .playlist
                            .as_ref()
                            .and_then(|categories| categories.filter(&search_req))
                            .map(Rc::new);
                        set_playlist.set(filtered);
                        set_current_item.set(ExplorerLevel::Categories);
                        services.event.broadcast(EventMessage::Busy(BusyStatus::Hide));
                    });
                }
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
        let logo = if logo.is_empty() {
            "assets/missing-logo.svg".to_string()
        } else {
            logo.to_string()
        };
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

    let render_episode = |chan: &&SeriesStreamDetailEpisodeProperties| {
        let channel_select = ChannelSelection {
            virtual_id: chan.id,
            cluster: XtreamCluster::Series,
            url: String::new(), // TODO provider url
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

    let render_channel = |chan: &Rc<UiPlaylistItem>| {
        match chan.xtream_cluster {
            XtreamCluster::Live => render_live(chan),
            XtreamCluster::Video => render_movie(chan),
            XtreamCluster::Series => render_series(chan),
        }
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
                let backdrop = series_props.backdrop_path.as_ref().and_then(|l| l.first()).map_or_else(|| if series_props.cover.is_empty() { series_info.logo.to_string() } else { series_props.cover.to_string() }, ToString::to_string);
                (Some(backdrop.to_string()),
                 series_props.plot.as_deref().map(ToString::to_string).unwrap_or_default(),
                 series_props.cast.to_string(),
                 series_props.genre.as_deref().map(ToString::to_string).unwrap_or_default(),
                 series_props.release_date.as_deref().map(ToString::to_string).unwrap_or_default(),
                 series_props.rating,
                 series_props.details.as_ref(),
                )
            }
            _ => {
                (None, String::new(), String::new(), String::new(), String::new(), 0.0, None)
            }
        };

        if !series_info.logo.is_empty() && backdrop.as_ref().is_none_or(|v| v.is_empty()) {
            backdrop = Some(series_info.logo.to_string());
        };

        let style = backdrop
            .as_ref()
            .map(|b| format!("background-image: url(\"{b}\");"))
            .unwrap_or_default();

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
                grouped.entry(item.season)
                    .or_default()
                    .push(item);
            };
            let mut grouped_list: Vec<(u32, Vec<&SeriesStreamDetailEpisodeProperties>)> = grouped.into_iter().collect();
            grouped_list.sort_by_key(|(season, _)| *season);

            html! {
                for grouped_list.iter().map(|(season, season_episodes)|
                    html! {
                    <>
                    <div class={"tp__playlist-explorer__series-info__season"}>
                        <span class={"tp__playlist-explorer__series-info__season-title"}>{translate.t("LABEL.SEASON")} {" - "} {season}</span>
                    </div>
                    <div class={"tp__playlist-explorer__group-list tp__playlist-explorer__group-list-episodes"}>
                    {
                        for season_episodes.iter().map(render_episode)
                    }
                    </div>
                    </>
                    }
                )
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
                        ExplorerLevel::Group(ref group) => html!{ <span>{&group.title}</span> },
                        ExplorerLevel::SeriesInfo(_, ref pli, _) => html!{ <span>{&pli.title}</span> },
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
        </PopupMenu>
      </div>
    }
}
