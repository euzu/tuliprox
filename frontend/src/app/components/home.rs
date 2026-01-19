use crate::app::components::{AppIcon, DashboardView, EpgView, IconButton, InputRow, Panel, PlaylistEditorView, PlaylistExplorerView, PlaylistUpdateView, Sidebar, SourceEditor, StatsView, StreamsView, ToastrView, UserlistView, WebsocketStatus};
use crate::app::context::{ConfigContext, PlaylistContext, StatusContext};
use crate::hooks::{use_server_status, use_service_context};
use crate::model::{EventMessage, ViewType};
use shared::model::{AppConfigDto, ConfigInputDto, LibraryScanSummaryStatus, PlaylistUpdateState, StatusCheck, SystemInfo};
use std::collections::HashMap;
use std::future;
use std::rc::Rc;
use std::sync::Arc;
use yew::prelude::*;
use yew::suspense::use_future;
use yew_i18n::use_translation;
use crate::app::components::config::{ConfigView};
use crate::app::components::loading_indicator::{BusyIndicator};
use crate::provider::DialogProvider;
use crate::services::{ToastCloseMode, ToastOptions};
use crate::app::components::theme::Theme;

#[function_component]
pub fn Home() -> Html {
    let services = use_service_context();
    let translate = use_translation();
    let config = use_state(|| None::<Rc<AppConfigDto>>);
    let status = use_state(|| None::<Rc<StatusCheck>>);
    let system_info = use_state(|| None::<Rc<SystemInfo>>);
    let view_visible = use_state(|| ViewType::Dashboard);
    let theme = use_state(Theme::get_current_theme);

    let handle_theme_switch = {
        let set_theme = theme.clone();
        Callback::from(move |_| {
            let new_theme = if *set_theme == Theme::Dark { Theme::Bright } else { Theme::Dark };
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
            let subid = services_ctx.event.subscribe(move |msg| {
                match msg {
                    EventMessage::Unauthorized => {
                        services_ctx_clone.auth.logout()
                    },
                    EventMessage::ServerError(msg) => {
                        services_ctx_clone.toastr.error(msg);
                    },
                    EventMessage::ConfigChange(config_type) => {
                        services_ctx_clone.toastr.warning_with_options(
                            format!("{}: {config_type}", translate_clone.t("MESSAGES.CONFIG_CHANGED")),
                            ToastOptions { close_mode: ToastCloseMode::Manual });
                    },
                    EventMessage::PlaylistUpdate(update_state) => {
                        match update_state {
                          PlaylistUpdateState::Success => services_ctx_clone.toastr.success(translate_clone.t("MESSAGES.PLAYLIST_UPDATE.SUCCESS_FINISH")),
                          PlaylistUpdateState::Failure => services_ctx_clone.toastr.error(translate_clone.t("MESSAGES.PLAYLIST_UPDATE.FAIL_FINISH")),
                        }
                    },
                    EventMessage::LibraryScanProgress(summary) => {
                        match summary.status {
                            LibraryScanSummaryStatus::Success => services_ctx_clone.toastr.success(summary.message),
                            LibraryScanSummaryStatus::Error => services_ctx_clone.toastr.error(summary.message),
                        }
                    },
                    _=> {}
                }
            });
            move || services_ctx.event.unsubscribe(subid)
        });
    }

    let _ = use_server_status(status.clone(), system_info.clone());

    {
        // first register for config update
        let services_ctx = services.clone();
        let config_state = config.clone();
        let _ = use_future(|| async move {
            services_ctx.config.config_subscribe(
                &mut |cfg| {
                    config_state.set(cfg.clone());
                    future::ready(())
                }
            ).await
        });
    }

    {
        let services_ctx = services.clone();
        let _ = use_future(|| async move {
            let _cfg = services_ctx.config.get_server_config().await;
        });
    }

    let sources = use_memo(config.clone(), |config_ctx| {
        if let Some(cfg) = config_ctx.as_ref() {
            let mut sources = vec![];
            // Create a map for a faster lookup of global inputs by name
            let inputs_map: HashMap<Arc<str>, &ConfigInputDto> = cfg.sources.inputs.iter().map(|i| (i.name.clone(), i)).collect();

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

    let config_context = ConfigContext {
        config: (*config).clone(),
    };

    let status_context = StatusContext {
        status: (*status).clone(),
        system_info: (*system_info).clone(),
    };
    let playlist_context = PlaylistContext {
        sources: sources.clone(),
    };

    let handle_view_change = {
        let view_vis = view_visible.clone();
        Callback::from(move |view| view_vis.set(view))
    };

    //<div class={"app-header__toolbar"}><select onchange={handle_language} defaultValue={i18next.language}>{services.config().getUiConfig().languages.map(l => <option key={l} value={l}>{l}</option>)}</select></div>

    html! {
        <ContextProvider<ConfigContext> context={config_context}>
        <ContextProvider<StatusContext> context={status_context}>
        <ContextProvider<PlaylistContext> context={playlist_context}>
        <DialogProvider>
            <ToastrView />
            <div class="tp__app">
               <BusyIndicator />
               <Sidebar onview={handle_view_change}/>

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
                            <WebsocketStatus/>
                            <IconButton name="Theme" icon={if *theme == Theme::Bright {"Moon"} else {"Sun"}} onclick={handle_theme_switch} />
                            <IconButton name="Logout" icon="Logout" onclick={handle_logout} />
                        </div>
                    </div>
                    <div class="tp__app-main__body">
                       <Panel class="tp__full-width" value={ViewType::Dashboard.to_string()} active={view_visible.to_string()}>
                        <DashboardView/>
                       </Panel>
                       <Panel class="tp__full-width" value={ViewType::Stats.to_string()} active={view_visible.to_string()}>
                        <StatsView/>
                       </Panel>
                       <Panel class="tp__full-width" value={ViewType::Streams.to_string()} active={view_visible.to_string()}>
                        <StreamsView/>
                       </Panel>
                       <Panel class="tp__full-width" value={ViewType::Users.to_string()} active={view_visible.to_string()}>
                          <UserlistView/>
                       </Panel>
                       <Panel class="tp__full-width" value={ViewType::Config.to_string()} active={view_visible.to_string()}>
                          <ConfigView/>
                       </Panel>
                       <Panel class="tp__full-width tp__full-height" value={ViewType::SourceEditor.to_string()} active={view_visible.to_string()}>
                          <SourceEditor/>
                       </Panel>
                       <Panel class="tp__full-width" value={ViewType::PlaylistUpdate.to_string()} active={view_visible.to_string()}>
                        <PlaylistUpdateView/>
                       </Panel>
                       <Panel class="tp__full-width" value={ViewType::PlaylistEditor.to_string()} active={view_visible.to_string()}>
                        <PlaylistEditorView/>
                       </Panel>
                       <Panel class="tp__full-width" value={ViewType::PlaylistExplorer.to_string()} active={view_visible.to_string()}>
                        <PlaylistExplorerView/>
                       </Panel>
                       <Panel class="tp__full-width" value={ViewType::PlaylistEpg.to_string()} active={view_visible.to_string()}>
                        <EpgView/>
                       </Panel>
                    </div>
              </div>
            </div>
        </DialogProvider>
        </ContextProvider<PlaylistContext>>
        </ContextProvider<StatusContext>>
        </ContextProvider<ConfigContext>>
    }
}
