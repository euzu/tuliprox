use crate::{
    app::components::{
        is_plugin_route, menu_item::MenuItem, svg_icon::AppIcon, use_plugin_ui, CollapsePanel, IconButton,
        PluginMenuEntry,
    },
    hooks::{use_icon_context, use_service_context},
    i18n::use_translation,
    model::ViewType,
    utils::{get_local_storage_item, html_if, set_local_storage_item, MOBILE_BREAKPOINT_PX},
};
use shared::model::permission::Permission;
use std::str::FromStr;
use wasm_bindgen::{closure::Closure, JsCast};
use web_sys::window;
use yew::prelude::*;
use yew_hooks::use_mount;

const TP_SIDEBAR_COLLAPSED_KEY: &str = "tp-sidebar-collapsed";

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
    #[prop_or(true)]
    pub show_stream_history: bool,
    #[prop_or(true)]
    pub show_recording: bool,
    #[prop_or_default]
    pub active_page: ViewType,
    /// Called with the `plugin/{id}/{element}` route of a plugin menu entry.
    #[prop_or_default]
    pub onplugin: Callback<String>,
    /// The route of the plugin page that is open, if one is.
    #[prop_or_default]
    pub active_plugin: Option<String>,
}

fn sidebar_variant_class(collapsed: CollapseState) -> &'static str {
    if matches!(collapsed, CollapseState::AutoCollapsed | CollapseState::ManualCollapsed) {
        "collapsed"
    } else {
        "expanded"
    }
}

fn resolved_sidebar_state(collapsed: CollapseState, is_mobile: bool) -> CollapseState {
    if is_mobile {
        match collapsed {
            CollapseState::ManualExpanded => CollapseState::ManualExpanded,
            CollapseState::AutoCollapsed | CollapseState::AutoExpanded | CollapseState::ManualCollapsed => {
                CollapseState::AutoCollapsed
            }
        }
    } else {
        collapsed
    }
}

fn is_sidebar_expanded(collapsed: CollapseState) -> bool {
    matches!(collapsed, CollapseState::AutoExpanded | CollapseState::ManualExpanded)
}

/// Should the DVR entries appear in the navigation?
///
/// Requires both recording read permission and recording enabled in config.
pub const fn show_recording_nav(has_recording_read: bool, recording_enabled: bool) -> bool {
    has_recording_read && recording_enabled
}

pub const fn show_stream_history_nav(has_system_read: bool, stream_history_enabled: bool) -> bool {
    has_system_read && stream_history_enabled
}

/// The expanded sidebar entry of a plugin menu contribution; greyed out when the plugin is not enabled.
fn render_plugin_item(
    entry: &PluginMenuEntry,
    active_plugin: Option<&str>,
    onclick: &Callback<(String, MouseEvent)>,
) -> Html {
    let class = classes!(
        (active_plugin == Some(entry.route.as_str())).then_some("active"),
        (!entry.available).then_some("tp__plugin--disabled")
    )
    .to_string();
    html! {
        <MenuItem key={entry.route.clone()} class={class} icon={entry.icon.clone()} name={entry.route.clone()}
            label={entry.title.clone()} onclick={onclick}></MenuItem>
    }
}

/// The collapsed sidebar entry of a plugin menu contribution.
fn render_plugin_icon(
    entry: &PluginMenuEntry,
    active_plugin: Option<&str>,
    onclick: &Callback<(String, MouseEvent)>,
) -> Html {
    let class = classes!(
        "tp__app-sidebar-menu--plugin",
        (active_plugin == Some(entry.route.as_str())).then_some("active"),
        (!entry.available).then_some("tp__plugin--disabled")
    )
    .to_string();
    html! {
        <IconButton key={entry.route.clone()} class={class} icon={entry.icon.clone()} name={entry.route.clone()}
            hint={entry.title.clone()} aria_label={entry.title.clone()} onclick={onclick}></IconButton>
    }
}

#[component]
pub fn Sidebar(props: &SidebarProps) -> Html {
    let services = use_service_context();
    let translate = use_translation();
    let collapsed = use_state(|| match get_local_storage_item(TP_SIDEBAR_COLLAPSED_KEY).as_deref() {
        Some("collapsed") => CollapseState::ManualCollapsed,
        Some("expanded") => CollapseState::ManualExpanded,
        _ => CollapseState::AutoExpanded,
    });
    let is_mobile = use_state(|| false);
    let resolved_state = resolved_sidebar_state(*collapsed, *is_mobile);
    let plugin_ui = use_plugin_ui();
    let icons = use_icon_context();
    let plugin_entries = plugin_ui.map(|ui| ui.menu_entries(&icons)).unwrap_or_default();
    let is_active = |view: ViewType| props.active_plugin.is_none() && props.active_page == view;
    let show_recording =
        show_recording_nav(services.auth.has_permission(Permission::RecordingRead), props.show_recording);
    let show_stream_history =
        show_stream_history_nav(services.auth.has_permission(Permission::SystemRead), props.show_stream_history);

    let handle_menu_click = {
        let viewchange = props.onview.clone();
        let pluginchange = props.onplugin.clone();
        let is_mobile = is_mobile.clone();
        let collapsed = collapsed.clone();
        Callback::from(move |(name, _): (String, _)| {
            let navigated = if is_plugin_route(&name) {
                pluginchange.emit(name);
                true
            } else if let Ok(view_type) = ViewType::from_str(&name) {
                viewchange.emit(view_type);
                true
            } else {
                false
            };
            if navigated && *is_mobile {
                set_local_storage_item(TP_SIDEBAR_COLLAPSED_KEY, "collapsed");
                collapsed.set(CollapseState::ManualCollapsed);
            }
        })
    };

    let set_sidebar_state = {
        let collapsed = collapsed.clone();
        Callback::from(move |next: CollapseState| {
            let stored = match next {
                CollapseState::ManualCollapsed => "collapsed",
                _ => "expanded",
            };
            set_local_storage_item(TP_SIDEBAR_COLLAPSED_KEY, stored);
            collapsed.set(next);
        })
    };
    let toggle_sidebar = {
        let set_sidebar_state = set_sidebar_state.clone();
        let collapsed = collapsed.clone();
        Callback::from(move |_| {
            let current = *collapsed;
            let next = match current {
                CollapseState::AutoCollapsed | CollapseState::ManualCollapsed => CollapseState::ManualExpanded,
                CollapseState::AutoExpanded | CollapseState::ManualExpanded => CollapseState::ManualCollapsed,
            };
            set_sidebar_state.emit(next);
        })
    };
    let close_sidebar = {
        let set_sidebar_state = set_sidebar_state.clone();
        Callback::from(move |_: MouseEvent| set_sidebar_state.emit(CollapseState::ManualCollapsed))
    };

    let check_sidebar_state = {
        let collapsed = collapsed.clone();
        let is_mobile = is_mobile.clone();

        Callback::from(move |()| {
            let Some(window) = window() else {
                return;
            };

            if let Ok(inner_width) = window.inner_width() {
                let mobile_view = inner_width.as_f64().unwrap_or(0.0) < MOBILE_BREAKPOINT_PX;

                match *collapsed {
                    CollapseState::AutoExpanded => {
                        if mobile_view {
                            collapsed.set(CollapseState::AutoCollapsed);
                        }
                    }
                    CollapseState::AutoCollapsed => {
                        if !mobile_view {
                            collapsed.set(CollapseState::AutoExpanded);
                        }
                    }
                    CollapseState::ManualExpanded | CollapseState::ManualCollapsed => {
                        // do nothing
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

            let window = window();
            if let Some(window) = window.as_ref() {
                if window.add_event_listener_with_callback("resize", closure.as_ref().unchecked_ref()).is_ok() {
                    // Save Closure so it can be cleaned up later
                    *callback_handle.borrow_mut() = Some(closure);
                }
            }

            // Cleanup
            move || {
                if let Some(window) = window {
                    if let Some(closure) = callback_handle.borrow_mut().take() {
                        let _ = window.remove_event_listener_with_callback("resize", closure.as_ref().unchecked_ref());
                    }
                }
            }
        });
    }

    let render_expanded = || {
        let auth = &services.auth;
        html! {
          <div class="tp__app-sidebar__content">
            <MenuItem class={if is_active(ViewType::Dashboard) { "active" } else {""}} icon="DashboardOutline" name={ViewType::Dashboard.to_string()} label={translate.t("LABEL.DASHBOARD")} onclick={&handle_menu_click}></MenuItem>
            {html_if!(auth.has_permission(Permission::SystemRead), {
                <MenuItem class={if is_active(ViewType::Stats) { "active" } else {""}} icon="Stats" name={ViewType::Stats.to_string()} label={translate.t("LABEL.STATS")} onclick={&handle_menu_click}></MenuItem>
            })}
            {html_if!(props.show_streams_page && auth.has_permission(Permission::SystemRead), {
                <MenuItem class={if is_active(ViewType::Streams) { "active" } else {""}} icon="Streams" name={ViewType::Streams.to_string()} label={translate.t("LABEL.STREAMS")} onclick={&handle_menu_click}></MenuItem>
             })}
            {html_if!(auth.has_permission(Permission::SystemRead), {
                <MenuItem class={if is_active(ViewType::Logs) { "active" } else {""}} icon="Log" name={ViewType::Logs.to_string()} label={translate.t("LABEL.LOGS")} onclick={&handle_menu_click}></MenuItem>
            })}
            {html_if!(auth.has_permission(Permission::SystemRead), {
                <MenuItem class={if is_active(ViewType::Plugins) { "active" } else {""}} icon="Nodes" name={ViewType::Plugins.to_string()} label={translate.t("LABEL.PLUGINS")} onclick={&handle_menu_click}></MenuItem>
            })}
            {html_if!(show_stream_history, {
                <MenuItem class={if is_active(ViewType::StreamHistory) { "active" } else {""}} icon="Clock" name={ViewType::StreamHistory.to_string()} label={translate.t("LABEL.STREAM_HISTORY")} onclick={&handle_menu_click}></MenuItem>
            })}
            {html_if!(
                auth.has_any_permissions(
                    Permission::ConfigRead
                        | Permission::SourceRead
                        | Permission::UserRead
                        | Permission::UserWrite
                        | Permission::PlaylistRead,
                ),
                {
                    <CollapsePanel title={translate.t("LABEL.SETTINGS")}>
                      {html_if!(auth.is_admin(), {
                        <MenuItem class={if is_active(ViewType::Rbac) { "active" } else {""}} icon="Shield" name={ViewType::Rbac.to_string()} label={translate.t("LABEL.RBAC")} onclick={&handle_menu_click}></MenuItem>
                      })}
                      {html_if!(auth.has_permission(Permission::ConfigRead), {
                          <MenuItem class={if is_active(ViewType::Config) { "active" } else {""}} icon="Config" name={ViewType::Config.to_string()} label={translate.t("LABEL.CONFIG")}  onclick={&handle_menu_click}></MenuItem>
                      })}
                      {html_if!(auth.has_any_permissions(Permission::UserRead | Permission::UserWrite), {
                          <MenuItem class={if is_active(ViewType::Users) { "active" } else {""}} icon="UserOutline" name={ViewType::Users.to_string()} label={translate.t("LABEL.USER")} onclick={&handle_menu_click}></MenuItem>
                      })}
                      {html_if!(auth.has_permission(Permission::ConfigRead), {
                          <MenuItem class={if is_active(ViewType::Plans) { "active" } else {""}} icon="Group" name={ViewType::Plans.to_string()} label={translate.t("LABEL.PLANS")} onclick={&handle_menu_click}></MenuItem>
                      })}
                      {html_if!(auth.has_permission(Permission::SourceRead), {
                          <MenuItem class={if is_active(ViewType::SourceEditor) { "active" } else {""}} icon="SourceEditor" name={ViewType::SourceEditor.to_string()} label={translate.t("LABEL.SOURCE_EDITOR")}  onclick={&handle_menu_click}></MenuItem>
                      })}
                      {html_if!(auth.has_any_permissions(Permission::SourceRead | Permission::PlaylistRead), {
                          <MenuItem class={if is_active(ViewType::PlaylistSettings) { "active" } else {""}} icon="PlayArrowOutline" name={ViewType::PlaylistSettings.to_string()} label={translate.t("LABEL.PLAYLIST")} onclick={&handle_menu_click}></MenuItem>
                      })}
                    </CollapsePanel>
                }
            )}
            {html_if!(
                auth.has_any_permissions(Permission::PlaylistRead | Permission::PlaylistWrite | Permission::EpgRead),
                {
                    <CollapsePanel title={translate.t("LABEL.PLAYLIST")}>
                      {html_if!(auth.has_permission(Permission::PlaylistWrite), {
                          <MenuItem class={if is_active(ViewType::PlaylistUpdate) { "active" } else {""}} icon="Refresh" name={ViewType::PlaylistUpdate.to_string()} label={translate.t("LABEL.UPDATE")} onclick={&handle_menu_click}></MenuItem>
                      })}
                      {html_if!(auth.has_permission(Permission::PlaylistRead), {
                          <MenuItem class={if is_active(ViewType::PlaylistStats) { "active" } else {""}} icon="Stats" name={ViewType::PlaylistStats.to_string()} label={translate.t("LABEL.PLAYLIST_STATS")} onclick={&handle_menu_click}></MenuItem>
                      })}
                      {html_if!(auth.has_permission(Permission::PlaylistRead), {
                          <>
                          <MenuItem class={if is_active(ViewType::PlaylistExplorer) { "active" } else {""}} icon="Live" name={ViewType::PlaylistExplorer.to_string()} label={translate.t("LABEL.PLAYLIST_VIEWER")} onclick={&handle_menu_click}></MenuItem>
                          </>
                      })}
                      {html_if!(auth.has_permission(Permission::EpgRead), {
                          <MenuItem class={if is_active(ViewType::PlaylistEpg) { "active" } else {""}} icon="Epg" name={ViewType::PlaylistEpg.to_string()} label={translate.t("LABEL.PLAYLIST_EPG")} onclick={&handle_menu_click}></MenuItem>
                      })}
                    </CollapsePanel>
                }
            )}
            {html_if!(
                show_recording,
                {
                    <CollapsePanel title={translate.t("LABEL.RECORDING")}>
                      <MenuItem class={if is_active(ViewType::RecordingLibrary) { "active" } else {""}} icon="VideoLibrary" name={ViewType::RecordingLibrary.to_string()} label={translate.t("LABEL.RECORDING_LIBRARY")} onclick={&handle_menu_click}></MenuItem>
                      <MenuItem class={if is_active(ViewType::RecordingRules) { "active" } else {""}} icon="Calendar" name={ViewType::RecordingRules.to_string()} label={translate.t("LABEL.RECORDING_RULES")} onclick={&handle_menu_click}></MenuItem>
                    </CollapsePanel>
                }
            )}
            {html_if!(!plugin_entries.is_empty(), {
                <CollapsePanel title={translate.t("LABEL.PLUGINS")}>
                  { for plugin_entries.iter().map(|entry| render_plugin_item(entry, props.active_plugin.as_deref(), &handle_menu_click)) }
                </CollapsePanel>
            })}
          </div>
        }
    };

    let render_collapsed = || {
        let auth = &services.auth;
        html! {
          <div class="tp__app-sidebar__content">
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::Dashboard, if is_active(ViewType::Dashboard) { " active" } else {""})}  icon="DashboardOutline" name={ViewType::Dashboard.to_string()} hint={translate.t("LABEL.DASHBOARD")} aria_label={translate.t("LABEL.DASHBOARD")} onclick={&handle_menu_click}></IconButton>
            {html_if!(auth.has_permission(Permission::SystemRead), {
                <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::Stats, if is_active(ViewType::Stats) { " active" } else {""})} icon="Stats" name={ViewType::Stats.to_string()} hint={translate.t("LABEL.STATS")} aria_label={translate.t("LABEL.STATS")} onclick={&handle_menu_click}></IconButton>
            })}
            {html_if!(props.show_streams_page && auth.has_permission(Permission::SystemRead), {
             <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::Streams, if is_active(ViewType::Streams) { " active" } else {""})} icon="Streams" name={ViewType::Streams.to_string()} hint={translate.t("LABEL.STREAMS")} aria_label={translate.t("LABEL.STREAMS")} onclick={&handle_menu_click}></IconButton>
            })}
            {html_if!(auth.has_permission(Permission::SystemRead), {
                <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::Logs, if is_active(ViewType::Logs) { " active" } else {""})} icon="Log" name={ViewType::Logs.to_string()} hint={translate.t("LABEL.LOGS")} aria_label={translate.t("LABEL.LOGS")} onclick={&handle_menu_click}></IconButton>
            })}
            {html_if!(auth.has_permission(Permission::SystemRead), {
                <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::Plugins, if is_active(ViewType::Plugins) { " active" } else {""})} icon="Nodes" name={ViewType::Plugins.to_string()} hint={translate.t("LABEL.PLUGINS")} aria_label={translate.t("LABEL.PLUGINS")} onclick={&handle_menu_click}></IconButton>
            })}
            {html_if!(show_stream_history, {
                <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::StreamHistory, if is_active(ViewType::StreamHistory) { " active" } else {""})} icon="Clock" name={ViewType::StreamHistory.to_string()} hint={translate.t("LABEL.STREAM_HISTORY")} aria_label={translate.t("LABEL.STREAM_HISTORY")} onclick={&handle_menu_click}></IconButton>
            })}
            {html_if!(
                auth.has_any_permissions(
                    Permission::ConfigRead
                        | Permission::SourceRead
                        | Permission::UserRead
                        | Permission::UserWrite
                        | Permission::PlaylistRead,
                ),
                {
                    <span class="tp__app-sidebar__content-space"></span>
                }
            )}
            {html_if!(auth.is_admin(), {
                <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::Rbac, if is_active(ViewType::Rbac) { " active" } else {""})} icon="Shield" name={ViewType::Rbac.to_string()} hint={translate.t("LABEL.RBAC")} aria_label={translate.t("LABEL.RBAC")} onclick={&handle_menu_click}></IconButton>
            })}
            {html_if!(auth.has_permission(Permission::ConfigRead), {
                <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::Config, if is_active(ViewType::Config) { " active" } else {""})} icon="Config" name={ViewType::Config.to_string()} hint={translate.t("LABEL.CONFIG")} aria_label={translate.t("LABEL.CONFIG")} onclick={&handle_menu_click}></IconButton>
            })}
            {html_if!(auth.has_any_permissions(Permission::UserRead | Permission::UserWrite), {
                <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::Users, if is_active(ViewType::Users) { " active" } else {""})} icon="UserOutline" name={ViewType::Users.to_string()} hint={translate.t("LABEL.USER")} aria_label={translate.t("LABEL.USER")} onclick={&handle_menu_click}></IconButton>
            })}
            {html_if!(auth.has_permission(Permission::ConfigRead), {
                <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::Plans, if is_active(ViewType::Plans) { " active" } else {""})} icon="Group" name={ViewType::Plans.to_string()} hint={translate.t("LABEL.PLANS")} aria_label={translate.t("LABEL.PLANS")} onclick={&handle_menu_click}></IconButton>
            })}
            {html_if!(auth.has_permission(Permission::SourceRead), {
                <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::SourceEditor, if is_active(ViewType::SourceEditor) { " active" } else {""})} icon="SourceEditor" name={ViewType::SourceEditor.to_string()} hint={translate.t("LABEL.SOURCE_EDITOR")} aria_label={translate.t("LABEL.SOURCE_EDITOR")} onclick={&handle_menu_click}></IconButton>
            })}
            {html_if!(auth.has_any_permissions(Permission::SourceRead | Permission::PlaylistRead), {
                <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::PlaylistSettings, if is_active(ViewType::PlaylistSettings) { " active" } else {""})} icon="PlayArrowOutline" name={ViewType::PlaylistSettings.to_string()} hint={translate.t("LABEL.PLAYLIST")} aria_label={translate.t("LABEL.PLAYLIST")} onclick={&handle_menu_click}></IconButton>
            })}
            {html_if!(
                auth.has_any_permissions(Permission::PlaylistRead | Permission::PlaylistWrite | Permission::EpgRead),
                {
                    <span class="tp__app-sidebar__content-space"></span>
                }
            )}
            {html_if!(auth.has_permission(Permission::PlaylistWrite), {
                <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::PlaylistUpdate, if is_active(ViewType::PlaylistUpdate) { " active" } else {""})} icon="Refresh" name={ViewType::PlaylistUpdate.to_string()} hint={translate.t("LABEL.UPDATE")} aria_label={translate.t("LABEL.UPDATE")} onclick={&handle_menu_click}></IconButton>
            })}
            {html_if!(auth.has_permission(Permission::PlaylistRead), {
                <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::PlaylistStats, if is_active(ViewType::PlaylistStats) { " active" } else {""})} icon="Stats" name={ViewType::PlaylistStats.to_string()} hint={translate.t("LABEL.PLAYLIST_STATS")} aria_label={translate.t("LABEL.PLAYLIST_STATS")} onclick={&handle_menu_click}></IconButton>
            })}
            {html_if!(auth.has_permission(Permission::PlaylistRead), {
               <>
               <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::PlaylistExplorer, if is_active(ViewType::PlaylistExplorer) { " active" } else {""})} icon="Live" name={ViewType::PlaylistExplorer.to_string()} hint={translate.t("LABEL.PLAYLIST_VIEWER")} aria_label={translate.t("LABEL.PLAYLIST_VIEWER")} onclick={&handle_menu_click}></IconButton>
               </>
            })}
            {html_if!(auth.has_permission(Permission::EpgRead), {
                <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::PlaylistEpg, if is_active(ViewType::PlaylistEpg) { " active" } else {""})} icon="Epg" name={ViewType::PlaylistEpg.to_string()} hint={translate.t("LABEL.PLAYLIST_EPG")} aria_label={translate.t("LABEL.PLAYLIST_EPG")} onclick={&handle_menu_click}></IconButton>
            })}
            {html_if!(show_recording, {
                <>
                <span class="tp__app-sidebar__content-space"></span>
                <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::RecordingLibrary, if is_active(ViewType::RecordingLibrary) { " active" } else {""})} icon="VideoLibrary" name={ViewType::RecordingLibrary.to_string()} hint={translate.t("LABEL.RECORDING_LIBRARY")} aria_label={translate.t("LABEL.RECORDING_LIBRARY")} onclick={&handle_menu_click}></IconButton>
                <IconButton class={format!("tp__app-sidebar-menu--{}{}", ViewType::RecordingRules, if is_active(ViewType::RecordingRules) { " active" } else {""})} icon="Calendar" name={ViewType::RecordingRules.to_string()} hint={translate.t("LABEL.RECORDING_RULES")} aria_label={translate.t("LABEL.RECORDING_RULES")} onclick={&handle_menu_click}></IconButton>
                </>
            })}
            {html_if!(!plugin_entries.is_empty(), {
                <>
                <span class="tp__app-sidebar__content-space"></span>
                { for plugin_entries.iter().map(|entry| render_plugin_icon(entry, props.active_plugin.as_deref(), &handle_menu_click)) }
                </>
            })}
          </div>
        }
    };

    html! {
        <>
            { html_if!(*is_mobile && is_sidebar_expanded(resolved_state), {
                <button
                    type="button"
                    class="tp__app-sidebar__backdrop"
                    onclick={close_sidebar}
                    aria-label={translate.t("LABEL.TOGGLE_SIDEBAR")}
                />
            }) }
            <div class={classes!(
                "tp__app-sidebar",
                sidebar_variant_class(resolved_state),
                if *is_mobile { "mobile" } else { "" }
            )}>
                <div class="tp__app-sidebar__header tp__app-header">
                  {
                    if is_sidebar_expanded(resolved_state) {
                      html! {
                       <span class="tp__app-header__logo">
                       {
                          if let Some(logo) = services.config.ui_config.app_logo.as_ref() {
                            let alt = format!("{} logo", services.config.ui_config.app_title.as_deref().unwrap_or("tuliprox"));
                            html! { <img src={logo.clone()} alt={alt}/> }
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
                  <IconButton
                    name="ToggleSidebar"
                    icon={"Sidebar"}
                    onclick={toggle_sidebar}
                    aria_expanded={is_sidebar_expanded(resolved_state)}
                    aria_label={translate.t("LABEL.TOGGLE_SIDEBAR")}
                  />
                </div>
                <div class="tp__app-sidebar__scroll">
                    {
                        if is_sidebar_expanded(resolved_state) {
                            render_expanded()
                        } else {
                            render_collapsed()
                        }
                    }
                </div>
            </div>
        </>
    }
}

#[cfg(test)]
mod tests {
    use super::{is_sidebar_expanded, resolved_sidebar_state, sidebar_variant_class, CollapseState};
    use crate::app::components::show_recording_nav;

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

    #[test]
    fn sidebar_resolves_mobile_view_to_collapsed() {
        assert_eq!(resolved_sidebar_state(CollapseState::ManualExpanded, true), CollapseState::ManualExpanded);
        assert_eq!(resolved_sidebar_state(CollapseState::AutoExpanded, true), CollapseState::AutoCollapsed);
        assert_eq!(resolved_sidebar_state(CollapseState::ManualCollapsed, true), CollapseState::AutoCollapsed);
    }

    #[test]
    fn sidebar_expanded_helper_reports_only_expanded_variants() {
        assert!(is_sidebar_expanded(CollapseState::AutoExpanded));
        assert!(is_sidebar_expanded(CollapseState::ManualExpanded));
        assert!(!is_sidebar_expanded(CollapseState::AutoCollapsed));
        assert!(!is_sidebar_expanded(CollapseState::ManualCollapsed));
    }

    #[test]
    fn recording_nav_requires_both_recording_read_and_recording_enabled() {
        assert!(show_recording_nav(true, true));
        assert!(!show_recording_nav(true, false));
        assert!(!show_recording_nav(false, true));
        assert!(!show_recording_nav(false, false));
    }

    #[test]
    fn stream_history_nav_requires_both_system_read_and_stream_history_enabled() {
        assert!(super::show_stream_history_nav(true, true));
        assert!(!super::show_stream_history_nav(true, false));
        assert!(!super::show_stream_history_nav(false, true));
        assert!(!super::show_stream_history_nav(false, false));
    }
}
