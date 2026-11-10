//! Channel catalog of the target: search, filter, paging and adding channels to the selected group.

use super::{
    dialogs::NameDialog,
    draft::{random_id_suffix, DraftOp, GroupRef},
    group_list::{find_group, group_ref_of, GroupRow, RowState},
    navigation::NumberedChannel,
};
use crate::{
    app::components::{
        define_table_columns, make_translated_header_callback, popup_menu::PopupMenuWidth, select::Select,
        selection_first, AppIcon, DropDownOption, DropDownSelection, IconButton, PagedTable, RadioButtonGroup, Search,
        TableDefinition, TextButton,
    },
    hooks::use_service_context,
    i18n::use_translation,
    services::FailoverService,
};
use shared::model::{
    CandidateDto, CandidatePageDto, ChannelMemberRefDto, FailoverCandidateFilter, FailoverGroupListDto, SearchRequest,
    TargetFailoverOverridesDto,
};
use std::{collections::HashMap, rc::Rc};
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

const DEFAULT_PAGE_SIZE: u16 = 100;

define_table_columns! {
    enum CandidateColumn {
        Select => ("select", "LABEL.FAILOVER_GROUPS_COL_SELECT") { can_hide: false },
        Input => ("input", "LABEL.FAILOVER_GROUPS_COL_INPUT"),
        Title => ("title", "LABEL.FAILOVER_GROUPS_COL_TITLE") { can_hide: false },
        PlaylistGroup => ("playlist_group", "LABEL.FAILOVER_GROUPS_COL_PLAYLIST_GROUP"),
        Membership => ("membership", "LABEL.FAILOVER_GROUPS_COL_IN_GROUP"),
        Actions => ("actions", "LABEL.FAILOVER_GROUPS_COL_ACTIONS") { can_hide: false },
    }
}

/// The group of the edited draft a channel currently belongs to.
pub type Membership = HashMap<ChannelMemberRefDto, (GroupRef, String)>;

/// Group membership of every channel of the target. The draft decides first (members of manual groups, including
/// those too small to be materialized, and `add` entries of automatic groups; a manual group wins over an `add`),
/// the preview of the draft supplies the channels that belong to a group by their natural grouping.
pub fn membership_of(preview: Option<&FailoverGroupListDto>, draft: &TargetFailoverOverridesDto) -> Membership {
    let mut membership: Membership = preview
        .into_iter()
        .flat_map(|preview| &preview.groups)
        .flat_map(|group| {
            let reference = group_ref_of(group);
            group.members.iter().map(move |member| (member.member_ref.clone(), (reference.clone(), group.name.clone())))
        })
        .collect();
    for entry in draft.auto_overrides.iter().filter(|entry| !entry.add.is_empty()) {
        let reference = GroupRef::Auto(entry.auto_key.clone());
        let name = preview
            .and_then(|preview| find_group(preview, &reference))
            .map(|group| group.name.clone())
            .or_else(|| entry.name.clone())
            .unwrap_or_else(|| entry.auto_key.clone());
        for member in &entry.add {
            membership.insert(member.clone(), (reference.clone(), name.clone()));
        }
    }
    for group in &draft.groups {
        let reference = GroupRef::Manual(group.id.clone());
        for member in &group.members {
            membership.insert(member.clone(), (reference.clone(), group.name.clone()));
        }
    }
    membership
}

/// Every channel paired with the group that holds it now, for the operations that move channels between groups.
pub fn with_current_groups<'a>(
    members: impl IntoIterator<Item = &'a ChannelMemberRefDto>,
    membership: &Membership,
) -> Vec<(ChannelMemberRefDto, Option<GroupRef>)> {
    members.into_iter().map(|member| (member.clone(), membership.get(member).map(|(group, _)| group.clone()))).collect()
}

/// The single operation that puts all `members` into `target`; `None` when there is nothing to put.
pub fn add_members_op<'a>(
    target: &GroupRef,
    members: impl IntoIterator<Item = &'a ChannelMemberRefDto>,
    membership: &Membership,
) -> Option<DraftOp> {
    let members = with_current_groups(members, membership);
    (!members.is_empty()).then(|| DraftOp::AddMembers { group: target.clone(), members })
}

/// Number of pages of `total` matches.
pub fn page_count(total: usize, page_size: u16) -> u32 {
    let size = usize::from(page_size.max(1));
    u32::try_from(total.div_ceil(size)).unwrap_or(u32::MAX).max(1)
}

const fn filter_id(filter: FailoverCandidateFilter) -> &'static str {
    match filter {
        FailoverCandidateFilter::All => "all",
        FailoverCandidateFilter::Ungrouped => "ungrouped",
        FailoverCandidateFilter::Grouped => "grouped",
        FailoverCandidateFilter::Excluded => "excluded",
    }
}

fn group_option_id(group: &GroupRef) -> String {
    match group {
        GroupRef::Auto(key) => format!("auto:{key}"),
        GroupRef::Manual(key) => format!("manual:{key}"),
    }
}

fn filter_from_id(id: &str) -> Option<FailoverCandidateFilter> {
    [
        FailoverCandidateFilter::Ungrouped,
        FailoverCandidateFilter::Grouped,
        FailoverCandidateFilter::Excluded,
        FailoverCandidateFilter::All,
    ]
    .into_iter()
    .find(|filter| filter_id(*filter) == id)
}

const fn filter_label_key(filter: FailoverCandidateFilter) -> &'static str {
    match filter {
        FailoverCandidateFilter::All => "LABEL.ALL",
        FailoverCandidateFilter::Ungrouped => "LABEL.FAILOVER_GROUPS_FILTER_UNGROUPED",
        FailoverCandidateFilter::Grouped => "LABEL.FAILOVER_GROUPS_FILTER_GROUPED",
        FailoverCandidateFilter::Excluded => "LABEL.FAILOVER_GROUPS_FILTER_EXCLUDED",
    }
}

/// Ticks `member` when the page lists it.
fn select_listed(selection: &UseStateHandle<Selection>, page: Option<&CandidatePageDto>, member: &ChannelMemberRefDto) {
    if let Some(candidate) = page.and_then(|page| page.items.iter().find(|candidate| candidate.member_ref == *member)) {
        let mut next = (**selection).clone();
        next.insert(member.clone(), candidate.clone());
        selection.set(next);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FetchStatus {
    Loading,
    Ready,
    /// No playlist update has published the channel catalog yet.
    NoCatalog,
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
struct CandidateRow {
    candidate: CandidateDto,
    group: Option<(GroupRef, String)>,
    excluded: bool,
    selected: bool,
}

type Selection = HashMap<ChannelMemberRefDto, CandidateDto>;

#[derive(Properties, PartialEq)]
pub struct CandidatePanelProps {
    pub target: String,
    /// Changes whenever the stored section changes, so the persisted group of every channel is read again.
    pub reload_key: Option<String>,
    pub selected_group: Option<GroupRef>,
    pub selected_group_name: Option<String>,
    /// Groups to choose the target group from.
    pub groups: Rc<Vec<GroupRow>>,
    pub on_select_group: Callback<GroupRef>,
    pub membership: Rc<Membership>,
    pub excluded: Rc<Vec<ChannelMemberRefDto>>,
    pub read_only: bool,
    pub on_op: Callback<DraftOp>,
    /// A channel to look at: seeds the search, lifts the filter and ticks the channel once it is listed.
    #[prop_or_default]
    pub focus: Option<Rc<NumberedChannel>>,
}

#[component]
pub fn CandidatePanel(props: &CandidatePanelProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let query = use_state(String::new);
    let regex = use_state(|| false);
    let search_value = use_state(|| None::<String>);
    let focused_member = use_mut_ref(|| None::<ChannelMemberRefDto>);
    let filter = use_state(|| FailoverCandidateFilter::Ungrouped);
    let page = use_state(|| 1_u32);
    let page_size = use_state(|| DEFAULT_PAGE_SIZE);
    let result = use_state(|| None::<Rc<CandidatePageDto>>);
    let status = use_state(|| FetchStatus::Loading);
    let selection = use_state(Selection::new);
    let show_name_dialog = use_state(|| false);
    let generation = use_mut_ref(|| 0_u64);
    let columns = use_memo((), |()| CandidateColumn::columns());
    let membership = props.membership.clone();

    {
        let result = result.clone();
        let status = status.clone();
        let generation = generation.clone();
        let services = services.clone();
        let translate = translate.clone();
        use_effect_with(
            (props.target.clone(), (*query).clone(), *regex, *filter, *page, *page_size, props.reload_key.clone()),
            move |(target, query, regex, filter, page, page_size, _)| {
                let ticket = {
                    let mut current = generation.borrow_mut();
                    *current += 1;
                    *current
                };
                status.set(FetchStatus::Loading);
                let (target, query, regex, filter, page, page_size) =
                    (target.clone(), query.clone(), *regex, *filter, *page, *page_size);
                spawn_local(async move {
                    let offset = usize::try_from(page.saturating_sub(1)).unwrap_or(0) * usize::from(page_size);
                    let outcome = FailoverService::search_candidates(
                        &target,
                        &query,
                        regex,
                        filter,
                        offset,
                        usize::from(page_size),
                    )
                    .await;
                    if *generation.borrow() != ticket {
                        return;
                    }
                    match outcome {
                        Ok(found) => {
                            result.set(Some(Rc::new(found)));
                            status.set(FetchStatus::Ready);
                        }
                        Err(crate::error::Error::Conflict(_)) => {
                            result.set(None);
                            status.set(FetchStatus::NoCatalog);
                        }
                        Err(err) => {
                            services
                                .toastr
                                .error(format!("{}: {err}", translate.t("MESSAGES.FAILOVER_GROUPS_CANDIDATES_FAILED")));
                            status.set(FetchStatus::Failed);
                        }
                    }
                });
                || ()
            },
        );
    }

    {
        let query = query.clone();
        let regex = regex.clone();
        let search_value = search_value.clone();
        let filter = filter.clone();
        let page = page.clone();
        let status = status.clone();
        let result = result.clone();
        let selection = selection.clone();
        let focused_member = focused_member.clone();
        use_effect_with(props.focus.clone(), move |focus| {
            if let Some(focus) = focus {
                let text = focus.channel.query().to_string();
                let member = focus.channel.member_ref.clone();
                let unchanged = *query == text
                    && !*regex
                    && *filter == FailoverCandidateFilter::All
                    && *page == 1
                    && *status == FetchStatus::Ready;
                if unchanged {
                    select_listed(&selection, result.as_deref(), &member);
                } else {
                    *focused_member.borrow_mut() = Some(member);
                    search_value.set(Some(text.clone()));
                    query.set(text);
                    regex.set(false);
                    filter.set(FailoverCandidateFilter::All);
                    page.set(1);
                }
            }
            || ()
        });
    }

    // The channel of a deep link is ticked as soon as the search that lists it is answered.
    {
        let result = result.clone();
        let selection = selection.clone();
        let focused_member = focused_member.clone();
        use_effect_with(*status, move |status| {
            if *status == FetchStatus::Ready {
                if let Some(member) = focused_member.borrow_mut().take() {
                    select_listed(&selection, result.as_deref(), &member);
                }
            }
            || ()
        });
    }

    let on_search = {
        let query = query.clone();
        let regex = regex.clone();
        let page = page.clone();
        Callback::from(move |request: SearchRequest| {
            let (text, is_regex) = match request {
                SearchRequest::Clear => (String::new(), false),
                SearchRequest::Text(text, _) => (text.trim().to_string(), false),
                SearchRequest::Regexp(pattern, _) => (pattern, true),
            };
            query.set(text);
            regex.set(is_regex);
            page.set(1);
        })
    };

    let on_filter = {
        let filter = filter.clone();
        let page = page.clone();
        Callback::from(move |selected: Rc<Vec<String>>| {
            if let Some(next) = selected.first().and_then(|id| filter_from_id(id)) {
                filter.set(next);
                page.set(1);
            }
        })
    };

    let group_of = |member: &ChannelMemberRefDto| membership.get(member).cloned();

    let toggle = {
        let selection = selection.clone();
        Callback::from(move |candidate: CandidateDto| {
            let mut next = (*selection).clone();
            if next.remove(&candidate.member_ref).is_none() {
                next.insert(candidate.member_ref.clone(), candidate);
            }
            selection.set(next);
        })
    };

    let add_one = {
        let on_op = props.on_op.clone();
        let target_group = props.selected_group.clone();
        Callback::from(move |(candidate, current): (CandidateDto, Option<GroupRef>)| {
            if let Some(group) = target_group.as_ref() {
                on_op
                    .emit(DraftOp::AddMembers { group: group.clone(), members: vec![(candidate.member_ref, current)] });
            }
        })
    };

    let add_selected = {
        let on_op = props.on_op.clone();
        let target_group = props.selected_group.clone();
        let selection = selection.clone();
        let membership = membership.clone();
        Callback::from(move |_: String| {
            if let Some(op) =
                target_group.as_ref().and_then(|group| add_members_op(group, selection.keys(), &membership))
            {
                on_op.emit(op);
                selection.set(Selection::new());
            }
        })
    };

    let new_group = {
        let on_op = props.on_op.clone();
        let selection = selection.clone();
        let membership = membership.clone();
        let show_name_dialog = show_name_dialog.clone();
        Callback::from(move |name: String| {
            let members = with_current_groups(selection.keys(), &membership);
            on_op.emit(DraftOp::NewGroupFromSelection { name, members, id_suffix: random_id_suffix() });
            selection.set(Selection::new());
            show_name_dialog.set(false);
        })
    };

    let rows: Rc<Vec<Rc<CandidateRow>>> = Rc::new(
        result
            .as_ref()
            .map(|page| {
                page.items
                    .iter()
                    .map(|candidate| {
                        Rc::new(CandidateRow {
                            group: group_of(&candidate.member_ref),
                            excluded: props.excluded.contains(&candidate.member_ref),
                            selected: selection.contains_key(&candidate.member_ref),
                            candidate: candidate.clone(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
    );

    let group_name = props.selected_group_name.clone().unwrap_or_default();
    let can_add = !props.read_only && props.selected_group.is_some();
    let render_header_cell = make_translated_header_callback(translate.clone(), |index| {
        CandidateColumn::from_index(index).map(CandidateColumn::header_label)
    });
    let render_data_cell = {
        let translate = translate.clone();
        let toggle = toggle.clone();
        let add_one = add_one.clone();
        let selected_group = props.selected_group.clone();
        let read_only = props.read_only;
        Callback::<(usize, usize, Rc<CandidateRow>), Html>::from(
            move |(_, col, row): (usize, usize, Rc<CandidateRow>)| {
                let candidate = &row.candidate;
                match CandidateColumn::from_index(col) {
                    Some(CandidateColumn::Select) if !read_only => {
                        let toggle = toggle.clone();
                        let candidate = candidate.clone();
                        html! {
                            <button type="button" class="tp__icon-button tp__failover-groups__candidate-select" aria-pressed={row.selected.to_string()}
                                aria-label={translate.t("LABEL.FAILOVER_GROUPS_COL_SELECT")}
                                onclick={Callback::from(move |_| toggle.emit(candidate.clone()))}>
                                <AppIcon name={if row.selected { "Checked" } else { "Unchecked" }}/>
                            </button>
                        }
                    }
                    Some(CandidateColumn::Input) => html! { candidate.member_ref.input.as_str() },
                    Some(CandidateColumn::Title) => html! {
                        <>
                            { &candidate.title }
                            if row.excluded {
                                <span class="tp__failover-groups__excluded-tag">{ translate.t("LABEL.FAILOVER_GROUPS_FILTER_EXCLUDED") }</span>
                            }
                        </>
                    },
                    Some(CandidateColumn::PlaylistGroup) => html! { candidate.playlist_group.as_str() },
                    Some(CandidateColumn::Membership) => {
                        html! { row.group.as_ref().map_or("\u{2014}", |(_, name)| name.as_str()) }
                    }
                    Some(CandidateColumn::Actions)
                        if !read_only
                            && selected_group.is_some()
                            && row.group.as_ref().map(|(group, _)| group) != selected_group.as_ref() =>
                    {
                        let current = row.group.as_ref().map(|(group, _)| group.clone());
                        let candidate = candidate.clone();
                        let add_one = add_one.clone();
                        let key = if row.group.is_some() {
                            "LABEL.FAILOVER_GROUPS_MOVE_TO_GROUP"
                        } else {
                            "LABEL.FAILOVER_GROUPS_ADD_TO_GROUP"
                        };
                        let title = translate.t(key);
                        html! {
                            <IconButton class="tp__failover-groups__candidate-add" name="add" icon="Add" hint={title.clone()} aria_label={Some(title)}
                                onclick={Callback::from(move |_| add_one.emit((candidate.clone(), current.clone())))}/>
                        }
                    }
                    _ => html! {},
                }
            },
        )
    };

    let total = result.as_ref().map_or(0, |found| found.total);
    let total_pages = page_count(total, *page_size);
    let definition = Rc::new(TableDefinition::<CandidateRow> {
        table_id: "failover_groups.candidates".into(),
        columns: columns.clone(),
        row_key: Callback::from(|(_, row): (usize, Rc<CandidateRow>)| {
            format!("{}/{}", row.candidate.member_ref.input, row.candidate.member_ref.stream_id).into()
        }),
        items: if rows.is_empty() { None } else { Some(rows) },
        on_sort: Callback::noop(),
        render_header_cell,
        render_data_cell,
    });

    let options = Rc::new(
        [
            FailoverCandidateFilter::Ungrouped,
            FailoverCandidateFilter::Grouped,
            FailoverCandidateFilter::Excluded,
            FailoverCandidateFilter::All,
        ]
        .iter()
        .map(|filter| filter_id(*filter).to_string())
        .collect::<Vec<_>>(),
    );
    let labels = Rc::new(
        [
            FailoverCandidateFilter::Ungrouped,
            FailoverCandidateFilter::Grouped,
            FailoverCandidateFilter::Excluded,
            FailoverCandidateFilter::All,
        ]
        .iter()
        .map(|filter| translate.t(filter_label_key(*filter)))
        .collect::<Vec<_>>(),
    );

    let on_page_change = {
        let page = page.clone();
        Callback::from(move |next: u32| page.set(next))
    };
    let on_page_size_change = {
        let page_size = page_size.clone();
        let page = page.clone();
        Callback::from(move |size: u16| {
            page_size.set(size);
            page.set(1);
        })
    };
    let on_new_group_open = {
        let show_name_dialog = show_name_dialog.clone();
        Callback::from(move |_: String| show_name_dialog.set(true))
    };
    let on_name_cancel = {
        let show_name_dialog = show_name_dialog.clone();
        Callback::from(move |()| show_name_dialog.set(false))
    };
    let selected_count = selection.len();

    let group_ids = Rc::new(
        props
            .groups
            .iter()
            .filter(|row| row.state != RowState::Dissolved)
            .map(|row| (group_option_id(&row.group), row))
            .collect::<Vec<_>>(),
    );
    let group_options = Rc::new(
        group_ids
            .iter()
            .map(|(id, row)| {
                DropDownOption::new(id, html! { row.name.as_str() }, props.selected_group.as_ref() == Some(&row.group))
            })
            .collect::<Vec<_>>(),
    );
    let on_group_select = {
        let on_select_group = props.on_select_group.clone();
        let groups = props.groups.clone();
        Callback::from(move |(_, selection): (String, DropDownSelection)| {
            let found =
                selection_first(&selection).and_then(|id| groups.iter().find(|row| group_option_id(&row.group) == id));
            if let Some(row) = found {
                on_select_group.emit(row.group.clone());
            }
        })
    };

    html! {
        <section class="tp__failover-groups__candidates" aria-label={translate.t("LABEL.FAILOVER_GROUPS_ADD_CHANNELS")}>
            <h3>{ translate.t("LABEL.FAILOVER_GROUPS_ADD_CHANNELS") }</h3>
            if !props.read_only {
                <div class="tp__failover-groups__candidates-target">
                    <span class="tp__failover-groups__candidates-target-label">
                        { translate.t("LABEL.FAILOVER_GROUPS_TARGET_GROUP") }
                    </span>
                    <Select name="failover_candidate_target_group" multi_select={false} searchable={true}
                        popup_width={PopupMenuWidth::MatchAnchor} on_select={on_group_select} options={group_options}/>
                </div>
            }
            <div class="tp__failover-groups__candidates-toolbar">
                <Search min_length={1} value={(*search_value).clone()} onsearch={Some(on_search)}/>
                <RadioButtonGroup multi_select={false} none_allowed={false} wrap={true} options={options}
                    labels={Some(labels)} selected={Rc::new(vec![filter_id(*filter).to_string()])} on_select={on_filter}/>
                if !props.read_only {
                    <span class="tp__failover-groups__selection-count">
                        { translate.t("LABEL.FAILOVER_GROUPS_SELECTED").replace("{count}", &selected_count.to_string()) }
                    </span>
                    <TextButton name="add_selected" icon="Add" class="tertiary"
                        title={translate.t("LABEL.FAILOVER_GROUPS_ADD_SELECTED").replace("{group}", &group_name)}
                        disabled={!can_add || selected_count == 0} onclick={add_selected}/>
                    <TextButton name="new_group" icon="Group" class="tertiary"
                        title={translate.t("LABEL.FAILOVER_GROUPS_NEW_FROM_SELECTION")}
                        disabled={selected_count == 0} onclick={on_new_group_open}/>
                }
            </div>
            {
                match *status {
                    FetchStatus::NoCatalog => html! {
                        <p class="tp__failover-groups__notice" role="status">{ translate.t("MESSAGES.FAILOVER_GROUPS_NO_CATALOG") }</p>
                    },
                    FetchStatus::Failed => html! {
                        <p class="tp__failover-groups__notice" role="alert">{ translate.t("MESSAGES.FAILOVER_GROUPS_CANDIDATES_FAILED") }</p>
                    },
                    FetchStatus::Loading | FetchStatus::Ready => html! {
                        <PagedTable<CandidateRow>
                            definition={definition}
                            page={*page}
                            page_size={*page_size}
                            total_items={total as u64}
                            total_pages={total_pages}
                            has_prev={*page > 1}
                            has_next={*page < total_pages}
                            on_page_change={on_page_change}
                            on_page_size_change={on_page_size_change}/>
                    },
                }
            }
            if *show_name_dialog {
                <NameDialog title={translate.t("LABEL.FAILOVER_GROUPS_NEW_FROM_SELECTION")} on_submit={new_group}
                    on_cancel={on_name_cancel}/>
            }
        </section>
    }
}
