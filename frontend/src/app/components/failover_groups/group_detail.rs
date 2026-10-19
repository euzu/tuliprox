//! Detail of the selected group: metadata, policy and the member table.

use super::{
    dialogs::{PickerDialog, PickerItem},
    draft::{DraftOp, GroupMetaPatch, GroupRef},
    group_list::{origin_chip_class, origin_label_key, GroupRow, RowState},
    member_table::MemberTable,
};
use crate::{
    app::components::{
        select::Select, selection_first, Chip, ConfirmDialog, DropDownOption, DropDownSelection, FieldLabel, TextButton,
    },
    edit_field_text_option, generate_form_reducer,
    i18n::use_translation,
    model::DialogResult,
    use_default_form_reducer,
    utils::in_body,
};
use shared::model::{
    ChannelMemberRefDto, FailoverGroupDto, FailoverGroupHealthDto, FailoverGroupPolicy, TargetFailoverOverridesDto,
};
use std::{rc::Rc, str::FromStr};
use yew::prelude::*;

/// Metadata overrides of a group as the draft stores them; the type name is the prefix of the field ids used for
/// labels and explanations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FailoverGroupMeta {
    pub name: Option<String>,
    pub logo: Option<String>,
    pub epg_channel_id: Option<String>,
    pub playlist_group: Option<String>,
}

generate_form_reducer!(
    state: FailoverGroupMetaFormState { form: FailoverGroupMeta },
    action_name: FailoverGroupMetaFormAction,
    fields {
        Name => name: Option<String>,
        Logo => logo: Option<String>,
        EpgChannelId => epg_channel_id: Option<String>,
        PlaylistGroup => playlist_group: Option<String>,
    }
);

fn trimmed(value: &Option<String>) -> Option<String> {
    value.as_deref().map(str::trim).filter(|text| !text.is_empty()).map(str::to_string)
}

impl FailoverGroupMeta {
    /// The values the draft would store for this form content.
    pub fn normalized(&self) -> Self {
        Self {
            name: trimmed(&self.name),
            logo: trimmed(&self.logo),
            epg_channel_id: trimmed(&self.epg_channel_id),
            playlist_group: trimmed(&self.playlist_group),
        }
    }

    pub fn into_patch(self) -> GroupMetaPatch {
        GroupMetaPatch {
            name: self.name,
            logo: self.logo,
            epg_channel_id: self.epg_channel_id,
            playlist_group: self.playlist_group,
        }
    }
}

/// The metadata the draft stores for `group`.
pub fn meta_of(draft: &TargetFailoverOverridesDto, group: &GroupRef) -> FailoverGroupMeta {
    match group {
        GroupRef::Auto(key) => draft
            .auto_overrides
            .iter()
            .find(|entry| entry.auto_key == *key)
            .map(|entry| FailoverGroupMeta {
                name: entry.name.clone(),
                logo: entry.logo.clone(),
                epg_channel_id: entry.epg_channel_id.clone(),
                playlist_group: entry.playlist_group.clone(),
            })
            .unwrap_or_default(),
        GroupRef::Manual(id) => draft
            .groups
            .iter()
            .find(|entry| entry.id == *id)
            .map(|entry| FailoverGroupMeta {
                name: Some(entry.name.clone()),
                logo: entry.logo.clone(),
                epg_channel_id: entry.epg_channel_id.clone(),
                playlist_group: entry.playlist_group.clone(),
            })
            .unwrap_or_default(),
    }
}

/// The policy override the draft stores for `group`; `None` follows the target.
pub fn policy_of(draft: &TargetFailoverOverridesDto, group: &GroupRef) -> Option<FailoverGroupPolicy> {
    match group {
        GroupRef::Auto(key) => draft.auto_overrides.iter().find(|entry| entry.auto_key == *key).and_then(|e| e.policy),
        GroupRef::Manual(id) => draft.groups.iter().find(|entry| entry.id == *id).and_then(|entry| entry.policy),
    }
}

const POLICY_DEFAULT: &str = "default";

#[derive(Properties, PartialEq)]
struct GroupMetaFormProps {
    initial: FailoverGroupMeta,
    on_change: Callback<FailoverGroupMeta>,
}

/// Form of the metadata overrides. Edits are reported as soon as they change the stored values; changes that come
/// from the draft (undo, reload) replace the form content unless they only echo what the form reported itself.
#[component]
fn GroupMetaForm(props: &GroupMetaFormProps) -> Html {
    let translate = use_translation();
    let form_state: UseReducerHandle<FailoverGroupMetaFormState> =
        use_default_form_reducer!(FailoverGroupMetaFormState { form: props.initial.clone() });
    let synced = use_mut_ref(|| props.initial.clone());
    let reported = use_mut_ref(Vec::<FailoverGroupMeta>::new);

    {
        let form_state = form_state.clone();
        let synced = synced.clone();
        let reported = reported.clone();
        use_effect_with(props.initial.clone(), move |initial| {
            if *synced.borrow() != *initial {
                *synced.borrow_mut() = initial.clone();
                let echo = reported.borrow().iter().position(|value| value == initial);
                if let Some(position) = echo {
                    reported.borrow_mut().drain(..=position);
                } else {
                    reported.borrow_mut().clear();
                    if form_state.form.normalized() != *initial {
                        form_state.dispatch(FailoverGroupMetaFormAction::SetAll(initial.clone()));
                    }
                }
            }
            || ()
        });
    }
    {
        let on_change = props.on_change.clone();
        let synced = synced.clone();
        let reported = reported.clone();
        use_effect_with(form_state.form.clone(), move |form| {
            let normalized = form.normalized();
            if normalized != *synced.borrow() {
                reported.borrow_mut().push(normalized);
                on_change.emit(form.clone());
            }
            || ()
        });
    }

    html! {
        <div class="tp__failover-groups__meta">
            { edit_field_text_option!(form_state, translate.t("LABEL.NAME"), name, FailoverGroupMetaFormAction::Name) }
            { edit_field_text_option!(form_state, translate.t("LABEL.LOGO"), logo, FailoverGroupMetaFormAction::Logo) }
            { edit_field_text_option!(form_state, translate.t("LABEL.EPG_CHANNEL_ID"), epg_channel_id,
                FailoverGroupMetaFormAction::EpgChannelId) }
            { edit_field_text_option!(form_state, translate.t("LABEL.FAILOVER_GROUPS_PLAYLIST_GROUP"), playlist_group,
                FailoverGroupMetaFormAction::PlaylistGroup) }
        </div>
    }
}

#[derive(Properties, PartialEq)]
pub struct GroupDetailProps {
    pub group: GroupRef,
    pub row: Option<GroupRow>,
    pub dto: Option<Rc<FailoverGroupDto>>,
    pub draft: Rc<TargetFailoverOverridesDto>,
    pub health: Option<Rc<FailoverGroupHealthDto>>,
    /// All rows of the list, the targets of "move to".
    pub rows: Rc<Vec<GroupRow>>,
    pub read_only: bool,
    pub busy: bool,
    pub on_op: Callback<DraftOp>,
    pub on_back: Callback<()>,
}

#[component]
pub fn GroupDetail(props: &GroupDetailProps) -> Html {
    let translate = use_translation();
    let moving = use_state(|| None::<ChannelMemberRefDto>);
    let confirm_delete = use_state(|| false);
    let group = &props.group;
    let is_manual = matches!(group, GroupRef::Manual(_));
    let state = props.row.as_ref().map_or(RowState::Active, |row| row.state);
    let title = props
        .dto
        .as_ref()
        .map(|dto| dto.name.clone())
        .or_else(|| props.row.as_ref().map(|row| row.name.clone()))
        .unwrap_or_default();
    let has_override =
        matches!(group, GroupRef::Auto(key) if props.draft.auto_overrides.iter().any(|e| e.auto_key == *key));
    let editable = !props.read_only;

    let on_meta = {
        let on_op = props.on_op.clone();
        let group = group.clone();
        Callback::from(move |meta: FailoverGroupMeta| {
            on_op.emit(DraftOp::SetMeta { group: group.clone(), patch: meta.into_patch() });
        })
    };

    let policy = policy_of(&props.draft, group);
    let policy_options = Rc::new(
        std::iter::once((POLICY_DEFAULT.to_string(), translate.t("LABEL.FAILOVER_GROUPS_GLOBAL"), policy.is_none()))
            .chain(
                FailoverGroupPolicy::ALL
                    .iter()
                    .map(|option| (option.to_string(), option.to_string(), policy == Some(*option))),
            )
            .map(|(id, label, selected)| DropDownOption::new(&id, html! { label }, selected))
            .collect::<Vec<_>>(),
    );
    let on_policy = {
        let on_op = props.on_op.clone();
        let group = group.clone();
        Callback::from(move |(_, selection): (String, DropDownSelection)| {
            let Some(id) = selection_first(&selection) else { return };
            let policy = if id == POLICY_DEFAULT { None } else { FailoverGroupPolicy::from_str(id).ok() };
            on_op.emit(DraftOp::SetPolicy { group: group.clone(), policy });
        })
    };

    let on_reset = {
        let on_op = props.on_op.clone();
        let key = match group {
            GroupRef::Auto(key) => Some(key.clone()),
            GroupRef::Manual(_) => None,
        };
        Callback::from(move |_: String| {
            if let Some(auto_key) = key.clone() {
                on_op.emit(DraftOp::ResetAuto { auto_key });
            }
        })
    };
    let on_dissolve = {
        let on_op = props.on_op.clone();
        let group = group.clone();
        let confirm_delete = confirm_delete.clone();
        Callback::from(move |_: String| {
            if matches!(group, GroupRef::Manual(_)) {
                confirm_delete.set(true);
            } else {
                on_op.emit(DraftOp::Dissolve { group: group.clone() });
            }
        })
    };
    let on_delete_confirmed = {
        let on_op = props.on_op.clone();
        let group = group.clone();
        let confirm_delete = confirm_delete.clone();
        Callback::from(move |result: DialogResult| {
            confirm_delete.set(false);
            if result == DialogResult::Ok {
                on_op.emit(DraftOp::Dissolve { group: group.clone() });
            }
        })
    };
    let on_move_to = {
        let moving = moving.clone();
        Callback::from(move |member: ChannelMemberRefDto| moving.set(Some(member)))
    };
    let targets: Vec<&GroupRow> =
        props.rows.iter().filter(|row| row.state == RowState::Active && row.group != *group).collect();
    let on_move_pick = {
        let moving = moving.clone();
        let on_op = props.on_op.clone();
        let from = group.clone();
        let targets: Vec<GroupRef> = targets.iter().map(|row| row.group.clone()).collect();
        Callback::from(move |index: String| {
            if let (Some(member), Some(to)) =
                ((*moving).clone(), index.parse::<usize>().ok().and_then(|i| targets.get(i)))
            {
                on_op.emit(DraftOp::MoveMember { from: from.clone(), to: to.clone(), member });
            }
            moving.set(None);
        })
    };
    let on_move_cancel = {
        let moving = moving.clone();
        Callback::from(move |()| moving.set(None))
    };

    let members = props.dto.as_ref().map(|dto| Rc::new(dto.members.clone())).unwrap_or_default();
    let unresolved = props.dto.as_ref().map(|dto| dto.unresolved.clone()).unwrap_or_default();

    html! {
        <div class="tp__failover-groups__detail">
            <div class="tp__failover-groups__detail-header">
                <TextButton name="back" icon="Back" class="tertiary tp__failover-groups__back"
                    title={translate.t("LABEL.BACK")} onclick={props.on_back.reform(|_: String| ())}/>
                <h3>{ title }</h3>
                if let Some(row) = props.row.as_ref() {
                    <Chip class={Some(origin_chip_class(row.origin).to_string())}
                        label={translate.t(match row.state {
                            RowState::Active => origin_label_key(row.origin),
                            RowState::Inactive => "LABEL.FAILOVER_GROUPS_INACTIVE",
                            RowState::Dissolved => "LABEL.FAILOVER_GROUPS_DISSOLVED",
                        })}/>
                }
                if editable && (has_override || state == RowState::Dissolved) {
                    <TextButton name="reset" icon="Refresh" class="tertiary"
                        title={translate.t("LABEL.FAILOVER_GROUPS_RESET_AUTO")} onclick={on_reset}/>
                }
            </div>
            if state == RowState::Dissolved {
                <p class="tp__failover-groups__notice">{ translate.t("MESSAGES.FAILOVER_GROUPS_DISSOLVED_HINT") }</p>
            } else {
                if state == RowState::Inactive {
                    <p class="tp__failover-groups__notice">{ translate.t("MESSAGES.FAILOVER_GROUPS_INACTIVE_HINT") }</p>
                }
                if let Some(dto) = props.dto.as_ref() {
                    <dl class="tp__failover-groups__summary" aria-label={translate.t("LABEL.FAILOVER_GROUPS_CURRENT_VALUES")}>
                        <dt>{ translate.t("LABEL.NAME") }</dt><dd>{ &dto.name }</dd>
                        <dt>{ translate.t("LABEL.EPG_CHANNEL_ID") }</dt><dd>{ dto.epg_channel_id.clone().unwrap_or_default() }</dd>
                        <dt>{ translate.t("LABEL.FAILOVER_GROUPS_PLAYLIST_GROUP") }</dt><dd>{ dto.playlist_group.clone().unwrap_or_default() }</dd>
                        <dt>{ translate.t("LABEL.FAILOVER_GROUPS_POLICY") }</dt><dd>{ dto.policy.to_string() }</dd>
                    </dl>
                }
                if editable {
                    <GroupMetaForm key={format!("{group:?}")} initial={meta_of(&props.draft, group)} on_change={on_meta}/>
                    <div class="tp__form-field tp__form-field__text">
                        <FieldLabel label={translate.t("LABEL.FAILOVER_GROUPS_POLICY")}
                            field_id="FAILOVER_GROUP_META.POLICY"/>
                        <Select name="FAILOVER_GROUP_META.POLICY" multi_select={false} on_select={on_policy}
                            options={policy_options}/>
                    </div>
                }
                <MemberTable
                    group={group.clone()}
                    members={members}
                    health={props.health.clone()}
                    read_only={props.read_only}
                    busy={props.busy}
                    on_op={props.on_op.clone()}
                    on_move_to={on_move_to}/>
                if !unresolved.is_empty() {
                    <div class="tp__failover-groups__unresolved">
                        <span>{ translate.t("LABEL.FAILOVER_GROUPS_UNRESOLVED") }</span>
                        { for unresolved.iter().map(|member| {
                            let label = format!("{}/{}", member.input, member.stream_id);
                            let on_op = props.on_op.clone();
                            let group = group.clone();
                            let member = member.clone();
                            html! {
                                <Chip key={label.clone()} label={label.clone()} removable={editable}
                                    on_remove={Callback::from(move |_: String| {
                                        on_op.emit(DraftOp::ForgetRef { group: group.clone(), member: member.clone() });
                                    })}/>
                            }
                        }) }
                    </div>
                }
                if editable {
                    <div class="tp__failover-groups__detail-footer">
                        <TextButton name="dissolve" icon="Delete" class="secondary"
                            title={translate.t(if is_manual { "LABEL.FAILOVER_GROUPS_DELETE_GROUP" } else { "LABEL.FAILOVER_GROUPS_DISSOLVE" })}
                            onclick={on_dissolve}/>
                    </div>
                }
            }
            if moving.is_some() {
                <PickerDialog
                    title={translate.t("LABEL.FAILOVER_GROUPS_MOVE_TO")}
                    items={targets.iter().enumerate()
                        .map(|(index, row)| PickerItem { id: index.to_string(), label: row.name.clone() })
                        .collect::<Vec<_>>()}
                    on_pick={on_move_pick}
                    on_cancel={on_move_cancel}/>
            }
            if *confirm_delete {
                { in_body(html! {
                    <ConfirmDialog title={translate.t("MESSAGES.FAILOVER_GROUPS_CONFIRM_DELETE_GROUP")}
                        ok_caption="LABEL.FAILOVER_GROUPS_DELETE_GROUP" cancel_caption="LABEL.CANCEL"
                        on_confirm={on_delete_confirmed}/>
                }) }
            }
        </div>
    }
}
