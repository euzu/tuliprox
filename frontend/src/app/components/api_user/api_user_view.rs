use crate::app::components::api_user::playlist::ApiUserPlaylist;
use crate::app::components::loading_indicator::BusyIndicator;
use crate::app::components::theme::Theme;
use crate::app::components::{AppIcon, IconButton, ToastrView, WebsocketStatus};
use crate::hooks::use_service_context;
use crate::provider::DialogProvider;
use yew::use_state;
use yew::{function_component, html, Callback, Html};

#[function_component]
pub fn ApiUserView() -> Html {
    let services = use_service_context();
    let theme = use_state(Theme::get_current_theme);

    let handle_theme_switch = {
        let set_theme = theme.clone();
        Callback::from(move |_| {
            let new_theme = if *set_theme == Theme::Dark {
                Theme::Bright
            } else {
                Theme::Dark
            };
            new_theme.switch_theme();
            set_theme.set(new_theme);
        })
    };

    let handle_logout = {
        let services_ctx = services.clone();
        Callback::from(move |_| services_ctx.auth.logout())
    };

    html! {
        <DialogProvider>
            <ToastrView />
            <div class="tp__app">
               <BusyIndicator />

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
                        <ApiUserPlaylist />
                    </div>
              </div>
            </div>
        </DialogProvider>
    }
}
