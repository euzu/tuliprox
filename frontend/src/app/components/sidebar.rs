use std::str::FromStr;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use web_sys::window;
use yew::prelude::*;
use yew_hooks::use_mount;
use yew_i18n::use_translation;

use crate::app::components::menu_item::MenuItem;
use crate::app::components::svg_icon::AppIcon;
use crate::app::components::{CollapsePanel, IconButton};
use crate::hooks::use_service_context;
use crate::model::ViewType;
use crate::utils::html_if;

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
}

#[function_component]
pub fn Sidebar(props: &SidebarProps) -> Html {
    let services = use_service_context();
    let translate = use_translation();
    let collapsed = use_state(|| CollapseState::AutoExpanded);
    let block_sidebar_toggle = use_state(|| false);
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
        let block_sidebar_toggle = block_sidebar_toggle.clone();
        Callback::from(move |_| {
            if !*block_sidebar_toggle {
                let current = *collapsed;
                let next = match current {
                    CollapseState::AutoCollapsed | CollapseState::ManualCollapsed => {
                        CollapseState::ManualExpanded
                    }
                    CollapseState::AutoExpanded | CollapseState::ManualExpanded => {
                        CollapseState::ManualCollapsed
                    }
                };
                if current != next {
                    collapsed.set(next);
                }
            }
        })
    };

    let check_sidebar_state = {
        let collapsed = collapsed.clone();
        let block_sidebar_toggle = block_sidebar_toggle.clone();

        Callback::from(move |_| {
            let window = window().expect("no global window");

            if let Ok(inner_width) = window.inner_width() {
                let is_mobile = inner_width.as_f64().unwrap_or(0.0) < 720.0;

                match *collapsed {
                    CollapseState::AutoExpanded | CollapseState::ManualExpanded => {
                        if is_mobile {
                            collapsed.set(CollapseState::AutoCollapsed);
                        }
                    }
                    CollapseState::ManualCollapsed => {
                        // do nothing
                    }
                    CollapseState::AutoCollapsed => {
                        if !is_mobile {
                            collapsed.set(CollapseState::AutoExpanded);
                        }
                    }
                }
                block_sidebar_toggle.set(is_mobile);
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
            let closure = Closure::<dyn FnMut(Event)>::wrap(Box::new(move |_event: Event| {
                check_sidebar.emit(())
            }));

            let window = window().expect("no global window");
            window
                .add_event_listener_with_callback("resize", closure.as_ref().unchecked_ref())
                .expect("could not add event listener");

            // Save Closure so it can be cleaned up later
            *callback_handle.borrow_mut() = Some(closure);

            // Cleanup
            move || {
                if let Some(closure) = callback_handle.borrow_mut().take() {
                    let _ = window.remove_event_listener_with_callback(
                        "resize",
                        closure.as_ref().unchecked_ref(),
                    );
                }
            }
        });
    }

    let render_expanded = || {
        html! {
          <div class="tp__app-sidebar__content">
            <MenuItem class={if *active_menu == ViewType::Dashboard { "active" } else {""}} icon="DashboardOutline" name={ViewType::Dashboard.to_string()} label={translate.t("LABEL.DASHBOARD")} onclick={&handle_menu_click}></MenuItem>
            <MenuItem class={if *active_menu == ViewType::Stats { "active" } else {""}} icon="Stats" name={ViewType::Stats.to_string()} label={translate.t("LABEL.STATS")} onclick={&handle_menu_click}></MenuItem>
            <MenuItem class={if *active_menu == ViewType::Streams { "active" } else {""}} icon="Streams" name={ViewType::Streams.to_string()} label={translate.t("LABEL.STREAMS")} onclick={&handle_menu_click}></MenuItem>
            <CollapsePanel title={translate.t("LABEL.SETTINGS")}>
              <MenuItem class={if *active_menu == ViewType::Config { "active" } else {""}} icon="Config" name={ViewType::Config.to_string()} label={translate.t("LABEL.CONFIG")}  onclick={&handle_menu_click}></MenuItem>
              <MenuItem class={if *active_menu == ViewType::Users { "active" } else {""}} icon="UserOutline" name={ViewType::Users.to_string()} label={translate.t("LABEL.USER")} onclick={&handle_menu_click}></MenuItem>
              <MenuItem class={if *active_menu == ViewType::SourceEditor { "active" } else {""}} icon="SourceEditor" name={ViewType::SourceEditor.to_string()} label={translate.t("LABEL.SOURCE_EDITOR")}  onclick={&handle_menu_click}></MenuItem>
            </CollapsePanel>
            <CollapsePanel title={translate.t("LABEL.PLAYLIST")}>
              <MenuItem class={if *active_menu == ViewType::PlaylistUpdate { "active" } else {""}} icon="Refresh" name={ViewType::PlaylistUpdate.to_string()} label={translate.t("LABEL.UPDATE")} onclick={&handle_menu_click}></MenuItem>
              <MenuItem class={if *active_menu == ViewType::PlaylistEditor { "active" } else {""}} icon="PlayArrowOutline" name={ViewType::PlaylistEditor.to_string()} label={translate.t("LABEL.PLAYLIST")} onclick={&handle_menu_click}></MenuItem>
              <MenuItem class={if *active_menu == ViewType::PlaylistExplorer { "active" } else {""}} icon="Live" name={ViewType::PlaylistExplorer.to_string()} label={translate.t("LABEL.PLAYLIST_VIEWER")} onclick={&handle_menu_click}></MenuItem>
              <MenuItem class={if *active_menu == ViewType::PlaylistEpg { "active" } else {""}} icon="Epg" name={ViewType::PlaylistEpg.to_string()} label={translate.t("LABEL.PLAYLIST_EPG")} onclick={&handle_menu_click}></MenuItem>
            </CollapsePanel>
          </div>
        }
    };

    let render_collapsed = || {
        html! {
          <div class="tp__app-sidebar__content">
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::Dashboard, if *active_menu == ViewType::Dashboard { " active" } else {""})}  icon="DashboardOutline" name={ViewType::Dashboard.to_string()} onclick={&handle_menu_click}></IconButton>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::Stats, if *active_menu == ViewType::Stats { " active" } else {""})} icon="Stats" name={ViewType::Stats.to_string()} onclick={&handle_menu_click}></IconButton>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::Streams, if *active_menu == ViewType::Streams { " active" } else {""})} icon="Streams" name={ViewType::Streams.to_string()} onclick={&handle_menu_click}></IconButton>
            <span class="tp__app-sidebar__content-space"></span>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::Config, if *active_menu == ViewType::Config { " active" } else {""})} icon="Config" name={ViewType::Config.to_string()} onclick={&handle_menu_click}></IconButton>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::Users, if *active_menu == ViewType::Users { " active" } else {""})} icon="UserOutline" name={ViewType::Users.to_string()} onclick={&handle_menu_click}></IconButton>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::SourceEditor, if *active_menu == ViewType::SourceEditor { " active" } else {""})} icon="SourceEditor" name={ViewType::SourceEditor.to_string()} onclick={&handle_menu_click}></IconButton>
            <span class="tp__app-sidebar__content-space"></span>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::PlaylistUpdate, if *active_menu == ViewType::PlaylistUpdate { " active" } else {""})} icon="Refresh" name={ViewType::PlaylistUpdate.to_string()} onclick={&handle_menu_click}></IconButton>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::PlaylistEditor, if *active_menu == ViewType::PlaylistEditor { " active" } else {""})} icon="PlayArrowOutline" name={ViewType::PlaylistEditor.to_string()} onclick={&handle_menu_click}></IconButton>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::PlaylistExplorer, if *active_menu == ViewType::PlaylistExplorer { " active" } else {""})} icon="Live" name={ViewType::PlaylistExplorer.to_string()} onclick={&handle_menu_click}></IconButton>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::PlaylistEpg, if *active_menu == ViewType::PlaylistEpg { " active" } else {""})} icon="Epg" name={ViewType::PlaylistEpg.to_string()} onclick={&handle_menu_click}></IconButton>
          </div>
        }
    };

    html! {
        <div class={classes!("tp__app-sidebar", if matches!(*collapsed, CollapseState::AutoCollapsed | CollapseState::ManualCollapsed) { "collapsed" } else { "expanded" })}>
            <div class="tp__app-sidebar__header tp__app-header">
              {
                if *block_sidebar_toggle || matches!(*collapsed, CollapseState::AutoExpanded | CollapseState::ManualExpanded) {
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
              { html_if!(
                  !*block_sidebar_toggle,
                  { <IconButton name="ToggleSidebar" icon={"Sidebar"} onclick={toggle_sidebar} /> }
                )}
            </div>
                {
                    if matches!(*collapsed, CollapseState::AutoCollapsed | CollapseState::ManualCollapsed) {
                        render_collapsed()
                    } else {
                        render_expanded()
                    }
                }
        </div>
    }
}
