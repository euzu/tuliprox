use std::fmt::Display;
use std::rc::Rc;
use std::str::FromStr;
use wasm_bindgen::JsCast;
use yew::platform::spawn_local;
use crate::app::context::PlaylistExplorerContext;
use yew::prelude::*;
use yew_hooks::use_clipboard;
use yew_i18n::use_translation;
use shared::error::{TuliproxError, info_err_res};
use shared::model::{CommonPlaylistItem, PlaylistRequest, SearchRequest, UiPlaylistGroup, XtreamCluster};
use crate::app::components::{AppIcon, IconButton, NoContent, Panel, Search};
use crate::app::components::menu_item::MenuItem;
use crate::app::components::popup_menu::PopupMenu;
use crate::hooks::use_service_context;
use crate::html_if;
use crate::model::{BusyStatus, EventMessage};
use crate::services::DialogService;

const COPY_LINK_TULIPROX_VIRTUAL_ID: &str = "copy_link_tuliprox_virtual_id";
const COPY_LINK_TULIPROX_WEBPLAYER_URL: &str = "copy_link_tuliprox_webplayer_url";
const COPY_LINK_PROVIDER_URL: &str = "copy_link_provider_url";

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
}

#[function_component]
pub fn PlaylistExplorer() -> Html {
    let context = use_context::<PlaylistExplorerContext>().expect("PlaylistExplorer context not found");
    let dialog = use_context::<DialogService>().expect("Dialog service not found");
    let translate = use_translation();
    let service_ctx = use_service_context();
    let current_item = use_state(|| ExplorerLevel::Categories);
    let playlist = use_state(|| (*context.playlist).clone());
    let selected_channel = use_state(|| None::<Rc<CommonPlaylistItem>>);
    let popup_anchor_ref = use_state(|| None::<web_sys::Element>);
    let popup_is_open = use_state(|| false);
    let clipboard = use_clipboard();
    let cluster_visible = use_state(|| XtreamCluster::Live);

    let handle_cluster_change = {
        let cluster_vis = cluster_visible.clone();
        Callback::from(move |(name, _event) : (String, MouseEvent)| {
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
        Callback::from(move |(dto, event): (Rc<CommonPlaylistItem>, MouseEvent)| {
            if let Some(target) = event.target_dyn_into::<web_sys::Element>() {
                set_selected_channel.set(Some(dto.clone()));
                set_anchor_ref.set(Some(target));
                set_is_open.set(true);
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
                                        let cluster = dto.xtream_cluster.unwrap_or_default();
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
                            copy_to_clipboard.emit(dto.url.clone());
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

    let render_channel_logo = |chan: &Rc<CommonPlaylistItem>| {
        let logo = if chan.logo.is_empty() { chan.logo_small.as_str() } else { chan.logo.as_str() };
        if logo.is_empty() {
            html! {}
        } else {
            html! { <img class="tp__playlist-explorer__channel-logo" alt={"n/a"} src={logo.to_owned()} loading="lazy"
                    onerror={Callback::from(move |e: web_sys::Event| {
                    if let Some(target)  = e.target() {
                        if let Ok(img) = target.dyn_into::<web_sys::HtmlMediaElement>() {
                            img.set_src("assets/missing-logo.svg");
                        }
                    }
                    })}
                />}
        }
    };

    let render_group = |group: &Rc<UiPlaylistGroup>| {
        html! {
                <div class="tp__playlist-explorer__group">
                  <div class={format!("tp__playlist-explorer__group-list tp__playlist-explorer__group-list-{}", group.xtream_cluster.to_string().to_lowercase())}>
                  {
                      group.channels.iter().map(|chan| {
                        let chan_clone = chan.clone();
                        let popup_onclick = handle_popup_onclick.clone();
                        html! {
                            <span class={format!("tp__playlist-explorer__channel tp__playlist-explorer__channel-{}", group.xtream_cluster.to_string().to_lowercase())}>
                                <button class="tp__icon-button"
                                    onclick={Callback::from(move |event: MouseEvent| popup_onclick.emit((chan_clone.clone(), event)))}>
                                    <AppIcon name="Popup"></AppIcon>
                                </button>
                                {render_channel_logo(chan)}
                                <span class="tp__playlist-explorer__channel-title">{chan.title.clone()}</span>
                            </span>
                          }
                       }).collect::<Html>()
                  }
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
