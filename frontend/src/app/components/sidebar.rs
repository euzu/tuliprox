use crate::{
    app::components::{menu_item::MenuItem, svg_icon::AppIcon, CollapsePanel, IconButton},
    hooks::use_service_context,
    i18n::use_translation,
    model::ViewType,
    services::AuthService,
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

/// What the signed-in user may open; the one input of the navigation, whether it is shown expanded or collapsed.
#[derive(Debug, Clone, Copy, Default)]
struct NavAccess {
    admin: bool,
    system_read: bool,
    config_read: bool,
    source_read: bool,
    user_read: bool,
    user_write: bool,
    playlist_read: bool,
    playlist_write: bool,
    epg_read: bool,
    show_streams_page: bool,
    show_stream_history: bool,
    show_recording: bool,
}

impl NavAccess {
    fn new(auth: &AuthService, props: &SidebarProps) -> Self {
        Self {
            admin: auth.is_admin(),
            system_read: auth.has_permission(Permission::SystemRead),
            config_read: auth.has_permission(Permission::ConfigRead),
            source_read: auth.has_permission(Permission::SourceRead),
            user_read: auth.has_permission(Permission::UserRead),
            user_write: auth.has_permission(Permission::UserWrite),
            playlist_read: auth.has_permission(Permission::PlaylistRead),
            playlist_write: auth.has_permission(Permission::PlaylistWrite),
            epg_read: auth.has_permission(Permission::EpgRead),
            show_streams_page: props.show_streams_page,
            show_stream_history: show_stream_history_nav(
                auth.has_permission(Permission::SystemRead),
                props.show_stream_history,
            ),
            show_recording: show_recording_nav(auth.has_permission(Permission::RecordingRead), props.show_recording),
        }
    }
}

/// One navigation entry, shown with the same icon in the expanded and the collapsed sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NavEntry {
    view: ViewType,
    icon: &'static str,
    label_key: &'static str,
}

impl NavEntry {
    const fn new(view: ViewType, icon: &'static str, label_key: &'static str) -> Self { Self { view, icon, label_key } }
}

/// Entries the user may see. A titled section is a collapsible panel when expanded and is preceded by a spacer
/// when collapsed; the untitled section is always shown as is.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NavSection {
    title_key: Option<&'static str>,
    entries: Vec<NavEntry>,
}

fn nav_section(title_key: Option<&'static str>, entries: &[(bool, NavEntry)]) -> NavSection {
    NavSection { title_key, entries: entries.iter().filter_map(|&(visible, entry)| visible.then_some(entry)).collect() }
}

/// The navigation of the sidebar in display order: the sections and entries the user may see, titled sections
/// without any visible entry left out.
fn nav_sections(access: &NavAccess) -> Vec<NavSection> {
    let sections = [
        nav_section(
            None,
            &[
                (true, NavEntry::new(ViewType::Dashboard, "DashboardOutline", "LABEL.DASHBOARD")),
                (access.system_read, NavEntry::new(ViewType::Stats, "Stats", "LABEL.STATS")),
                (
                    access.show_streams_page && access.system_read,
                    NavEntry::new(ViewType::Streams, "Streams", "LABEL.STREAMS"),
                ),
                (access.system_read, NavEntry::new(ViewType::Logs, "Log", "LABEL.LOGS")),
                (access.show_stream_history, NavEntry::new(ViewType::StreamHistory, "Clock", "LABEL.STREAM_HISTORY")),
            ],
        ),
        nav_section(
            Some("LABEL.SETTINGS"),
            &[
                (access.admin, NavEntry::new(ViewType::Rbac, "Shield", "LABEL.RBAC")),
                (access.config_read, NavEntry::new(ViewType::Config, "Config", "LABEL.CONFIG")),
                (access.user_read || access.user_write, NavEntry::new(ViewType::Users, "UserOutline", "LABEL.USER")),
                (access.config_read, NavEntry::new(ViewType::Plans, "Group", "LABEL.PLANS")),
                (access.source_read, NavEntry::new(ViewType::SourceEditor, "SourceEditor", "LABEL.SOURCE_EDITOR")),
                (access.playlist_read, NavEntry::new(ViewType::FailoverGroups, "Failover", "LABEL.FAILOVER_GROUPS")),
            ],
        ),
        nav_section(
            Some("LABEL.PLAYLIST"),
            &[
                (access.playlist_write, NavEntry::new(ViewType::PlaylistUpdate, "Refresh", "LABEL.UPDATE")),
                (access.playlist_read, NavEntry::new(ViewType::PlaylistStats, "Stats", "LABEL.PLAYLIST_STATS")),
                (access.playlist_read, NavEntry::new(ViewType::PlaylistExplorer, "Live", "LABEL.PLAYLIST_VIEWER")),
                (access.epg_read, NavEntry::new(ViewType::PlaylistEpg, "Epg", "LABEL.PLAYLIST_EPG")),
                (
                    access.source_read || access.playlist_read,
                    NavEntry::new(ViewType::PlaylistSettings, "PlayArrowOutline", "LABEL.PLAYLIST_OVERVIEW"),
                ),
            ],
        ),
        nav_section(
            Some("LABEL.RECORDING"),
            &[
                (
                    access.show_recording,
                    NavEntry::new(ViewType::RecordingLibrary, "VideoLibrary", "LABEL.RECORDING_LIBRARY"),
                ),
                (access.show_recording, NavEntry::new(ViewType::RecordingRules, "Calendar", "LABEL.RECORDING_RULES")),
            ],
        ),
    ];
    sections.into_iter().filter(|section| section.title_key.is_none() || !section.entries.is_empty()).collect()
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
    let active_menu = props.active_page;
    let access = NavAccess::new(&services.auth, props);

    let handle_menu_click = {
        let viewchange = props.onview.clone();
        let is_mobile = is_mobile.clone();
        let collapsed = collapsed.clone();
        Callback::from(move |(name, _): (String, _)| {
            if let Ok(view_type) = ViewType::from_str(&name) {
                viewchange.emit(view_type);
                if *is_mobile {
                    set_local_storage_item(TP_SIDEBAR_COLLAPSED_KEY, "collapsed");
                    collapsed.set(CollapseState::ManualCollapsed);
                }
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

    let sections = nav_sections(&access);

    let render_expanded = || {
        html! {
          <div class="tp__app-sidebar__content">
            { for sections.iter().map(|section| {
                let items = html! {
                    { for section.entries.iter().map(|entry| html! {
                        <MenuItem class={if active_menu == entry.view { "active" } else { "" }} icon={entry.icon}
                            name={entry.view.to_string()} label={translate.t(entry.label_key)} onclick={&handle_menu_click}></MenuItem>
                    }) }
                };
                match section.title_key {
                    Some(title_key) => html! { <CollapsePanel title={translate.t(title_key)}>{ items }</CollapsePanel> },
                    None => items,
                }
            }) }
          </div>
        }
    };

    let render_collapsed = || {
        html! {
          <div class="tp__app-sidebar__content">
            { for sections.iter().map(|section| html! {
                <>
                    { html_if!(section.title_key.is_some(), { <span class="tp__app-sidebar__content-space"></span> }) }
                    { for section.entries.iter().map(|entry| {
                        let label = translate.t(entry.label_key);
                        html! {
                            <IconButton class={format!("tp__app-sidebar-menu--{}{}", entry.view, if active_menu == entry.view { " active" } else { "" })}
                                icon={entry.icon} name={entry.view.to_string()} hint={label.clone()} aria_label={label}
                                onclick={&handle_menu_click}></IconButton>
                        }
                    }) }
                </>
            }) }
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
    use super::{
        is_sidebar_expanded, nav_sections, resolved_sidebar_state, sidebar_variant_class, CollapseState, NavAccess,
        NavSection,
    };
    use crate::{app::components::show_recording_nav, model::ViewType};

    fn full_access() -> NavAccess {
        NavAccess {
            admin: true,
            system_read: true,
            config_read: true,
            source_read: true,
            user_read: true,
            user_write: true,
            playlist_read: true,
            playlist_write: true,
            epg_read: true,
            show_streams_page: true,
            show_stream_history: true,
            show_recording: true,
        }
    }

    fn views(sections: &[NavSection]) -> Vec<ViewType> {
        sections.iter().flat_map(|section| section.entries.iter().map(|entry| entry.view)).collect()
    }

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

    #[test]
    fn nav_without_any_access_offers_only_the_dashboard() {
        let sections = nav_sections(&NavAccess::default());
        assert_eq!(views(&sections), vec![ViewType::Dashboard]);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].title_key, None);
    }

    #[test]
    fn nav_with_full_access_keeps_the_display_order() {
        let sections = nav_sections(&full_access());
        assert_eq!(
            views(&sections),
            vec![
                ViewType::Dashboard,
                ViewType::Stats,
                ViewType::Streams,
                ViewType::Logs,
                ViewType::StreamHistory,
                ViewType::Rbac,
                ViewType::Config,
                ViewType::Users,
                ViewType::Plans,
                ViewType::SourceEditor,
                ViewType::FailoverGroups,
                ViewType::PlaylistUpdate,
                ViewType::PlaylistStats,
                ViewType::PlaylistExplorer,
                ViewType::PlaylistEpg,
                ViewType::PlaylistSettings,
                ViewType::RecordingLibrary,
                ViewType::RecordingRules,
            ]
        );
        let titles: Vec<_> = sections.iter().map(|section| section.title_key).collect();
        assert_eq!(titles, vec![None, Some("LABEL.SETTINGS"), Some("LABEL.PLAYLIST"), Some("LABEL.RECORDING")]);
    }

    #[test]
    fn nav_entries_follow_their_permissions() {
        let playlist_reader = NavAccess { playlist_read: true, ..NavAccess::default() };
        assert_eq!(
            views(&nav_sections(&playlist_reader)),
            vec![
                ViewType::Dashboard,
                ViewType::FailoverGroups,
                ViewType::PlaylistStats,
                ViewType::PlaylistExplorer,
                ViewType::PlaylistSettings
            ]
        );

        let epg_reader = NavAccess { epg_read: true, ..NavAccess::default() };
        assert_eq!(views(&nav_sections(&epg_reader)), vec![ViewType::Dashboard, ViewType::PlaylistEpg]);

        let streams_without_system_read = NavAccess { show_streams_page: true, ..NavAccess::default() };
        assert_eq!(views(&nav_sections(&streams_without_system_read)), vec![ViewType::Dashboard]);

        let user_writer = NavAccess { user_write: true, ..NavAccess::default() };
        assert_eq!(views(&nav_sections(&user_writer)), vec![ViewType::Dashboard, ViewType::Users]);

        let config_reader = NavAccess { config_read: true, ..NavAccess::default() };
        assert_eq!(views(&nav_sections(&config_reader)), vec![ViewType::Dashboard, ViewType::Config, ViewType::Plans]);
    }

    #[test]
    fn nav_recording_entries_use_existing_icons() {
        let sections = nav_sections(&NavAccess { show_recording: true, ..NavAccess::default() });
        let recording = sections.last().expect("recording section");
        assert_eq!(recording.title_key, Some("LABEL.RECORDING"));
        assert_eq!(
            recording.entries.iter().map(|entry| entry.icon).collect::<Vec<_>>(),
            vec!["VideoLibrary", "Calendar"]
        );
    }
}
