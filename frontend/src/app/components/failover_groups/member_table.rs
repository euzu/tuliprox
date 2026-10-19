//! Member table of the selected group: order, health columns and the per-member actions.

use super::{
    draft::{DraftOp, GroupRef},
    health::{bitrate_label, circuit_label_key, health_class, qos_label, slots_label},
};
use crate::{
    app::components::{
        define_table_columns, make_translated_header_callback, selection_first, AppIcon, DropDownIconButton,
        DropDownOption, IconButton, Table, TableDefinition,
    },
    i18n::use_translation,
};
use shared::model::{ChannelMemberRefDto, FailoverGroupHealthDto, FailoverGroupMemberDto, MemberHealthDto};
use std::{cell::RefCell, rc::Rc};
use yew::prelude::*;

define_table_columns! {
    enum MemberColumn {
        Handle => ("handle", "LABEL.FAILOVER_GROUPS_COL_ORDER") { can_hide: false },
        Position => ("position", "LABEL.FAILOVER_GROUPS_COL_POSITION") { can_hide: false },
        Input => ("input", "LABEL.FAILOVER_GROUPS_COL_INPUT"),
        Title => ("title", "LABEL.FAILOVER_GROUPS_COL_TITLE") { can_hide: false },
        Resolution => ("resolution", "LABEL.FAILOVER_GROUPS_COL_RESOLUTION"),
        Codec => ("codec", "LABEL.FAILOVER_GROUPS_COL_CODEC"),
        Bitrate => ("bitrate", "LABEL.FAILOVER_GROUPS_COL_BITRATE"),
        Qos => ("qos", "LABEL.FAILOVER_GROUPS_COL_QOS"),
        Circuit => ("circuit", "LABEL.FAILOVER_GROUPS_COL_CIRCUIT"),
        Slots => ("slots", "LABEL.FAILOVER_GROUPS_COL_SLOTS"),
        Actions => ("actions", "LABEL.FAILOVER_GROUPS_COL_ACTIONS") { can_hide: false },
    }
}

const NO_VALUE: &str = "\u{2014}";

/// The text, or a dash when it is missing or empty.
fn or_dash(text: Option<String>) -> String {
    text.filter(|text| !text.is_empty()).unwrap_or_else(|| NO_VALUE.to_string())
}

/// What a member action asks for; the position is the index in the visible member list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberAction {
    Up,
    Down,
    First,
    Remove,
    Exclude,
    /// Drag and drop: move the member to this index.
    MoveTo(usize),
}

/// The draft operation for `action` on the member at `index` of `refs` (the visible members in order).
pub fn member_action_op(
    group: &GroupRef,
    refs: &[ChannelMemberRefDto],
    index: usize,
    action: MemberAction,
) -> Option<DraftOp> {
    let member = refs.get(index)?.clone();
    let reorder = |to: usize| {
        (to != index && to < refs.len()).then(|| DraftOp::Reorder {
            group: group.clone(),
            from: index,
            to,
            current: refs.to_vec(),
        })
    };
    match action {
        MemberAction::Up => reorder(index.checked_sub(1)?),
        MemberAction::Down => reorder(index + 1),
        MemberAction::MoveTo(to) => reorder(to),
        MemberAction::First => {
            (index > 0).then(|| DraftOp::MoveFirst { group: group.clone(), index, current: refs.to_vec() })
        }
        MemberAction::Remove => Some(DraftOp::RemoveMember { group: group.clone(), member }),
        MemberAction::Exclude => Some(DraftOp::ExcludeMember { group: Some(group.clone()), member }),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MemberRow {
    pub index: usize,
    pub member: FailoverGroupMemberDto,
    pub health: Option<MemberHealthDto>,
}

/// Pairs every visible member with its health entry, when health is known.
pub fn member_rows(members: &[FailoverGroupMemberDto], health: Option<&FailoverGroupHealthDto>) -> Vec<Rc<MemberRow>> {
    members
        .iter()
        .enumerate()
        .map(|(index, member)| {
            let health = health
                .and_then(|health| health.members.iter().find(|entry| entry.member_ref == member.member_ref))
                .cloned();
            Rc::new(MemberRow { index, member: member.clone(), health })
        })
        .collect()
}

type DragState = Rc<RefCell<Option<usize>>>;

/// Every cell of a row accepts a dragged member, so the whole row is a drop target.
fn drop_cell(content: Html, index: usize, drag: &DragState, on_move: &Callback<(usize, usize)>, enabled: bool) -> Html {
    if !enabled {
        return content;
    }
    let ondragover = Callback::from(|event: DragEvent| event.prevent_default());
    let ondrop = {
        let drag = drag.clone();
        let on_move = on_move.clone();
        Callback::from(move |event: DragEvent| {
            event.prevent_default();
            if let Some(from) = drag.borrow_mut().take() {
                on_move.emit((from, index));
            }
        })
    };
    html! { <div class="tp__failover-groups__cell" ondragover={ondragover} ondrop={ondrop}>{ content }</div> }
}

#[derive(Properties, PartialEq)]
pub struct MemberTableProps {
    pub group: GroupRef,
    pub members: Rc<Vec<FailoverGroupMemberDto>>,
    pub health: Option<Rc<FailoverGroupHealthDto>>,
    pub read_only: bool,
    /// The shown members are out of date with the draft; actions wait for the new preview.
    pub busy: bool,
    pub on_op: Callback<DraftOp>,
    pub on_move_to: Callback<ChannelMemberRefDto>,
}

#[component]
pub fn MemberTable(props: &MemberTableProps) -> Html {
    let translate = use_translation();
    let columns = use_memo((), |()| MemberColumn::columns());
    let drag: DragState = use_mut_ref(|| None);
    let rows = use_memo((props.members.clone(), props.health.clone()), |(members, health)| {
        member_rows(members, health.as_deref())
    });

    let on_action = {
        let group = props.group.clone();
        let members = props.members.clone();
        let on_op = props.on_op.clone();
        let on_move_to = props.on_move_to.clone();
        let busy = props.busy;
        Callback::from(move |(index, action): (usize, Option<MemberAction>)| {
            if busy {
                return;
            }
            let refs: Vec<ChannelMemberRefDto> = members.iter().map(|member| member.member_ref.clone()).collect();
            match action {
                Some(action) => {
                    if let Some(op) = member_action_op(&group, &refs, index, action) {
                        on_op.emit(op);
                    }
                }
                None => {
                    if let Some(member) = refs.get(index) {
                        on_move_to.emit(member.clone());
                    }
                }
            }
        })
    };

    let render_header_cell = make_translated_header_callback(translate.clone(), |index| {
        MemberColumn::from_index(index).map(MemberColumn::header_label)
    });

    let definition = {
        let columns = columns.clone();
        let translate = translate.clone();
        let read_only = props.read_only;
        use_memo((rows.clone(), read_only, on_action.clone()), move |(rows, read_only, on_action)| {
            let read_only = *read_only;
            let last = rows.len().saturating_sub(1);
            let on_move = {
                let on_action = on_action.clone();
                Callback::from(move |(from, to): (usize, usize)| {
                    on_action.emit((from, Some(MemberAction::MoveTo(to))));
                })
            };
            let render_data_cell = {
                let drag = drag.clone();
                let on_action = on_action.clone();
                let translate = translate.clone();
                Callback::<(usize, usize, Rc<MemberRow>), Html>::from(
                    move |(_, col, row): (usize, usize, Rc<MemberRow>)| {
                        let index = row.index;
                        let health = row.health.as_ref();
                        let content = match MemberColumn::from_index(col) {
                            Some(MemberColumn::Handle) if !read_only => {
                                let ondragstart = {
                                    let drag = drag.clone();
                                    Callback::from(move |event: DragEvent| {
                                        *drag.borrow_mut() = Some(index);
                                        if let Some(transfer) = event.data_transfer() {
                                            transfer.set_effect_allowed("move");
                                            let _ = transfer.set_data("text/plain", &index.to_string());
                                        }
                                    })
                                };
                                let ondragend = {
                                    let drag = drag.clone();
                                    Callback::from(move |_: DragEvent| *drag.borrow_mut() = None)
                                };
                                html! {
                                    <span class="tp__failover-groups__drag-handle" draggable="true"
                                        title={translate.t("LABEL.FAILOVER_GROUPS_DRAG")}
                                        ondragstart={ondragstart} ondragend={ondragend}>
                                        <AppIcon name="DragHandle"/>
                                    </span>
                                }
                            }
                            Some(MemberColumn::Position) => {
                                let step =
                                    |name: &'static str, icon: &'static str, hint: &str, action, disabled: bool| {
                                        let on_action = on_action.clone();
                                        html! {
                                            <IconButton name={name} icon={icon} hint={hint.to_string()}
                                                aria_label={Some(hint.to_string())}
                                                class={if disabled { "disabled" } else { "" }}
                                                onclick={Callback::from(move |_| {
                                                    if !disabled {
                                                        on_action.emit((index, Some(action)));
                                                    }
                                                })}/>
                                        }
                                    };
                                html! {
                                    <span class="tp__failover-groups__position">
                                        { index + 1 }
                                        if !read_only {
                                            { step("up", "ArrowUp", &translate.t("LABEL.FAILOVER_GROUPS_MOVE_UP"),
                                                MemberAction::Up, index == 0) }
                                            { step("down", "ArrowDown", &translate.t("LABEL.FAILOVER_GROUPS_MOVE_DOWN"),
                                                MemberAction::Down, index == last) }
                                        }
                                    </span>
                                }
                            }
                            Some(MemberColumn::Input) => html! { &row.member.input_name },
                            Some(MemberColumn::Title) => html! { &row.member.title },
                            Some(MemberColumn::Resolution) => {
                                html! { or_dash(health.and_then(|h| h.resolution.as_ref().map(ToString::to_string))) }
                            }
                            Some(MemberColumn::Codec) => {
                                html! { or_dash(health.and_then(|h| h.codec.as_ref().map(ToString::to_string))) }
                            }
                            Some(MemberColumn::Bitrate) => {
                                html! { or_dash(health.and_then(|h| bitrate_label(h.bitrate_kbps))) }
                            }
                            Some(MemberColumn::Qos) => {
                                html! { health.map_or_else(|| NO_VALUE.to_string(), |h| qos_label(h.qos_24h, h.qos_7d)) }
                            }
                            Some(MemberColumn::Circuit) => match health {
                                Some(h) => html! {
                                    <span class={classes!("tp__failover-groups__health",
                                        format!("tp__failover-groups__health--{}", health_class(&h.circuit, &h.input_circuit, h.failure_streak)))}>
                                        <span class="tp__failover-groups__health-dot" aria-hidden="true"></span>
                                        { translate.t(circuit_label_key(&h.circuit)) }
                                    </span>
                                },
                                None => html! { NO_VALUE },
                            },
                            Some(MemberColumn::Slots) => {
                                html! { health.map_or_else(|| NO_VALUE.to_string(), |h| slots_label(h.slots_used, h.slots_max)) }
                            }
                            Some(MemberColumn::Actions) if !read_only => {
                                let options = Rc::new(
                                    [
                                        ("up", "LABEL.FAILOVER_GROUPS_MOVE_UP"),
                                        ("down", "LABEL.FAILOVER_GROUPS_MOVE_DOWN"),
                                        ("first", "LABEL.FAILOVER_GROUPS_MOVE_FIRST"),
                                        ("move_to", "LABEL.FAILOVER_GROUPS_MOVE_TO"),
                                        ("remove", "LABEL.FAILOVER_GROUPS_REMOVE_MEMBER"),
                                        ("exclude", "LABEL.FAILOVER_GROUPS_EXCLUDE_MEMBER"),
                                    ]
                                    .iter()
                                    .map(|(id, key)| DropDownOption::new(id, html! { translate.t(key) }, false))
                                    .collect::<Vec<_>>(),
                                );
                                let on_select = {
                                    let on_action = on_action.clone();
                                    Callback::from(
                                        move |(_, selection): (String, crate::app::components::DropDownSelection)| {
                                            let action = match selection_first(&selection) {
                                                Some("up") => Some(MemberAction::Up),
                                                Some("down") => Some(MemberAction::Down),
                                                Some("first") => Some(MemberAction::First),
                                                Some("remove") => Some(MemberAction::Remove),
                                                Some("exclude") => Some(MemberAction::Exclude),
                                                Some("move_to") => None,
                                                _ => return,
                                            };
                                            on_action.emit((index, action));
                                        },
                                    )
                                };
                                html! {
                                    <DropDownIconButton name={format!("member-actions-{index}")} icon="Dots"
                                        aria_label={Some(translate.t("LABEL.FAILOVER_GROUPS_COL_ACTIONS"))}
                                        options={options} on_select={on_select}/>
                                }
                            }
                            _ => html! {},
                        };
                        drop_cell(content, index, &drag, &on_move, !read_only)
                    },
                )
            };
            Rc::new(TableDefinition::<MemberRow> {
                table_id: "failover_groups.members".into(),
                columns: columns.clone(),
                row_key: Callback::from(|(_, row): (usize, Rc<MemberRow>)| {
                    format!("{}/{}", row.member.member_ref.input, row.member.member_ref.stream_id).into()
                }),
                items: if rows.is_empty() { None } else { Some(rows.clone()) },
                on_sort: Callback::noop(),
                render_header_cell: render_header_cell.clone(),
                render_data_cell,
            })
        })
    };

    html! {
        <div class={classes!("tp__failover-groups__members", props.busy.then_some("tp__failover-groups__members--busy"))}>
            <Table::<MemberRow> definition={(*definition).clone()} />
        </div>
    }
}
