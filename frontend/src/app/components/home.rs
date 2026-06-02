use crate::{
    app::{
        components::{
            config::ConfigView, loading_indicator::BusyIndicator, map_sources_to_playlist_rows, theme::Theme, AppIcon,
            DashboardView, DownloadsView, EpgView, ErrorBoundary, IconButton, LanguagePicker, NoAccess, Panel,
            ParticleFlowBackground, PlaylistExplorerView, PlaylistSettingsView, PlaylistUpdateView, RbacView, Setup,
            Sidebar, SourceEditor, StatsView, StreamHistoryView, StreamsView, ThemePicker, ToastrView, UserlistView,
            WebsocketStatus,
        },
        context::{ConfigContext, PlaylistContext, StatusContext},
    },
    hooks::{use_server_status, use_service_context},
    html_if,
    i18n::use_translation,
    model::{EventMessage, ViewType},
    provider::DialogProvider,
    services::{FlagsLoadState, ToastCloseMode, ToastOptions},
    utils::{get_location_hash, set_location_hash},
};
use gloo_timers::future::TimeoutFuture;
use log::error;
use shared::{
    model::{
        permission::{Permission, PERM_ALL},
        ApiProxyConfigDto, AppConfigDto, LibraryScanSummaryStatus, PlaylistUpdateState, StatusCheck, SystemInfo,
    },
    utils::Internable,
};
use std::{cell::Cell, future, rc::Rc, str::FromStr};
use wasm_bindgen::{closure::Closure, JsCast};
use web_sys::window;
use yew::{platform::spawn_local, prelude::*, suspense::use_future};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HomeViewAccess {
    setup_mode: bool,
    show_streams_page: bool,
    can_read_system_status: bool,
    can_read_config: bool,
    can_read_users: bool,
    can_read_sources: bool,
    can_write_playlist: bool,
    can_read_playlist: bool,
    can_read_epg: bool,
    can_read_downloads: bool,
    is_admin: bool,
}

fn configured_home_fallback(landing_page: ViewType, combine_views_stats_streams: bool) -> ViewType {
    if combine_views_stats_streams && landing_page == ViewType::Streams {
        ViewType::Stats
    } else {
        landing_page
    }
}

fn normalize_requested_home_view(view: ViewType, access: HomeViewAccess) -> ViewType {
    if !access.show_streams_page && view == ViewType::Streams {
        ViewType::Stats
    } else {
        view
    }
}

fn is_allowed_home_view(view: ViewType, access: HomeViewAccess) -> bool {
    if access.setup_mode {
        return view == ViewType::Config;
    }

    match normalize_requested_home_view(view, access) {
        ViewType::Dashboard => true,
        ViewType::Stats | ViewType::StreamHistory => access.can_read_system_status,
        ViewType::Streams => access.show_streams_page && access.can_read_system_status,
        ViewType::Downloads => access.can_read_downloads,
        ViewType::Users => access.can_read_users,
        ViewType::Config => access.can_read_config,
        ViewType::SourceEditor => access.can_read_sources,
        ViewType::PlaylistUpdate => access.can_write_playlist,
        ViewType::PlaylistSettings | ViewType::PlaylistExplorer => access.can_read_playlist,
        ViewType::PlaylistEpg => access.can_read_epg,
        ViewType::Rbac => access.is_admin,
    }
}

fn first_allowed_home_view(access: HomeViewAccess) -> ViewType {
    [
        ViewType::Dashboard,
        ViewType::Stats,
        ViewType::Streams,
        ViewType::StreamHistory,
        ViewType::Downloads,
        ViewType::Config,
        ViewType::Users,
        ViewType::SourceEditor,
        ViewType::PlaylistUpdate,
        ViewType::PlaylistSettings,
        ViewType::PlaylistExplorer,
        ViewType::PlaylistEpg,
        ViewType::Rbac,
    ]
    .into_iter()
    .map(|view| normalize_requested_home_view(view, access))
    .find(|view| is_allowed_home_view(*view, access))
    .unwrap_or(ViewType::Dashboard)
}

fn resolve_home_view(requested: Option<ViewType>, fallback: ViewType, access: HomeViewAccess) -> ViewType {
    if access.setup_mode {
        return ViewType::Config;
    }

    requested
        .map(|view| normalize_requested_home_view(view, access))
        .filter(|view| is_allowed_home_view(*view, access))
        .or_else(|| {
            let fallback = normalize_requested_home_view(fallback, access);
            is_allowed_home_view(fallback, access).then_some(fallback)
        })
        .unwrap_or_else(|| first_allowed_home_view(access))
}

#[component]
pub fn Home() -> Html {
    let services = use_service_context();
    let setup_mode = services.config.ui_config.setup_mode;
    let translate = use_translation();
    let config = use_state(|| None::<Rc<AppConfigDto>>);
    let api_proxy_config = use_state(|| None::<Rc<ApiProxyConfigDto>>);
    let status = use_state(|| None::<Rc<StatusCheck>>);
    let system_info = use_state(|| None::<Rc<SystemInfo>>);
    let view_visible = use_state(|| {
        if setup_mode {
            Some(ViewType::Config)
        } else {
            get_location_hash().and_then(|hash| ViewType::from_str(&hash).ok())
        }
    });
    let theme = use_state(Theme::get_current_theme);
    let force_update = use_force_update();

    let handle_theme_select = {
        let set_theme = theme.clone();
        Callback::from(move |new_theme: Theme| {
            new_theme.switch_theme();
            set_theme.set(new_theme);
        })
    };

    let handle_logout = {
        let services_ctx = services.clone();
        Callback::from(move |_| services_ctx.auth.logout())
    };

    let handle_view_change_sidebar = {
        let view_vis = view_visible.clone();
        Callback::from(move |view: ViewType| {
            set_location_hash(view.as_str());
            view_vis.set(Some(view));
        })
    };

    {
        let services_ctx = services.clone();
        let translate_clone = translate.clone();
        use_effect_with((), move |_| {
            let services_ctx = services_ctx.clone();
            let services_ctx_clone = services_ctx.clone();
            let translate_clone = translate_clone.clone();
            let subid = services_ctx.event.subscribe(move |msg| match msg {
                EventMessage::Unauthorized => services_ctx_clone.auth.logout(),
                EventMessage::ServerError(msg) => {
                    services_ctx_clone.toastr.error(msg);
                }
                EventMessage::ConfigChange(config_type) => {
                    services_ctx_clone.toastr.warning_with_options(
                        format!("{}: {config_type}", translate_clone.t("MESSAGES.CONFIG_CHANGED")),
                        ToastOptions { close_mode: ToastCloseMode::Manual },
                    );
                }
                EventMessage::PlaylistUpdate(update_state) => match update_state {
                    PlaylistUpdateState::Success => {
                        services_ctx_clone.toastr.success(translate_clone.t("MESSAGES.PLAYLIST_UPDATE.SUCCESS_FINISH"))
                    }
                    PlaylistUpdateState::Failure => {
                        services_ctx_clone.toastr.error(translate_clone.t("MESSAGES.PLAYLIST_UPDATE.FAIL_FINISH"))
                    }
                },
                EventMessage::LibraryScanProgress(summary) => match summary.status {
                    LibraryScanSummaryStatus::Success => services_ctx_clone.toastr.success(summary.message),
                    LibraryScanSummaryStatus::Error => services_ctx_clone.toastr.error(summary.message),
                },
                _ => {}
            });
            move || services_ctx.event.unsubscribe(subid)
        });
    }

    let can_read_system_status = services.auth.has_permission(Permission::SystemRead);
    let can_read_config = services.auth.has_permission(Permission::ConfigRead);
    let can_read_users = services.auth.has_permission(Permission::UserRead);
    let can_read_sources = services.auth.has_permission(Permission::SourceRead);
    let can_write_playlist = services.auth.has_permission(Permission::PlaylistWrite);
    let can_read_playlist = services.auth.has_permission(Permission::PlaylistRead);
    let can_read_epg = services.auth.has_permission(Permission::EpgRead);
    let can_read_downloads = services.auth.has_permission(Permission::DownloadRead);
    let is_admin = services.auth.is_admin();
    let _ = use_server_status(status.clone(), system_info.clone(), !setup_mode && can_read_system_status);

    {
        // first register for config update
        let services_ctx = services.clone();
        let config_state = config.clone();
        let _ = use_future(|| async move {
            services_ctx
                .config
                .config_subscribe(&mut |cfg| {
                    config_state.set(cfg.clone());
                    future::ready(())
                })
                .await
        });

        let services_ctx = services.clone();
        let api_proxy_config_state = api_proxy_config.clone();
        let _ = use_future(|| async move {
            services_ctx
                .config
                .api_proxy_config_subscribe(&mut |cfg| {
                    api_proxy_config_state.set(cfg.clone());
                    future::ready(())
                })
                .await
        });
    }

    {
        let services_ctx = services.clone();
        let _ = use_future(|| async move {
            let _cfg = services_ctx.config.get_server_config().await;
        });
    }

    let sources = use_memo((*config).clone(), |config_ctx| {
        config_ctx.as_ref().map(|cfg| map_sources_to_playlist_rows(&cfg.sources))
    });

    let config_context = ConfigContext { config: (*config).clone(), api_proxy: (*api_proxy_config).clone() };

    let status_context = StatusContext { status: (*status).clone(), system_info: (*system_info).clone() };
    let playlist_context = PlaylistContext { sources: sources.clone() };

    // combine_views_stats_streams=true means embed streams in stats (no separate page), so show_streams_page = !combine_views_stats_streams.
    // The default unwrap_or(true) correctly preserves backward compatibility (separate pages by default).
    let show_streams_page = config_context
        .config
        .as_ref()
        .and_then(|app_cfg| app_cfg.config.web_ui.as_ref())
        .map(|web_ui| !web_ui.combine_views_stats_streams)
        .unwrap_or(true);
    let home_access = HomeViewAccess {
        setup_mode,
        show_streams_page,
        can_read_system_status,
        can_read_config,
        can_read_users,
        can_read_sources,
        can_write_playlist,
        can_read_playlist,
        can_read_epg,
        can_read_downloads,
        is_admin,
    };
    let configured_fallback = config_context
        .config
        .as_ref()
        .and_then(|app_cfg| app_cfg.config.web_ui.as_ref())
        .map_or(ViewType::Dashboard, |web_ui| {
            configured_home_fallback(web_ui.landing_page, web_ui.combine_views_stats_streams)
        });

    //<div class={"app-header__toolbar"}><select onchange={handle_language} defaultValue={i18next.language}>{services.config().getUiConfig().languages.map(l => <option key={l} value={l}>{l}</option>)}</select></div>

    let geoip_enabled = config.as_ref().is_some_and(|cfg| cfg.config.is_geoip_enabled());

    {
        let flags_service = services.flags.clone();
        let flags_loaded = force_update.clone();
        use_effect_with(geoip_enabled, move |geoip_enabled| {
            let cancelled = Rc::new(Cell::new(false));
            if *geoip_enabled {
                let flags_service = flags_service.clone();
                let flags_loaded = flags_loaded.clone();
                let cancelled = cancelled.clone();
                spawn_local(async move {
                    while !cancelled.get() && !flags_service.is_loaded() {
                        match flags_service.ensure_loaded_from_assets().await {
                            Ok(FlagsLoadState::Loaded) => {
                                flags_loaded.force_update();
                                break;
                            }
                            Ok(FlagsLoadState::InProgress) => {
                                TimeoutFuture::new(250).await;
                            }
                            Err(err) => {
                                error!("Failed to load flags {err}");
                                TimeoutFuture::new(5000).await;
                            }
                        }
                    }
                    if flags_service.is_loaded() && !cancelled.get() {
                        flags_loaded.force_update();
                    }
                });
            } else {
                flags_loaded.force_update();
            }
            move || cancelled.set(true)
        });
    }

    {
        use_effect_with(
            (*view_visible, configured_fallback, home_access),
            move |(current, configured_fallback, access)| {
                if !setup_mode {
                    let resolved = resolve_home_view(*current, *configured_fallback, *access);
                    let want = resolved.as_str();
                    if get_location_hash().as_deref() != Some(want) {
                        set_location_hash(want);
                    }
                }
                || ()
            },
        );
    }

    {
        let view_vis = view_visible.clone();
        use_effect_with((configured_fallback, home_access), move |(configured_fallback, access)| {
            let configured_fallback = *configured_fallback;
            let access = *access;
            let closure = Closure::<dyn FnMut(Event)>::wrap(Box::new(move |_event: Event| {
                let requested = get_location_hash().and_then(|hash| ViewType::from_str(&hash).ok());
                let resolved = resolve_home_view(requested, configured_fallback, access);
                view_vis.set(Some(resolved));
            }));
            let win = window();
            if let Some(win) = win.as_ref() {
                if win.add_event_listener_with_callback("hashchange", closure.as_ref().unchecked_ref()).is_err() {
                    error!("failed to register hashchange listener");
                }
            }
            move || {
                if let Some(win) = window() {
                    let _ = win.remove_event_listener_with_callback("hashchange", closure.as_ref().unchecked_ref());
                }
            }
        });
    }

    {
        let view_vis = view_visible.clone();
        use_effect_with((configured_fallback, home_access), move |(configured_fallback, access)| {
            let current = *view_vis;
            let resolved = resolve_home_view(current, *configured_fallback, *access);
            if current != Some(resolved) {
                view_vis.set(Some(resolved));
            }
            || ()
        });
    }

    if config.is_none() {
        return html! {};
    }

    // Check if non-admin user has any permissions at all
    let has_any_permission = services.auth.has_any_permissions(PERM_ALL);

    if !has_any_permission && !setup_mode {
        return html! {
            <div class="tp__app">
                <div class="tp__app-main">
                    <div class="tp__app-main__header tp__app-header">
                        <div class="tp__app-main__header-left">
                        {
                            if let Some(ref title) = services.config.ui_config.app_title {
                                 html! { <span class="tp__app-title">{ title }</span> }
                            } else {
                                html! { <AppIcon name="AppTitle" /> }
                            }
                        }
                        </div>
                        <div class={"tp__app-header-toolbar"}>
                            <LanguagePicker />
                            <ThemePicker theme={*theme} on_select={handle_theme_select.clone()} />
                            <IconButton name="Logout" icon="Logout" onclick={handle_logout.clone()} />
                        </div>
                    </div>
                    <div class="tp__app-main__body">
                        <NoAccess />
                    </div>
                </div>
            </div>
        };
    }

    let resolved_view = resolve_home_view(*view_visible, configured_fallback, home_access);
    let view_page = resolved_view.intern();
    html! {
        <ContextProvider<ConfigContext> context={config_context}>
        <ContextProvider<StatusContext> context={status_context}>
        <ContextProvider<PlaylistContext> context={playlist_context}>
        <DialogProvider>
            <ToastrView />
            <div class="tp__app">
               <BusyIndicator />
               { if setup_mode {
                    html! {}
                 } else {
                    html! { <Sidebar active_page={resolved_view} onview={handle_view_change_sidebar} show_streams_page={show_streams_page}/> }
                 }
               }

              <div class="tp__app-main">
                    <div class="tp__app-main__header tp__app-header">
                      <div class="tp__app-main__header-left">
                        {
                            if let Some(ref title) = services.config.ui_config.app_title {
                                 html! { <span class="tp__app-title">{ title }</span> }
                            } else {
                                html! { <AppIcon name="AppTitle" /> }
                            }
                        }
                        </div>
                        {
                            if setup_mode {
                                html! {}
                            } else {
                                html! {
                                    <div class={"tp__app-header-toolbar"}>
                                        <WebsocketStatus/>
                                        <LanguagePicker />
                                        <ThemePicker theme={*theme} on_select={handle_theme_select} />
                                        <IconButton name="Logout" icon="Logout" onclick={handle_logout} />
                                    </div>
                                }
                            }
                        }
                    </div>
                    <div class="tp__app-main__body">
                      { html_if!(setup_mode, { <ParticleFlowBackground /> }) }

                       { html_if!(setup_mode || can_read_config, {
                       <Panel class="tp__full-width" value={ViewType::Config.intern()} active={view_page.clone()}>
                          {
                              if setup_mode {
                                  html! { <Setup/> }
                              } else {
                                  html! {
                                      <ErrorBoundary name={translate.t("LABEL.CONFIG")}>
                                          <ConfigView/>
                                      </ErrorBoundary>
                                  }
                              }
                          }
                       </Panel>
                       })}
                       {
                            if setup_mode {
                                html! {}
                            } else {
                                html! {
                                    <>
                                       <Panel class="tp__full-width" value={ViewType::Dashboard.intern()} active={view_page.clone()}>
                                        <ErrorBoundary name={translate.t("LABEL.DASHBOARD")}>
                                          <DashboardView/>
                                        </ErrorBoundary>
                                       </Panel>
                                       { html_if!(can_read_system_status, {
                                       <Panel class="tp__full-width" value={ViewType::Stats.intern()} active={view_page.clone()}>
                                        <ErrorBoundary name={translate.t("LABEL.STATS")}>
                                          <StatsView show_streams={!show_streams_page}/>
                                        </ErrorBoundary>
                                       </Panel>
                                       })}
                                        { html_if!(show_streams_page && can_read_system_status, {
                                                   <Panel class="tp__full-width" value={ViewType::Streams.intern()} active={view_page.clone()}>
                                              <ErrorBoundary name={translate.t("LABEL.STREAMS")}>
                                                <StreamsView embedded={false}/>
                                              </ErrorBoundary>
                                            </Panel>
                                        })}
                                       { html_if!(can_read_downloads, {
                                       <Panel class="tp__full-width" value={ViewType::Downloads.intern()} active={view_page.clone()}>
                                         <ErrorBoundary name={translate.t("LABEL.DOWNLOADS")}>
                                           <DownloadsView/>
                                         </ErrorBoundary>
                                       </Panel>
                                       })}
                                        { html_if!(can_read_system_status, {
                                            <Panel class="tp__full-width" value={ViewType::StreamHistory.intern()} active={view_page.clone()}>
                                                <ErrorBoundary name={translate.t("LABEL.STREAM_HISTORY")}>
                                                  <StreamHistoryView/>
                                                </ErrorBoundary>
                                            </Panel>
                                        })}
                                       { html_if!(can_read_users, {
                                       <Panel class="tp__full-width" value={ViewType::Users.intern()} active={view_page.clone()}>
                                          <ErrorBoundary name={translate.t("LABEL.USER")}>
                                            <UserlistView/>
                                          </ErrorBoundary>
                                       </Panel>
                                       })}
                                       { html_if!(can_read_sources, {
                                       <Panel class="tp__full-width tp__full-height" value={ViewType::SourceEditor.intern()} active={view_page.clone()}>
                                          <ErrorBoundary name={translate.t("LABEL.SOURCE_EDITOR")}>
                                            <SourceEditor/>
                                          </ErrorBoundary>
                                       </Panel>
                                       })}
                                       { html_if!(can_write_playlist, {
                                       <Panel class="tp__full-width" value={ViewType::PlaylistUpdate.intern()} active={view_page.clone()}>
                                         <ErrorBoundary name={translate.t("LABEL.UPDATE")}>
                                           <PlaylistUpdateView/>
                                         </ErrorBoundary>
                                       </Panel>
                                       })}
                                       { html_if!(can_read_playlist, {
                                       <>
                                       <Panel class="tp__full-width" value={ViewType::PlaylistSettings.intern()} active={view_page.clone()}>
                                         <ErrorBoundary name={translate.t("LABEL.PLAYLIST")}>
                                           <PlaylistSettingsView/>
                                         </ErrorBoundary>
                                       </Panel>
                                       <Panel class="tp__full-width" value={ViewType::PlaylistExplorer.intern()} active={view_page.clone()}>
                                         <ErrorBoundary name={translate.t("LABEL.PLAYLIST_VIEWER")}>
                                           <PlaylistExplorerView/>
                                         </ErrorBoundary>
                                       </Panel>
                                       </>
                                       })}
                                       { html_if!(can_read_epg, {
                                       <Panel class="tp__full-width" value={ViewType::PlaylistEpg.intern()} active={view_page.clone()}>
                                         <ErrorBoundary name={translate.t("LABEL.PLAYLIST_EPG")}>
                                           <EpgView/>
                                         </ErrorBoundary>
                                       </Panel>
                                       })}
                                       { html_if!(is_admin, {
                                           <Panel class="tp__full-width" value={ViewType::Rbac.intern()} active={view_page}>
                                               <ErrorBoundary name={translate.t("LABEL.RBAC")}>
                                                 <RbacView />
                                               </ErrorBoundary>
                                           </Panel>
                                       })}
                                    </>
                                }
                            }
                       }
                    </div>
              </div>
            </div>
        </DialogProvider>
        </ContextProvider<PlaylistContext>>
        </ContextProvider<StatusContext>>
        </ContextProvider<ConfigContext>>
    }
}

#[cfg(test)]
mod tests {
    use super::{is_allowed_home_view, resolve_home_view};
    use crate::model::ViewType;

    fn full_access() -> super::HomeViewAccess {
        super::HomeViewAccess {
            setup_mode: false,
            show_streams_page: true,
            can_read_system_status: true,
            can_read_config: true,
            can_read_users: true,
            can_read_sources: true,
            can_write_playlist: true,
            can_read_playlist: true,
            can_read_epg: true,
            can_read_downloads: true,
            is_admin: true,
        }
    }

    #[test]
    fn resolve_home_view_falls_back_for_unknown_hash() {
        let resolved = resolve_home_view(None, ViewType::Dashboard, full_access());

        assert_eq!(resolved, ViewType::Dashboard);
    }

    #[test]
    fn resolve_home_view_rejects_disallowed_hash() {
        let resolved = resolve_home_view(
            Some(ViewType::Users),
            ViewType::Dashboard,
            super::HomeViewAccess {
                can_read_users: false,
                can_read_config: false,
                can_read_sources: false,
                can_write_playlist: false,
                can_read_playlist: false,
                can_read_epg: false,
                can_read_downloads: false,
                can_read_system_status: false,
                is_admin: false,
                ..full_access()
            },
        );

        assert_eq!(resolved, ViewType::Dashboard);
    }

    #[test]
    fn resolve_home_view_maps_hidden_streams_page_to_stats() {
        let resolved = resolve_home_view(
            Some(ViewType::Streams),
            ViewType::Dashboard,
            super::HomeViewAccess {
                show_streams_page: false,
                can_read_users: false,
                can_read_config: false,
                can_read_sources: false,
                can_write_playlist: false,
                can_read_playlist: false,
                can_read_epg: false,
                can_read_downloads: false,
                is_admin: false,
                ..full_access()
            },
        );

        assert_eq!(resolved, ViewType::Stats);
    }

    #[test]
    fn is_allowed_home_view_allows_only_config_in_setup_mode() {
        let access = super::HomeViewAccess { setup_mode: true, ..full_access() };
        assert!(is_allowed_home_view(ViewType::Config, access));
        assert!(!is_allowed_home_view(ViewType::Dashboard, access));
    }
}
