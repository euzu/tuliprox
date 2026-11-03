use crate::{app::components::playlist::explorer::identity::Id, i18n::use_translation};
use shared::model::{UiPlaylistGroup, XtreamCluster};
use std::{rc::Rc, sync::Arc};
use yew::prelude::*;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SelectedGroup {
    All,
    WithoutEpg,
    Edited,
    Group(Arc<str>),
}

#[derive(Properties, PartialEq, Clone)]
pub struct ExplorerSidebarProps {
    pub cluster: XtreamCluster,
    /// Group list of the active cluster; `Id` keeps the prop comparison at a pointer check.
    pub groups: Option<Id<Vec<Rc<UiPlaylistGroup>>>>,
    /// Counts are computed once by the parent, so the sidebar does not rescan the catalog
    /// on every render.
    pub total_items: usize,
    pub edited_count: usize,
    pub without_epg_count: usize,
    pub selected_group: SelectedGroup,
    pub on_select_group: Callback<SelectedGroup>,
}

#[component]
pub fn ExplorerSidebar(props: &ExplorerSidebarProps) -> Html {
    let translate = use_translation();

    let on_click_all = {
        let on_select = props.on_select_group.clone();
        Callback::from(move |_: MouseEvent| on_select.emit(SelectedGroup::All))
    };

    let on_click_without_epg = {
        let on_select = props.on_select_group.clone();
        Callback::from(move |_: MouseEvent| on_select.emit(SelectedGroup::WithoutEpg))
    };

    let on_click_edited = {
        let on_select = props.on_select_group.clone();
        Callback::from(move |_: MouseEvent| on_select.emit(SelectedGroup::Edited))
    };

    html! {
        <div class="side">
            <button
                type="button"
                class={if props.selected_group == SelectedGroup::All { "on" } else { "" }}
                aria-pressed={(props.selected_group == SelectedGroup::All).to_string()}
                onclick={on_click_all}
            >
                <span>{ translate.t("LABEL.EXPLORER_VIRTUAL_GROUP_ALL") }</span>
                <small>{ props.total_items }</small>
            </button>
            {
                if let Some(ref groups) = props.groups {
                    groups.iter().map(|g| {
                        let is_selected = match &props.selected_group {
                            SelectedGroup::Group(title) => title == &g.title,
                            _ => false,
                        };
                        let group_title = g.title.clone();
                        let on_select = props.on_select_group.clone();
                        let on_click = Callback::from(move |_: MouseEvent| {
                            on_select.emit(SelectedGroup::Group(group_title.clone()));
                        });
                        html! {
                            <button
                                type="button"
                                key={format!("grp-{}", g.title)}
                                class={if is_selected { "on" } else { "" }}
                                aria-pressed={is_selected.to_string()}
                                onclick={on_click}
                            >
                                <span>{ g.title.clone() }</span>
                                <small>{ g.channels.len() }</small>
                            </button>
                        }
                    }).collect::<Html>()
                } else {
                    html! {}
                }
            }
            {
                if props.cluster == XtreamCluster::Live {
                    html! {
                        <button
                            type="button"
                            class={format!("w {}", if props.selected_group == SelectedGroup::WithoutEpg { "on" } else { "" })}
                            aria-pressed={(props.selected_group == SelectedGroup::WithoutEpg).to_string()}
                            onclick={on_click_without_epg}
                        >
                            <span>{ "⚠ " }{ translate.t("LABEL.EXPLORER_VIRTUAL_GROUP_WITHOUT_EPG") }</span>
                            <small>{ props.without_epg_count }</small>
                        </button>
                    }
                } else {
                    html! {}
                }
            }
            <button
                type="button"
                class={format!("e {}", if props.selected_group == SelectedGroup::Edited { "on" } else { "" })}
                aria-pressed={(props.selected_group == SelectedGroup::Edited).to_string()}
                onclick={on_click_edited}
            >
                <span>{ "✎ " }{ translate.t("LABEL.EXPLORER_VIRTUAL_GROUP_EDITED") }</span>
                <small>{ props.edited_count }</small>
            </button>
        </div>
    }
}
