use std::str::FromStr;
use crate::app::components::menu_item::MenuItem;
use crate::app::components::svg_icon::AppIcon;
use crate::app::components::{CollapsePanel};
use crate::hooks::use_service_context;
use yew::prelude::*;
use yew_i18n::use_translation;
use crate::model::ViewType;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct SidebarProps {
    #[prop_or_default]
    pub onview: Callback<ViewType>,
}

#[function_component]
pub fn Sidebar(props: &SidebarProps) -> Html {
    let services = use_service_context();
    let translate = use_translation();

    let app_logo = if let Some(logo) = services.config.ui_config.app_logo.as_ref() {
        html! { <img src={logo.to_string()} alt="logo"/> }
    } else {
        html! { <AppIcon name="Logo"  width={"48"} height={"48"}/> }
    };

    let handle_menu_click = {
        let viewchange = props.onview.clone();
        Callback::from(move |name:String| {
            if let Ok(view_type) = ViewType::from_str(&name) {
                viewchange.emit(view_type);
            }
        })
    };

    html! {
        <div class="tp__app-sidebar">
            <div class="tp__app-sidebar__header tp__app-header">
                <span class="tp__app-header__logo">{app_logo}</span>
                <AppIcon name={"ChevronLeft"}></AppIcon>
            </div>
            <div class="tp__app-sidebar__content">
                <MenuItem icon="DashboardOutline" name={ViewType::Dashboard.to_string()} label={translate.t("LABEL.DASHBOARD")}
                    onclick={&handle_menu_click}></MenuItem>
                <MenuItem icon="Stats" name={ViewType::Stats.to_string()} label={translate.t("LABEL.STATS")}
                    onclick={&handle_menu_click}></MenuItem>
                <CollapsePanel title={translate.t("LABEL.SETTINGS")}>
                    <MenuItem icon="UserOutline" name={ViewType::Users.to_string()} label={translate.t("LABEL.USER")}
                        onclick={&handle_menu_click}></MenuItem>
                </CollapsePanel>
                <CollapsePanel title={translate.t("LABEL.PLAYLIST")}>
                    <MenuItem icon="PlayArrowOutline" name={ViewType::Playlists.to_string()} label={translate.t("LABEL.PLAYLIST")}
                        onclick={&handle_menu_click}></MenuItem>
                </CollapsePanel>
            </div>
        </div>
    }
}
