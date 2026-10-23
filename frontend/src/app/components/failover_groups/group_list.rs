//! Master list of the failover groups of one target: filter select, search and one row per group.

use super::draft::GroupRef;
use crate::{
    app::components::{select::Select, selection_first, AppIcon, Chip, DropDownOption, DropDownSelection, Search},
    i18n::use_translation,
};
use shared::model::{
    FailoverGroupDto, FailoverGroupListDto, FailoverGroupOrigin, SearchRequest, TargetFailoverOverridesDto,
};
use std::rc::Rc;
use yew::prelude::*;

/// Whether a row stands for a working group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
    Active,
    /// A manual group that resolved too few members to be materialized.
    Inactive,
    /// An automatic group dissolved through its override.
    Dissolved,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupRow {
    pub group: GroupRef,
    pub name: String,
    pub origin: FailoverGroupOrigin,
    pub member_count: usize,
    pub state: RowState,
    /// The group holds references that matched no channel.
    pub problem: bool,
    /// The playlist entry of the group still shows the name and logo of its previous first member.
    pub display_stale: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GroupFilter {
    #[default]
    All,
    Auto,
    Edited,
    Manual,
    Dissolved,
    Problems,
}

impl GroupFilter {
    pub const ALL: [Self; 6] = [Self::All, Self::Auto, Self::Edited, Self::Manual, Self::Dissolved, Self::Problems];

    pub const fn id(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Auto => "auto",
            Self::Edited => "edited",
            Self::Manual => "manual",
            Self::Dissolved => "dissolved",
            Self::Problems => "problems",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> { Self::ALL.into_iter().find(|filter| filter.id() == id) }

    pub const fn label_key(self) -> &'static str {
        match self {
            Self::All => "LABEL.ALL",
            Self::Auto => "LABEL.FAILOVER_GROUPS_ORIGIN_AUTO",
            Self::Edited => "LABEL.FAILOVER_GROUPS_ORIGIN_EDITED",
            Self::Manual => "LABEL.FAILOVER_GROUPS_ORIGIN_MANUAL",
            Self::Dissolved => "LABEL.FAILOVER_GROUPS_DISSOLVED",
            Self::Problems => "LABEL.FAILOVER_GROUPS_PROBLEMS",
        }
    }

    pub fn matches(self, row: &GroupRow) -> bool {
        match self {
            Self::All => row.state != RowState::Dissolved,
            Self::Auto => row.state == RowState::Active && row.origin == FailoverGroupOrigin::Auto,
            Self::Edited => row.state == RowState::Active && row.origin == FailoverGroupOrigin::AutoEdited,
            Self::Manual => row.origin == FailoverGroupOrigin::Manual,
            Self::Dissolved => row.state == RowState::Dissolved,
            Self::Problems => row.problem || row.state == RowState::Inactive,
        }
    }
}

/// The reference a group of the list is edited through: manual groups by id, all others by auto key.
pub fn group_ref_of(group: &FailoverGroupDto) -> GroupRef {
    match group.origin {
        FailoverGroupOrigin::Manual => GroupRef::Manual(group.key.clone()),
        FailoverGroupOrigin::Auto | FailoverGroupOrigin::AutoEdited => GroupRef::Auto(group.key.clone()),
    }
}

/// The group of `list` that `group` points at.
pub fn find_group<'a>(list: &'a FailoverGroupListDto, group: &GroupRef) -> Option<&'a FailoverGroupDto> {
    list.groups.iter().find(|candidate| &group_ref_of(candidate) == group)
}

/// One row per resolved group, followed by the manual groups that could not be formed and the dissolved
/// automatic groups.
pub fn build_rows(preview: Option<&FailoverGroupListDto>, draft: &TargetFailoverOverridesDto) -> Vec<GroupRow> {
    let Some(preview) = preview else { return Vec::new() };
    let diagnostics = &preview.diagnostics;
    let mut rows: Vec<GroupRow> = preview
        .groups
        .iter()
        .map(|group| GroupRow {
            group: group_ref_of(group),
            name: group.name.clone(),
            origin: group.origin,
            member_count: group.members.len(),
            state: RowState::Active,
            problem: !group.unresolved.is_empty(),
            display_stale: preview.display_stale.contains(&group.key),
        })
        .collect();
    rows.extend(diagnostics.inactive_manual_groups.iter().map(|inactive| {
        GroupRow {
            group: GroupRef::Manual(inactive.id.clone()),
            name: draft
                .groups
                .iter()
                .find(|group| group.id == inactive.id)
                .map_or_else(|| inactive.id.clone(), |group| group.name.clone()),
            origin: FailoverGroupOrigin::Manual,
            member_count: inactive.resolved,
            state: RowState::Inactive,
            problem: true,
            display_stale: false,
        }
    }));
    rows.extend(diagnostics.dissolved.iter().map(|key| GroupRow {
        group: GroupRef::Auto(key.clone()),
        name: key.clone(),
        origin: FailoverGroupOrigin::AutoEdited,
        member_count: 0,
        state: RowState::Dissolved,
        problem: false,
        display_stale: false,
    }));
    rows
}

pub const fn origin_chip_class(origin: FailoverGroupOrigin) -> &'static str {
    match origin {
        FailoverGroupOrigin::Auto => "tp__failover-groups__origin--auto",
        FailoverGroupOrigin::AutoEdited => "tp__failover-groups__origin--edited",
        FailoverGroupOrigin::Manual => "tp__failover-groups__origin--manual",
    }
}

pub const fn origin_label_key(origin: FailoverGroupOrigin) -> &'static str {
    match origin {
        FailoverGroupOrigin::Auto => "LABEL.FAILOVER_GROUPS_ORIGIN_AUTO",
        FailoverGroupOrigin::AutoEdited => "LABEL.FAILOVER_GROUPS_ORIGIN_EDITED",
        FailoverGroupOrigin::Manual => "LABEL.FAILOVER_GROUPS_ORIGIN_MANUAL",
    }
}

/// Rows that pass the filter and the name search.
pub fn visible_rows<'a>(rows: &'a [GroupRow], filter: GroupFilter, search: &SearchRequest) -> Vec<&'a GroupRow> {
    let matches_name = search.name_matcher();
    rows.iter().filter(|row| filter.matches(row)).filter(|row| matches_name(&row.name)).collect()
}

#[derive(Properties, PartialEq)]
pub struct GroupListProps {
    pub rows: Rc<Vec<GroupRow>>,
    pub selected: Option<GroupRef>,
    pub filter: GroupFilter,
    pub search: SearchRequest,
    /// Text shown in the search field, changed by deep links only.
    pub search_value: Option<String>,
    pub problem_count: usize,
    pub on_filter: Callback<GroupFilter>,
    pub on_search: Callback<SearchRequest>,
    pub on_select: Callback<GroupRef>,
}

#[component]
pub fn GroupList(props: &GroupListProps) -> Html {
    let translate = use_translation();
    let options = Rc::new(
        GroupFilter::ALL
            .iter()
            .map(|filter| {
                let label = translate.t(filter.label_key());
                let label =
                    if *filter == GroupFilter::Problems { format!("{label} ({})", props.problem_count) } else { label };
                DropDownOption::new(filter.id(), html! { label }, *filter == props.filter)
            })
            .collect::<Vec<_>>(),
    );
    let on_filter = {
        let on_filter = props.on_filter.clone();
        Callback::from(move |(_, selection): (String, DropDownSelection)| {
            if let Some(filter) = selection_first(&selection).and_then(GroupFilter::from_id) {
                on_filter.emit(filter);
            }
        })
    };
    let visible = visible_rows(&props.rows, props.filter, &props.search);

    html! {
        <div class="tp__failover-groups__list">
            <div class="tp__failover-groups__list-toolbar">
                <Search min_length={1} value={props.search_value.clone()} onsearch={Some(props.on_search.clone())}/>
                <Select name="failover_group_filter" multi_select={false} on_select={on_filter} options={options}/>
            </div>
            <ul class="tp__failover-groups__list-items" aria-label={translate.t("LABEL.FAILOVER_GROUPS")}>
                { for visible.iter().map(|row| {
                    let selected = props.selected.as_ref() == Some(&row.group);
                    let onclick = {
                        let on_select = props.on_select.clone();
                        let group = row.group.clone();
                        Callback::from(move |_: MouseEvent| on_select.emit(group.clone()))
                    };
                    html! {
                        <li key={format!("{:?}", row.group)}>
                            <button type="button" aria-current={selected.then_some("true")}
                                class={classes!("tp__failover-groups__list-item", selected.then_some("selected"),
                                    (row.state != RowState::Active).then_some("inactive"))}
                                onclick={onclick}>
                                if row.problem {
                                    <span class="tp__failover-groups__problem" title={translate.t("LABEL.FAILOVER_GROUPS_PROBLEMS")}>
                                        <AppIcon name="Warning"/>
                                    </span>
                                }
                                if row.display_stale {
                                    <span class="tp__failover-groups__display-stale"
                                        title={translate.t("MESSAGES.FAILOVER_GROUPS_DISPLAY_STALE")}>
                                        <AppIcon name="Refresh"/>
                                    </span>
                                }
                                <span class="tp__failover-groups__list-item-name">{ &row.name }</span>
                                <span class="tp__failover-groups__list-item-count">{ row.member_count }</span>
                                <Chip class={Some(origin_chip_class(row.origin).to_string())}
                                    label={translate.t(match row.state {
                                        RowState::Active => origin_label_key(row.origin),
                                        RowState::Inactive => "LABEL.FAILOVER_GROUPS_INACTIVE",
                                        RowState::Dissolved => "LABEL.FAILOVER_GROUPS_DISSOLVED",
                                    })}/>
                            </button>
                        </li>
                    }
                }) }
                if visible.is_empty() {
                    <li class="tp__failover-groups__list-empty">{ translate.t("LABEL.NO_CONTENT") }</li>
                }
            </ul>
        </div>
    }
}
