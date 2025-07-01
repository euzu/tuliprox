use yew::prelude::*;
use crate::app::components::{IconButton, Sidebar, DashboardView};
use crate::model::ViewType;
use crate::hooks::use_service_context;

#[function_component]
pub fn Home() -> Html {
    let services = use_service_context();
    let app_title = services.config.ui_config.app_title.as_ref().map_or("tuliprox", |v| v.as_str());

    let view_visible = use_state(|| ViewType::Dashboard);

    let handle_logout = {
        let services_ctx = services.clone();
        Callback::from(move |_| services_ctx.auth.logout())
    };

    let handle_view_change = {
        let view_vis = view_visible.clone();
        Callback::from(move |view| view_vis.set(view))
    };

    //<div class={"app-header__toolbar"}><select onchange={handle_language} defaultValue={i18next.language}>{services.config().getUiConfig().languages.map(l => <option key={l} value={l}>{l}</option>)}</select></div>
    // <div class={"app-header__toolbar"}><button data-tooltip={preferencesVisible ? "LABEL.PLAYLIST_BROWSER" : "LABEL.CONFIGURATION"} onClick={handlePreferences}>{getIconByName(preferencesVisible ? "Live" : "Config")}</button></div>

    html! {
        <div class="app">
           <Sidebar onview={handle_view_change}/>

          <div class="app-main">
                <div class="app-main__header app-header">
                    {app_title}
                    <div class={"app-header-toolbar"}>
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
                <div class="app-main__body">
                   {
                        match *view_visible {
                            ViewType::Dashboard => html! { <DashboardView/> },
                            _ => html! { <div>{"Unknown view"}</div> },
                        }
                    }
                </div>
          </div>
        </div>
    }
}
