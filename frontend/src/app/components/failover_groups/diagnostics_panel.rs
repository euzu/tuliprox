//! Problems of the overrides: findings, stale and conflicting corrections, groups that cannot be formed and
//! sections of targets that no longer exist.

use super::{
    dialogs::{PickerDialog, PickerItem},
    draft::{random_id_suffix, DraftOp, GroupRef},
};
use crate::{
    app::components::{AppIcon, TextButton},
    i18n::use_translation,
};
use shared::model::{
    ChannelMemberRefDto, FailoverGroupTargetStatusDto, FailoverOverrideDiagnosticsDto, OverrideValidationError,
    TargetFailoverOverridesDto,
};
use std::rc::Rc;
use yew::prelude::*;

/// The draft entries that hold `member`: manual groups by member list, automatic groups by `add` list.
pub fn holders(draft: &TargetFailoverOverridesDto, member: &ChannelMemberRefDto) -> Vec<GroupRef> {
    draft
        .groups
        .iter()
        .filter(|group| group.members.contains(member))
        .map(|group| GroupRef::Manual(group.id.clone()))
        .chain(
            draft
                .auto_overrides
                .iter()
                .filter(|entry| entry.add.contains(member))
                .map(|entry| GroupRef::Auto(entry.auto_key.clone())),
        )
        .collect()
}

/// The entry a diagnostics `group` key points at: a manual group id when the draft has one, an auto key otherwise.
pub fn group_ref_for_key(draft: &TargetFailoverOverridesDto, key: &str) -> GroupRef {
    if draft.groups.iter().any(|group| group.id == key) {
        GroupRef::Manual(key.to_string())
    } else {
        GroupRef::Auto(key.to_string())
    }
}

/// Number of entries the problems view lists for the open target and the sections of unknown or disabled targets.
pub fn problem_total(
    diagnostics: &FailoverOverrideDiagnosticsDto,
    findings: &[OverrideValidationError],
    groups_with_unresolved: usize,
    sections: usize,
) -> usize {
    findings.len()
        + diagnostics.stale_auto_overrides.len()
        + diagnostics.inactive_manual_groups.len()
        + diagnostics.conflicting_refs.len()
        + groups_with_unresolved
        + sections
}

fn member_label(member: &ChannelMemberRefDto) -> String { format!("{}/{}", member.input, member.stream_id) }

#[derive(Properties, PartialEq)]
pub struct DiagnosticsPanelProps {
    pub diagnostics: FailoverOverrideDiagnosticsDto,
    pub findings: Vec<OverrideValidationError>,
    pub draft: Rc<TargetFailoverOverridesDto>,
    /// Sections of targets that do not exist.
    pub orphans: Vec<FailoverGroupTargetStatusDto>,
    /// Sections of targets that exist but have failover groups disabled.
    pub disabled: Vec<FailoverGroupTargetStatusDto>,
    /// Targets a section can be assigned to.
    pub assignable: Vec<String>,
    pub read_only: bool,
    pub on_op: Callback<DraftOp>,
    pub on_select: Callback<GroupRef>,
    /// `(section target, new target, etag of the section)`
    pub on_assign: Callback<(String, String, String)>,
    /// `(section target, etag of the section)`
    pub on_delete_orphan: Callback<(String, String)>,
}

#[component]
pub fn DiagnosticsPanel(props: &DiagnosticsPanelProps) -> Html {
    let translate = use_translation();
    let assigning = use_state(|| None::<(String, String)>);
    let diagnostics = &props.diagnostics;
    let read_only = props.read_only;

    let button = |name: &str, key: &str, icon: &str, onclick: Callback<String>| {
        html! { <TextButton name={name.to_string()} icon={icon.to_string()} class="tertiary"
        title={translate.t(key)} disabled={read_only} onclick={onclick}/> }
    };
    let op_button = |name: &str, key: &str, icon: &str, op: DraftOp| {
        let on_op = props.on_op.clone();
        button(name, key, icon, Callback::from(move |_| on_op.emit(op.clone())))
    };

    // Sections that cannot be edited: heading, hint and one row per section with the assign and delete actions.
    let section_list = |heading: &str, hint: &str, sections: &[FailoverGroupTargetStatusDto], note: Option<&str>| {
        html! {
            <>
                <h3>{ translate.t(heading) }</h3>
                <p class="tp__failover-groups__hint">{ translate.t(hint) }</p>
                <ul class="tp__failover-groups__problem-list">
                    { for sections.iter().map(|section| {
                        let etag = section.etag.clone().unwrap_or_default();
                        let assign = {
                            let assigning = assigning.clone();
                            let name = section.name.clone();
                            let etag = etag.clone();
                            Callback::from(move |_: String| assigning.set(Some((name.clone(), etag.clone()))))
                        };
                        let delete = {
                            let on_delete = props.on_delete_orphan.clone();
                            let name = section.name.clone();
                            Callback::from(move |_: String| on_delete.emit((name.clone(), etag.clone())))
                        };
                        html! {
                            <li key={section.name.clone()}>
                                <span class="tp__failover-groups__problem-name">
                                    { &section.name }
                                    if let Some(file) = section.file.as_ref() { { format!(" ({file})") } }
                                    if let Some(note) = note { { format!(": {}", translate.t(note)) } }
                                </span>
                                { button("assign", "LABEL.FAILOVER_GROUPS_ASSIGN_TO_TARGET", "Target", assign) }
                                { button("delete_section", "LABEL.FAILOVER_GROUPS_DELETE_SECTION", "Delete", delete) }
                            </li>
                        }
                    }) }
                </ul>
            </>
        }
    };

    let no_problems = props.findings.is_empty()
        && diagnostics.stale_auto_overrides.is_empty()
        && diagnostics.inactive_manual_groups.is_empty()
        && diagnostics.conflicting_refs.is_empty()
        && diagnostics.unresolved_refs.is_empty()
        && props.orphans.is_empty()
        && props.disabled.is_empty();

    let on_pick = {
        let assigning = assigning.clone();
        let on_assign = props.on_assign.clone();
        Callback::from(move |to: String| {
            if let Some((from, etag)) = (*assigning).clone() {
                on_assign.emit((from, to, etag));
            }
            assigning.set(None);
        })
    };
    let on_pick_cancel = {
        let assigning = assigning.clone();
        Callback::from(move |()| assigning.set(None))
    };

    html! {
        <section class="tp__failover-groups__diagnostics" aria-label={translate.t("LABEL.FAILOVER_GROUPS_PROBLEMS")}>
            if no_problems {
                <p class="tp__failover-groups__notice" role="status">{ translate.t("MESSAGES.FAILOVER_GROUPS_NO_PROBLEMS") }</p>
            }
            if !props.findings.is_empty() {
                <h3>{ translate.t("LABEL.FAILOVER_GROUPS_FINDINGS") }</h3>
                <ul class="tp__failover-groups__problem-list">
                    { for props.findings.iter().map(|finding| html! {
                        <li><span class="tp__failover-groups__problem"><AppIcon name="Warning"/></span>{ finding.to_string() }</li>
                    }) }
                </ul>
            }
            if !diagnostics.stale_auto_overrides.is_empty() {
                <h3>{ translate.t("LABEL.FAILOVER_GROUPS_STALE") }</h3>
                <p class="tp__failover-groups__hint">{ translate.t("MESSAGES.FAILOVER_GROUPS_STALE_HINT") }</p>
                <ul class="tp__failover-groups__problem-list">
                    { for diagnostics.stale_auto_overrides.iter().map(|key| html! {
                        <li key={key.clone()}>
                            <span class="tp__failover-groups__problem-name">{ key }</span>
                            { op_button("convert", "LABEL.FAILOVER_GROUPS_CONVERT_TO_MANUAL", "Group",
                                DraftOp::ConvertStaleToManual { auto_key: key.clone(), id_suffix: random_id_suffix() }) }
                            { op_button("remove", "LABEL.FAILOVER_GROUPS_REMOVE_OVERRIDE", "Delete",
                                DraftOp::ResetAuto { auto_key: key.clone() }) }
                        </li>
                    }) }
                </ul>
            }
            if !diagnostics.inactive_manual_groups.is_empty() {
                <h3>{ translate.t("LABEL.FAILOVER_GROUPS_INACTIVE_GROUPS") }</h3>
                <ul class="tp__failover-groups__problem-list">
                    { for diagnostics.inactive_manual_groups.iter().map(|inactive| {
                        let group = GroupRef::Manual(inactive.id.clone());
                        let name = props.draft.groups.iter().find(|entry| entry.id == inactive.id)
                            .map_or_else(|| inactive.id.clone(), |entry| entry.name.clone());
                        let on_select = props.on_select.clone();
                        let open = {
                            let group = group.clone();
                            Callback::from(move |_: String| on_select.emit(group.clone()))
                        };
                        html! {
                            <li key={inactive.id.clone()}>
                                <span class="tp__failover-groups__problem-name">
                                    { format!("{name} ({})", translate.t("MESSAGES.FAILOVER_GROUPS_RESOLVED_MEMBERS")
                                        .replace("{count}", &inactive.resolved.to_string())) }
                                </span>
                                <TextButton name="open" icon="Edit" class="tertiary" title={translate.t("LABEL.FAILOVER_GROUPS_OPEN_GROUP")}
                                    onclick={open}/>
                                { op_button("delete", "LABEL.FAILOVER_GROUPS_DELETE_GROUP", "Delete", DraftOp::Dissolve { group }) }
                            </li>
                        }
                    }) }
                </ul>
            }
            if !diagnostics.conflicting_refs.is_empty() {
                <h3>{ translate.t("LABEL.FAILOVER_GROUPS_CONFLICTS") }</h3>
                <p class="tp__failover-groups__hint">{ translate.t("MESSAGES.FAILOVER_GROUPS_CONFLICT_HINT") }</p>
                <ul class="tp__failover-groups__problem-list">
                    { for diagnostics.conflicting_refs.iter().map(|member| html! {
                        <li key={member_label(member)}>
                            <span class="tp__failover-groups__problem-name">{ member_label(member) }</span>
                            { for holders(&props.draft, member).into_iter().map(|group| {
                                let label = match &group {
                                    GroupRef::Manual(id) | GroupRef::Auto(id) => id.clone(),
                                };
                                let title = translate.t("LABEL.FAILOVER_GROUPS_REMOVE_FROM").replace("{group}", &label);
                                let on_op = props.on_op.clone();
                                let op = DraftOp::ForgetRef { group, member: member.clone() };
                                html! { <TextButton name="remove" icon="Delete" class="tertiary" title={title}
                                    disabled={read_only} onclick={Callback::from(move |_| on_op.emit(op.clone()))}/> }
                            }) }
                        </li>
                    }) }
                </ul>
            }
            if !diagnostics.unresolved_refs.is_empty() {
                <h3>{ translate.t("LABEL.FAILOVER_GROUPS_UNRESOLVED") }</h3>
                <p class="tp__failover-groups__hint">{ translate.t("MESSAGES.FAILOVER_GROUPS_UNRESOLVED_HINT") }</p>
                <ul class="tp__failover-groups__problem-list">
                    { for diagnostics.unresolved_refs.iter().map(|unresolved| {
                        let group = group_ref_for_key(&props.draft, &unresolved.group);
                        html! {
                            <li key={format!("{}:{}", unresolved.group, member_label(&unresolved.member_ref))}>
                                <span class="tp__failover-groups__problem-name">
                                    { format!("{}: {}", unresolved.group, member_label(&unresolved.member_ref)) }
                                </span>
                                { op_button("remove", "LABEL.FAILOVER_GROUPS_REMOVE_REFERENCE", "Delete",
                                    DraftOp::ForgetRef { group, member: unresolved.member_ref.clone() }) }
                            </li>
                        }
                    }) }
                </ul>
            }
            if !props.orphans.is_empty() {
                { section_list("LABEL.FAILOVER_GROUPS_ORPHANS", "MESSAGES.FAILOVER_GROUPS_ORPHAN_HINT", &props.orphans, None) }
            }
            if !props.disabled.is_empty() {
                { section_list("LABEL.FAILOVER_GROUPS_DISABLED_SECTIONS", "MESSAGES.FAILOVER_GROUPS_DISABLED_HINT",
                    &props.disabled, Some("MESSAGES.FAILOVER_GROUPS_TARGET_DISABLED")) }
            }
            if assigning.is_some() {
                <PickerDialog
                    title={translate.t("LABEL.FAILOVER_GROUPS_ASSIGN_TO_TARGET")}
                    items={props.assignable.iter().map(|name| PickerItem { id: name.clone(), label: name.clone() }).collect::<Vec<_>>()}
                    on_pick={on_pick}
                    on_cancel={on_pick_cancel}/>
            }
        </section>
    }
}
