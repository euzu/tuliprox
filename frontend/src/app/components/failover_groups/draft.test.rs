use super::*;

fn member(input: &str, stream_id: &str) -> ChannelMemberRefDto {
    ChannelMemberRefDto { input: input.to_string(), stream_id: stream_id.to_string() }
}

fn section() -> TargetFailoverOverridesDto { TargetFailoverOverridesDto::new("Main TV") }

fn manual(id: &str, members: &[ChannelMemberRefDto]) -> ManualFailoverGroupDto {
    ManualFailoverGroupDto {
        id: id.to_string(),
        name: format!("Group {id}"),
        members: members.to_vec(),
        ..Default::default()
    }
}

fn auto(key: &str) -> GroupRef { GroupRef::Auto(key.to_string()) }

fn manual_ref(id: &str) -> GroupRef { GroupRef::Manual(id.to_string()) }

fn auto_entry<'a>(section: &'a TargetFailoverOverridesDto, key: &str) -> Option<&'a AutoGroupOverrideDto> {
    section.auto_overrides.iter().find(|entry| entry.auto_key == key)
}

#[test]
fn reorder_auto_group_writes_full_order() {
    let (a, b, c) = (member("p", "1"), member("p", "2"), member("p", "3"));
    let mut section = section();
    reorder(&mut section, &auto("zdf"), 2, 0, &[a.clone(), b.clone(), c.clone()]);
    assert_eq!(auto_entry(&section, "zdf").expect("override").order, vec![c, a, b]);
}

#[test]
fn reorder_manual_group_sorts_members_and_keeps_unresolved_at_end() {
    let (a, b, c, ghost) = (member("p", "1"), member("p", "2"), member("p", "3"), member("p", "gone"));
    let mut section = section();
    section.groups.push(manual("sport", &[a.clone(), ghost.clone(), b.clone(), c.clone()]));
    reorder(&mut section, &manual_ref("sport"), 0, 2, &[a.clone(), b.clone(), c.clone()]);
    assert_eq!(section.groups[0].members, vec![b, c, a, ghost]);
}

#[test]
fn reorder_with_same_or_invalid_positions_is_a_noop() {
    let (a, b) = (member("p", "1"), member("p", "2"));
    let mut section = section();
    reorder(&mut section, &auto("zdf"), 1, 1, &[a.clone(), b.clone()]);
    reorder(&mut section, &auto("zdf"), 0, 5, &[a, b]);
    assert!(section.is_empty());
}

#[test]
fn move_first_puts_member_at_front() {
    let (a, b, c) = (member("p", "1"), member("p", "2"), member("p", "3"));
    let mut section = section();
    move_first(&mut section, &auto("zdf"), 1, &[a.clone(), b.clone(), c.clone()]);
    assert_eq!(auto_entry(&section, "zdf").expect("override").order, vec![b, a, c]);
}

#[test]
fn add_member_to_auto_group_writes_add_once() {
    let a = member("p", "1");
    let mut section = section();
    add_member(&mut section, &auto("zdf"), &a);
    add_member(&mut section, &auto("zdf"), &a);
    assert_eq!(auto_entry(&section, "zdf").expect("override").add, vec![a]);
}

#[test]
fn add_member_back_to_auto_group_only_clears_remove() {
    let a = member("p", "1");
    let mut section = section();
    remove_member(&mut section, &auto("zdf"), &a);
    add_member(&mut section, &auto("zdf"), &a);
    assert!(section.is_empty());
}

#[test]
fn add_member_to_manual_group_appends_without_duplicates() {
    let (a, b) = (member("p", "1"), member("p", "2"));
    let mut section = section();
    section.groups.push(manual("sport", std::slice::from_ref(&a)));
    add_member(&mut section, &manual_ref("sport"), &b);
    add_member(&mut section, &manual_ref("sport"), &b);
    assert_eq!(section.groups[0].members, vec![a, b]);
}

#[test]
fn add_member_includes_an_excluded_channel_again() {
    let a = member("p", "1");
    let mut section = section();
    section.excluded.push(a.clone());
    add_member(&mut section, &auto("zdf"), &a);
    assert!(section.excluded.is_empty());
    assert_eq!(auto_entry(&section, "zdf").expect("override").add, vec![a]);
}

#[test]
fn remove_member_from_auto_group_writes_remove() {
    let (a, b) = (member("p", "1"), member("p", "2"));
    let mut section = section();
    section.auto_overrides.push(AutoGroupOverrideDto {
        auto_key: "zdf".to_string(),
        order: vec![a.clone(), b.clone()],
        ..Default::default()
    });
    remove_member(&mut section, &auto("zdf"), &a);
    remove_member(&mut section, &auto("zdf"), &a);
    let entry = auto_entry(&section, "zdf").expect("override");
    assert_eq!(entry.remove, vec![a]);
    assert_eq!(entry.order, vec![b]);
}

#[test]
fn remove_member_added_to_auto_group_drops_the_addition() {
    let a = member("p", "1");
    let mut section = section();
    add_member(&mut section, &auto("zdf"), &a);
    remove_member(&mut section, &auto("zdf"), &a);
    assert!(section.is_empty());
}

#[test]
fn remove_member_from_manual_group_drops_member() {
    let (a, b) = (member("p", "1"), member("p", "2"));
    let mut section = section();
    section.groups.push(manual("sport", &[a.clone(), b.clone()]));
    remove_member(&mut section, &manual_ref("sport"), &a);
    assert_eq!(section.groups[0].members, vec![b]);
    assert!(section.auto_overrides.is_empty());
}

#[test]
fn move_member_between_auto_groups_removes_and_adds() {
    let a = member("p", "1");
    let mut section = section();
    move_member(&mut section, &auto("zdf"), &auto("ard"), &a);
    assert_eq!(auto_entry(&section, "zdf").expect("override").remove, vec![a.clone()]);
    assert_eq!(auto_entry(&section, "ard").expect("override").add, vec![a]);
}

#[test]
fn move_member_from_manual_to_auto_group() {
    let (a, b) = (member("p", "1"), member("p", "2"));
    let mut section = section();
    section.groups.push(manual("sport", &[a.clone(), b.clone()]));
    move_member(&mut section, &manual_ref("sport"), &auto("zdf"), &a);
    assert_eq!(section.groups[0].members, vec![b]);
    assert_eq!(auto_entry(&section, "zdf").expect("override").add, vec![a]);
}

#[test]
fn move_member_within_same_group_is_a_noop() {
    let a = member("p", "1");
    let mut section = section();
    move_member(&mut section, &auto("zdf"), &auto("zdf"), &a);
    assert!(section.is_empty());
}

#[test]
fn exclude_member_in_auto_group_adds_excluded_and_remove() {
    let a = member("p", "1");
    let mut section = section();
    exclude_member(&mut section, Some(&auto("zdf")), &a);
    exclude_member(&mut section, Some(&auto("zdf")), &a);
    assert_eq!(section.excluded, vec![a.clone()]);
    assert_eq!(auto_entry(&section, "zdf").expect("override").remove, vec![a]);
}

#[test]
fn exclude_member_in_manual_group_drops_member_and_adds_excluded() {
    let (a, b) = (member("p", "1"), member("p", "2"));
    let mut section = section();
    section.groups.push(manual("sport", &[a.clone(), b.clone()]));
    exclude_member(&mut section, Some(&manual_ref("sport")), &a);
    assert_eq!(section.groups[0].members, vec![b]);
    assert_eq!(section.excluded, vec![a]);
}

#[test]
fn exclude_ungrouped_member_only_adds_excluded() {
    let a = member("p", "1");
    let mut section = section();
    exclude_member(&mut section, None, &a);
    assert_eq!(section.excluded, vec![a]);
    assert!(section.auto_overrides.is_empty());
}

#[test]
fn set_meta_on_auto_group_writes_override_fields_and_clears_blank() {
    let mut section = section();
    let patch = GroupMetaPatch {
        name: Some("  Das Erste ".to_string()),
        logo: Some("http://x/logo.png".to_string()),
        epg_channel_id: Some("  ".to_string()),
        playlist_group: None,
    };
    set_meta(&mut section, &auto("ard"), patch);
    let entry = auto_entry(&section, "ard").expect("override");
    assert_eq!(entry.name.as_deref(), Some("Das Erste"));
    assert_eq!(entry.logo.as_deref(), Some("http://x/logo.png"));
    assert_eq!(entry.epg_channel_id, None);

    set_meta(&mut section, &auto("ard"), GroupMetaPatch::default());
    assert!(section.is_empty());
}

#[test]
fn set_meta_on_manual_group_keeps_name_when_blank() {
    let mut section = section();
    section.groups.push(manual("sport", &[]));
    let patch = GroupMetaPatch {
        name: Some("   ".to_string()),
        playlist_group: Some("Sport".to_string()),
        ..Default::default()
    };
    set_meta(&mut section, &manual_ref("sport"), patch);
    assert_eq!(section.groups[0].name, "Group sport");
    assert_eq!(section.groups[0].playlist_group.as_deref(), Some("Sport"));

    set_meta(
        &mut section,
        &manual_ref("sport"),
        GroupMetaPatch { name: Some("Sports".to_string()), ..Default::default() },
    );
    assert_eq!(section.groups[0].name, "Sports");
    assert_eq!(section.groups[0].playlist_group, None);
}

#[test]
fn set_policy_on_auto_and_manual_groups() {
    let mut section = section();
    section.groups.push(manual("sport", &[]));
    set_policy(&mut section, &auto("zdf"), Some(FailoverGroupPolicy::Priority));
    set_policy(&mut section, &manual_ref("sport"), Some(FailoverGroupPolicy::Priority));
    assert_eq!(auto_entry(&section, "zdf").expect("override").policy, Some(FailoverGroupPolicy::Priority));
    assert_eq!(section.groups[0].policy, Some(FailoverGroupPolicy::Priority));

    set_policy(&mut section, &auto("zdf"), None);
    assert!(section.auto_overrides.is_empty());
}

#[test]
fn dissolve_auto_sets_disabled() {
    let mut section = section();
    dissolve(&mut section, &auto("zdf"));
    dissolve(&mut section, &auto("zdf"));
    assert_eq!(section.auto_overrides.len(), 1);
    assert!(auto_entry(&section, "zdf").expect("override").disabled);
}

#[test]
fn dissolve_manual_deletes_group() {
    let mut section = section();
    section.groups.push(manual("sport", &[member("p", "1")]));
    section.groups.push(manual("news", &[]));
    dissolve(&mut section, &manual_ref("sport"));
    assert_eq!(section.groups.iter().map(|group| group.id.as_str()).collect::<Vec<_>>(), vec!["news"]);
}

#[test]
fn reset_auto_removes_entry() {
    let mut section = section();
    dissolve(&mut section, &auto("zdf"));
    add_member(&mut section, &auto("ard"), &member("p", "1"));
    reset_auto(&mut section, "zdf");
    reset_auto(&mut section, "zdf");
    assert!(auto_entry(&section, "zdf").is_none());
    assert!(auto_entry(&section, "ard").is_some());
}

#[test]
fn reset_auto_op_removes_the_override_of_a_stale_key() {
    let mut section = section();
    add_member(&mut section, &auto("gone"), &member("p", "1"));
    let created = DraftOp::ResetAuto { auto_key: "gone".to_string() }.apply(&mut section);
    assert_eq!(created, None);
    assert!(section.is_empty());
}

#[test]
fn add_members_adds_moves_and_skips_in_one_operation() {
    let (a, b, c) = (member("p", "1"), member("p", "2"), member("p", "3"));
    let mut section = section();
    section.groups.push(manual("sport", std::slice::from_ref(&b)));
    section.groups.push(manual("news", std::slice::from_ref(&c)));
    let op = DraftOp::AddMembers {
        group: manual_ref("sport"),
        members: vec![(a.clone(), None), (b.clone(), Some(manual_ref("sport"))), (c.clone(), Some(manual_ref("news")))],
    };
    assert_eq!(op.apply(&mut section), None);
    assert_eq!(section.groups[0].members, vec![b, a, c]);
    assert!(section.groups[1].members.is_empty());
}

#[test]
fn add_members_moves_out_of_auto_group_and_includes_excluded_channels() {
    let (a, b) = (member("p", "1"), member("p", "2"));
    let mut section = section();
    section.groups.push(manual("sport", &[]));
    section.excluded.push(b.clone());
    add_members(&mut section, &manual_ref("sport"), &[(a.clone(), Some(auto("zdf"))), (b.clone(), None)]);
    assert_eq!(auto_entry(&section, "zdf").expect("override").remove, vec![a.clone()]);
    assert_eq!(section.groups[0].members, vec![a, b]);
    assert!(section.excluded.is_empty());
}

#[test]
fn add_members_of_nothing_changes_nothing() {
    let mut section = section();
    section.groups.push(manual("sport", &[member("p", "1")]));
    let before = section.clone();
    add_members(&mut section, &manual_ref("sport"), &[]);
    add_members(&mut section, &manual_ref("sport"), &[(member("p", "1"), Some(manual_ref("sport")))]);
    assert_eq!(section, before);
}

#[test]
fn create_manual_group_id_matches_format() {
    let mut section = section();
    let id = create_manual_group(&mut section, "  Sport  HD!! ", &[member("p", "1")], "ab12");
    assert_eq!(id, "sport-hd-ab12");
    assert_eq!(section.groups[0].id, id);
    assert_eq!(section.groups[0].name, "Sport  HD!!");
    assert_eq!(section.groups[0].members, vec![member("p", "1")]);
}

#[test]
fn create_manual_group_id_slug_is_limited_and_never_empty() {
    let mut section = section();
    let long = create_manual_group(&mut section, &"ä-x".repeat(60), &[], "0000");
    assert!(long.len() <= 64, "{long}");
    assert!(long.ends_with("-0000"));
    let empty = create_manual_group(&mut section, "!!!", &[], "0000");
    assert_eq!(empty, "group-0000");
    let exact = create_manual_group(&mut section, &"a".repeat(80), &[], "0000");
    assert_eq!(exact, format!("{}-0000", "a".repeat(59)));
}

#[test]
fn create_manual_group_with_random_suffix_is_valid() {
    let suffix = random_id_suffix();
    assert_eq!(suffix.len(), 4);
    assert!(suffix.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()));
}

#[test]
fn create_manual_group_never_reuses_an_existing_id() {
    let mut section = section();
    let first = create_manual_group(&mut section, "Sport", &[], "ab12");
    let second = create_manual_group(&mut section, "Sport", &[], "ab12");
    assert_ne!(first, second);
    assert!(second.starts_with("sport-"));
}

#[test]
fn create_manual_group_releases_excluded_members() {
    let a = member("p", "1");
    let mut section = section();
    section.excluded.push(a.clone());
    create_manual_group(&mut section, "Sport", &[a], "ab12");
    assert!(section.excluded.is_empty());
}

#[test]
fn convert_stale_to_manual_moves_refs_and_metadata() {
    let (a, b, c) = (member("p", "1"), member("p", "2"), member("p", "3"));
    let mut section = section();
    section.auto_overrides.push(AutoGroupOverrideDto {
        auto_key: "old-zdf".to_string(),
        name: Some("ZDF".to_string()),
        logo: Some("https://x/zdf.png".to_string()),
        policy: Some(FailoverGroupPolicy::Priority),
        order: vec![a.clone(), b.clone()],
        add: vec![b.clone(), c.clone()],
        remove: vec![a.clone()],
        ..Default::default()
    });
    let id = convert_stale_to_manual(&mut section, "old-zdf", "ab12").expect("id");
    assert_eq!(id, "zdf-ab12");
    assert!(section.auto_overrides.is_empty());
    let group = &section.groups[0];
    assert_eq!(group.name, "ZDF");
    assert_eq!(group.members, vec![b, c]);
    assert_eq!(group.logo.as_deref(), Some("https://x/zdf.png"));
    assert_eq!(group.policy, Some(FailoverGroupPolicy::Priority));
}

#[test]
fn convert_stale_to_manual_without_override_returns_none() {
    let mut section = section();
    assert_eq!(convert_stale_to_manual(&mut section, "gone", "ab12"), None);
    assert!(section.is_empty());
}

#[test]
fn convert_stale_to_manual_falls_back_to_auto_key_as_name() {
    let mut section = section();
    section.auto_overrides.push(AutoGroupOverrideDto {
        auto_key: "old key".to_string(),
        add: vec![member("p", "1")],
        ..Default::default()
    });
    let id = convert_stale_to_manual(&mut section, "old key", "ab12").expect("id");
    assert_eq!(id, "old-key-ab12");
    assert_eq!(section.groups[0].name, "old key");
}

#[test]
fn new_group_from_selection_takes_members_out_of_their_groups() {
    let (a, b) = (member("p", "1"), member("p", "2"));
    let mut section = section();
    section.groups.push(manual("sport", std::slice::from_ref(&b)));
    let op = DraftOp::NewGroupFromSelection {
        name: "Mixed".to_string(),
        members: vec![(a.clone(), Some(auto("zdf"))), (b.clone(), Some(manual_ref("sport"))), (member("p", "3"), None)],
        id_suffix: "ab12".to_string(),
    };
    let created = op.apply(&mut section);
    assert_eq!(created, Some(manual_ref("mixed-ab12")));
    assert_eq!(auto_entry(&section, "zdf").expect("override").remove, vec![a.clone()]);
    assert!(section.groups[0].members.is_empty());
    assert_eq!(section.groups[1].members, vec![a, b, member("p", "3")]);
}

#[test]
fn draft_op_apply_dispatches_to_operations() {
    let a = member("p", "1");
    let mut section = section();
    let created = DraftOp::AddMembers { group: auto("zdf"), members: vec![(a.clone(), None)] }.apply(&mut section);
    assert_eq!(created, None);
    assert_eq!(auto_entry(&section, "zdf").expect("override").add, vec![a]);
}

#[test]
fn forget_ref_drops_the_reference_from_every_list_without_recording_a_removal() {
    let (a, b) = (member("p", "1"), member("p", "2"));
    let mut section = section();
    section.auto_overrides.push(AutoGroupOverrideDto {
        auto_key: "zdf".to_string(),
        order: vec![a.clone(), b.clone()],
        add: vec![a.clone()],
        ..Default::default()
    });
    forget_ref(&mut section, &auto("zdf"), &a);
    let entry = auto_entry(&section, "zdf").expect("override");
    assert_eq!(entry.order, vec![b.clone()]);
    assert!(entry.add.is_empty());
    assert!(entry.remove.is_empty());

    forget_ref(&mut section, &auto("zdf"), &b);
    assert!(auto_entry(&section, "zdf").is_none(), "an empty override is removed again");
}

#[test]
fn forget_ref_on_manual_group_and_unknown_entries_is_harmless() {
    let (a, b) = (member("p", "1"), member("p", "2"));
    let mut section = section();
    section.groups.push(manual("sport", &[a.clone(), b.clone()]));
    forget_ref(&mut section, &manual_ref("sport"), &a);
    assert_eq!(section.groups[0].members, vec![b.clone()]);

    forget_ref(&mut section, &auto("missing"), &b);
    forget_ref(&mut section, &manual_ref("missing"), &b);
    assert!(section.auto_overrides.is_empty(), "no override is created for an unknown key");
    assert_eq!(section.groups[0].members, vec![b]);
}
