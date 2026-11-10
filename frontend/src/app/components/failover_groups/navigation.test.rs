use super::*;
use crate::app::components::failover_groups::state::{FailoverGroupsEditorAction, FailoverGroupsEditorState};
use std::rc::Rc;
use yew::Reducible;

fn editable() -> Vec<String> { vec!["Main TV".to_string(), "Sport".to_string()] }

#[test]
fn navigation_preselects_target_and_group() {
    let preselect = FailoverGroupsPreselect {
        group: Some(GroupRef::Manual("zdf".to_string())),
        ..FailoverGroupsPreselect::for_target("Sport").with_group_search("ZDF")
    };

    assert_eq!(preselected_target(&preselect, &editable()), Some("Sport"));
    assert_eq!(preselect.group_search.as_deref(), Some("ZDF"));

    let state = Rc::new(FailoverGroupsEditorState::new("Sport"))
        .reduce(FailoverGroupsEditorAction::Select(preselect.group.clone()));
    assert_eq!(state.selected, Some(GroupRef::Manual("zdf".to_string())));
}

#[test]
fn navigation_ignores_targets_that_cannot_be_edited() {
    let preselect = FailoverGroupsPreselect::for_target("Gone");
    assert_eq!(preselected_target(&preselect, &editable()), None);
    assert_eq!(preselected_target(&preselect, &[]), None);
}

#[test]
fn navigation_is_applied_once_per_request() {
    let first = NumberedPreselect { seq: 1, preselect: FailoverGroupsPreselect::for_target("Sport") };
    let second = NumberedPreselect { seq: 2, preselect: FailoverGroupsPreselect::for_target("Sport") };
    let mut applied = 0;

    assert!(take_unapplied(&mut applied, None).is_none());
    assert!(take_unapplied(&mut applied, Some(&first)).is_some());
    assert!(take_unapplied(&mut applied, Some(&first)).is_none(), "a re-render must not apply it again");
    assert!(take_unapplied(&mut applied, Some(&second)).is_some(), "the same target again is a new request");
}

#[test]
fn channel_preselect_prefills_the_candidate_search_with_its_title() {
    let member = ChannelMemberRefDto { input: "prov-a".to_string(), stream_id: "42".to_string() };
    let preselect = FailoverGroupsPreselect::for_target("Sport").with_channel(member.clone(), "  ZDF HD ");

    let channel = preselect.channel.as_ref().expect("channel preselected");
    assert_eq!(channel.member_ref, member);
    assert_eq!(channel.query(), "ZDF HD");
}
