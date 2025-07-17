use std::future;
use std::rc::Rc;
use gloo_timers::callback::Interval;
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;
use yew::suspense::use_future;
use shared::model::{AppConfigDto, StatusCheck};
use crate::app::components::{IconButton, Sidebar, DashboardView, PlaylistView, Panel, UserlistView, StatsView};
use crate::app::context::{ConfigContext, StatusContext};
use crate::model::ViewType;
use crate::hooks::use_service_context;

#[function_component]
pub fn Home() -> Html {
    let services = use_service_context();
    let app_title = services.config.ui_config.app_title.as_ref().map_or("tuliprox", |v| v.as_str());
    let config = use_state(|| None::<Rc<AppConfigDto>>);
    let status = use_state(|| None::<Rc<StatusCheck>>);

    let view_visible = use_state(|| ViewType::Users);

    let handle_logout = {
        let services_ctx = services.clone();
        Callback::from(move |_| services_ctx.auth.logout())
    };

    let handle_view_change = {
        let view_vis = view_visible.clone();
        Callback::from(move |view| view_vis.set(view))
    };

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

    {
        let services_ctx = services.clone();
        let status_signal = status.clone();

        use_effect_with((), move |_| {
            let fetch_status = {
                let status = status_signal.clone();
                let services_ctx = services_ctx.clone();
                move || {
                    let status = status.clone();
                    let services_ctx = services_ctx.clone();
                    spawn_local(async move {
                        status.set(services_ctx.status.get_server_status().await.ok());
                    });
                }
            };

            fetch_status();
            // all 5 seconds
            let interval = Interval::new(5000, move || {
                fetch_status();
            });

            // Cleanup function
            || drop(interval)
        });
    }


    let config_context = ConfigContext {
            config: (*config).clone(),
    };

    let status_context = StatusContext {
        status: (*status).clone(),
    };

    //<div class={"app-header__toolbar"}><select onchange={handle_language} defaultValue={i18next.language}>{services.config().getUiConfig().languages.map(l => <option key={l} value={l}>{l}</option>)}</select></div>
    // <div class={"app-header__toolbar"}><button data-tooltip={preferencesVisible ? "LABEL.PLAYLIST_BROWSER" : "LABEL.CONFIGURATION"} onClick={handlePreferences}>{getIconByName(preferencesVisible ? "Live" : "Config")}</button></div>

    html! {
        <ContextProvider<ConfigContext> context={config_context}>
        <ContextProvider<StatusContext> context={status_context}>
            <div class="tp__app">
               <Sidebar onview={handle_view_change}/>

              <div class="tp__app-main">
                    <div class="tp__app-main__header tp__app-header">
                        {app_title}
                        <div class={"tp__app-header-toolbar"}>
                            // <select onchange={handle_language} defaultValue={i18next.language}>{
                            //     services.config().getUiConfig().languages.map(l => <option key={l} value={l}>{l}</option>)
                            // }</select>
                           // <button data-tooltip={ if *preferences_visible { "LABEL.PLAYLIST_BROWSER" } else { "LABEL.CONFIGURATION" }}
                           //     onclick={handle_view_change}>
                           //          { if *preferences_visible {
                           //                  html! { <AppIcon name="Live" /> }
                           //              } else {
                           //                  html! { <AppIcon name="Config" /> }
                           //              }
                           //          }
                           //   </button>
                           //
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
                       <Panel class="tp__full-width" value={ViewType::Playlists.to_string()} active={view_visible.to_string()}>
                        <PlaylistView/>
                       </Panel>
                       <Panel class="tp__full-width" value={ViewType::Users.to_string()} active={view_visible.to_string()}>
                          <UserlistView/>
                       </Panel>
                    </div>
              </div>
            </div>
        </ContextProvider<StatusContext>>
        </ContextProvider<ConfigContext>>
    }
}
