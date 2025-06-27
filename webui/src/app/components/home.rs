use crate::app::components::preferences::Preferences;
use crate::app::components::svg_icon::AppIcon;
use crate::hooks::use_service_context;
use yew::prelude::*;

#[function_component]
pub fn Home() -> Html {
    let services = use_service_context();
    let app_title = services.config.ui_config.app_title.as_ref().map_or("tuliprox", |v| v.as_str());

    let app_logo = if let Some(logo) = services.config.ui_config.app_logo.as_ref() {
        html! { <img src={logo.to_string()} alt="logo"/> }
    } else {
        html! { <AppIcon name="Logo"  width={"48"} height={"48"}/> }
    };

    let preferences_visible = use_state(|| true);

    let handle_logout = {
        let services_ctx = services.clone();
        Callback::from(move |_| services_ctx.auth.logout())
    };
    let handle_view_change = {
        let prefs_vis = preferences_visible.clone();
        Callback::from(move |_| prefs_vis.set(!&*prefs_vis))
    };

    //<div class={"app-header__toolbar"}><select onchange={handle_language} defaultValue={i18next.language}>{services.config().getUiConfig().languages.map(l => <option key={l} value={l}>{l}</option>)}</select></div>
    // <div class={"app-header__toolbar"}><button data-tooltip={preferencesVisible ? "LABEL.PLAYLIST_BROWSER" : "LABEL.CONFIGURATION"} onClick={handlePreferences}>{getIconByName(preferencesVisible ? "Live" : "Config")}</button></div>

    html! {
        <div class="app">
            <div class="app-header">
                <div class="app-header__caption">
                    <span class="app-header__logo">{app_logo}</span>
                    {app_title}
                </div>
                <div class={"app-header__toolbar"}>
                    // <select onchange={handle_language} defaultValue={i18next.language}>{
                    //     services.config().getUiConfig().languages.map(l => <option key={l} value={l}>{l}</option>)
                    // }</select>
                   <button data-tooltip={ if *preferences_visible { "LABEL.PLAYLIST_BROWSER" } else { "LABEL.CONFIGURATION" }}
                       onclick={handle_view_change}>
                            { if *preferences_visible {
                                    html! { <AppIcon name="Live" /> }
                                } else {
                                    html! { <AppIcon name="Config" /> }
                                }
                            }
                     </button>
                    <button data-tooltip="LABEL.LOGOUT" onclick={handle_logout}>
                        <AppIcon name="Logout"/>
                    </button>
                </div>
            </div>
            <div class="app-main">
            {  if *preferences_visible {
                   html! { <div class="app-content"><Preferences /></div> }
                } else {
                html! { <div>{"Plylist"}</div> }
                    //html! { <PlaylistBrowser config={server_config} /> }
                }
            }
            </div>
        </div>
    }
}
