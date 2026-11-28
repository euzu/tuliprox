use crate::{
    app::components::{
        playlist::explorer::identity::{CatalogGeneration, GroupOrderUpdate, Id},
        AppIcon, IconButton, TextButton,
    },
    hooks::{use_service_context, Services},
    i18n::{use_translation, YewI18n},
    services::GroupOrderSaveResult,
};
use shared::model::{
    FieldError, TargetGroupCluster, TargetGroupOrderSaveRequest, UiPlaylistCategories, UiPlaylistGroup, XtreamCluster,
};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::Arc,
};
use wasm_bindgen::JsCast;
use yew::{platform::spawn_local, prelude::*};

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SelectedGroup {
    All,
    WithoutEpg,
    Edited,
    Group(Arc<str>),
}

/// Maps an Explorer cluster to the backend `TargetGroupCluster` DTO enum.
#[must_use]
pub const fn target_group_cluster(cluster: XtreamCluster) -> TargetGroupCluster {
    match cluster {
        XtreamCluster::Live => TargetGroupCluster::Live,
        XtreamCluster::Video => TargetGroupCluster::Vod,
        XtreamCluster::Series => TargetGroupCluster::Series,
    }
}

/// Merges a stored group order with the cluster's current sidebar groups.
///
/// Groups from `stored` that exist in `current` keep their stored relative order (deduplicated),
/// followed by the remaining groups in `current` in their current sidebar order.
#[must_use]
pub fn merge_group_order(stored: &[String], current: &[String]) -> Vec<String> {
    let current_set: HashSet<&str> = current.iter().map(String::as_str).collect();
    let mut seen: HashSet<&str> = HashSet::with_capacity(current.len());
    let mut merged = Vec::with_capacity(current.len());

    for group in stored {
        let name = group.as_str();
        if current_set.contains(name) && seen.insert(name) {
            merged.push(group.clone());
        }
    }

    for group in current {
        if seen.insert(group.as_str()) {
            merged.push(group.clone());
        }
    }

    merged
}

/// Moves the group at `from` to `to`, returning `None` when the indices are equal or out of bounds.
#[must_use]
pub fn move_group_to(order: &[String], from: usize, to: usize) -> Option<Vec<String>> {
    if from == to || from >= order.len() || to >= order.len() {
        return None;
    }
    let mut next = order.to_vec();
    let item = next.remove(from);
    next.insert(to, item);
    Some(next)
}

/// Moves the group at `index` one slot up toward the start of the list.
#[must_use]
pub fn move_group_up(order: &[String], index: usize) -> Option<Vec<String>> {
    index.checked_sub(1).and_then(|to| move_group_to(order, index, to))
}

/// Moves the group at `index` one slot down toward the end of the list.
#[must_use]
pub fn move_group_down(order: &[String], index: usize) -> Option<Vec<String>> {
    move_group_to(order, index, index.saturating_add(1))
}

/// Inserts `new_group` directly after `after_group` (or at the end when `after_group` is absent).
///
/// Empty or already-present names leave `order` unchanged.
#[must_use]
pub fn insert_group_after(order: &[String], new_group: &str, after_group: &str) -> Vec<String> {
    let new_group = new_group.trim();
    if new_group.is_empty() || order.iter().any(|group| group == new_group) {
        return order.to_vec();
    }
    let mut next = order.to_vec();
    if let Some(pos) = next.iter().position(|group| group == after_group) {
        next.insert(pos + 1, new_group.to_string());
    } else {
        next.push(new_group.to_string());
    }
    next
}

/// Reorders the groups of `cluster` in `categories` to match `ordered_titles`.
/// Groups absent from `ordered_titles` keep their relative order at the end.
#[must_use]
pub fn reorder_categories_groups(
    categories: &UiPlaylistCategories,
    cluster: XtreamCluster,
    ordered_titles: &[String],
) -> UiPlaylistCategories {
    let reorder_list = |groups: Option<&Vec<Rc<UiPlaylistGroup>>>| -> Option<Vec<Rc<UiPlaylistGroup>>> {
        let groups = groups?;
        let rank: HashMap<&str, usize> =
            ordered_titles.iter().enumerate().map(|(idx, title)| (title.as_str(), idx)).collect();
        let mut next = groups.clone();
        next.sort_by_key(|group| rank.get(group.title.as_ref()).copied().unwrap_or(usize::MAX));
        Some(next)
    };

    match cluster {
        XtreamCluster::Live => UiPlaylistCategories {
            live: reorder_list(categories.live.as_ref()),
            vod: categories.vod.clone(),
            series: categories.series.clone(),
        },
        XtreamCluster::Video => UiPlaylistCategories {
            live: categories.live.clone(),
            vod: reorder_list(categories.vod.as_ref()),
            series: categories.series.clone(),
        },
        XtreamCluster::Series => UiPlaylistCategories {
            live: categories.live.clone(),
            vod: categories.vod.clone(),
            series: reorder_list(categories.series.as_ref()),
        },
    }
}

/// Maximum number of groups allowed in interactive sidebar reorder mode.
pub const MAX_REORDER_GROUPS: usize = 10_000;

/// Returns `true` when the cluster has more groups than interactive reorder mode supports.
#[must_use]
pub const fn is_reorder_over_limit(group_count: usize) -> bool { group_count > MAX_REORDER_GROUPS }

/// Returns `true` when reorder mode can be entered for a target cluster.
#[must_use]
pub const fn can_enter_reorder_mode(can_reorder: bool, group_count: usize) -> bool {
    can_reorder && group_count > 0 && !is_reorder_over_limit(group_count)
}

/// Returns `true` when the reorder toggle button should be disabled.
#[must_use]
pub const fn is_reorder_toggle_disabled(can_reorder: bool, group_count: usize, saving: bool, loading: bool) -> bool {
    !can_enter_reorder_mode(can_reorder, group_count) || saving || loading
}

/// Returns `true` when saving the reorder draft is disabled.
#[must_use]
pub const fn is_reorder_save_disabled(
    can_reorder: bool,
    saving: bool,
    loading: bool,
    load_failed: bool,
    draft_len: usize,
) -> bool {
    !can_reorder || saving || loading || load_failed || draft_len == 0
}

/// Returns `(up_disabled, down_disabled)` for the reorder row at `index` in a list of `len` items.
#[must_use]
pub const fn reorder_move_disabled(index: usize, len: usize, saving: bool, loading: bool) -> (bool, bool) {
    if saving || loading || len == 0 || index >= len {
        return (true, true);
    }
    (index == 0, index + 1 >= len)
}

/// Returns `true` when a drag-over event on a reorder row belongs to an active internal drag.
#[must_use]
pub const fn should_accept_reorder_drag(drag_source_index: Option<usize>, saving: bool, loading: bool) -> bool {
    drag_source_index.is_some() && !saving && !loading
}

/// Returns `true` when a drag-leave event on reorder row `index` must clear the drop highlight.
///
/// A related target inside the row (a child element such as the handle or a button) keeps the
/// highlight; leaving the row clears it.
#[must_use]
pub fn clears_reorder_drag_over(related_inside_row: bool, drag_over_index: Option<usize>, index: usize) -> bool {
    !related_inside_row && drag_over_index == Some(index)
}

/// Returns `true` when an async sidebar group-order operation still matches the mounted
/// component, active request generation, and current catalog generation.
#[must_use]
pub fn is_sidebar_request_current(
    mounted: bool,
    expected_request_gen: u64,
    current_request_gen: u64,
    expected_catalog: &CatalogGeneration,
    current_catalog: &CatalogGeneration,
) -> bool {
    mounted && expected_request_gen == current_request_gen && expected_catalog.matches_catalog(current_catalog)
}

/// Selects the conflict message to display for a `409 Conflict` group-order response.
///
/// Uses the server message when non-empty (covering revision conflicts, unsupported layouts,
/// and prepare/parse errors), falling back to `fallback_msg` only when the message is empty.
#[must_use]
pub fn select_group_order_conflict_message(server_msg: &str, fallback_msg: &str) -> String {
    let trimmed = server_msg.trim();
    if trimmed.is_empty() {
        fallback_msg.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Formats `422` field errors using a caller-supplied translator and empty-list fallback.
#[must_use]
pub fn format_group_order_field_errors_with(
    errors: &[FieldError],
    translate_fn: impl Fn(&str) -> String,
    fallback: &str,
) -> String {
    let joined = errors
        .iter()
        .map(|err| {
            let message = translate_fn(&err.message_key);
            match err.entry.as_deref() {
                Some(entry) if !entry.is_empty() => format!("{message} ({entry})"),
                _ => message,
            }
        })
        .collect::<Vec<_>>()
        .join("; ");
    if joined.is_empty() {
        fallback.to_string()
    } else {
        joined
    }
}

/// Translates `422` field errors returned by the group-order endpoint into one display string.
pub fn format_group_order_field_errors(errors: &[FieldError], translate: &YewI18n) -> String {
    format_group_order_field_errors_with(
        errors,
        |key| translate.t(key),
        &translate.t("LABEL.EXPLORER_STATUS_VALIDATION_ERROR"),
    )
}

/// Outcome of handling a `PUT /playlist/group_order` response (including conflict reload & merge).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupOrderSaveOutcome {
    Saved { groups: Vec<String>, pending: bool },
    ConflictReloaded { merged_groups: Vec<String>, error: String },
    Failed(String),
    Stale,
}

/// Resolves the outcome after reloading the stored group order on a `409 Conflict`.
#[must_use]
pub fn resolve_conflict_reload_outcome(
    conflict_msg: &str,
    reload_result: Result<Option<Vec<String>>, String>,
    current_titles: &[String],
    load_failed_template: &str,
) -> GroupOrderSaveOutcome {
    match reload_result {
        Ok(stored_opt) => {
            let stored = stored_opt.unwrap_or_default();
            GroupOrderSaveOutcome::ConflictReloaded {
                merged_groups: merge_group_order(&stored, current_titles),
                error: conflict_msg.to_string(),
            }
        }
        Err(err) => GroupOrderSaveOutcome::Failed(load_failed_template.replace("{error}", &err)),
    }
}

/// Handles a `GroupOrderSaveResult` consistently across sidebar reorder and bulk-edit paths:
/// shows the appropriate toast, reloads & merges stored order on `409 Conflict`, formats
/// `422` field errors, and checks staleness after async conflict reload.
pub async fn handle_group_order_save_result(
    services: &Services,
    translate: &YewI18n,
    request: &TargetGroupOrderSaveRequest,
    current_titles: &[String],
    result: GroupOrderSaveResult,
    show_success_toast: bool,
    is_still_current: impl Fn() -> bool,
) -> GroupOrderSaveOutcome {
    match result {
        GroupOrderSaveResult::Ok(resp) => {
            if show_success_toast {
                services.toastr.success(translate.t("MESSAGES.PLAYLIST_EXPLORER.GROUP_ORDER_SAVED"));
            }
            GroupOrderSaveOutcome::Saved { groups: resp.groups, pending: resp.pending }
        }
        GroupOrderSaveResult::Conflict(server_msg) => {
            let conflict_msg = select_group_order_conflict_message(
                &server_msg,
                &translate.t("MESSAGES.PLAYLIST_EXPLORER.GROUP_ORDER_CONFLICT"),
            );
            services.toastr.error(conflict_msg.clone());
            let reload_res = services.playlist.get_group_order(&request.target, request.cluster).await;
            if !is_still_current() {
                return GroupOrderSaveOutcome::Stale;
            }
            let mapped = reload_res.map(|opt| opt.map(|dto| dto.groups)).map_err(|err| err.to_string());
            let outcome = resolve_conflict_reload_outcome(
                &conflict_msg,
                mapped,
                current_titles,
                &translate.t("MESSAGES.PLAYLIST_EXPLORER.GROUP_ORDER_LOAD_FAILED"),
            );
            if let GroupOrderSaveOutcome::Failed(ref msg) = outcome {
                services.toastr.error(msg.clone());
            }
            outcome
        }
        GroupOrderSaveResult::FieldErrors(errs) => {
            let msg = format_group_order_field_errors(&errs, translate);
            services.toastr.error(msg.clone());
            GroupOrderSaveOutcome::Failed(msg)
        }
        GroupOrderSaveResult::Err(err_msg) => {
            let msg = translate.t("MESSAGES.PLAYLIST_EXPLORER.SAVE_FAILED_ERROR").replace("{error}", &err_msg);
            services.toastr.error(msg.clone());
            GroupOrderSaveOutcome::Failed(msg)
        }
    }
}

#[derive(Default, PartialEq, Eq)]
struct ReorderGroupData {
    titles: Vec<String>,
    counts: HashMap<String, usize>,
}

fn build_reorder_group_data(groups: Option<&Vec<Rc<UiPlaylistGroup>>>) -> ReorderGroupData {
    let Some(list) = groups else {
        return ReorderGroupData::default();
    };
    let mut titles = Vec::with_capacity(list.len());
    let mut counts = HashMap::with_capacity(list.len());
    for group in list {
        let title = group.title.to_string();
        counts.insert(title.clone(), group.channels.len());
        titles.push(title);
    }
    ReorderGroupData { titles, counts }
}

fn extract_group_titles(groups: Option<&Vec<Rc<UiPlaylistGroup>>>) -> Vec<String> {
    groups.map(|list| list.iter().map(|group| group.title.to_string()).collect()).unwrap_or_default()
}

type DragState = Rc<RefCell<Option<usize>>>;

#[derive(Properties, PartialEq, Clone)]
pub struct ExplorerSidebarProps {
    pub cluster: XtreamCluster,
    /// Group list of the active cluster; `Id` keeps the prop comparison at a pointer check.
    pub groups: Option<Id<Vec<Rc<UiPlaylistGroup>>>>,
    /// Full group list of the active cluster (unfiltered by search) used when saving group order.
    #[prop_or_default]
    pub all_cluster_groups: Option<Id<Vec<Rc<UiPlaylistGroup>>>>,
    #[prop_or_default]
    pub target_name: Option<String>,
    #[prop_or_default]
    pub generation: CatalogGeneration,
    #[prop_or_default]
    pub can_reorder: bool,
    #[prop_or_default]
    pub group_order_pending: bool,
    #[prop_or_default]
    pub on_group_order_saved: Option<Callback<GroupOrderUpdate>>,
    /// Counts are computed once by the parent, so the sidebar does not rescan the catalog
    /// on every render.
    pub total_items: usize,
    pub edited_count: usize,
    pub without_epg_count: usize,
    pub selected_group: SelectedGroup,
    pub on_select_group: Callback<SelectedGroup>,
}

#[component]
pub fn ExplorerSidebar(props: &ExplorerSidebarProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();

    let reorder_mode = use_state(|| false);
    let draft_order = use_state(Vec::<String>::new);
    let loading = use_state(|| false);
    let load_failed = use_state(|| false);
    let saving = use_state(|| false);
    let reorder_error = use_state(|| None::<String>);
    let drag_over_index = use_state(|| None::<usize>);
    let drag: DragState = use_mut_ref(|| None);
    // One ref per reorder row: the containment check of a drag-leave event needs the row node, and
    // `current_target()` is already the row itself by then.
    let row_refs = use_mut_ref(HashMap::<String, NodeRef>::new);
    let request_gen = use_mut_ref(|| 0u64);
    let is_mounted = use_mut_ref(|| true);
    let latest_generation = use_mut_ref(|| props.generation.clone());
    *latest_generation.borrow_mut() = props.generation.clone();

    {
        let is_mounted = is_mounted.clone();
        let request_gen = request_gen.clone();
        use_effect_with((), move |()| {
            move || {
                *is_mounted.borrow_mut() = false;
                *request_gen.borrow_mut() += 1;
            }
        });
    }

    // Exit reorder mode when switching cluster, target, or catalog generation so a draft
    // cannot be saved against a reloaded or edited catalog.
    let full_groups = props.all_cluster_groups.as_ref().or(props.groups.as_ref()).cloned();
    let group_count = full_groups.as_ref().map_or(0, |list| list.len());
    let too_many_groups = is_reorder_over_limit(group_count);

    {
        let reorder_mode = reorder_mode.clone();
        let draft_order = draft_order.clone();
        let loading = loading.clone();
        let load_failed = load_failed.clone();
        let saving = saving.clone();
        let reorder_error = reorder_error.clone();
        let drag_over_index = drag_over_index.clone();
        let drag = drag.clone();
        let request_gen = request_gen.clone();
        use_effect_with((props.generation.clone(), props.can_reorder, too_many_groups), move |_| {
            *request_gen.borrow_mut() += 1;
            *drag.borrow_mut() = None;
            reorder_mode.set(false);
            draft_order.set(Vec::new());
            loading.set(false);
            load_failed.set(false);
            saving.set(false);
            reorder_error.set(None);
            drag_over_index.set(None);
            || {}
        });
    }

    // Build titles and channel counts only in reorder mode, memoized by group-list identity,
    // so regular sidebar renders pay zero extra allocations.
    let reorder_data: Rc<ReorderGroupData> = use_memo((*reorder_mode, full_groups.clone()), |(in_reorder, groups)| {
        if *in_reorder {
            build_reorder_group_data(groups.as_deref())
        } else {
            ReorderGroupData::default()
        }
    });

    let exit_reorder_mode = {
        let reorder_mode = reorder_mode.clone();
        let draft_order = draft_order.clone();
        let loading = loading.clone();
        let load_failed = load_failed.clone();
        let reorder_error = reorder_error.clone();
        let drag_over_index = drag_over_index.clone();
        let drag = drag.clone();
        let request_gen = request_gen.clone();
        Callback::from(move |()| {
            *request_gen.borrow_mut() += 1;
            *drag.borrow_mut() = None;
            reorder_mode.set(false);
            draft_order.set(Vec::new());
            loading.set(false);
            load_failed.set(false);
            reorder_error.set(None);
            drag_over_index.set(None);
        })
    };

    let on_toggle_reorder = {
        let reorder_mode = reorder_mode.clone();
        let draft_order = draft_order.clone();
        let loading = loading.clone();
        let load_failed = load_failed.clone();
        let saving = saving.clone();
        let reorder_error = reorder_error.clone();
        let exit_reorder_mode = exit_reorder_mode.clone();
        let services = services.clone();
        let translate = translate.clone();
        let target_name = props.target_name.clone();
        let cluster = props.cluster;
        let can_reorder = props.can_reorder;
        let full_groups = full_groups.clone();
        let request_gen = request_gen.clone();
        let is_mounted = is_mounted.clone();
        let expected_gen = props.generation.clone();
        let latest_generation = latest_generation.clone();

        Callback::from(move |_: String| {
            if !can_reorder || *saving || *loading {
                return;
            }
            if *reorder_mode {
                exit_reorder_mode.emit(());
                return;
            }
            if !can_enter_reorder_mode(can_reorder, group_count) {
                return;
            }
            let Some(ref target) = target_name else {
                return;
            };
            let current_titles = extract_group_titles(full_groups.as_deref());
            let gen = {
                let mut g = request_gen.borrow_mut();
                *g += 1;
                *g
            };
            reorder_mode.set(true);
            loading.set(true);
            load_failed.set(false);
            reorder_error.set(None);
            draft_order.set(current_titles.clone());

            let services = services.clone();
            let translate = translate.clone();
            let target = target.clone();
            let draft_order = draft_order.clone();
            let loading = loading.clone();
            let load_failed = load_failed.clone();
            let reorder_error = reorder_error.clone();
            let request_gen = request_gen.clone();
            let is_mounted = is_mounted.clone();
            let expected_gen = expected_gen.clone();
            let latest_generation = latest_generation.clone();

            spawn_local(async move {
                let res = services.playlist.get_group_order(&target, target_group_cluster(cluster)).await;
                if !is_sidebar_request_current(
                    *is_mounted.borrow(),
                    gen,
                    *request_gen.borrow(),
                    &expected_gen,
                    &latest_generation.borrow(),
                ) {
                    return;
                }
                match res {
                    Ok(dto_opt) => {
                        let stored = dto_opt.map(|dto| dto.groups).unwrap_or_default();
                        draft_order.set(merge_group_order(&stored, &current_titles));
                        load_failed.set(false);
                    }
                    Err(err) => {
                        let msg = translate
                            .t("MESSAGES.PLAYLIST_EXPLORER.GROUP_ORDER_LOAD_FAILED")
                            .replace("{error}", &err.to_string());
                        services.toastr.error(msg.clone());
                        reorder_error.set(Some(msg));
                        load_failed.set(true);
                    }
                }
                loading.set(false);
            });
        })
    };

    let on_cancel_reorder = {
        let exit_reorder_mode = exit_reorder_mode.clone();
        let saving = saving.clone();
        Callback::from(move |_: String| {
            if !*saving {
                exit_reorder_mode.emit(());
            }
        })
    };

    let on_save_reorder = {
        let can_reorder = props.can_reorder;
        let target_name = props.target_name.clone();
        let cluster = props.cluster;
        let draft_order = draft_order.clone();
        let reorder_data = Rc::clone(&reorder_data);
        let loading = loading.clone();
        let load_failed = load_failed.clone();
        let saving = saving.clone();
        let reorder_mode = reorder_mode.clone();
        let reorder_error = reorder_error.clone();
        let services = services.clone();
        let translate = translate.clone();
        let on_group_order_saved = props.on_group_order_saved.clone();
        let request_gen = request_gen.clone();
        let is_mounted = is_mounted.clone();
        let expected_gen = props.generation.clone();
        let latest_generation = latest_generation.clone();

        Callback::from(move |_: String| {
            if is_reorder_save_disabled(can_reorder, *saving, *loading, *load_failed, draft_order.len()) {
                return;
            }
            let Some(ref target) = target_name else {
                return;
            };
            let gen = *request_gen.borrow();
            saving.set(true);
            reorder_error.set(None);

            let services = services.clone();
            let translate = translate.clone();
            let target = target.clone();
            let groups = (*draft_order).clone();
            let reorder_data = Rc::clone(&reorder_data);
            let draft_order = draft_order.clone();
            let saving = saving.clone();
            let reorder_mode = reorder_mode.clone();
            let reorder_error = reorder_error.clone();
            let on_group_order_saved = on_group_order_saved.clone();
            let request_gen = request_gen.clone();
            let is_mounted = is_mounted.clone();
            let expected_gen = expected_gen.clone();
            let latest_generation = latest_generation.clone();

            spawn_local(async move {
                let is_still_current = || {
                    is_sidebar_request_current(
                        *is_mounted.borrow(),
                        gen,
                        *request_gen.borrow(),
                        &expected_gen,
                        &latest_generation.borrow(),
                    )
                };
                let req = TargetGroupOrderSaveRequest {
                    target: target.clone(),
                    cluster: target_group_cluster(cluster),
                    groups,
                };
                let res = services.playlist.save_group_order(&req).await;
                if !is_still_current() {
                    return;
                }
                match handle_group_order_save_result(
                    &services,
                    &translate,
                    &req,
                    &reorder_data.titles,
                    res,
                    true,
                    is_still_current,
                )
                .await
                {
                    GroupOrderSaveOutcome::Stale => {}
                    GroupOrderSaveOutcome::Saved { groups, pending } => {
                        saving.set(false);
                        reorder_mode.set(false);
                        reorder_error.set(None);
                        if let Some(ref cb) = on_group_order_saved {
                            cb.emit(GroupOrderUpdate { generation: expected_gen, groups, pending: Some(pending) });
                        }
                    }
                    GroupOrderSaveOutcome::ConflictReloaded { merged_groups, error } => {
                        draft_order.set(merged_groups);
                        reorder_error.set(Some(error));
                        saving.set(false);
                    }
                    GroupOrderSaveOutcome::Failed(msg) => {
                        reorder_error.set(Some(msg));
                        saving.set(false);
                    }
                }
            });
        })
    };

    let on_move_to = {
        let draft_order = draft_order.clone();
        let saving = saving.clone();
        let loading = loading.clone();
        Callback::from(move |(from, to): (usize, usize)| {
            if *saving || *loading {
                return;
            }
            if let Some(next) = move_group_to(&draft_order, from, to) {
                draft_order.set(next);
            }
        })
    };

    let on_click_all = {
        let on_select = props.on_select_group.clone();
        Callback::from(move |_: MouseEvent| on_select.emit(SelectedGroup::All))
    };

    let on_click_without_epg = {
        let on_select = props.on_select_group.clone();
        Callback::from(move |_: MouseEvent| on_select.emit(SelectedGroup::WithoutEpg))
    };

    let on_click_edited = {
        let on_select = props.on_select_group.clone();
        Callback::from(move |_: MouseEvent| on_select.emit(SelectedGroup::Edited))
    };

    html! {
        <div class="side">
            {
                if props.can_reorder {
                    html! {
                        <>
                            <div class="tp__explorer-side-header">
                                <TextButton
                                    name="reorder_groups"
                                    icon="Sort"
                                    class={if *reorder_mode { "active" } else { "" }}
                                    aria_pressed={Some((*reorder_mode).to_string())}
                                    title={translate.t("LABEL.EXPLORER_REORDER_GROUPS")}
                                    disabled={is_reorder_toggle_disabled(props.can_reorder, group_count, *saving, *loading)}
                                    onclick={on_toggle_reorder}
                                />
                                {
                                    if props.group_order_pending {
                                        html! {
                                            <div class="tp__explorer-pending-list tp__explorer-side-pending">
                                                <span class="chip on" title={translate.t("LABEL.EXPLORER_PENDING_UNTIL_UPDATE")}>
                                                    { "⏳ " }{ translate.t("LABEL.EXPLORER_PENDING_UNTIL_UPDATE") }
                                                </span>
                                            </div>
                                        }
                                    } else {
                                        html! {}
                                    }
                                }
                            </div>
                            {
                                if too_many_groups {
                                    html! {
                                        <div class="tp__explorer-notice warning tp__explorer-side-notice" role="status">
                                            {
                                                translate
                                                    .t("MESSAGES.PLAYLIST_EXPLORER.GROUP_ORDER_TOO_MANY_GROUPS")
                                                    .replace("{max}", &MAX_REORDER_GROUPS.to_string())
                                            }
                                        </div>
                                    }
                                } else {
                                    html! {}
                                }
                            }
                        </>
                    }
                } else {
                    html! {}
                }
            }
            {
                if *reorder_mode {
                    let draft_len = draft_order.len();
                    html! {
                        <>
                            {
                                if let Some(ref err) = *reorder_error {
                                    html! {
                                        <div class="tp__explorer-notice warning tp__explorer-side-notice" role="alert">
                                            { err.clone() }
                                        </div>
                                    }
                                } else {
                                    html! {}
                                }
                            }
                            <div class="tp__explorer-reorder-list">
                                {
                                    draft_order.iter().enumerate().map(|(index, title)| {
                                        let count = reorder_data.counts.get(title).copied().unwrap_or(0);
                                        let row_ref =
                                            row_refs.borrow_mut().entry(title.clone()).or_default().clone();
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
                                            let drag_over_index = drag_over_index.clone();
                                            Callback::from(move |_: DragEvent| {
                                                *drag.borrow_mut() = None;
                                                drag_over_index.set(None);
                                            })
                                        };
                                        let ondragover = {
                                            let drag = drag.clone();
                                            let drag_over_index = drag_over_index.clone();
                                            let is_saving = *saving;
                                            let is_loading = *loading;
                                            Callback::from(move |event: DragEvent| {
                                                if !should_accept_reorder_drag(*drag.borrow(), is_saving, is_loading) {
                                                    return;
                                                }
                                                event.prevent_default();
                                                if *drag_over_index != Some(index) {
                                                    drag_over_index.set(Some(index));
                                                }
                                            })
                                        };
                                        let ondragleave = {
                                            let row_ref = row_ref.clone();
                                            let drag_over_index = drag_over_index.clone();
                                            Callback::from(move |event: DragEvent| {
                                                let related_inside_row = event
                                                    .related_target()
                                                    .and_then(|target| target.dyn_into::<web_sys::Node>().ok())
                                                    .is_some_and(|related| {
                                                        row_ref
                                                            .cast::<web_sys::Node>()
                                                            .is_some_and(|row| row.contains(Some(&related)))
                                                    });
                                                if clears_reorder_drag_over(
                                                    related_inside_row,
                                                    *drag_over_index,
                                                    index,
                                                ) {
                                                    drag_over_index.set(None);
                                                }
                                            })
                                        };
                                        let ondrop = {
                                            let drag = drag.clone();
                                            let drag_over_index = drag_over_index.clone();
                                            let on_move_to = on_move_to.clone();
                                            let is_saving = *saving;
                                            let is_loading = *loading;
                                            Callback::from(move |event: DragEvent| {
                                                event.prevent_default();
                                                drag_over_index.set(None);
                                                if !should_accept_reorder_drag(*drag.borrow(), is_saving, is_loading) {
                                                    return;
                                                }
                                                if let Some(from) = drag.borrow_mut().take() {
                                                    on_move_to.emit((from, index));
                                                }
                                            })
                                        };
                                        let (up_disabled, down_disabled) =
                                            reorder_move_disabled(index, draft_len, *saving, *loading);
                                        let on_move_up = {
                                            let draft_order = draft_order.clone();
                                            Callback::from(move |_: (String, MouseEvent)| {
                                                if !up_disabled {
                                                    if let Some(next) = move_group_up(&draft_order, index) {
                                                        draft_order.set(next);
                                                    }
                                                }
                                            })
                                        };
                                        let on_move_down = {
                                            let draft_order = draft_order.clone();
                                            Callback::from(move |_: (String, MouseEvent)| {
                                                if !down_disabled {
                                                    if let Some(next) = move_group_down(&draft_order, index) {
                                                        draft_order.set(next);
                                                    }
                                                }
                                            })
                                        };
                                        html! {
                                            <div
                                                key={format!("reorder-{title}")}
                                                ref={row_ref}
                                                class={classes!(
                                                    "tp__explorer-reorder-row",
                                                    (*drag_over_index == Some(index)).then_some("drag-over"),
                                                )}
                                                ondragover={ondragover}
                                                ondragleave={ondragleave}
                                                ondrop={ondrop}
                                            >
                                                <span
                                                    class="tp__explorer-drag-handle"
                                                    draggable={(!*saving && !*loading).to_string()}
                                                    title={translate.t("LABEL.EXPLORER_DRAG_GROUP")}
                                                    ondragstart={ondragstart}
                                                    ondragend={ondragend}
                                                >
                                                    <AppIcon name="DragHandle"/>
                                                </span>
                                                <span class="tp__explorer-reorder-title" title={title.clone()}>
                                                    { title.clone() }
                                                </span>
                                                <small class="tp__explorer-reorder-count">{ count }</small>
                                                <div class="tp__explorer-reorder-actions">
                                                    <IconButton
                                                        name={format!("group_up_{index}")}
                                                        icon="ArrowUp"
                                                        hint={translate.t("LABEL.EXPLORER_MOVE_GROUP_UP")}
                                                        aria_label={Some(format!("{}: {title}", translate.t("LABEL.EXPLORER_MOVE_GROUP_UP")))}
                                                        aria_disabled={Some(up_disabled)}
                                                        class={if up_disabled { "disabled" } else { "" }}
                                                        onclick={on_move_up}
                                                    />
                                                    <IconButton
                                                        name={format!("group_down_{index}")}
                                                        icon="ArrowDown"
                                                        hint={translate.t("LABEL.EXPLORER_MOVE_GROUP_DOWN")}
                                                        aria_label={Some(format!("{}: {title}", translate.t("LABEL.EXPLORER_MOVE_GROUP_DOWN")))}
                                                        aria_disabled={Some(down_disabled)}
                                                        class={if down_disabled { "disabled" } else { "" }}
                                                        onclick={on_move_down}
                                                    />
                                                </div>
                                            </div>
                                        }
                                    }).collect::<Html>()
                                }
                            </div>
                            <div class="tp__dialog__toolbar tp__explorer-reorder-toolbar">
                                <div class="tp__dialog__toolbar-left">
                                    <TextButton
                                        class="secondary"
                                        name="cancel_group_order"
                                        icon="Cancel"
                                        title={translate.t("LABEL.CANCEL")}
                                        disabled={*saving}
                                        onclick={on_cancel_reorder}
                                    />
                                </div>
                                <div class="tp__dialog__toolbar-right">
                                    <TextButton
                                        class="primary"
                                        name="save_group_order"
                                        icon="Save"
                                        title={translate.t("LABEL.SAVE")}
                                        disabled={is_reorder_save_disabled(props.can_reorder, *saving, *loading, *load_failed, draft_order.len())}
                                        onclick={on_save_reorder}
                                    />
                                </div>
                            </div>
                        </>
                    }
                } else {
                    html! {
                        <>
                            <button
                                type="button"
                                class={if props.selected_group == SelectedGroup::All { "on" } else { "" }}
                                aria-pressed={(props.selected_group == SelectedGroup::All).to_string()}
                                onclick={on_click_all}
                            >
                                <span>{ translate.t("LABEL.EXPLORER_VIRTUAL_GROUP_ALL") }</span>
                                <small>{ props.total_items }</small>
                            </button>
                            {
                                if let Some(ref groups) = props.groups {
                                    groups.iter().map(|g| {
                                        let is_selected = match &props.selected_group {
                                            SelectedGroup::Group(title) => title == &g.title,
                                            _ => false,
                                        };
                                        let group_title = g.title.clone();
                                        let on_select = props.on_select_group.clone();
                                        let on_click = Callback::from(move |_: MouseEvent| {
                                            on_select.emit(SelectedGroup::Group(group_title.clone()));
                                        });
                                        html! {
                                            <button
                                                type="button"
                                                key={format!("grp-{}", g.title)}
                                                class={if is_selected { "on" } else { "" }}
                                                aria-pressed={is_selected.to_string()}
                                                onclick={on_click}
                                            >
                                                <span>{ g.title.clone() }</span>
                                                <small>{ g.channels.len() }</small>
                                            </button>
                                        }
                                    }).collect::<Html>()
                                } else {
                                    html! {}
                                }
                            }
                            {
                                if props.cluster == XtreamCluster::Live {
                                    html! {
                                        <button
                                            type="button"
                                            class={format!("w {}", if props.selected_group == SelectedGroup::WithoutEpg { "on" } else { "" })}
                                            aria-pressed={(props.selected_group == SelectedGroup::WithoutEpg).to_string()}
                                            onclick={on_click_without_epg}
                                        >
                                            <span>{ "⚠ " }{ translate.t("LABEL.EXPLORER_VIRTUAL_GROUP_WITHOUT_EPG") }</span>
                                            <small>{ props.without_epg_count }</small>
                                        </button>
                                    }
                                } else {
                                    html! {}
                                }
                            }
                            <button
                                type="button"
                                class={format!("e {}", if props.selected_group == SelectedGroup::Edited { "on" } else { "" })}
                                aria-pressed={(props.selected_group == SelectedGroup::Edited).to_string()}
                                onclick={on_click_edited}
                            >
                                <span>{ "✎ " }{ translate.t("LABEL.EXPLORER_VIRTUAL_GROUP_EDITED") }</span>
                                <small>{ props.edited_count }</small>
                            </button>
                        </>
                    }
                }
            }
        </div>
    }
}

#[cfg(test)]
#[path = "sidebar.test.rs"]
mod tests;
