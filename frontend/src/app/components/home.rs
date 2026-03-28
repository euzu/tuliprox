use crate::{
    app::{
        components::{
            config::ConfigView, loading_indicator::BusyIndicator, theme::Theme, AppIcon, DashboardView, EpgView,
            IconButton, InputRow, NoAccess, Panel, ParticleFlowBackground, PlaylistExplorerView, PlaylistSettingsView,
            PlaylistUpdateView, RbacView, Setup, Sidebar, SourceEditor, StatsView, StreamsView, ThemePicker,
            ToastrView, UserlistView, WebsocketStatus,
        },
        context::{ConfigContext, PlaylistContext, StatusContext},
    },
    hooks::{use_server_status, use_service_context},
    html_if,
    i18n::use_translation,
    model::{EventMessage, ViewType},
    provider::DialogProvider,
    services::{ToastCloseMode, ToastOptions},
};
use shared::model::{
    permission::{Permission, PERM_ALL},
    ApiProxyConfigDto, AppConfigDto, ConfigInputDto, LibraryScanSummaryStatus, PlaylistUpdateState, StatusCheck,
    SystemInfo,
};
use std::{collections::HashMap, future, rc::Rc, sync::Arc};
use yew::{prelude::*, suspense::use_future};

#[component]
pub fn Home() -> Html {
    let services = use_service_context();
    let setup_mode = services.config.ui_config.setup_mode;
    let translate = use_translation();
    let config = use_state(|| None::<Rc<AppConfigDto>>);
    let api_proxy_config = use_state(|| None::<Rc<ApiProxyConfigDto>>);
    let status = use_state(|| None::<Rc<StatusCheck>>);
    let system_info = use_state(|| None::<Rc<SystemInfo>>);
    let view_visible = use_state(|| if setup_mode { ViewType::Config } else { ViewType::Dashboard });
    let theme = use_state(Theme::get_current_theme);

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
        if let Some(cfg) = config_ctx.as_ref() {
            let mut sources = vec![];
            // Create a map for a faster lookup of global inputs by name
            let inputs_map: HashMap<Arc<str>, &ConfigInputDto> =
                cfg.sources.inputs.iter().map(|i| (i.name.clone(), i)).collect();

            for source in &cfg.sources.sources {
                let mut inputs = vec![];
                for input_name in &source.inputs {
                    if let Some(input_cfg) = inputs_map.get(input_name) {
                        let input = Rc::new((*input_cfg).clone());
                        inputs.push(Rc::new(InputRow::Input(Rc::clone(&input))));
                        if let Some(aliases) = input_cfg.aliases.as_ref() {
                            for alias in aliases {
                                inputs.push(Rc::new(InputRow::Alias(Rc::new(alias.clone()), Rc::clone(&input))));
                            }
                        }
                    } else {
                        log::error!("Input '{}' not found in global inputs", input_name);
                    }
                }
                let mut targets = vec![];
                for target in &source.targets {
                    targets.push(Rc::new(target.clone()));
                }
                sources.push((inputs, targets));
            }
            Some(Rc::new(sources))
        } else {
            None
        }
    });

    let config_context = ConfigContext { config: (*config).clone(), api_proxy: (*api_proxy_config).clone() };

    let status_context = StatusContext { status: (*status).clone(), system_info: (*system_info).clone() };
    let playlist_context = PlaylistContext { sources: sources.clone() };

    let handle_view_change = {
        let view_vis = view_visible.clone();
        Callback::from(move |view| view_vis.set(view))
    };

    //<div class={"app-header__toolbar"}><select onchange={handle_language} defaultValue={i18next.language}>{services.config().getUiConfig().languages.map(l => <option key={l} value={l}>{l}</option>)}</select></div>

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

    // combine_views_stats_streams=true means embed streams in stats (no separate page), so show_streams_page = !combine_views_stats_streams.
    // The default unwrap_or(true) correctly preserves backward compatibility (separate pages by default).
    let show_streams_page = config_context
        .config
        .as_ref()
        .and_then(|app_cfg| app_cfg.config.web_ui.as_ref())
        .map(|web_ui| !web_ui.combine_views_stats_streams)
        .unwrap_or(true);

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
                    html! { <Sidebar onview={handle_view_change} show_streams_page={show_streams_page}/> }
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
                       <Panel class="tp__full-width" value={ViewType::Config.to_string()} active={view_visible.to_string()}>
                          {
                              if setup_mode {
                                  html! { <Setup/> }
                              } else {
                                  html! { <ConfigView/> }
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
                                       <Panel class="tp__full-width" value={ViewType::Dashboard.to_string()} active={view_visible.to_string()}>
                                        <DashboardView/>
                                       </Panel>
                                       { html_if!(can_read_system_status, {
                                       <Panel class="tp__full-width" value={ViewType::Stats.to_string()} active={view_visible.to_string()}>
                                        <StatsView show_streams={!show_streams_page}/>
                                       </Panel>
                                       })}
                                        { html_if!(show_streams_page && can_read_system_status, {
                                                   <Panel class="tp__full-width" value={ViewType::Streams.to_string()} active={view_visible.to_string()}>
                                              <StreamsView embedded={false}/>
                                            </Panel>
                                        })}
                                       { html_if!(can_read_users, {
                                       <Panel class="tp__full-width" value={ViewType::Users.to_string()} active={view_visible.to_string()}>
                                          <UserlistView/>
                                       </Panel>
                                       })}
                                       { html_if!(can_read_sources, {
                                       <Panel class="tp__full-width tp__full-height" value={ViewType::SourceEditor.to_string()} active={view_visible.to_string()}>
                                          <SourceEditor/>
                                       </Panel>
                                       })}
                                       { html_if!(can_write_playlist, {
                                       <Panel class="tp__full-width" value={ViewType::PlaylistUpdate.to_string()} active={view_visible.to_string()}>
                                         <PlaylistUpdateView/>
                                       </Panel>
                                       })}
                                       { html_if!(can_read_playlist, {
                                       <>
                                       <Panel class="tp__full-width" value={ViewType::PlaylistSettings.to_string()} active={view_visible.to_string()}>
                                         <PlaylistSettingsView/>
                                       </Panel>
                                       <Panel class="tp__full-width" value={ViewType::PlaylistExplorer.to_string()} active={view_visible.to_string()}>
                                         <PlaylistExplorerView/>
                                       </Panel>
                                       </>
                                       })}
                                       { html_if!(can_read_epg, {
                                       <Panel class="tp__full-width" value={ViewType::PlaylistEpg.to_string()} active={view_visible.to_string()}>
                                         <EpgView/>
                                       </Panel>
                                       })}
                                       { html_if!(is_admin, {
                                           <Panel class="tp__full-width" value={ViewType::Rbac.to_string()} active={view_visible.to_string()}>
                                               <RbacView />
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
