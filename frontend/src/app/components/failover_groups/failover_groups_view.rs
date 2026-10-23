//! The "Failover groups" page: target selection, status line and the editor of one target.

use super::{
    candidate_panel::{membership_of, CandidatePanel},
    diagnostics_panel::{problem_total, DiagnosticsPanel},
    dialogs::NameDialog,
    draft::{random_id_suffix, DraftOp, GroupRef},
    group_detail::GroupDetail,
    group_list::{build_rows, find_group, GroupFilter, GroupList, RowState},
    navigation::{preselected_target, take_unapplied, FailoverGroupsPreselect, NumberedChannel, NumberedPreselect},
    state::{FailoverGroupsEditorAction, FailoverGroupsEditorState},
};
use crate::{
    app::{
        components::{
            select::Select, selection_first, Chip, ConfirmDialog, DropDownOption, FieldLabel, LoadingIndicator,
            NoContent, SplitPane, TabItem, TabSet, TextButton,
        },
        context::FailoverGroupsNavigation,
    },
    error::Error,
    hooks::{use_service_context, use_unsaved_changes_guard},
    i18n::use_translation,
    model::{DialogResult, EventMessage},
    services::{FailoverService, SaveOutcome},
    utils::{get_session_storage_item, in_body, set_session_storage_item},
};
use gloo_timers::callback::Timeout;
use shared::model::{
    permission::Permission, FailoverGroupHealthDto, FailoverGroupListDto, FailoverGroupTargetStatusDto,
    FailoverGroupTargetsDto, FailoverTargetState, OverrideValidationError, PlaylistUpdateRunStateEvent,
    PlaylistUpdateState, SearchRequest,
};
use std::rc::Rc;
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

const PREVIEW_DEBOUNCE_MS: u32 = 300;
const TAB_GROUPS: &str = "groups";
const TAB_CANDIDATES: &str = "candidates";
const SHARED_FILE_NOTICE_KEY: &str = "tp_failover_shared_file_notice";

/// Where the target stands relative to the failover files and the last playlist update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveStatus {
    FileError,
    Unsaved,
    ApplyPending,
    Applied,
}

pub fn save_status(file_error: bool, dirty: bool, etag: Option<&str>, applied_etag: Option<&str>) -> SaveStatus {
    if file_error {
        SaveStatus::FileError
    } else if dirty {
        SaveStatus::Unsaved
    } else if etag != applied_etag {
        SaveStatus::ApplyPending
    } else {
        SaveStatus::Applied
    }
}

pub const fn status_label_key(status: SaveStatus) -> &'static str {
    match status {
        SaveStatus::FileError => "LABEL.FAILOVER_GROUPS_STATUS_FILE_ERROR",
        SaveStatus::Unsaved => "LABEL.FAILOVER_GROUPS_STATUS_UNSAVED",
        SaveStatus::ApplyPending => "LABEL.FAILOVER_GROUPS_STATUS_APPLY_PENDING",
        SaveStatus::Applied => "LABEL.FAILOVER_GROUPS_STATUS_APPLIED",
    }
}

pub const fn status_chip_class(status: SaveStatus) -> &'static str {
    match status {
        SaveStatus::FileError => "inactive",
        SaveStatus::Unsaved | SaveStatus::ApplyPending => "tp__failover-groups__status--pending",
        SaveStatus::Applied => "active",
    }
}

/// Whether the file that holds the section of `target` also holds sections of other targets.
pub fn file_is_shared(targets: &[FailoverGroupTargetStatusDto], target: &str) -> bool {
    let Some(file) = targets.iter().find(|status| status.name == target).and_then(|status| status.file.as_deref())
    else {
        return false;
    };
    targets.iter().filter(|status| status.file.as_deref() == Some(file)).count() > 1
}

/// Names of the targets that can be edited (configured, failover groups enabled).
pub fn editable_targets(targets: &FailoverGroupTargetsDto) -> Vec<String> {
    targets
        .targets
        .iter()
        .filter(|status| status.state == FailoverTargetState::Enabled)
        .map(|status| status.name.clone())
        .collect()
}

/// The override sections in `state`: of unknown targets (`Orphan`) or of targets without failover groups (`Disabled`).
pub fn sections_in_state(
    targets: &FailoverGroupTargetsDto,
    state: FailoverTargetState,
) -> Vec<FailoverGroupTargetStatusDto> {
    targets.targets.iter().filter(|status| status.state == state).cloned().collect()
}

/// Whether the playlist entry of a group still shows the name and logo of its previous first member.
pub fn has_display_stale(preview: Option<&FailoverGroupListDto>) -> bool {
    preview.is_some_and(|list| !list.display_stale.is_empty())
}

/// Whether a finished playlist update run can have published new channels or groups of `target`: the run did not
/// fail and processed the target.
pub fn update_publishes_changes(event: &PlaylistUpdateRunStateEvent, target: &str) -> bool {
    !matches!(event.state, PlaylistUpdateState::Failure) && event.processed_target(target)
}

/// A counter whose changes re-run the effects that depend on it.
#[derive(Debug, Default, PartialEq, Eq)]
struct Tick(u32);

impl Reducible for Tick {
    type Action = ();

    fn reduce(self: Rc<Self>, (): ()) -> Rc<Self> { Rc::new(Tick(self.0.wrapping_add(1))) }
}

#[derive(Debug, Clone, PartialEq)]
enum PendingConfirm {
    Discard,
    /// The save rewrites a file that other targets share; the flag asks to apply afterwards.
    SharedFile(bool),
}

#[derive(Properties, PartialEq)]
struct TargetEditorProps {
    target: String,
    targets: Rc<FailoverGroupTargetsDto>,
    /// The user may not write, or the failover files are broken.
    read_only: bool,
    on_dirty: Callback<bool>,
    on_targets_changed: Callback<()>,
    on_assign: Callback<(String, String, String)>,
    on_delete_orphan: Callback<(String, String)>,
    /// A deep link request for this target; applied once when the section is loaded.
    preselect: Option<Rc<NumberedPreselect>>,
    on_preselect_applied: Callback<u32>,
}

#[component]
fn TargetEditor(props: &TargetEditorProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let target = props.target.clone();
    let editor = use_reducer({
        let target = target.clone();
        move || FailoverGroupsEditorState::new(&target)
    });
    let load_tick = use_reducer(Tick::default);
    let refresh = use_reducer(Tick::default);
    let loaded = use_state(|| false);
    let no_catalog = use_state(|| false);
    let filter = use_state(GroupFilter::default);
    let search = use_state(|| SearchRequest::Clear);
    let search_value = use_state(|| None::<String>);
    let health = use_state(|| None::<Rc<FailoverGroupHealthDto>>);
    let findings = use_state(Vec::<OverrideValidationError>::new);
    let save_guard = use_mut_ref(|| false);
    let locked = use_state(|| None::<String>);
    let dismissed_conflict = use_state(|| None::<Option<String>>);
    let confirm = use_state(|| None::<PendingConfirm>);
    let show_new_group = use_state(|| false);
    let candidate_focus = use_state(|| None::<Rc<NumberedChannel>>);
    let active_tab = use_state(|| TAB_GROUPS.to_string());
    let applied_preselect = use_mut_ref(|| 0_u32);
    let latest_seq = use_mut_ref(|| 0_u64);
    *latest_seq.borrow_mut() = editor.preview_seq;

    let status_entry = props.targets.targets.iter().find(|status| status.name == target).cloned();
    let read_only = props.read_only || locked.is_some();
    let stale = editor.preview_stale();

    use_unsaved_changes_guard(editor.dirty);
    {
        let on_dirty = props.on_dirty.clone();
        use_effect_with(editor.dirty, move |dirty| {
            on_dirty.emit(*dirty);
            || ()
        });
    }

    // Load the section of the target.
    {
        let dispatcher = editor.dispatcher();
        let loaded = loaded.clone();
        let services = services.clone();
        use_effect_with((target.clone(), load_tick.0), move |(target, _)| {
            let target = target.clone();
            spawn_local(async move {
                match FailoverService::fetch_overrides(&target).await {
                    Ok(response) => {
                        dispatcher.dispatch(FailoverGroupsEditorAction::Loaded(response));
                        loaded.set(true);
                    }
                    Err(err) => services.toastr.error(err.to_string()),
                }
            });
            || ()
        });
    }

    // Apply a deep link request once the section is there.
    {
        let dispatcher = editor.dispatcher();
        let search = search.clone();
        let search_value = search_value.clone();
        let filter = filter.clone();
        let candidate_focus = candidate_focus.clone();
        let active_tab = active_tab.clone();
        let applied_preselect = applied_preselect.clone();
        let on_applied = props.on_preselect_applied.clone();
        use_effect_with((props.preselect.clone(), *loaded), move |(preselect, loaded)| {
            if *loaded {
                let numbered = preselect.as_deref();
                if let Some(request) = take_unapplied(&mut applied_preselect.borrow_mut(), numbered) {
                    if request.group.is_some() {
                        dispatcher.dispatch(FailoverGroupsEditorAction::Select(request.group.clone()));
                    }
                    if request.group.is_some() || request.group_search.is_some() {
                        filter.set(GroupFilter::default());
                        let text = request.group_search.clone().unwrap_or_default();
                        search.set(if text.is_empty() {
                            SearchRequest::Clear
                        } else {
                            SearchRequest::Text(text.clone(), None)
                        });
                        search_value.set(Some(text));
                    }
                    if let Some(channel) = request.channel.clone() {
                        let seq = numbered.map_or(0, |numbered| numbered.seq);
                        candidate_focus.set(Some(Rc::new(NumberedChannel { seq, channel })));
                        active_tab.set(TAB_CANDIDATES.to_string());
                    }
                    if let Some(numbered) = numbered {
                        on_applied.emit(numbered.seq);
                    }
                }
            }
            || ()
        });
    }

    // Resolve the groups of the draft (debounced while editing).
    {
        let dispatcher = editor.dispatcher();
        let draft = editor.draft.clone();
        let dirty = editor.dirty;
        let persisted = editor.server.as_ref().map(|server| server.etag.clone());
        let applied = status_entry.as_ref().and_then(|status| status.applied_etag.clone());
        let no_catalog = no_catalog.clone();
        let latest_seq = latest_seq.clone();
        let services = services.clone();
        let ready = *loaded;
        use_effect_with((target.clone(), editor.preview_seq, ready, refresh.0), move |(target, seq, ready, _)| {
            let seq = *seq;
            let timer = ready.then(|| {
                let target = target.clone();
                Timeout::new(if dirty { PREVIEW_DEBOUNCE_MS } else { 0 }, move || {
                    spawn_local(async move {
                        // Without edits the persisted groups are shown when they match the stored section.
                        let outcome = if !dirty && persisted.as_ref().is_some_and(|etag| *etag == applied) {
                            FailoverService::fetch_groups(&target).await
                        } else {
                            FailoverService::preview(&target, &draft).await
                        };
                        if *latest_seq.borrow() != seq {
                            return;
                        }
                        match outcome {
                            Ok(list) => {
                                no_catalog.set(false);
                                dispatcher.dispatch(FailoverGroupsEditorAction::PreviewLoaded(seq, list));
                            }
                            Err(Error::Conflict(_)) => {
                                no_catalog.set(true);
                                dispatcher.dispatch(FailoverGroupsEditorAction::PreviewFailed(seq));
                            }
                            Err(err) => {
                                services.toastr.error(err.to_string());
                                dispatcher.dispatch(FailoverGroupsEditorAction::PreviewFailed(seq));
                            }
                        }
                    });
                })
            });
            move || drop(timer)
        });
    }

    // Changes made elsewhere: another tab, a hand edit of the file, a finished playlist update.
    {
        let dispatcher = editor.dispatcher();
        let refresh = refresh.dispatcher();
        let latest_seq = latest_seq.clone();
        let on_targets_changed = props.on_targets_changed.clone();
        let services = services.clone();
        use_effect_with(target.clone(), move |target| {
            let target = target.clone();
            let subscription = services.event.subscribe(move |message| match message {
                EventMessage::FailoverOverridesChanged(changed) if changed.contains(&target) => {
                    let dispatcher = dispatcher.clone();
                    let target = target.clone();
                    spawn_local(async move {
                        if let Ok(response) = FailoverService::fetch_overrides(&target).await {
                            dispatcher.dispatch(FailoverGroupsEditorAction::ExternalChange(response));
                        }
                    });
                    on_targets_changed.emit(());
                }
                EventMessage::PlaylistUpdate(event) if update_publishes_changes(&event, &target) => {
                    dispatcher.dispatch(FailoverGroupsEditorAction::PreviewRequested(*latest_seq.borrow() + 1));
                    refresh.dispatch(());
                    on_targets_changed.emit(());
                }
                _ => {}
            });
            move || services.event.unsubscribe(subscription)
        });
    }

    let preview = editor.preview.as_ref();
    let rows = Rc::new(build_rows(preview, &editor.draft));
    let selected = editor.selected.clone();
    let selected_row = selected.as_ref().and_then(|group| rows.iter().find(|row| row.group == *group)).cloned();
    let selected_dto = selected.as_ref().and_then(|group| preview.and_then(|list| find_group(list, group))).cloned();
    let group_vid = selected_dto.as_ref().and_then(|dto| dto.group_vid);

    // Health of the selected group only; it lives in memory of the server, no provider is asked.
    {
        let health = health.clone();
        use_effect_with((target.clone(), group_vid, refresh.0), move |(target, vid, _)| {
            health.set(None);
            if let Some(vid) = *vid {
                let target = target.clone();
                let health = health.clone();
                spawn_local(async move {
                    if let Ok(found) = FailoverService::fetch_health(&target, vid).await {
                        health.set(Some(Rc::new(found)));
                    }
                });
            }
            || ()
        });
    }

    let membership = use_memo((editor.settled_seq, editor.preview_seq), {
        let editor = editor.clone();
        move |_| membership_of(editor.preview.as_ref(), &editor.draft)
    });

    let server_etag = editor.server.as_ref().and_then(|server| server.etag.clone());
    let applied_etag =
        preview.and_then(|list| list.applied_etag.clone()).or_else(|| status_entry.as_ref()?.applied_etag.clone());
    let status = save_status(false, editor.dirty, server_etag.as_deref(), applied_etag.as_deref());
    let file =
        editor.server.as_ref().and_then(|server| server.file.clone()).or_else(|| status_entry.as_ref()?.file.clone());
    let shared_file = file_is_shared(&props.targets.targets, &target);

    let all_findings: Vec<OverrideValidationError> = findings
        .iter()
        .cloned()
        .chain(status_entry.iter().flat_map(|status| status.findings.iter().cloned()))
        .fold(Vec::new(), |mut unique, finding| {
            if !unique.contains(&finding) {
                unique.push(finding);
            }
            unique
        });
    let orphans = sections_in_state(&props.targets, FailoverTargetState::Orphan);
    let disabled = sections_in_state(&props.targets, FailoverTargetState::Disabled);
    let diagnostics = preview.map(|list| list.diagnostics.clone()).unwrap_or_default();
    let problems = problem_total(
        &diagnostics,
        &all_findings,
        rows.iter().filter(|row| row.problem && row.state == RowState::Active).count(),
        orphans.len() + disabled.len(),
    );

    let on_op = {
        let dispatcher = editor.dispatcher();
        Callback::from(move |op: DraftOp| dispatcher.dispatch(FailoverGroupsEditorAction::Edit(op)))
    };

    let run_save = {
        let dispatcher = editor.dispatcher();
        let draft = editor.draft.clone();
        let target = target.clone();
        let server_etag = server_etag.clone();
        let services = services.clone();
        let translate = translate.clone();
        let save_guard = save_guard.clone();
        let locked = locked.clone();
        let findings = findings.clone();
        let filter = filter.clone();
        let dismissed = dismissed_conflict.clone();
        let latest_seq = latest_seq.clone();
        let on_targets_changed = props.on_targets_changed.clone();
        Callback::from(move |apply: bool| {
            // The guard flips synchronously, so a second click before the next render cannot send a second request.
            if std::mem::replace(&mut *save_guard.borrow_mut(), true) {
                return;
            }
            dispatcher.dispatch(FailoverGroupsEditorAction::SaveStarted);
            let (dispatcher, draft, target, server_etag) =
                (dispatcher.clone(), draft.clone(), target.clone(), server_etag.clone());
            let (services, translate, save_guard, locked, findings, filter, dismissed, latest_seq, on_targets_changed) = (
                services.clone(),
                translate.clone(),
                save_guard.clone(),
                locked.clone(),
                findings.clone(),
                filter.clone(),
                dismissed.clone(),
                latest_seq.clone(),
                on_targets_changed.clone(),
            );
            spawn_local(async move {
                match FailoverService::save(&target, server_etag, draft.clone()).await {
                    Ok(SaveOutcome::Saved(result)) => {
                        let hot_applied = result.hot_applied;
                        dispatcher.dispatch(FailoverGroupsEditorAction::Saved(result, draft));
                        dispatcher.dispatch(FailoverGroupsEditorAction::PreviewRequested(*latest_seq.borrow() + 1));
                        findings.set(Vec::new());
                        locked.set(None);
                        on_targets_changed.emit(());
                        if hot_applied {
                            services.toastr.success(translate.t("MESSAGES.FAILOVER_GROUPS_SAVED_APPLIED"));
                        } else if apply {
                            match FailoverService::apply(&target).await {
                                Ok(_) => services.toastr.success(translate.t("MESSAGES.FAILOVER_GROUPS_APPLY_QUEUED")),
                                Err(Error::Conflict(_)) => {
                                    services.toastr.warning(translate.t("MESSAGES.FAILOVER_GROUPS_APPLY_BUSY"));
                                }
                                Err(err) => services.toastr.error(err.to_string()),
                            }
                        } else {
                            services.toastr.success(translate.t("MESSAGES.FAILOVER_GROUPS_SAVED_PENDING"));
                        }
                    }
                    Ok(SaveOutcome::Conflict(current)) => {
                        dismissed.set(None);
                        dispatcher.dispatch(FailoverGroupsEditorAction::Conflict(current));
                    }
                    Ok(SaveOutcome::Invalid(found)) => {
                        services.toastr.error(translate.t("MESSAGES.FAILOVER_GROUPS_INVALID"));
                        findings.set(found);
                        filter.set(GroupFilter::Problems);
                    }
                    Ok(SaveOutcome::Locked(message)) => locked.set(Some(message)),
                    Err(err) => services.toastr.error(err.to_string()),
                }
                *save_guard.borrow_mut() = false;
                dispatcher.dispatch(FailoverGroupsEditorAction::SaveFinished);
            });
        })
    };

    let on_save = |apply: bool| {
        let run_save = run_save.clone();
        let confirm = confirm.clone();
        let ask = shared_file && get_session_storage_item(SHARED_FILE_NOTICE_KEY).is_none();
        Callback::from(move |_: String| {
            if ask {
                confirm.set(Some(PendingConfirm::SharedFile(apply)));
            } else {
                run_save.emit(apply);
            }
        })
    };

    let on_apply = {
        let target = target.clone();
        let services = services.clone();
        let translate = translate.clone();
        Callback::from(move |_: String| {
            let (target, services, translate) = (target.clone(), services.clone(), translate.clone());
            spawn_local(async move {
                match FailoverService::apply(&target).await {
                    Ok(_) => services.toastr.success(translate.t("MESSAGES.FAILOVER_GROUPS_APPLY_QUEUED")),
                    Err(Error::Conflict(_)) => {
                        services.toastr.warning(translate.t("MESSAGES.FAILOVER_GROUPS_APPLY_BUSY"));
                    }
                    Err(err) => services.toastr.error(err.to_string()),
                }
            });
        })
    };

    let on_confirm = {
        let confirm = confirm.clone();
        let run_save = run_save.clone();
        let dispatcher = editor.dispatcher();
        let findings = findings.clone();
        Callback::from(move |result: DialogResult| {
            let pending = (*confirm).clone();
            confirm.set(None);
            if result != DialogResult::Ok {
                return;
            }
            match pending {
                Some(PendingConfirm::Discard) => {
                    dispatcher.dispatch(FailoverGroupsEditorAction::Discard);
                    findings.set(Vec::new());
                }
                Some(PendingConfirm::SharedFile(apply)) => {
                    set_session_storage_item(SHARED_FILE_NOTICE_KEY, "1");
                    run_save.emit(apply);
                }
                None => {}
            }
        })
    };

    let conflict_open = editor.conflict.is_some() && *dismissed_conflict != editor.conflict;
    let on_conflict = {
        let dismissed = dismissed_conflict.clone();
        let conflict = editor.conflict.clone();
        let load_tick = load_tick.dispatcher();
        Callback::from(move |result: DialogResult| match result {
            DialogResult::Ok => load_tick.dispatch(()),
            DialogResult::Cancel => dismissed.set(conflict.clone()),
        })
    };

    let on_reload = {
        let load_tick = load_tick.dispatcher();
        Callback::from(move |_: String| load_tick.dispatch(()))
    };
    let on_refresh = {
        let dispatcher = editor.dispatcher();
        let refresh = refresh.dispatcher();
        let latest_seq = latest_seq.clone();
        Callback::from(move |_: String| {
            dispatcher.dispatch(FailoverGroupsEditorAction::PreviewRequested(*latest_seq.borrow() + 1));
            refresh.dispatch(());
        })
    };
    let on_undo = {
        let dispatcher = editor.dispatcher();
        Callback::from(move |_: String| dispatcher.dispatch(FailoverGroupsEditorAction::Undo))
    };
    let on_discard = {
        let confirm = confirm.clone();
        Callback::from(move |_: String| confirm.set(Some(PendingConfirm::Discard)))
    };
    let on_select = {
        let dispatcher = editor.dispatcher();
        Callback::from(move |group: GroupRef| dispatcher.dispatch(FailoverGroupsEditorAction::Select(Some(group))))
    };
    let on_tab_change = {
        let active_tab = active_tab.clone();
        Callback::from(move |tab: String| active_tab.set(tab))
    };
    let on_back = {
        let dispatcher = editor.dispatcher();
        Callback::from(move |()| dispatcher.dispatch(FailoverGroupsEditorAction::Select(None)))
    };
    let on_filter = {
        let filter = filter.clone();
        Callback::from(move |next: GroupFilter| filter.set(next))
    };
    let on_search = {
        let search = search.clone();
        Callback::from(move |request: SearchRequest| search.set(request))
    };
    let on_new_group = {
        let show = show_new_group.clone();
        Callback::from(move |_: String| show.set(true))
    };
    let on_new_group_submit = {
        let show = show_new_group.clone();
        let on_op = on_op.clone();
        Callback::from(move |name: String| {
            show.set(false);
            on_op.emit(DraftOp::NewGroupFromSelection { name, members: Vec::new(), id_suffix: random_id_suffix() });
        })
    };
    let on_new_group_cancel = {
        let show = show_new_group.clone();
        Callback::from(move |()| show.set(false))
    };

    let can_edit = !read_only;
    let apply_class = if status == SaveStatus::ApplyPending { "primary" } else { "tertiary" };
    let sent_group_name =
        selected_dto.as_ref().map(|dto| dto.name.clone()).or_else(|| selected_row.as_ref().map(|row| row.name.clone()));

    let tabs = Rc::new(vec![
        TabItem {
            id: TAB_GROUPS.to_string(),
            title: translate.t("LABEL.GROUPS"),
            icon: "Failover".to_string(),
            children: html! {
            <SplitPane
                class={classes!("tp__failover-groups__body", selected.is_some().then_some("tp__failover-groups__body--detail"))}
                initial_size="26rem"
                min_size="14rem"
                max_size="60rem"
                min_other_size="20rem"
                storage_key="tp-failover-groups-list-width"
                stack_on_mobile={true}
                first={html! {
                    <GroupList
                        rows={rows.clone()}
                        selected={selected.clone()}
                        filter={*filter}
                        search={(*search).clone()}
                        search_value={(*search_value).clone()}
                        problem_count={problems}
                        on_filter={on_filter}
                        on_search={on_search}
                        on_select={on_select.clone()}/>
                }}
                second={html! {
                    <div class="tp__failover-groups__pane">
                        if *filter == GroupFilter::Problems {
                            <DiagnosticsPanel
                                diagnostics={diagnostics}
                                findings={all_findings}
                                draft={Rc::new(editor.draft.clone())}
                                orphans={orphans}
                                disabled={disabled}
                                assignable={editable_targets(&props.targets)}
                                read_only={read_only}
                                on_op={on_op.clone()}
                                on_select={on_select.clone()}
                                on_assign={props.on_assign.clone()}
                                on_delete_orphan={props.on_delete_orphan.clone()}/>
                        }
                        if let Some(group) = selected.clone() {
                            <GroupDetail
                                group={group}
                                row={selected_row}
                                dto={selected_dto.map(Rc::new)}
                                draft={Rc::new(editor.draft.clone())}
                                health={(*health).clone()}
                                rows={rows.clone()}
                                read_only={read_only}
                                busy={stale}
                                on_op={on_op.clone()}
                                on_back={on_back}/>
                        } else if *filter != GroupFilter::Problems {
                            <p class="tp__failover-groups__notice">{ translate.t("MESSAGES.FAILOVER_GROUPS_SELECT_GROUP") }</p>
                        }
                    </div>
                }}/>
            },
            active_class: None,
            inactive_class: None,
        },
        TabItem {
            id: TAB_CANDIDATES.to_string(),
            title: translate.t("LABEL.FAILOVER_GROUPS_ADD_CHANNELS"),
            icon: "Search".to_string(),
            children: html! {
            <CandidatePanel
                target={target.clone()}
                reload_key={server_etag}
                selected_group={selected}
                selected_group_name={sent_group_name}
                groups={rows.clone()}
                on_select_group={on_select}
                focus={(*candidate_focus).clone()}
                membership={membership}
                excluded={Rc::new(editor.draft.excluded.clone())}
                read_only={read_only}
                on_op={on_op}/>
            },
            active_class: None,
            inactive_class: None,
        },
    ]);

    html! {
        <div class="tp__failover-groups__editor">
            <div class="tp__failover-groups__statusbar">
                <span class="tp__failover-groups__file">
                    { file.clone().unwrap_or_else(|| translate.t("MESSAGES.FAILOVER_GROUPS_NEW_FILE")) }
                </span>
                <Chip class={Some(status_chip_class(status).to_string())} label={translate.t(status_label_key(status))}/>
                if status == SaveStatus::ApplyPending && can_edit {
                    <span class="tp__failover-groups__hint">{ translate.t("MESSAGES.FAILOVER_GROUPS_APPLY_HINT") }</span>
                }
                if stale && *loaded {
                    <span class="tp__failover-groups__hint">{ translate.t("MESSAGES.FAILOVER_GROUPS_UPDATING") }</span>
                }
                if has_display_stale(preview) {
                    <span class="tp__failover-groups__hint tp__failover-groups__hint--display-stale">
                        { translate.t("MESSAGES.FAILOVER_GROUPS_DISPLAY_STALE") }
                    </span>
                }
            </div>
            <div class="tp__failover-groups__toolbar">
                if can_edit {
                    <TextButton name="new_group" icon="Add" class="tertiary" title={translate.t("LABEL.FAILOVER_GROUPS_NEW_GROUP")} hint={Some(translate.t("LABEL.FAILOVER_GROUPS_HINT_NEW_GROUP"))}
                        onclick={on_new_group}/>
                }
                <TextButton name="refresh" icon="Refresh" class="tertiary" title={translate.t("LABEL.FAILOVER_GROUPS_REFRESH_PREVIEW")} hint={Some(translate.t("LABEL.FAILOVER_GROUPS_HINT_REFRESH_PREVIEW"))}
                    onclick={on_refresh}/>
                if can_edit {
                    <TextButton name="undo" icon="Back" class="tertiary" title={translate.t("LABEL.FAILOVER_GROUPS_UNDO")} hint={Some(translate.t("LABEL.FAILOVER_GROUPS_HINT_UNDO"))}
                        disabled={editor.undo.is_empty()} onclick={on_undo}/>
                    <TextButton name="discard" icon="Cancel" class="secondary" title={translate.t("LABEL.FAILOVER_GROUPS_DISCARD")} hint={Some(translate.t("LABEL.FAILOVER_GROUPS_HINT_DISCARD"))}
                        disabled={!editor.dirty} onclick={on_discard}/>
                    <TextButton name="apply" icon="Play" class={apply_class} title={translate.t("LABEL.FAILOVER_GROUPS_APPLY")} hint={Some(translate.t("LABEL.FAILOVER_GROUPS_HINT_APPLY"))}
                        disabled={editor.dirty || editor.saving} onclick={on_apply}/>
                    <TextButton name="save" icon="Save" class="primary" title={translate.t("LABEL.SAVE")} hint={Some(translate.t("LABEL.FAILOVER_GROUPS_HINT_SAVE"))}
                        disabled={!editor.dirty || editor.saving} onclick={on_save(false)}/>
                    <TextButton name="save_apply" icon="Rocket" class="primary" title={translate.t("LABEL.FAILOVER_GROUPS_SAVE_AND_APPLY")} hint={Some(translate.t("LABEL.FAILOVER_GROUPS_HINT_SAVE_AND_APPLY"))}
                        disabled={!editor.dirty || editor.saving} onclick={on_save(true)}/>
                }
            </div>
            if let Some(message) = locked.as_ref() {
                <p class="tp__failover-groups__banner tp__failover-groups__banner--error" role="alert">
                    { format!("{}: {message}", translate.t("MESSAGES.FAILOVER_GROUPS_LOCKED")) }
                </p>
            }
            if editor.conflict.is_some() && !conflict_open {
                <p class="tp__failover-groups__banner" role="alert">
                    { translate.t("MESSAGES.FAILOVER_GROUPS_CHANGED_ELSEWHERE_BANNER") }
                    <TextButton name="reload" icon="Refresh" class="tertiary"
                        title={translate.t("LABEL.FAILOVER_GROUPS_RELOAD_DISCARD")} onclick={on_reload}/>
                </p>
            }
            if *no_catalog {
                <p class="tp__failover-groups__banner" role="status">{ translate.t("MESSAGES.FAILOVER_GROUPS_NO_CATALOG") }</p>
            }
            if !*loaded {
                <LoadingIndicator loading={true}/>
            }
            <TabSet tabs={tabs} class="tp__failover-groups__tabset" active_tab={Some((*active_tab).clone())}
                on_tab_change={on_tab_change}/>
            if conflict_open {
                { in_body(html! {
                    <ConfirmDialog title={translate.t("MESSAGES.FAILOVER_GROUPS_CHANGED_ELSEWHERE")}
                        ok_caption="LABEL.FAILOVER_GROUPS_RELOAD_DISCARD" cancel_caption="LABEL.FAILOVER_GROUPS_KEEP_EDITING"
                        close_on_backdrop_click={false} on_confirm={on_conflict}/>
                }) }
            }
            if let Some(pending) = (*confirm).clone() {
                { in_body(html! {
                    <ConfirmDialog
                        title={match pending {
                            PendingConfirm::Discard => translate.t("MESSAGES.FAILOVER_GROUPS_CONFIRM_DISCARD"),
                            PendingConfirm::SharedFile(_) => translate.t("MESSAGES.FAILOVER_GROUPS_SHARED_FILE_NOTICE")
                                .replace("{file}", file.as_deref().unwrap_or_default()),
                        }}
                        ok_caption={match pending {
                            PendingConfirm::Discard => "LABEL.FAILOVER_GROUPS_DISCARD",
                            PendingConfirm::SharedFile(_) => "LABEL.SAVE",
                        }}
                        cancel_caption="LABEL.CANCEL" on_confirm={on_confirm}/>
                }) }
            }
            if *show_new_group {
                <NameDialog title={translate.t("LABEL.FAILOVER_GROUPS_NEW_GROUP")} on_submit={on_new_group_submit}
                    on_cancel={on_new_group_cancel}/>
            }
        </div>
    }
}

#[component]
pub fn FailoverGroupsView() -> Html {
    let services = use_service_context();
    let translate = use_translation();
    let can_write = services.auth.has_permission(Permission::PlaylistWrite);
    let targets = use_state(|| None::<Rc<FailoverGroupTargetsDto>>);
    let tick = use_reducer(Tick::default);
    let selected = use_state(|| None::<String>);
    let dirty = use_state(|| false);
    let switch_to = use_state(|| None::<String>);
    let delete_orphan = use_state(|| None::<(String, String)>);
    let navigation = use_context::<FailoverGroupsNavigation>();
    let pending_request = navigation.as_ref().and_then(|navigation| (*navigation.pending).clone());
    let active_preselect = use_state(|| None::<Rc<NumberedPreselect>>);
    let deferred_preselect = use_state(|| None::<Rc<FailoverGroupsPreselect>>);
    let preselect_seq = use_mut_ref(|| 0_u32);

    {
        let targets = targets.clone();
        let services = services.clone();
        use_effect_with(tick.0, move |_| {
            spawn_local(async move {
                match FailoverService::fetch_targets().await {
                    Ok(found) => targets.set(Some(Rc::new(found))),
                    Err(err) => services.toastr.error(err.to_string()),
                }
            });
            || ()
        });
    }
    {
        let tick = tick.dispatcher();
        let services = services.clone();
        use_effect_with((), move |()| {
            let subscription = services.event.subscribe(move |message| {
                if matches!(message, EventMessage::FailoverOverridesChanged(_)) {
                    tick.dispatch(());
                }
            });
            move || services.event.unsubscribe(subscription)
        });
    }

    let names = targets.as_deref().map(editable_targets).unwrap_or_default();
    let current = selected.as_ref().filter(|name| names.contains(name)).cloned().or_else(|| names.first().cloned());

    let on_targets_changed = {
        let tick = tick.dispatcher();
        Callback::from(move |()| tick.dispatch(()))
    };
    let activate_preselect = {
        let active_preselect = active_preselect.clone();
        let preselect_seq = preselect_seq.clone();
        Callback::from(move |request: Rc<FailoverGroupsPreselect>| {
            let seq = {
                let mut counter = preselect_seq.borrow_mut();
                *counter += 1;
                *counter
            };
            active_preselect.set(Some(Rc::new(NumberedPreselect { seq, preselect: (*request).clone() })));
        })
    };
    // A deep link request is taken over once the targets are known; the sender's slot is emptied right away.
    {
        let navigation = navigation.clone();
        let services = services.clone();
        let translate = translate.clone();
        let selected = selected.clone();
        let switch_to = switch_to.clone();
        let deferred_preselect = deferred_preselect.clone();
        let activate_preselect = activate_preselect.clone();
        let dirty = *dirty;
        let current = current.clone();
        let names = names.clone();
        let loaded = targets.is_some();
        use_effect_with((pending_request, loaded), move |(pending, loaded)| {
            if let (Some(request), true, Some(navigation)) = (pending, *loaded, navigation) {
                navigation.pending.set(None);
                match preselected_target(request, &names) {
                    None => services.toastr.warning(translate.t("MESSAGES.FAILOVER_GROUPS_TARGET_UNAVAILABLE")),
                    Some(target) if dirty && current.as_deref() != Some(target) => {
                        deferred_preselect.set(Some(request.clone()));
                        switch_to.set(Some(target.to_string()));
                    }
                    Some(target) => {
                        selected.set(Some(target.to_string()));
                        activate_preselect.emit(request.clone());
                    }
                }
            }
            || ()
        });
    }
    let on_preselect_applied = {
        let active_preselect = active_preselect.clone();
        Callback::from(move |seq: u32| {
            if active_preselect.as_ref().is_some_and(|active| active.seq == seq) {
                active_preselect.set(None);
            }
        })
    };
    let on_dirty = {
        let dirty = dirty.clone();
        Callback::from(move |value: bool| dirty.set(value))
    };
    let on_target_select = {
        let selected = selected.clone();
        let switch_to = switch_to.clone();
        let dirty = *dirty;
        let current = current.clone();
        Callback::from(move |(_, selection): (String, crate::app::components::DropDownSelection)| {
            let Some(name) = selection_first(&selection).map(str::to_string) else { return };
            if current.as_deref() == Some(name.as_str()) {
                return;
            }
            if dirty {
                switch_to.set(Some(name));
            } else {
                selected.set(Some(name));
            }
        })
    };
    let on_switch_confirm = {
        let selected = selected.clone();
        let switch_to = switch_to.clone();
        let deferred_preselect = deferred_preselect.clone();
        let activate_preselect = activate_preselect.clone();
        Callback::from(move |result: DialogResult| {
            if result == DialogResult::Ok {
                selected.set((*switch_to).clone());
                if let Some(request) = (*deferred_preselect).clone() {
                    activate_preselect.emit(request);
                }
            }
            deferred_preselect.set(None);
            switch_to.set(None);
        })
    };

    let report = {
        let services = services.clone();
        let translate = translate.clone();
        let on_targets_changed = on_targets_changed.clone();
        Rc::new(move |outcome: Result<SaveOutcome, Error>| match outcome {
            Ok(SaveOutcome::Saved(_)) => {
                services.toastr.success(translate.t("MESSAGES.FAILOVER_GROUPS_SECTION_UPDATED"));
                on_targets_changed.emit(());
            }
            Ok(SaveOutcome::Conflict(_)) => {
                services.toastr.warning(translate.t("MESSAGES.FAILOVER_GROUPS_CHANGED_ELSEWHERE"));
            }
            Ok(SaveOutcome::Invalid(found)) => {
                services.toastr.error(found.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "));
            }
            Ok(SaveOutcome::Locked(message)) => services.toastr.error(message),
            Err(err) => services.toastr.error(err.to_string()),
        })
    };
    let on_assign = {
        let report = report.clone();
        Callback::from(move |(from, to, etag): (String, String, String)| {
            let report = report.clone();
            spawn_local(async move { report(FailoverService::reassign(&from, &to, &etag).await) });
        })
    };
    let on_delete_orphan = {
        let delete_orphan = delete_orphan.clone();
        Callback::from(move |request: (String, String)| delete_orphan.set(Some(request)))
    };
    let on_delete_confirm = {
        let delete_orphan = delete_orphan.clone();
        let report = report.clone();
        Callback::from(move |result: DialogResult| {
            let request = (*delete_orphan).clone();
            delete_orphan.set(None);
            if let (DialogResult::Ok, Some((name, etag))) = (result, request) {
                let report = report.clone();
                spawn_local(async move { report(FailoverService::delete(&name, &etag).await) });
            }
        })
    };

    let load_error = targets.as_ref().and_then(|found| found.load_error.clone());
    let read_only = !can_write || load_error.is_some();
    let options = Rc::new(
        names
            .iter()
            .map(|name| DropDownOption::new(name, html! { name.as_str() }, current.as_deref() == Some(name.as_str())))
            .collect::<Vec<_>>(),
    );

    html! {
        <div class="tp__failover-groups">
            <div class="tp__failover-groups__header">
                <h2>{ translate.t("LABEL.FAILOVER_GROUPS") }</h2>
                if !names.is_empty() {
                    <div class="tp__failover-groups__target">
                        <FieldLabel label={translate.t("LABEL.TARGET")} field_id="FAILOVER_GROUP_TARGET.TARGET"/>
                        <Select name="failover_group_target" multi_select={false} on_select={on_target_select} options={options}/>
                    </div>
                }
            </div>
            if let Some(error) = load_error {
                <p class="tp__failover-groups__banner tp__failover-groups__banner--error" role="alert">
                    { format!("{}: {error}", translate.t("MESSAGES.FAILOVER_GROUPS_FILE_ERROR")) }
                </p>
            }
            {
                match (targets.as_ref(), current.as_ref()) {
                    (None, _) => html! { <LoadingIndicator loading={true}/> },
                    (Some(found), Some(name)) => html! {
                        <TargetEditor
                            key={name.clone()}
                            target={name.clone()}
                            targets={found.clone()}
                            read_only={read_only}
                            on_dirty={on_dirty}
                            on_targets_changed={on_targets_changed}
                            on_assign={on_assign}
                            on_delete_orphan={on_delete_orphan}
                            preselect={(*active_preselect).clone().filter(|active| active.preselect.target == *name)}
                            on_preselect_applied={on_preselect_applied}/>
                    },
                    (Some(found), None) => html! {
                        <>
                            <NoContent text={translate.t("MESSAGES.FAILOVER_GROUPS_NO_TARGETS")}/>
                            <DiagnosticsPanel
                                diagnostics={shared::model::FailoverOverrideDiagnosticsDto::default()}
                                findings={Vec::new()}
                                draft={Rc::new(shared::model::TargetFailoverOverridesDto::new(""))}
                                orphans={sections_in_state(found, FailoverTargetState::Orphan)}
                                disabled={sections_in_state(found, FailoverTargetState::Disabled)}
                                assignable={Vec::new()}
                                read_only={read_only}
                                on_op={Callback::noop()}
                                on_select={Callback::noop()}
                                on_assign={on_assign}
                                on_delete_orphan={on_delete_orphan}/>
                        </>
                    },
                }
            }
            if switch_to.is_some() {
                { in_body(html! {
                    <ConfirmDialog title={translate.t("MESSAGES.FAILOVER_GROUPS_CONFIRM_SWITCH")}
                        ok_caption="LABEL.FAILOVER_GROUPS_DISCARD" cancel_caption="LABEL.CANCEL" on_confirm={on_switch_confirm}/>
                }) }
            }
            if delete_orphan.is_some() {
                { in_body(html! {
                    <ConfirmDialog title={translate.t("MESSAGES.FAILOVER_GROUPS_CONFIRM_DELETE_SECTION")}
                        ok_caption="LABEL.FAILOVER_GROUPS_DELETE_SECTION" cancel_caption="LABEL.CANCEL"
                        on_confirm={on_delete_confirm}/>
                }) }
            }
        </div>
    }
}

#[cfg(test)]
#[path = "failover_groups_view.test.rs"]
mod tests;
