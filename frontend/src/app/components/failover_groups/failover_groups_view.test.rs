use super::*;
use crate::app::components::failover_groups::{
    candidate_panel::{add_members_op, membership_of, page_count, with_current_groups, Membership},
    diagnostics_panel::{group_ref_for_key, holders, problem_total},
    group_detail::{meta_of, policy_of, FailoverGroupMeta},
    group_list::{origin_chip_class, origin_label_key, visible_rows, GroupFilter, GroupRow, RowState},
    member_table::{member_action_op, member_rows, MemberAction},
};
use shared::model::{
    AutoGroupOverrideDto, ChannelMemberRefDto, FailoverGroupDto, FailoverGroupListDto, FailoverGroupMemberDto,
    FailoverGroupOrigin, FailoverGroupPolicy, FailoverOverrideDiagnosticsDto, FailoverTargetState,
    InactiveManualGroupDto, ManualFailoverGroupDto, MemberHealthDto, ScoreBreakdown, SearchRequest,
    TargetFailoverOverridesDto, UnresolvedRefDto, VirtualId,
};

fn member(input: &str, stream_id: &str) -> ChannelMemberRefDto {
    ChannelMemberRefDto { input: input.to_string(), stream_id: stream_id.to_string() }
}

fn status(name: &str, file: Option<&str>, state: FailoverTargetState) -> FailoverGroupTargetStatusDto {
    FailoverGroupTargetStatusDto {
        name: name.to_string(),
        etag: None,
        applied_etag: None,
        file: file.map(str::to_string),
        findings: Vec::new(),
        group_count: 0,
        problem_count: 0,
        state,
    }
}

fn dto_member(input: &str, stream_id: &str) -> FailoverGroupMemberDto {
    FailoverGroupMemberDto {
        member_vid: Some(VirtualId::new(1)),
        member_ref: member(input, stream_id),
        title: format!("{input} {stream_id}"),
        input_name: input.to_string(),
        provider_id: 1,
        has_archive: false,
    }
}

fn dto_group(key: &str, origin: FailoverGroupOrigin, members: &[FailoverGroupMemberDto]) -> FailoverGroupDto {
    FailoverGroupDto {
        group_vid: Some(VirtualId::new(7)),
        key: key.to_string(),
        origin,
        policy: FailoverGroupPolicy::Auto,
        name: key.to_uppercase(),
        logo: None,
        epg_channel_id: None,
        playlist_group: None,
        members: members.to_vec(),
        unresolved: Vec::new(),
    }
}

fn row(name: &str, origin: FailoverGroupOrigin, state: RowState, problem: bool) -> GroupRow {
    GroupRow {
        group: if origin == FailoverGroupOrigin::Manual {
            GroupRef::Manual(name.to_string())
        } else {
            GroupRef::Auto(name.to_string())
        },
        name: name.to_string(),
        origin,
        member_count: 2,
        state,
        problem,
        display_stale: false,
    }
}

#[test]
fn save_status_ranks_file_error_before_unsaved_before_apply_pending() {
    assert_eq!(save_status(true, true, Some("a"), Some("b")), SaveStatus::FileError);
    assert_eq!(save_status(false, true, Some("a"), Some("b")), SaveStatus::Unsaved);
    assert_eq!(save_status(false, false, Some("a"), Some("b")), SaveStatus::ApplyPending);
    assert_eq!(save_status(false, false, Some("a"), None), SaveStatus::ApplyPending);
    assert_eq!(save_status(false, false, None, Some("b")), SaveStatus::ApplyPending);
    assert_eq!(save_status(false, false, Some("a"), Some("a")), SaveStatus::Applied);
    assert_eq!(save_status(false, false, None, None), SaveStatus::Applied);
}

#[test]
fn every_status_has_its_own_label_key() {
    let keys: Vec<&str> = [SaveStatus::FileError, SaveStatus::Unsaved, SaveStatus::ApplyPending, SaveStatus::Applied]
        .into_iter()
        .map(status_label_key)
        .collect();
    let mut unique = keys.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), keys.len());
    assert_eq!(status_chip_class(SaveStatus::Applied), "active");
    assert_eq!(status_chip_class(SaveStatus::FileError), "inactive");
}

#[test]
fn shared_file_needs_a_second_section_in_the_same_file() {
    let targets = vec![
        status("main", Some("failover.yml"), FailoverTargetState::Enabled),
        status("kids", Some("failover.yml"), FailoverTargetState::Enabled),
        status("sport", Some("failover.d/sport.yml"), FailoverTargetState::Enabled),
        status("new", None, FailoverTargetState::Enabled),
    ];
    assert!(file_is_shared(&targets, "main"));
    assert!(file_is_shared(&targets, "kids"));
    assert!(!file_is_shared(&targets, "sport"));
    assert!(!file_is_shared(&targets, "new"));
    assert!(!file_is_shared(&targets, "unknown"));
}

#[test]
fn orphan_and_disabled_sections_are_not_editable_targets() {
    let targets = FailoverGroupTargetsDto {
        targets: vec![
            status("main", None, FailoverTargetState::Enabled),
            status("gone", Some("f.yml"), FailoverTargetState::Orphan),
            status("plain", Some("f.yml"), FailoverTargetState::Disabled),
        ],
        ..Default::default()
    };
    assert_eq!(editable_targets(&targets), vec!["main".to_string()]);
    let orphans = sections_in_state(&targets, FailoverTargetState::Orphan);
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].name, "gone");
    let disabled = sections_in_state(&targets, FailoverTargetState::Disabled);
    assert_eq!(disabled.len(), 1);
    assert_eq!(disabled[0].name, "plain");
}

#[test]
fn display_stale_marks_header_and_rows() {
    assert!(!has_display_stale(None));
    let mut list = FailoverGroupListDto {
        groups: vec![
            dto_group("rtl|rtl.de", FailoverGroupOrigin::Auto, &[]),
            dto_group("g1", FailoverGroupOrigin::Manual, &[]),
        ],
        ..Default::default()
    };
    assert!(!has_display_stale(Some(&list)));
    assert!(build_rows(Some(&list), &TargetFailoverOverridesDto::new("t")).iter().all(|row| !row.display_stale));
    list.display_stale = vec!["g1".to_string()];
    assert!(has_display_stale(Some(&list)));
    let rows = build_rows(Some(&list), &TargetFailoverOverridesDto::new("t"));
    let flags: Vec<(&str, bool)> = rows.iter().map(|row| (row.name.as_str(), row.display_stale)).collect();
    assert_eq!(flags, vec![("RTL|RTL.DE", false), ("G1", true)]);
}

#[test]
fn origin_helpers_cover_every_origin() {
    assert_eq!(origin_chip_class(FailoverGroupOrigin::Auto), "tp__failover-groups__origin--auto");
    assert_eq!(origin_chip_class(FailoverGroupOrigin::AutoEdited), "tp__failover-groups__origin--edited");
    assert_eq!(origin_chip_class(FailoverGroupOrigin::Manual), "tp__failover-groups__origin--manual");
    assert_eq!(origin_label_key(FailoverGroupOrigin::AutoEdited), "LABEL.FAILOVER_GROUPS_ORIGIN_EDITED");
}

#[test]
fn filters_select_rows_by_origin_state_and_problem() {
    let auto = row("zdf", FailoverGroupOrigin::Auto, RowState::Active, false);
    let edited = row("rtl", FailoverGroupOrigin::AutoEdited, RowState::Active, true);
    let manual = row("sport", FailoverGroupOrigin::Manual, RowState::Active, false);
    let inactive = row("lonely", FailoverGroupOrigin::Manual, RowState::Inactive, true);
    let dissolved = row("arte", FailoverGroupOrigin::AutoEdited, RowState::Dissolved, false);
    let rows = vec![auto, edited, manual, inactive, dissolved];
    let names =
        |filter| visible_rows(&rows, filter, &SearchRequest::Clear).iter().map(|r| r.name.clone()).collect::<Vec<_>>();

    assert_eq!(names(GroupFilter::All), ["zdf", "rtl", "sport", "lonely"]);
    assert_eq!(names(GroupFilter::Auto), ["zdf"]);
    assert_eq!(names(GroupFilter::Edited), ["rtl"]);
    assert_eq!(names(GroupFilter::Manual), ["sport", "lonely"]);
    assert_eq!(names(GroupFilter::Dissolved), ["arte"]);
    assert_eq!(names(GroupFilter::Problems), ["rtl", "lonely"]);
    assert_eq!(visible_rows(&rows, GroupFilter::All, &SearchRequest::Text(" SP ".to_string(), None)).len(), 1);
    assert_eq!(visible_rows(&rows, GroupFilter::All, &SearchRequest::Regexp("^(zdf|rtl)$".to_string(), None)).len(), 2);
    assert_eq!(visible_rows(&rows, GroupFilter::All, &SearchRequest::Regexp("(".to_string(), None)).len(), 0);
    assert!(GroupFilter::ALL.iter().all(|filter| GroupFilter::from_id(filter.id()) == Some(*filter)));
    assert_eq!(GroupFilter::from_id("nope"), None);
}

#[test]
fn rows_list_groups_then_inactive_manual_groups_then_dissolved_keys() {
    let mut draft = TargetFailoverOverridesDto::new("T");
    draft.groups.push(ManualFailoverGroupDto {
        id: "solo".to_string(),
        name: "Solo channel".to_string(),
        ..Default::default()
    });
    let mut broken = dto_group("rtl", FailoverGroupOrigin::AutoEdited, &[dto_member("a", "1"), dto_member("b", "2")]);
    broken.unresolved.push(member("a", "gone"));
    let preview = FailoverGroupListDto {
        groups: vec![dto_group("zdf", FailoverGroupOrigin::Auto, &[dto_member("a", "3")]), broken],
        diagnostics: FailoverOverrideDiagnosticsDto {
            inactive_manual_groups: vec![InactiveManualGroupDto { id: "solo".to_string(), resolved: 1 }],
            dissolved: vec!["arte".to_string()],
            ..Default::default()
        },
        ..Default::default()
    };

    let rows = build_rows(Some(&preview), &draft);
    assert_eq!(rows.len(), 4);
    assert_eq!((rows[0].name.as_str(), rows[0].problem, rows[0].member_count), ("ZDF", false, 1));
    assert!(rows[1].problem, "unresolved references mark the group");
    assert_eq!(
        (rows[2].group.clone(), rows[2].name.as_str(), rows[2].state),
        (GroupRef::Manual("solo".into()), "Solo channel", RowState::Inactive)
    );
    assert_eq!((rows[3].group.clone(), rows[3].state), (GroupRef::Auto("arte".into()), RowState::Dissolved));
    assert_eq!(build_rows(None, &draft).len(), 0);
}

#[test]
fn member_actions_become_draft_operations() {
    let refs = vec![member("p", "1"), member("p", "2"), member("p", "3")];
    let group = GroupRef::Auto("zdf".to_string());

    assert_eq!(
        member_action_op(&group, &refs, 1, MemberAction::Up),
        Some(DraftOp::Reorder { group: group.clone(), from: 1, to: 0, current: refs.clone() })
    );
    assert_eq!(member_action_op(&group, &refs, 0, MemberAction::Up), None);
    assert_eq!(
        member_action_op(&group, &refs, 1, MemberAction::Down),
        Some(DraftOp::Reorder { group: group.clone(), from: 1, to: 2, current: refs.clone() })
    );
    assert_eq!(member_action_op(&group, &refs, 2, MemberAction::Down), None);
    assert_eq!(
        member_action_op(&group, &refs, 2, MemberAction::First),
        Some(DraftOp::MoveFirst { group: group.clone(), index: 2, current: refs.clone() })
    );
    assert_eq!(member_action_op(&group, &refs, 0, MemberAction::First), None);
    assert_eq!(
        member_action_op(&group, &refs, 0, MemberAction::MoveTo(2)),
        Some(DraftOp::Reorder { group: group.clone(), from: 0, to: 2, current: refs.clone() })
    );
    assert_eq!(member_action_op(&group, &refs, 0, MemberAction::MoveTo(0)), None);
    assert_eq!(member_action_op(&group, &refs, 0, MemberAction::MoveTo(9)), None);
    assert_eq!(
        member_action_op(&group, &refs, 1, MemberAction::Remove),
        Some(DraftOp::RemoveMember { group: group.clone(), member: member("p", "2") })
    );
    assert_eq!(
        member_action_op(&group, &refs, 1, MemberAction::Exclude),
        Some(DraftOp::ExcludeMember { group: Some(group.clone()), member: member("p", "2") })
    );
    assert_eq!(member_action_op(&group, &refs, 5, MemberAction::Remove), None);
}

#[test]
fn member_rows_pair_health_by_reference() {
    let members = vec![dto_member("a", "1"), dto_member("b", "2")];
    let health = FailoverGroupHealthDto {
        members: vec![MemberHealthDto {
            member_vid: VirtualId::new(2),
            member_ref: member("b", "2"),
            title: "b 2".to_string(),
            rank: 1,
            qos_24h: Some(90),
            qos_7d: None,
            confidence: None,
            failure_streak: 0,
            resolution: None,
            codec: None,
            bitrate_kbps: Some(3000),
            circuit: "closed".to_string(),
            circuit_remaining_secs: None,
            input_circuit: "closed".to_string(),
            slots_used: 1,
            slots_max: 2,
            score: ScoreBreakdown::default(),
        }],
        ..Default::default()
    };
    let rows = member_rows(&members, Some(&health));
    assert!(rows[0].health.is_none());
    assert_eq!(rows[1].health.as_ref().map(|h| h.rank), Some(1));
    assert_eq!(rows[1].index, 1);
    assert!(member_rows(&members, None).iter().all(|row| row.health.is_none()));
}

#[test]
fn selected_candidates_become_one_operation_with_their_current_groups() {
    let target = GroupRef::Manual("sport".to_string());
    let other = GroupRef::Auto("zdf".to_string());
    let (free, held) = (member("p", "1"), member("p", "2"));
    let mut membership = Membership::new();
    membership.insert(held.clone(), (other.clone(), "ZDF".to_string()));

    assert_eq!(
        with_current_groups([&free, &held], &membership),
        vec![(free.clone(), None), (held.clone(), Some(other.clone()))]
    );
    assert_eq!(
        add_members_op(&target, [&free, &held], &membership),
        Some(DraftOp::AddMembers {
            group: target.clone(),
            members: vec![(free.clone(), None), (held.clone(), Some(other))],
        })
    );
    assert_eq!(add_members_op(&target, [], &membership), None);
}

#[test]
fn pages_round_up_and_never_drop_below_one() {
    assert_eq!(page_count(0, 100), 1);
    assert_eq!(page_count(100, 100), 1);
    assert_eq!(page_count(101, 100), 2);
    assert_eq!(page_count(250, 50), 5);
    assert_eq!(page_count(5, 0), 5);
}

#[test]
fn membership_maps_every_resolved_member_to_its_group() {
    let preview = FailoverGroupListDto {
        groups: vec![
            dto_group("zdf", FailoverGroupOrigin::Auto, &[dto_member("a", "1"), dto_member("b", "2")]),
            dto_group("sport", FailoverGroupOrigin::Manual, &[dto_member("c", "3")]),
        ],
        ..Default::default()
    };
    let membership = membership_of(Some(&preview), &TargetFailoverOverridesDto::new("T"));
    assert_eq!(membership.len(), 3);
    assert_eq!(membership[&member("b", "2")], (GroupRef::Auto("zdf".to_string()), "ZDF".to_string()));
    assert_eq!(membership[&member("c", "3")].0, GroupRef::Manual("sport".to_string()));
    assert!(!membership.contains_key(&member("x", "9")));
    assert!(membership_of(None, &TargetFailoverOverridesDto::new("T")).is_empty());
}

#[test]
fn membership_sees_members_of_manual_groups_the_preview_does_not_list() {
    let mut draft = TargetFailoverOverridesDto::new("T");
    draft.groups.push(ManualFailoverGroupDto {
        id: "inactive".to_string(),
        name: "Too small".to_string(),
        members: vec![member("a", "1")],
        ..Default::default()
    });
    let membership = membership_of(Some(&FailoverGroupListDto::default()), &draft);
    assert_eq!(membership[&member("a", "1")], (GroupRef::Manual("inactive".to_string()), "Too small".to_string()));
    assert_eq!(membership_of(None, &draft).len(), 1, "the draft alone is enough");
}

#[test]
fn membership_prefers_the_draft_over_the_preview_and_manual_over_add() {
    let preview = FailoverGroupListDto {
        groups: vec![dto_group("zdf", FailoverGroupOrigin::Auto, &[dto_member("a", "1"), dto_member("b", "2")])],
        ..Default::default()
    };
    let mut draft = TargetFailoverOverridesDto::new("T");
    draft.groups.push(ManualFailoverGroupDto {
        id: "sport".to_string(),
        name: "Sport".to_string(),
        members: vec![member("a", "1"), member("c", "3")],
        ..Default::default()
    });
    draft.auto_overrides.push(AutoGroupOverrideDto {
        auto_key: "zdf".to_string(),
        add: vec![member("c", "3"), member("d", "4")],
        ..Default::default()
    });
    draft.auto_overrides.push(AutoGroupOverrideDto {
        auto_key: "new".to_string(),
        name: Some("New name".to_string()),
        add: vec![member("e", "5")],
        ..Default::default()
    });
    let membership = membership_of(Some(&preview), &draft);
    assert_eq!(membership[&member("a", "1")].0, GroupRef::Manual("sport".to_string()));
    assert_eq!(membership[&member("b", "2")].0, GroupRef::Auto("zdf".to_string()));
    assert_eq!(membership[&member("c", "3")].0, GroupRef::Manual("sport".to_string()), "manual wins over add");
    assert_eq!(membership[&member("d", "4")], (GroupRef::Auto("zdf".to_string()), "ZDF".to_string()));
    assert_eq!(membership[&member("e", "5")], (GroupRef::Auto("new".to_string()), "New name".to_string()));
}

#[test]
fn holders_and_keys_point_at_the_draft_entries() {
    let channel = member("p", "1");
    let mut draft = TargetFailoverOverridesDto::new("T");
    draft.groups.push(ManualFailoverGroupDto {
        id: "sport".to_string(),
        name: "Sport".to_string(),
        members: vec![channel.clone()],
        ..Default::default()
    });
    draft.auto_overrides.push(AutoGroupOverrideDto {
        auto_key: "zdf".to_string(),
        add: vec![channel.clone()],
        ..Default::default()
    });
    draft.auto_overrides.push(AutoGroupOverrideDto {
        auto_key: "rtl".to_string(),
        order: vec![channel.clone()],
        ..Default::default()
    });
    assert_eq!(
        holders(&draft, &channel),
        vec![GroupRef::Manual("sport".to_string()), GroupRef::Auto("zdf".to_string())]
    );
    assert_eq!(group_ref_for_key(&draft, "sport"), GroupRef::Manual("sport".to_string()));
    assert_eq!(group_ref_for_key(&draft, "zdf"), GroupRef::Auto("zdf".to_string()));
}

#[test]
fn problem_total_adds_every_kind_of_problem() {
    let diagnostics = FailoverOverrideDiagnosticsDto {
        stale_auto_overrides: vec!["a".to_string()],
        inactive_manual_groups: vec![InactiveManualGroupDto { id: "b".to_string(), resolved: 0 }],
        conflicting_refs: vec![member("p", "1"), member("p", "2")],
        unresolved_refs: vec![UnresolvedRefDto { group: "c".to_string(), member_ref: member("p", "3") }],
        dissolved: vec!["d".to_string()],
    };
    let findings = vec![OverrideValidationError::EmptyManualId];
    assert_eq!(problem_total(&diagnostics, &findings, 3, 2), 1 + 1 + 1 + 2 + 3 + 2);
    assert_eq!(problem_total(&FailoverOverrideDiagnosticsDto::default(), &[], 0, 0), 0);
}

#[test]
fn metadata_and_policy_are_read_from_the_draft_override() {
    let mut draft = TargetFailoverOverridesDto::new("T");
    draft.groups.push(ManualFailoverGroupDto {
        id: "sport".to_string(),
        name: "Sport".to_string(),
        logo: Some("https://example.invalid/l.png".to_string()),
        policy: Some(FailoverGroupPolicy::Priority),
        ..Default::default()
    });
    draft.auto_overrides.push(AutoGroupOverrideDto {
        auto_key: "zdf".to_string(),
        name: Some("ZDF HD".to_string()),
        epg_channel_id: Some("zdf.de".to_string()),
        ..Default::default()
    });

    let manual = GroupRef::Manual("sport".to_string());
    assert_eq!(meta_of(&draft, &manual).name.as_deref(), Some("Sport"));
    assert_eq!(meta_of(&draft, &manual).logo.as_deref(), Some("https://example.invalid/l.png"));
    assert_eq!(policy_of(&draft, &manual), Some(FailoverGroupPolicy::Priority));

    let auto = GroupRef::Auto("zdf".to_string());
    assert_eq!(meta_of(&draft, &auto).epg_channel_id.as_deref(), Some("zdf.de"));
    assert_eq!(policy_of(&draft, &auto), None);

    let untouched = GroupRef::Auto("rtl".to_string());
    assert_eq!(meta_of(&draft, &untouched), FailoverGroupMeta::default());
}

#[test]
fn form_values_are_normalized_like_the_draft_stores_them() {
    let meta = FailoverGroupMeta {
        name: Some("  Das Erste ".to_string()),
        logo: Some("   ".to_string()),
        epg_channel_id: None,
        playlist_group: Some("News".to_string()),
    };
    let normalized = meta.normalized();
    assert_eq!(normalized.name.as_deref(), Some("Das Erste"));
    assert_eq!(normalized.logo, None);
    assert_eq!(normalized.playlist_group.as_deref(), Some("News"));
    assert_eq!(normalized, normalized.normalized());
}

#[test]
fn a_finished_playlist_update_publishes_changes_for_its_processed_targets_only() {
    let run = |state| PlaylistUpdateRunStateEvent::uncorrelated(state).with_targets(vec!["main".to_string()]);
    for state in [PlaylistUpdateState::Success, PlaylistUpdateState::Partial] {
        assert!(update_publishes_changes(&run(state), "main"));
        assert!(!update_publishes_changes(&run(state), "kids"));
        assert!(!update_publishes_changes(&PlaylistUpdateRunStateEvent::uncorrelated(state), "main"));
    }
    assert!(!update_publishes_changes(&run(PlaylistUpdateState::Failure), "main"));
}
