use super::*;
use crate::app::components::failover_groups::draft::GroupMetaPatch;
use shared::model::ChannelMemberRefDto;

fn member(stream_id: &str) -> ChannelMemberRefDto {
    ChannelMemberRefDto { input: "p".to_string(), stream_id: stream_id.to_string() }
}

fn response(etag: &str, overrides: TargetFailoverOverridesDto) -> TargetOverridesResponseDto {
    TargetOverridesResponseDto { etag: Some(etag.to_string()), file: Some("failover.yml".to_string()), overrides }
}

fn dispatch(state: Rc<FailoverGroupsEditorState>, action: FailoverGroupsEditorAction) -> Rc<FailoverGroupsEditorState> {
    state.reduce(action)
}

fn loaded_state() -> Rc<FailoverGroupsEditorState> {
    dispatch(
        Rc::new(FailoverGroupsEditorState::new("Main TV")),
        FailoverGroupsEditorAction::Loaded(response("e1", TargetFailoverOverridesDto::new("Main TV"))),
    )
}

fn add_op(auto_key: &str, stream_id: &str) -> FailoverGroupsEditorAction {
    FailoverGroupsEditorAction::Edit(DraftOp::AddMembers {
        group: GroupRef::Auto(auto_key.to_string()),
        members: vec![(member(stream_id), None)],
    })
}

#[test]
fn loaded_replaces_draft_and_resets_edit_state() {
    let mut overrides = TargetFailoverOverridesDto::new("Main TV");
    overrides.excluded.push(member("9"));
    let state = dispatch(loaded_state(), add_op("zdf", "1"));
    let state = dispatch(state, FailoverGroupsEditorAction::Loaded(response("e2", overrides.clone())));
    assert_eq!(state.draft, overrides);
    assert_eq!(state.undo.len(), 0);
    assert!(!state.dirty);
    assert_eq!(state.server.as_ref().and_then(|s| s.etag.as_deref()), Some("e2"));
}

#[test]
fn edit_marks_dirty() {
    let state = dispatch(loaded_state(), add_op("zdf", "1"));
    assert!(state.dirty);
    assert_eq!(state.undo.len(), 1);
    assert_eq!(state.draft.auto_overrides.len(), 1);
}

#[test]
fn edit_without_effect_changes_nothing() {
    let before = loaded_state();
    let after =
        dispatch(before.clone(), FailoverGroupsEditorAction::Edit(DraftOp::ResetAuto { auto_key: "none".to_string() }));
    assert_eq!(*after, *before);
}

#[test]
fn edit_bumps_preview_seq() {
    let before = loaded_state();
    let after = dispatch(before.clone(), add_op("zdf", "1"));
    assert!(after.preview_seq > before.preview_seq);
}

#[test]
fn move_member_is_single_undo_step() {
    let state = dispatch(loaded_state(), add_op("zdf", "1"));
    let undo_len = state.undo.len();
    let state = dispatch(
        state,
        FailoverGroupsEditorAction::Edit(DraftOp::MoveMember {
            from: GroupRef::Auto("zdf".to_string()),
            to: GroupRef::Auto("ard".to_string()),
            member: member("1"),
        }),
    );
    assert_eq!(state.undo.len(), undo_len + 1);
    let state = dispatch(state, FailoverGroupsEditorAction::Undo);
    assert_eq!(state.undo.len(), undo_len);
    assert!(state.draft.auto_overrides.iter().any(|entry| entry.auto_key == "zdf" && entry.add == vec![member("1")]));
    assert!(!state.draft.auto_overrides.iter().any(|entry| entry.auto_key == "ard"));
}

#[test]
fn undo_back_to_server_state_is_clean() {
    let state = dispatch(loaded_state(), add_op("zdf", "1"));
    let state = dispatch(state, FailoverGroupsEditorAction::Undo);
    assert!(!state.dirty);
    assert_eq!(state.undo.len(), 0);
    let again = dispatch(state.clone(), FailoverGroupsEditorAction::Undo);
    assert_eq!(*again, *state);
}

#[test]
fn undo_stack_is_bounded() {
    let mut state = loaded_state();
    for index in 0..(MAX_UNDO_STEPS + 20) {
        state = dispatch(state, add_op("zdf", &index.to_string()));
    }
    assert_eq!(state.undo.len(), MAX_UNDO_STEPS);
}

#[test]
fn stale_preview_response_is_ignored() {
    let state = dispatch(loaded_state(), add_op("zdf", "1"));
    let seq_of_first_edit = state.preview_seq;
    let state = dispatch(state, add_op("zdf", "2"));
    assert!(state.preview_seq > seq_of_first_edit);

    let stale = FailoverGroupListDto { etag: Some("stale".to_string()), ..Default::default() };
    let state = dispatch(state, FailoverGroupsEditorAction::PreviewLoaded(seq_of_first_edit, stale));
    assert!(state.preview.is_none());

    let fresh = FailoverGroupListDto { etag: Some("fresh".to_string()), ..Default::default() };
    let seq = state.preview_seq;
    let state = dispatch(state, FailoverGroupsEditorAction::PreviewLoaded(seq, fresh.clone()));
    assert_eq!(state.preview, Some(fresh));
}

#[test]
fn preview_requested_invalidates_older_responses() {
    let state = loaded_state();
    let seq = state.preview_seq;
    let state = dispatch(state, FailoverGroupsEditorAction::PreviewRequested(seq + 1));
    let state = dispatch(state, FailoverGroupsEditorAction::PreviewLoaded(seq, FailoverGroupListDto::default()));
    assert!(state.preview.is_none());
    let lower = dispatch(state.clone(), FailoverGroupsEditorAction::PreviewRequested(0));
    assert_eq!(lower.preview_seq, state.preview_seq);
}

#[test]
fn conflict_keeps_draft() {
    let state = dispatch(loaded_state(), add_op("zdf", "1"));
    let draft = state.draft.clone();
    let state = dispatch(state, FailoverGroupsEditorAction::Conflict(Some("e9".to_string())));
    assert_eq!(state.draft, draft);
    assert!(state.dirty);
    assert_eq!(state.conflict, Some(Some("e9".to_string())));
}

#[test]
fn external_change_reloads_when_clean() {
    let mut changed = TargetFailoverOverridesDto::new("Main TV");
    changed.excluded.push(member("7"));
    let state = dispatch(loaded_state(), FailoverGroupsEditorAction::ExternalChange(response("e2", changed.clone())));
    assert_eq!(state.draft, changed);
    assert!(!state.dirty);
    assert!(state.conflict.is_none());
    assert_eq!(state.server.as_ref().and_then(|s| s.etag.as_deref()), Some("e2"));
}

#[test]
fn external_change_sets_conflict_when_dirty() {
    let state = dispatch(loaded_state(), add_op("zdf", "1"));
    let draft = state.draft.clone();
    let mut changed = TargetFailoverOverridesDto::new("Main TV");
    changed.excluded.push(member("7"));
    let state = dispatch(state, FailoverGroupsEditorAction::ExternalChange(response("e2", changed)));
    assert_eq!(state.draft, draft);
    assert_eq!(state.conflict, Some(Some("e2".to_string())));
    assert_eq!(state.server.as_ref().and_then(|s| s.etag.as_deref()), Some("e1"));
}

#[test]
fn external_change_with_known_etag_is_ignored() {
    let state = dispatch(loaded_state(), add_op("zdf", "1"));
    let echo = response("e1", TargetFailoverOverridesDto::new("Main TV"));
    let after = dispatch(state.clone(), FailoverGroupsEditorAction::ExternalChange(echo));
    assert_eq!(*after, *state);
}

#[test]
fn saved_makes_draft_the_server_state_and_keeps_later_edits_dirty() {
    let state = dispatch(loaded_state(), add_op("zdf", "1"));
    let saved = state.draft.clone();
    let result = FailoverGroupSaveResultDto { etag: Some("e2".to_string()), file: None, hot_applied: false };
    let state = dispatch(state, FailoverGroupsEditorAction::Conflict(Some("x".to_string())));
    let state = dispatch(state, FailoverGroupsEditorAction::Saved(result.clone(), saved.clone()));
    assert!(!state.dirty);
    assert!(state.conflict.is_none());
    assert_eq!(state.server.as_ref().map(|s| s.overrides.clone()), Some(saved.clone()));
    assert_eq!(state.server.as_ref().and_then(|s| s.etag.as_deref()), Some("e2"));

    let edited_during_save = dispatch(dispatch(loaded_state(), add_op("zdf", "1")), add_op("zdf", "2"));
    let state = dispatch(edited_during_save, FailoverGroupsEditorAction::Saved(result, saved));
    assert!(state.dirty);
}

#[test]
fn discard_reverts_to_server_state() {
    let state = dispatch(loaded_state(), add_op("zdf", "1"));
    let state = dispatch(state, FailoverGroupsEditorAction::Conflict(None));
    let state = dispatch(state, FailoverGroupsEditorAction::Discard);
    assert!(state.draft.is_empty());
    assert!(!state.dirty);
    assert_eq!(state.undo.len(), 0);
    assert!(state.conflict.is_none());
}

#[test]
fn creating_a_group_selects_it_and_dissolving_clears_the_selection() {
    let state = dispatch(
        loaded_state(),
        FailoverGroupsEditorAction::Edit(DraftOp::NewGroupFromSelection {
            name: "Sport".to_string(),
            members: vec![(member("1"), None)],
            id_suffix: "ab12".to_string(),
        }),
    );
    assert_eq!(state.selected, Some(GroupRef::Manual("sport-ab12".to_string())));
    let state = dispatch(
        state,
        FailoverGroupsEditorAction::Edit(DraftOp::Dissolve { group: GroupRef::Manual("sport-ab12".to_string()) }),
    );
    assert_eq!(state.selected, None);
}

#[test]
fn select_sets_selection() {
    let state = dispatch(loaded_state(), FailoverGroupsEditorAction::Select(Some(GroupRef::Auto("zdf".to_string()))));
    assert_eq!(state.selected, Some(GroupRef::Auto("zdf".to_string())));
}

fn meta_op(auto_key: &str, name: &str) -> FailoverGroupsEditorAction {
    FailoverGroupsEditorAction::Edit(DraftOp::SetMeta {
        group: GroupRef::Auto(auto_key.to_string()),
        patch: GroupMetaPatch { name: Some(name.to_string()), ..Default::default() },
    })
}

#[test]
fn consecutive_meta_edits_share_one_undo_step() {
    let state = dispatch(loaded_state(), meta_op("zdf", "Z"));
    let seq = state.preview_seq;
    let state = dispatch(state, meta_op("zdf", "ZD"));
    let state = dispatch(state, meta_op("zdf", "ZDF"));
    assert_eq!(state.undo.len(), 1);
    assert_eq!(state.preview_seq, seq + 2, "every edit still asks for a new preview");
    assert_eq!(state.draft.auto_overrides[0].name.as_deref(), Some("ZDF"));

    let state = dispatch(state, FailoverGroupsEditorAction::Undo);
    assert_eq!(state.draft.auto_overrides.len(), 0);
    assert!(!state.dirty);
}

#[test]
fn meta_edit_after_other_op_starts_new_undo_step() {
    let state = dispatch(loaded_state(), meta_op("zdf", "Z"));
    let state = dispatch(state, add_op("zdf", "1"));
    let state = dispatch(state, meta_op("zdf", "ZD"));
    assert_eq!(state.undo.len(), 3);

    let other_group = dispatch(dispatch(loaded_state(), meta_op("zdf", "Z")), meta_op("rtl", "R"));
    assert_eq!(other_group.undo.len(), 2);

    let after_undo = dispatch(dispatch(state, FailoverGroupsEditorAction::Undo), meta_op("zdf", "ZDF"));
    assert_eq!(after_undo.undo.len(), 3, "undo ends the run");
}

#[test]
fn preview_answer_settles_the_sequence() {
    let state = dispatch(loaded_state(), add_op("zdf", "1"));
    assert!(state.preview_stale());
    let seq = state.preview_seq;
    let state = dispatch(state, FailoverGroupsEditorAction::PreviewLoaded(seq, FailoverGroupListDto::default()));
    assert!(!state.preview_stale());
    let state = dispatch(state, add_op("zdf", "2"));
    assert!(state.preview_stale(), "a new edit makes the preview stale again");
}

#[test]
fn failed_preview_stops_the_page_from_waiting() {
    let state = dispatch(loaded_state(), add_op("zdf", "1"));
    assert!(state.preview_stale());
    let seq = state.preview_seq;
    let state = dispatch(state, FailoverGroupsEditorAction::PreviewFailed(seq));
    assert!(!state.preview_stale());
    assert!(state.preview.is_none(), "no preview is invented");
}

#[test]
fn failed_preview_of_an_older_draft_is_ignored() {
    let state = dispatch(loaded_state(), add_op("zdf", "1"));
    let old_seq = state.preview_seq;
    let state = dispatch(state, add_op("zdf", "2"));
    let state = dispatch(state, FailoverGroupsEditorAction::PreviewFailed(old_seq));
    assert!(state.preview_stale());
}

#[test]
fn failed_preview_keeps_the_previous_preview() {
    let state = loaded_state();
    let seq = state.preview_seq;
    let preview = FailoverGroupListDto { etag: Some("p".to_string()), ..Default::default() };
    let state = dispatch(state, FailoverGroupsEditorAction::PreviewLoaded(seq, preview.clone()));
    let state = dispatch(state, add_op("zdf", "1"));
    let seq = state.preview_seq;
    let state = dispatch(state, FailoverGroupsEditorAction::PreviewFailed(seq));
    assert_eq!(state.preview, Some(preview));
    assert!(!state.preview_stale());
}

#[test]
fn external_change_is_ignored_while_a_save_is_in_flight() {
    let state = dispatch(loaded_state(), add_op("zdf", "1"));
    let state = dispatch(state, FailoverGroupsEditorAction::SaveStarted);
    assert!(state.saving);
    let mut changed = TargetFailoverOverridesDto::new("Main TV");
    changed.excluded.push(member("7"));
    let after = dispatch(state.clone(), FailoverGroupsEditorAction::ExternalChange(response("e2", changed.clone())));
    assert_eq!(*after, *state);
    assert!(after.conflict.is_none());

    let finished = dispatch(after, FailoverGroupsEditorAction::SaveFinished);
    assert!(!finished.saving);
    let after = dispatch(finished, FailoverGroupsEditorAction::ExternalChange(response("e2", changed)));
    assert_eq!(after.conflict, Some(Some("e2".to_string())), "a later change is a conflict again");
}

#[test]
fn own_save_does_not_raise_a_conflict() {
    let state = dispatch(loaded_state(), add_op("zdf", "1"));
    let saved = state.draft.clone();
    let state = dispatch(state, FailoverGroupsEditorAction::SaveStarted);
    let result = FailoverGroupSaveResultDto { etag: Some("e2".to_string()), file: None, hot_applied: false };
    let state = dispatch(state, FailoverGroupsEditorAction::Saved(result, saved.clone()));
    let state = dispatch(state, add_op("zdf", "2"));
    let state = dispatch(state, FailoverGroupsEditorAction::SaveFinished);
    assert!(state.dirty);
    let echo = dispatch(state.clone(), FailoverGroupsEditorAction::ExternalChange(response("e2", saved)));
    assert_eq!(*echo, *state, "the notification of the own write changes nothing");
    assert!(echo.conflict.is_none());
}

#[test]
fn adding_several_members_is_one_undo_step() {
    let state = dispatch(
        loaded_state(),
        FailoverGroupsEditorAction::Edit(DraftOp::AddMembers {
            group: GroupRef::Auto("zdf".to_string()),
            members: vec![
                (member("1"), None),
                (member("2"), None),
                (member("3"), Some(GroupRef::Auto("ard".to_string()))),
            ],
        }),
    );
    assert_eq!(state.undo.len(), 1);
    assert_eq!(
        state.draft.auto_overrides.iter().find(|entry| entry.auto_key == "zdf").map(|entry| entry.add.len()),
        Some(3)
    );
    let state = dispatch(state, FailoverGroupsEditorAction::Undo);
    assert!(state.draft.is_empty());
    assert!(!state.dirty);
}
