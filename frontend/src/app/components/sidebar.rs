use crate::{
    app::components::{menu_item::MenuItem, svg_icon::AppIcon, CollapsePanel, IconButton},
    hooks::use_service_context,
    i18n::use_translation,
    model::ViewType,
    utils::html_if,
};
use shared::model::permission::Permission;
use std::str::FromStr;
use wasm_bindgen::{closure::Closure, JsCast};
use web_sys::window;
use yew::prelude::*;
use yew_hooks::use_mount;

#[derive(Debug, Copy, Clone, PartialEq)]
enum CollapseState {
    AutoCollapsed,
    AutoExpanded,
    ManualCollapsed,
    ManualExpanded,
}

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct SidebarProps {
    #[prop_or_default]
    pub onview: Callback<ViewType>,
    #[prop_or_default]
    pub show_streams_page: bool,
}

fn sidebar_variant_class(collapsed: CollapseState) -> &'static str {
    if matches!(collapsed, CollapseState::AutoCollapsed | CollapseState::ManualCollapsed) {
        "collapsed"
    } else {
        "expanded"
    }
}

#[component]
pub fn Sidebar(props: &SidebarProps) -> Html {
    let services = use_service_context();
    let translate = use_translation();
    let collapsed = use_state(|| CollapseState::AutoExpanded);
    let is_mobile = use_state(|| false);
    let active_menu = use_state(|| ViewType::Dashboard);

    let handle_menu_click = {
        let viewchange = props.onview.clone();
        let active_menu = active_menu.clone();
        Callback::from(move |(name, _): (String, _)| {
            if let Ok(view_type) = ViewType::from_str(&name) {
                active_menu.set(view_type);
                viewchange.emit(view_type);
            }
        })
    };

    let toggle_sidebar = {
        let collapsed = collapsed.clone();
        Callback::from(move |_| {
            let current = *collapsed;
            let next = match current {
                CollapseState::AutoCollapsed | CollapseState::ManualCollapsed => CollapseState::ManualExpanded,
                CollapseState::AutoExpanded | CollapseState::ManualExpanded => CollapseState::ManualCollapsed,
            };
            if current != next {
                collapsed.set(next);
            }
        })
    };

    let check_sidebar_state = {
        let collapsed = collapsed.clone();
        let is_mobile = is_mobile.clone();

        Callback::from(move |_| {
            let window = window().expect("no global window");

            if let Ok(inner_width) = window.inner_width() {
                let mobile_view = inner_width.as_f64().unwrap_or(0.0) < 720.0;

                match *collapsed {
                    CollapseState::AutoExpanded | CollapseState::ManualExpanded => {
                        if mobile_view && matches!(*collapsed, CollapseState::AutoExpanded) {
                            collapsed.set(CollapseState::AutoCollapsed);
                        }
                    }
                    CollapseState::ManualCollapsed => {
                        // do nothing
                    }
                    CollapseState::AutoCollapsed => {
                        if !mobile_view {
                            collapsed.set(CollapseState::AutoExpanded);
                        }
                    }
                }
                is_mobile.set(mobile_view);
            }
        })
    };

    {
        let check_sidebar_state = check_sidebar_state.clone();
        use_mount(move || check_sidebar_state.emit(()));
    }

    let callback_handle = use_mut_ref(|| None::<Closure<dyn FnMut(Event)>>);

    {
        let callback_handle = callback_handle.clone();
        let check_sidebar_state = check_sidebar_state.clone();

        use_effect_with(check_sidebar_state, move |check_sidebar| {
            let check_sidebar = check_sidebar.clone();
            let closure = Closure::<dyn FnMut(Event)>::wrap(Box::new(move |_event: Event| check_sidebar.emit(())));

            let window = window().expect("no global window");
            window
                .add_event_listener_with_callback("resize", closure.as_ref().unchecked_ref())
                .expect("could not add event listener");

            // Save Closure so it can be cleaned up later
            *callback_handle.borrow_mut() = Some(closure);

            // Cleanup
            move || {
                if let Some(closure) = callback_handle.borrow_mut().take() {
                    let _ = window.remove_event_listener_with_callback("resize", closure.as_ref().unchecked_ref());
                }
            }
        });
    }

    let render_expanded = || {
        let auth = &services.auth;
        html! {
          <div class="tp__app-sidebar__content">
            <MenuItem class={if *active_menu == ViewType::Dashboard { "active" } else {""}} icon="DashboardOutline" name={ViewType::Dashboard.to_string()} label={translate.t("LABEL.DASHBOARD")} onclick={&handle_menu_click}></MenuItem>
            {html_if!(auth.has_permission(Permission::SystemRead), {
                <MenuItem class={if *active_menu == ViewType::Stats { "active" } else {""}} icon="Stats" name={ViewType::Stats.to_string()} label={translate.t("LABEL.STATS")} onclick={&handle_menu_click}></MenuItem>
            })}
            {html_if!(props.show_streams_page && auth.has_permission(Permission::SystemRead), {
                <MenuItem class={if *active_menu == ViewType::Streams { "active" } else {""}} icon="Streams" name={ViewType::Streams.to_string()} label={translate.t("LABEL.STREAMS")} onclick={&handle_menu_click}></MenuItem>
             })}
            {html_if!(
                auth.has_any_permissions(Permission::ConfigRead | Permission::SourceRead | Permission::UserRead),
                {
                    <CollapsePanel title={translate.t("LABEL.SETTINGS")}>
                      {html_if!(auth.is_admin(), {
                        <MenuItem class={if *active_menu == ViewType::Rbac { "active" } else {""}} icon="Shield" name={ViewType::Rbac.to_string()} label={translate.t("LABEL.RBAC")} onclick={&handle_menu_click}></MenuItem>
                      })}
                      {html_if!(auth.has_permission(Permission::ConfigRead), {
                          <MenuItem class={if *active_menu == ViewType::Config { "active" } else {""}} icon="Config" name={ViewType::Config.to_string()} label={translate.t("LABEL.CONFIG")}  onclick={&handle_menu_click}></MenuItem>
                      })}
                      {html_if!(auth.has_permission(Permission::UserRead), {
                          <MenuItem class={if *active_menu == ViewType::Users { "active" } else {""}} icon="UserOutline" name={ViewType::Users.to_string()} label={translate.t("LABEL.USER")} onclick={&handle_menu_click}></MenuItem>
                      })}
                      {html_if!(auth.has_permission(Permission::SourceRead), {
                          <MenuItem class={if *active_menu == ViewType::SourceEditor { "active" } else {""}} icon="SourceEditor" name={ViewType::SourceEditor.to_string()} label={translate.t("LABEL.SOURCE_EDITOR")}  onclick={&handle_menu_click}></MenuItem>
                      })}
                    </CollapsePanel>
                }
            )}
            {html_if!(
                auth.has_any_permissions(Permission::PlaylistRead | Permission::PlaylistWrite | Permission::EpgRead),
                {
                    <CollapsePanel title={translate.t("LABEL.PLAYLIST")}>
                      {html_if!(auth.has_permission(Permission::PlaylistWrite), {
                          <MenuItem class={if *active_menu == ViewType::PlaylistUpdate { "active" } else {""}} icon="Refresh" name={ViewType::PlaylistUpdate.to_string()} label={translate.t("LABEL.UPDATE")} onclick={&handle_menu_click}></MenuItem>
                      })}
                      {html_if!(auth.has_permission(Permission::PlaylistRead), {
                          <>
                            <MenuItem class={if *active_menu == ViewType::PlaylistSettings { "active" } else {""}} icon="PlayArrowOutline" name={ViewType::PlaylistSettings.to_string()} label={translate.t("LABEL.PLAYLIST")} onclick={&handle_menu_click}></MenuItem>
                            <MenuItem class={if *active_menu == ViewType::PlaylistExplorer { "active" } else {""}} icon="Live" name={ViewType::PlaylistExplorer.to_string()} label={translate.t("LABEL.PLAYLIST_VIEWER")} onclick={&handle_menu_click}></MenuItem>
                          </>
                      })}
                      {html_if!(auth.has_permission(Permission::EpgRead), {
                          <MenuItem class={if *active_menu == ViewType::PlaylistEpg { "active" } else {""}} icon="Epg" name={ViewType::PlaylistEpg.to_string()} label={translate.t("LABEL.PLAYLIST_EPG")} onclick={&handle_menu_click}></MenuItem>
                      })}
                    </CollapsePanel>
                }
            )}
          </div>
        }
    };

    let render_collapsed = || {
        let auth = &services.auth;
        html! {
          <div class="tp__app-sidebar__content">
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::Dashboard, if *active_menu == ViewType::Dashboard { " active" } else {""})}  icon="DashboardOutline" name={ViewType::Dashboard.to_string()} onclick={&handle_menu_click}></IconButton>
            {html_if!(auth.has_permission(Permission::SystemRead), {
                <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::Stats, if *active_menu == ViewType::Stats { " active" } else {""})} icon="Stats" name={ViewType::Stats.to_string()} onclick={&handle_menu_click}></IconButton>
            })}
            {html_if!(props.show_streams_page && auth.has_permission(Permission::SystemRead), {
             <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::Streams, if *active_menu == ViewType::Streams { " active" } else {""})} icon="Streams" name={ViewType::Streams.to_string()} onclick={&handle_menu_click}></IconButton>
            })}
            {html_if!(
                auth.has_any_permissions(Permission::ConfigRead | Permission::SourceRead | Permission::UserRead),
                {
                    <span class="tp__app-sidebar__content-space"></span>
                }
            )}
            {html_if!(auth.is_admin(), {
                <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::Rbac, if *active_menu == ViewType::Rbac { " active" } else {""})} icon="Shield" name={ViewType::Rbac.to_string()} onclick={&handle_menu_click}></IconButton>
            })}
            {html_if!(auth.has_permission(Permission::ConfigRead), {
                <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::Config, if *active_menu == ViewType::Config { " active" } else {""})} icon="Config" name={ViewType::Config.to_string()} onclick={&handle_menu_click}></IconButton>
            })}
            {html_if!(auth.has_permission(Permission::UserRead), {
                <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::Users, if *active_menu == ViewType::Users { " active" } else {""})} icon="UserOutline" name={ViewType::Users.to_string()} onclick={&handle_menu_click}></IconButton>
            })}
            {html_if!(auth.has_permission(Permission::SourceRead), {
                <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::SourceEditor, if *active_menu == ViewType::SourceEditor { " active" } else {""})} icon="SourceEditor" name={ViewType::SourceEditor.to_string()} onclick={&handle_menu_click}></IconButton>
            })}
            {html_if!(
                auth.has_any_permissions(Permission::PlaylistRead | Permission::PlaylistWrite | Permission::EpgRead),
                {
                    <span class="tp__app-sidebar__content-space"></span>
                }
            )}
            {html_if!(auth.has_permission(Permission::PlaylistWrite), {
                <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::PlaylistUpdate, if *active_menu == ViewType::PlaylistUpdate { " active" } else {""})} icon="Refresh" name={ViewType::PlaylistUpdate.to_string()} onclick={&handle_menu_click}></IconButton>
            })}
            {html_if!(auth.has_permission(Permission::PlaylistRead), {
                <>
                  <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::PlaylistSettings, if *active_menu == ViewType::PlaylistSettings { " active" } else {""})} icon="PlayArrowOutline" name={ViewType::PlaylistSettings.to_string()} onclick={&handle_menu_click}></IconButton>
                  <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::PlaylistExplorer, if *active_menu == ViewType::PlaylistExplorer { " active" } else {""})} icon="Live" name={ViewType::PlaylistExplorer.to_string()} onclick={&handle_menu_click}></IconButton>
                </>
            })}
            {html_if!(auth.has_permission(Permission::EpgRead), {
                <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::PlaylistEpg, if *active_menu == ViewType::PlaylistEpg { " active" } else {""})} icon="Epg" name={ViewType::PlaylistEpg.to_string()} onclick={&handle_menu_click}></IconButton>
            })}
          </div>
        }
    };

    html! {
        <div class={classes!(
            "tp__app-sidebar",
            sidebar_variant_class(*collapsed),
            if *is_mobile { "mobile" } else { "" }
        )}>
            <div class="tp__app-sidebar__header tp__app-header">
              {
                if matches!(*collapsed, CollapseState::AutoExpanded | CollapseState::ManualExpanded) && !*is_mobile {
                  html! {
                   <span class="tp__app-header__logo">
                   {
                      if let Some(logo) = services.config.ui_config.app_logo.as_ref() {
                        html! { <img src={logo.to_string()} alt="logo"/> }
                      } else {
                        html! { <AppIcon name="Logo"/> }
                      }
                   }
                   </span>
                  }
                } else {
                  html! {}
                }
              }
              <IconButton name="ToggleSidebar" icon={"Sidebar"} onclick={toggle_sidebar} />
            </div>
            <div class="tp__app-sidebar__scroll">
                {
                    if *is_mobile || matches!(*collapsed, CollapseState::AutoCollapsed | CollapseState::ManualCollapsed) {
                        render_collapsed()
                    } else {
                        render_expanded()
                    }
                }
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::{sidebar_variant_class, CollapseState};

    #[test]
    fn sidebar_variant_class_reports_collapsed_variants() {
        assert_eq!(sidebar_variant_class(CollapseState::AutoCollapsed), "collapsed");
        assert_eq!(sidebar_variant_class(CollapseState::ManualCollapsed), "collapsed");
    }

    #[test]
    fn sidebar_variant_class_reports_expanded_variants() {
        assert_eq!(sidebar_variant_class(CollapseState::AutoExpanded), "expanded");
        assert_eq!(sidebar_variant_class(CollapseState::ManualExpanded), "expanded");
    }
}
