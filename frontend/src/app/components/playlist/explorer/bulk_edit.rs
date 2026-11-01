use crate::{
    app::components::{
        custom_dialog::CustomDialog,
        input::Input,
        playlist::explorer::{epg_picker::EpgPicker, identity::Id, image_preview::ImagePreview},
        DropDownOption, DropDownSelection, FieldLabel, Select, TextButton,
    },
    hooks::use_service_context,
    i18n::use_translation,
};
use shared::{
    model::{
        FieldError, ItemOverridePatch, ItemPatchDto, OverrideFieldOp, OverrideItemKind, OverrideSaveRequest,
        OverrideScopeDto, UiPlaylistItem,
    },
    utils::parse_localized_float,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    rc::Rc,
    sync::Arc,
};
use yew::{create_portal, platform::spawn_local, prelude::*};

fn portal_to_body(content: Html) -> Html {
    if let Some(body) = web_sys::window().and_then(|w| w.document()).and_then(|d| d.body()) {
        create_portal(content, body.into())
    } else {
        content
    }
}

pub const BULK_UNIQUE_FIELDS: &[&str] = &["chno", "name", "title", "tmdb"];

pub fn is_bulk_field_allowed(field: &str, kind: OverrideItemKind) -> bool {
    if BULK_UNIQUE_FIELDS.contains(&field) {
        return false;
    }
    match kind {
        OverrideItemKind::Live => matches!(
            field,
            "group" | "logo" | "logo_small" | "epg_channel_id" | "parent_code" | "audio_track" | "time_shift" | "rec"
        ),
        OverrideItemKind::Vod | OverrideItemKind::Series => {
            matches!(field, "group" | "logo" | "genre" | "rating" | "plot" | "cast" | "director" | "release_date")
        }
    }
}

pub fn group_patches_by_parent_input(
    items: &[Rc<UiPlaylistItem>],
    patch: &ItemOverridePatch,
) -> BTreeMap<String, Vec<ItemPatchDto>> {
    let mut map: BTreeMap<String, Vec<ItemPatchDto>> = BTreeMap::new();
    for item in items {
        let input_name = item.input_name.to_string();
        let dto = ItemPatchDto {
            input: Some(input_name.clone()),
            kind: OverrideItemKind::from(item.item_type),
            id: item.provider_id.to_string(),
            patch: patch.clone(),
            hint: Some(format!("{} / {}", item.group, item.title)),
        };
        map.entry(input_name).or_default().push(dto);
    }
    map
}

#[allow(dead_code)]
pub fn prepare_bulk_input_save_requests(
    base_revisions: &HashMap<String, String>,
    items: &[Rc<UiPlaylistItem>],
    patch: &ItemOverridePatch,
) -> Vec<OverrideSaveRequest> {
    let grouped = group_patches_by_parent_input(items, patch);
    grouped
        .into_iter()
        .map(|(input_name, patches)| {
            let base_revision = base_revisions.get(&input_name).cloned().unwrap_or_default();
            OverrideSaveRequest { scope: OverrideScopeDto::Input, name: input_name, base_revision, patches }
        })
        .collect()
}

#[allow(dead_code)]
pub fn prepare_bulk_target_save_request(
    target_name: &str,
    base_revision: &str,
    items: &[Rc<UiPlaylistItem>],
    patch: &ItemOverridePatch,
) -> OverrideSaveRequest {
    let patches = items
        .iter()
        .map(|item| ItemPatchDto {
            input: Some(item.input_name.to_string()),
            kind: OverrideItemKind::from(item.item_type),
            id: item.provider_id.to_string(),
            patch: patch.clone(),
            hint: Some(format!("{} / {}", item.group, item.title)),
        })
        .collect();

    OverrideSaveRequest {
        scope: OverrideScopeDto::Target,
        name: target_name.to_string(),
        base_revision: base_revision.to_string(),
        patches,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BulkInputStatus {
    Saved(String),
    Conflict(String),
    ValidationError(Vec<FieldError>),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BulkInputResult {
    pub input: String,
    pub count: usize,
    pub status: BulkInputStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BulkResultsSummary {
    pub results: Vec<BulkInputResult>,
}

impl BulkResultsSummary {
    #[allow(dead_code)]
    pub fn is_all_success(&self) -> bool {
        !self.results.is_empty() && self.results.iter().all(|r| matches!(r.status, BulkInputStatus::Saved(_)))
    }

    pub fn successful_inputs(&self) -> Vec<String> {
        self.results
            .iter()
            .filter_map(|r| match &r.status {
                BulkInputStatus::Saved(_) => Some(r.input.clone()),
                _ => None,
            })
            .collect()
    }
}

pub fn summarize_bulk_results(results: Vec<BulkInputResult>) -> BulkResultsSummary { BulkResultsSummary { results } }

pub fn patch_playlist_item_bulk(item: &UiPlaylistItem, patch: &ItemOverridePatch) -> UiPlaylistItem {
    let mut updated = item.clone();

    if let Some(op) = &patch.group {
        match op {
            OverrideFieldOp::Set(g) => updated.group = Arc::from(g.as_str()),
            OverrideFieldOp::Clear => updated.group = Arc::from(""),
            OverrideFieldOp::Reset => {}
        }
    }

    if let Some(op) = &patch.logo {
        match op {
            OverrideFieldOp::Set(l) => updated.logo = Arc::from(l.as_str()),
            OverrideFieldOp::Clear => updated.logo = Arc::from(""),
            OverrideFieldOp::Reset => {}
        }
    }

    if let Some(op) = &patch.epg_channel_id {
        match op {
            OverrideFieldOp::Set(e) => updated.epg_channel_id = Some(Arc::from(e.as_str())),
            OverrideFieldOp::Clear => updated.epg_channel_id = None,
            OverrideFieldOp::Reset => {}
        }
    }

    let mut meta = updated.explorer_meta.unwrap_or_default();
    meta.override_state.edited = true;
    meta.override_state.pending = true;
    updated.explorer_meta = Some(meta);

    updated
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BulkFieldAction {
    #[default]
    Unchanged,
    Set,
    Clear,
    Reset,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BulkFieldState {
    pub action: BulkFieldAction,
    pub value: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BulkFormData {
    pub group: BulkFieldState,
    pub logo: BulkFieldState,
    pub logo_small: BulkFieldState,
    pub epg_channel_id: BulkFieldState,
    pub genre: BulkFieldState,
    pub rating: BulkFieldState,
    pub plot: BulkFieldState,
    pub cast: BulkFieldState,
    pub director: BulkFieldState,
    pub release_date: BulkFieldState,
}

impl BulkFormData {
    pub fn build_patch(&self, kind: OverrideItemKind) -> ItemOverridePatch {
        let mut patch = ItemOverridePatch::default();

        macro_rules! apply_string_field {
            ($field:ident) => {
                match self.$field.action {
                    BulkFieldAction::Unchanged => {}
                    BulkFieldAction::Reset => patch.$field = Some(OverrideFieldOp::Reset),
                    BulkFieldAction::Clear => patch.$field = Some(OverrideFieldOp::Clear),
                    BulkFieldAction::Set => {
                        let trimmed = self.$field.value.trim();
                        patch.$field = Some(OverrideFieldOp::Set(trimmed.to_string()));
                    }
                }
            };
        }

        if is_bulk_field_allowed("group", kind) {
            match self.group.action {
                BulkFieldAction::Unchanged | BulkFieldAction::Clear => {}
                BulkFieldAction::Reset => patch.group = Some(OverrideFieldOp::Reset),
                BulkFieldAction::Set => {
                    let trimmed = self.group.value.trim();
                    if !trimmed.is_empty() {
                        patch.group = Some(OverrideFieldOp::Set(trimmed.to_string()));
                    }
                }
            }
        }

        if is_bulk_field_allowed("logo", kind) {
            apply_string_field!(logo);
        }

        if kind == OverrideItemKind::Live {
            if is_bulk_field_allowed("logo_small", kind) {
                apply_string_field!(logo_small);
            }
            if is_bulk_field_allowed("epg_channel_id", kind) {
                apply_string_field!(epg_channel_id);
            }
        } else {
            if is_bulk_field_allowed("genre", kind) {
                apply_string_field!(genre);
            }
            if is_bulk_field_allowed("plot", kind) {
                apply_string_field!(plot);
            }
            if is_bulk_field_allowed("cast", kind) {
                apply_string_field!(cast);
            }
            if is_bulk_field_allowed("director", kind) {
                apply_string_field!(director);
            }
            if is_bulk_field_allowed("release_date", kind) {
                apply_string_field!(release_date);
            }
            if is_bulk_field_allowed("rating", kind) {
                match self.rating.action {
                    BulkFieldAction::Unchanged => {}
                    BulkFieldAction::Reset => patch.rating = Some(OverrideFieldOp::Reset),
                    BulkFieldAction::Clear => patch.rating = Some(OverrideFieldOp::Clear),
                    BulkFieldAction::Set => {
                        if let Some(rating) = parse_localized_float(self.rating.value.trim()) {
                            patch.rating = Some(OverrideFieldOp::Set(rating));
                        }
                    }
                }
            }
        }

        patch
    }

    pub fn has_changes(&self) -> bool {
        self.group.action != BulkFieldAction::Unchanged
            || self.logo.action != BulkFieldAction::Unchanged
            || self.logo_small.action != BulkFieldAction::Unchanged
            || self.epg_channel_id.action != BulkFieldAction::Unchanged
            || self.genre.action != BulkFieldAction::Unchanged
            || self.rating.action != BulkFieldAction::Unchanged
            || self.plot.action != BulkFieldAction::Unchanged
            || self.cast.action != BulkFieldAction::Unchanged
            || self.director.action != BulkFieldAction::Unchanged
            || self.release_date.action != BulkFieldAction::Unchanged
    }
}

#[derive(Properties, PartialEq, Clone)]
pub struct BulkEditPanelProps {
    /// `Id` keeps the prop comparison at a pointer check.
    pub items: Id<Vec<Rc<UiPlaylistItem>>>,
    pub active_scope: UseStateHandle<OverrideScopeDto>,
    #[prop_or_default]
    pub target_id: Option<u16>,
    #[prop_or_default]
    pub target_name: Option<String>,
    pub on_close: Callback<()>,
    pub on_saved: Callback<Vec<UiPlaylistItem>>,
}

#[component]
pub fn BulkEditPanel(props: &BulkEditPanelProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();

    let form_data = use_state(BulkFormData::default);
    let is_saving = use_state(|| false);
    let confirm_open = use_state(|| false);
    let results_summary = use_state(|| None::<BulkResultsSummary>);

    let items = props.items.clone();
    let count = items.len();

    let Some(first_item) = items.first() else {
        return html! {};
    };

    let kind = OverrideItemKind::from(first_item.item_type);
    let is_target_view = props.target_id.is_some();

    // Grouping summary for the confirmation dialog, built once per selection.
    let input_counts: Rc<BTreeMap<String, usize>> = use_memo(items.clone(), |items| {
        let mut map = BTreeMap::new();
        for it in items.iter() {
            *map.entry(it.input_name.to_string()).or_insert(0) += 1;
        }
        map
    });

    let on_save_click = {
        let confirm_open = confirm_open.clone();
        let form_data = form_data.clone();
        let scope = *props.active_scope;

        Callback::from(move |_: String| {
            if !form_data.has_changes() {
                return;
            }
            if scope == OverrideScopeDto::Input {
                confirm_open.set(true);
            } else {
                // In target scope, directly proceed to save
                confirm_open.set(true);
            }
        })
    };

    let execute_save = {
        let services = services.clone();
        let items = items.clone();
        let form_data = form_data.clone();
        let is_saving = is_saving.clone();
        let confirm_open = confirm_open.clone();
        let results_summary = results_summary.clone();
        let scope = *props.active_scope;
        let on_saved = props.on_saved.clone();
        let target_name = props.target_name.clone().unwrap_or_default();
        let translate = translate.clone();

        Callback::from(move |_: String| {
            confirm_open.set(false);
            is_saving.set(true);

            let services = services.clone();
            let items = items.clone();
            let patch = form_data.build_patch(kind);
            let results_summary = results_summary.clone();
            let is_saving = is_saving.clone();
            let on_saved = on_saved.clone();
            let target_name = target_name.clone();
            let translate = translate.clone();

            spawn_local(async move {
                if scope == OverrideScopeDto::Input {
                    let grouped = group_patches_by_parent_input(&items[..], &patch);
                    let mut input_results = Vec::new();

                    for (input_name, patches) in grouped {
                        let count = patches.len();
                        // 1. Fetch current revision. A failed read must not silently start from
                        // an empty revision, so it is reported in the result list.
                        let current_rev =
                            match services.playlist.get_overrides(OverrideScopeDto::Input, &input_name).await {
                                Ok(Some(f)) => f.revision,
                                Ok(None) => String::new(),
                                Err(err) => {
                                    input_results.push(BulkInputResult {
                                        input: input_name.clone(),
                                        count: patches.len(),
                                        status: BulkInputStatus::Error(err.to_string()),
                                    });
                                    continue;
                                }
                            };

                        // 2. Build request
                        let req = OverrideSaveRequest {
                            scope: OverrideScopeDto::Input,
                            name: input_name.clone(),
                            base_revision: current_rev,
                            patches,
                        };

                        // 3. Save
                        let save_res = services.playlist.save_overrides(&req).await;
                        let status = match save_res {
                            crate::services::OverrideSaveResult::Ok(rev) => BulkInputStatus::Saved(rev.revision),
                            crate::services::OverrideSaveResult::Conflict(c) => BulkInputStatus::Conflict(
                                c.error.unwrap_or_else(|| translate.t("MESSAGES.PLAYLIST_EXPLORER.STALE_REVISION")),
                            ),
                            crate::services::OverrideSaveResult::FieldErrors(errs) => {
                                BulkInputStatus::ValidationError(errs)
                            }
                            crate::services::OverrideSaveResult::Err(e) => BulkInputStatus::Error(e),
                        };

                        input_results.push(BulkInputResult { input: input_name, count, status });
                    }

                    let summary = summarize_bulk_results(input_results);
                    let successful_inputs: HashSet<String> = summary.successful_inputs().into_iter().collect();

                    // Locally patch items from successful inputs
                    let patched_items: Vec<UiPlaylistItem> = items
                        .iter()
                        .filter(|it| successful_inputs.contains(it.input_name.as_ref()))
                        .map(|it| patch_playlist_item_bulk(it, &patch))
                        .collect();

                    if !patched_items.is_empty() {
                        on_saved.emit(patched_items);
                    }

                    results_summary.set(Some(summary));
                } else {
                    // Target scope: one request
                    let current_rev =
                        match services.playlist.get_overrides(OverrideScopeDto::Target, &target_name).await {
                            Ok(Some(f)) => f.revision,
                            Ok(None) => String::new(),
                            Err(err) => {
                                results_summary.set(Some(summarize_bulk_results(vec![BulkInputResult {
                                    input: target_name.clone(),
                                    count: items.len(),
                                    status: BulkInputStatus::Error(err.to_string()),
                                }])));
                                is_saving.set(false);
                                return;
                            }
                        };

                    let req = prepare_bulk_target_save_request(&target_name, &current_rev, &items[..], &patch);
                    let save_res = services.playlist.save_overrides(&req).await;

                    let status = match save_res {
                        crate::services::OverrideSaveResult::Ok(rev) => BulkInputStatus::Saved(rev.revision),
                        crate::services::OverrideSaveResult::Conflict(c) => BulkInputStatus::Conflict(
                            c.error.unwrap_or_else(|| translate.t("MESSAGES.PLAYLIST_EXPLORER.STALE_REVISION")),
                        ),
                        crate::services::OverrideSaveResult::FieldErrors(errs) => {
                            BulkInputStatus::ValidationError(errs)
                        }
                        crate::services::OverrideSaveResult::Err(e) => BulkInputStatus::Error(e),
                    };

                    let is_success = matches!(status, BulkInputStatus::Saved(_));
                    let summary = summarize_bulk_results(vec![BulkInputResult {
                        input: target_name,
                        count: items.len(),
                        status,
                    }]);

                    if is_success {
                        let patched_items: Vec<UiPlaylistItem> =
                            items.iter().map(|it| patch_playlist_item_bulk(it, &patch)).collect();
                        on_saved.emit(patched_items);
                    }

                    results_summary.set(Some(summary));
                }

                is_saving.set(false);
            });
        })
    };

    let on_close_results = {
        let results_summary = results_summary.clone();
        let on_close = props.on_close.clone();
        Callback::from(move |_: String| {
            results_summary.set(None);
            on_close.emit(());
        })
    };

    let on_cancel_confirm = {
        let confirm_open = confirm_open.clone();
        Callback::from(move |_: String| confirm_open.set(false))
    };

    let on_cancel_confirm_dialog = {
        let confirm_open = confirm_open.clone();
        Callback::from(move |()| confirm_open.set(false))
    };

    let on_close_results_dialog = {
        let results_summary = results_summary.clone();
        let on_close = props.on_close.clone();
        Callback::from(move |()| {
            results_summary.set(None);
            on_close.emit(());
        })
    };

    let toggle_scope = {
        let active_scope = props.active_scope.clone();
        Callback::from(move |new_scope: OverrideScopeDto| active_scope.set(new_scope))
    };

    html! {
        <div class="det tp__explorer-edit-panel tp__explorer-bulk-panel">
            <div class="in tp__explorer-bulk-panel-content">
                <div class="tp__explorer-edit-header">
                    <h2>{ translate.t("LABEL.EXPLORER_BULK_EDIT") }</h2>
                    <div class="meta">
                        <span class="tp__explorer-count-badge">
                            { format!("{count} {}", translate.t("LABEL.EXPLORER_ITEMS_SELECTED")) }
                        </span>
                    </div>
                </div>

                // Scope toggle
                <div class="scope">
                    <TextButton
                        class={if *props.active_scope == OverrideScopeDto::Input { "active" } else { "" }}
                        name="scope_input"
                        title={translate.t("LABEL.EXPLORER_SCOPE_INPUT")}
                        onclick={let toggle_scope = toggle_scope.clone(); Callback::from(move |_: String| toggle_scope.emit(OverrideScopeDto::Input))}
                    />
                    {
                        if is_target_view {
                            html! {
                                <TextButton
                                    class={if *props.active_scope == OverrideScopeDto::Target { "active" } else { "" }}
                                    name="scope_target"
                                    title={translate.t("LABEL.EXPLORER_SCOPE_TARGET")}
                                    onclick={let toggle_scope = toggle_scope.clone(); Callback::from(move |_: String| toggle_scope.emit(OverrideScopeDto::Target))}
                                />
                            }
                        } else {
                            html! {}
                        }
                    }
                </div>

                <div class="tp__explorer-notice info tp__bulk-disabled-note">
                    { format!("{}: {}, {}, {}, {} (chno, name, title, tmdb)",
                        translate.t("LABEL.EXPLORER_BULK_DISABLED_HINT"),
                        translate.t("LABEL.CHNO"),
                        translate.t("LABEL.NAME"),
                        translate.t("LABEL.TITLE"),
                        translate.t("LABEL.TMDB")) }
                </div>

                <div class="tp__explorer-fields-scroll">
                    // Group Field
                    { render_bulk_field(
                        "group",
                        "LABEL.GROUP",
                        &form_data.group,
                        false,
                        {
                            let form_data = form_data.clone();
                            Callback::from(move |st| {
                                let mut d = (*form_data).clone();
                                d.group = st;
                                form_data.set(d);
                            })
                        },
                        None,
                        None,
                        &translate,
                    ) }

                    // Logo Field + Preview
                    { render_bulk_field(
                        "logo",
                        "LABEL.LOGO",
                        &form_data.logo,
                        true,
                        {
                            let form_data = form_data.clone();
                            Callback::from(move |st| {
                                let mut d = (*form_data).clone();
                                d.logo = st;
                                form_data.set(d);
                            })
                        },
                        None,
                        Some(html! {
                            <ImagePreview
                                url={form_data.logo.value.clone()}
                                is_provider_value={false}
                                class={"tp__explorer-logo-preview"}
                            />
                        }),
                        &translate,
                    ) }

                    // Live-specific fields
                    {
                        if kind == OverrideItemKind::Live {
                            let epg_input = {
                                let on_epg_change = {
                                    let form_data = form_data.clone();
                                    Callback::from(move |val: String| {
                                        let mut d = (*form_data).clone();
                                        d.epg_channel_id.value = val;
                                        form_data.set(d);
                                    })
                                };
                                html! {
                                    <EpgPicker
                                        value={form_data.epg_channel_id.value.clone()}
                                        on_select={on_epg_change}
                                        target_id={props.target_id}
                                        input={Some(first_item.input_name.to_string())}
                                        field_id={Some("PLAYLIST_OVERRIDE.EPG_CHANNEL_ID".to_string())}
                                    />
                                }
                            };
                            html! {
                                <>
                                    { render_bulk_field(
                                        "epg_channel_id",
                                        "LABEL.EPG_CHANNEL_ID",
                                        &form_data.epg_channel_id,
                                        true,
                                        {
                                            let form_data = form_data.clone();
                                            Callback::from(move |st| {
                                                let mut d = (*form_data).clone();
                                                d.epg_channel_id = st;
                                                form_data.set(d);
                                            })
                                        },
                                        Some(epg_input),
                                        None,
                                        &translate,
                                    ) }

                                    { render_bulk_field(
                                        "logo_small",
                                        "LABEL.LOGO_SMALL",
                                        &form_data.logo_small,
                                        true,
                                        {
                                            let form_data = form_data.clone();
                                            Callback::from(move |st| {
                                                let mut d = (*form_data).clone();
                                                d.logo_small = st;
                                                form_data.set(d);
                                            })
                                        },
                                        None,
                                        Some(html! {
                                            <ImagePreview
                                                url={form_data.logo_small.value.clone()}
                                                is_provider_value={false}
                                                class={"tp__explorer-logo-preview"}
                                            />
                                        }),
                                        &translate,
                                    ) }
                                </>
                            }
                        } else {
                            // VOD / Series fields
                            html! {
                                <>
                                    { render_bulk_field(
                                        "genre",
                                        "LABEL.GENRE",
                                        &form_data.genre,
                                        true,
                                        {
                                            let form_data = form_data.clone();
                                            Callback::from(move |st| {
                                                let mut d = (*form_data).clone();
                                                d.genre = st;
                                                form_data.set(d);
                                            })
                                        },
                                        None,
                                        None,
                                        &translate,
                                    ) }

                                    { render_bulk_field(
                                        "rating",
                                        "LABEL.RATING",
                                        &form_data.rating,
                                        true,
                                        {
                                            let form_data = form_data.clone();
                                            Callback::from(move |st| {
                                                let mut d = (*form_data).clone();
                                                d.rating = st;
                                                form_data.set(d);
                                            })
                                        },
                                        None,
                                        None,
                                        &translate,
                                    ) }

                                    { render_bulk_field(
                                        "plot",
                                        "LABEL.PLOT",
                                        &form_data.plot,
                                        true,
                                        {
                                            let form_data = form_data.clone();
                                            Callback::from(move |st| {
                                                let mut d = (*form_data).clone();
                                                d.plot = st;
                                                form_data.set(d);
                                            })
                                        },
                                        None,
                                        None,
                                        &translate,
                                    ) }
                                </>
                            }
                        }
                    }
                </div>

                // Toolbar
                <div class="tp__dialog__toolbar">
                    <div class="tp__dialog__toolbar-left">
                        <TextButton
                            class="secondary"
                            name="cancel"
                            icon="Cancel"
                            title={translate.t("LABEL.CANCEL")}
                            onclick={let on_close = props.on_close.clone(); Callback::from(move |_: String| on_close.emit(()))}
                        />
                    </div>
                    <div class="tp__dialog__toolbar-right">
                        <TextButton
                            class="primary"
                            name="save"
                            icon="Save"
                            title={translate.t("LABEL.SAVE")}
                            disabled={!form_data.has_changes() || *is_saving}
                            onclick={on_save_click}
                        />
                    </div>
                </div>
            </div>

            // Confirmation Dialog
            {
                if *confirm_open {
                    portal_to_body(html! {
                        <CustomDialog
                            class={Some("tp__bulk-confirm-dialog".to_string())}
                            open={true}
                            modal={true}
                            on_close={Some(on_cancel_confirm_dialog.clone())}
                        >
                            <h2>{ translate.t("LABEL.EXPLORER_CONFIRM_BULK_SAVE") }</h2>
                            <div class="tp__dialog__body">
                                <p>
                                    { translate.t("LABEL.EXPLORER_CONFIRM_BULK_MSG")
                                        .replace("{count}", &count.to_string())
                                        .replace("{inputs_count}", &input_counts.len().to_string()) }
                                </p>
                                <ul class="tp__explorer-bulk-inputs-list">
                                    { for input_counts.iter().map(|(inp, cnt)| {
                                        html! {
                                            <li key={inp.clone()}>
                                                { translate.t("LABEL.EXPLORER_BULK_INPUT_ITEMS")
                                                    .replace("{input}", inp)
                                                    .replace("{count}", &cnt.to_string()) }
                                            </li>
                                        }
                                    })}
                                </ul>
                            </div>
                            <div class="tp__dialog__toolbar">
                                <div class="tp__dialog__toolbar-left">
                                    <TextButton
                                        class="secondary"
                                        name="cancel_confirm"
                                        icon="Cancel"
                                        title={translate.t("LABEL.CANCEL")}
                                        onclick={on_cancel_confirm}
                                    />
                                </div>
                                <div class="tp__dialog__toolbar-right">
                                    <TextButton
                                        class="primary"
                                        name="execute_bulk_save"
                                        icon="Check"
                                        title={translate.t("LABEL.EXPLORER_CONFIRM")}
                                        disabled={*is_saving}
                                        onclick={execute_save}
                                    />
                                </div>
                            </div>
                        </CustomDialog>
                    })
                } else {
                    html! {}
                }
            }

            // Results Dialog
            {
                if let Some(summary) = (*results_summary).as_ref() {
                    portal_to_body(html! {
                        <CustomDialog
                            class={Some("tp__bulk-results-dialog".to_string())}
                            open={true}
                            modal={true}
                            on_close={Some(on_close_results_dialog.clone())}
                        >
                            <h2>{ translate.t("LABEL.EXPLORER_BULK_RESULTS_TITLE") }</h2>
                            <div class="tp__dialog__body">
                                <div class="tp__explorer-bulk-results-list">
                                    { for summary.results.iter().map(|res| {
                                        let (status_class, status_label) = match &res.status {
                                            BulkInputStatus::Saved(_) => ("status-saved", translate.t("LABEL.EXPLORER_STATUS_SAVED")),
                                            BulkInputStatus::Conflict(msg) => ("status-conflict", format!("{} ({})", translate.t("LABEL.EXPLORER_STATUS_CONFLICT"), msg)),
                                            BulkInputStatus::ValidationError(_) => ("status-error", translate.t("LABEL.EXPLORER_STATUS_VALIDATION_ERROR")),
                                            BulkInputStatus::Error(msg) => ("status-error", format!("{}: {}", translate.t("LABEL.EXPLORER_STATUS_ERROR"), msg)),
                                        };
                                        html! {
                                            <div key={res.input.clone()} class="tp__explorer-bulk-result-row">
                                                <span class="input-name">
                                                    { translate.t("LABEL.EXPLORER_BULK_RESULT_COUNT")
                                                        .replace("{input}", &res.input)
                                                        .replace("{count}", &res.count.to_string()) }
                                                </span>
                                                <span class={format!("badge {}", status_class)}>{ status_label }</span>
                                            </div>
                                        }
                                    })}
                                </div>
                            </div>
                            <div class="tp__dialog__toolbar">
                                <div class="tp__dialog__toolbar-right">
                                    <TextButton
                                        class="primary"
                                        name="close_results"
                                        icon="Check"
                                        title={translate.t("LABEL.OK")}
                                        onclick={on_close_results}
                                    />
                                </div>
                            </div>
                        </CustomDialog>
                    })
                } else {
                    html! {}
                }
            }
        </div>
    }
}

#[allow(clippy::too_many_arguments)]
fn render_bulk_field(
    field_id: &'static str,
    label_key: &'static str,
    state: &BulkFieldState,
    clearable: bool,
    on_update: Callback<BulkFieldState>,
    custom_input: Option<Html>,
    extra_content: Option<Html>,
    translate: &crate::i18n::YewI18n,
) -> Html {
    let full_field_id = format!("PLAYLIST_OVERRIDE.{}", field_id.to_uppercase());

    let mut options = vec![
        DropDownOption::new(
            "unchanged",
            html! { translate.t("LABEL.EXPLORER_UNCHANGED") },
            state.action == BulkFieldAction::Unchanged,
        ),
        DropDownOption::new(
            "set",
            html! { translate.t("LABEL.EXPLORER_SET_VALUE") },
            state.action == BulkFieldAction::Set,
        ),
    ];

    if clearable {
        options.push(DropDownOption::new(
            "clear",
            html! { translate.t("LABEL.EXPLORER_CLEAR_FIELD") },
            state.action == BulkFieldAction::Clear,
        ));
    }

    options.push(DropDownOption::new(
        "reset",
        html! { translate.t("LABEL.RESET") },
        state.action == BulkFieldAction::Reset,
    ));

    let on_action_select = {
        let state = state.clone();
        let on_update = on_update.clone();
        Callback::from(move |(_, selection): (String, DropDownSelection)| {
            if let DropDownSelection::Single(val) = selection {
                let action = match val.as_str() {
                    "set" => BulkFieldAction::Set,
                    "clear" => BulkFieldAction::Clear,
                    "reset" => BulkFieldAction::Reset,
                    _ => BulkFieldAction::Unchanged,
                };
                on_update.emit(BulkFieldState { action, value: state.value.clone() });
            }
        })
    };

    let on_value_change = {
        let state = state.clone();
        let on_update = on_update.clone();
        Callback::from(move |val: String| {
            on_update.emit(BulkFieldState { action: state.action, value: val });
        })
    };

    html! {
        <div class="tp__explorer-field-row tp__explorer-bulk-field-row tp__bulk-field-group">
            <div class="tp__explorer-field-header">
                <FieldLabel
                    label={translate.t(label_key)}
                    field_id={full_field_id.clone()}
                />
                <div class="tp__bulk-action-select-wrap">
                    <Select
                        name={format!("{field_id}_op")}
                        options={Rc::new(options)}
                        on_select={on_action_select}
                    />
                </div>
            </div>

            {
                if state.action == BulkFieldAction::Set {
                    html! {
                        <div class="tp__form-field tp__form-field__text tp__bulk-field-input-wrap">
                            {
                                if let Some(custom) = custom_input {
                                    custom
                                } else {
                                    html! {
                                        <Input
                                            label={Option::<String>::None}
                                            aria_label={Some(translate.t(label_key))}
                                            name={field_id}
                                            field_id={Some(full_field_id)}
                                            value={state.value.clone()}
                                            on_change={on_value_change}
                                        />
                                    }
                                }
                            }
                            { extra_content.unwrap_or_default() }
                        </div>
                    }
                } else {
                    html! {}
                }
            }
        </div>
    }
}

#[cfg(test)]
#[path = "bulk_edit.test.rs"]
mod tests;
