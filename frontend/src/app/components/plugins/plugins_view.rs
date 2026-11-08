use crate::{
    app::components::{
        plugins::{
            marketplace_view::MarketplaceView,
            plugin_category::category_label,
            plugin_logs::PluginLogs,
            plugin_settings::PluginSettings,
            plugin_slot::use_plugin_ui,
            plugin_updates::{
                is_update_all_candidate, render_policy_selector, render_update_candidate, staged_review,
                update_all_eligible, CandidateActions, PluginUpdatePanel,
            },
            plugin_upload::PluginUpload,
        },
        Card, Checkbox, Chip, ConfirmDialog, ContentDialog, LoadingIndicator, NoContent, TabItem, TabSet, Table,
        TableDefinition, TextButton, ToggleSwitch,
    },
    hooks::use_service_context,
    i18n::{use_translation, YewI18n},
    model::{DialogAction, DialogActions, DialogResult},
    services::PluginService,
};
use shared::model::{
    Permission, PluginHookStats, PluginReferenceDto, PluginReferenceKind, PluginStateDto, PluginStatsDto,
    PluginSummaryDto, PluginTaskDto, PluginUpdateCandidateDto, PluginUpdateDto, PluginUpdatePolicy,
    PluginUpdateRequestDto, PluginUpdateResultKindDto, PluginUpdatesDto, PluginUploadReviewDto,
};
use std::rc::Rc;
use web_sys::HtmlInputElement;
use yew::{
    classes, component, html, platform::spawn_local, use_effect_with, use_memo, use_state, Callback, Event, Html,
    Properties, TargetCast,
};

/// Tab ids of the Plugin Manager.
const TAB_INSTALLED: &str = "installed";
const TAB_MARKETPLACE: &str = "marketplace";

/// The i18n key of a lifecycle state.
fn state_label_key(state: PluginStateDto) -> &'static str {
    match state {
        PluginStateDto::Discovered => "LABEL.PLUGIN_STATE_DISCOVERED",
        PluginStateDto::Invalid => "LABEL.PLUGIN_STATE_INVALID",
        PluginStateDto::PendingApproval => "LABEL.PLUGIN_STATE_PENDING_APPROVAL",
        PluginStateDto::Approved => "LABEL.PLUGIN_STATE_APPROVED",
        PluginStateDto::Loaded => "LABEL.PLUGIN_STATE_LOADED",
        PluginStateDto::Enabled => "LABEL.ENABLED",
        PluginStateDto::Disabled => "LABEL.DISABLED",
        PluginStateDto::Failed => "LABEL.PLUGIN_STATE_FAILED",
    }
}

/// The pill style of a lifecycle state, from the shared task-status pill.
fn state_modifier(state: PluginStateDto) -> &'static str {
    match state {
        PluginStateDto::Enabled => "tp__task-status--active",
        PluginStateDto::Loaded | PluginStateDto::Approved => "tp__task-status--done",
        PluginStateDto::PendingApproval => "tp__task-status--waiting",
        PluginStateDto::Discovered => "tp__task-status--pending",
        PluginStateDto::Disabled => "tp__task-status--paused",
        PluginStateDto::Invalid | PluginStateDto::Failed => "tp__task-status--failed",
    }
}

/// Whether the enable switch does anything: a plugin that is waiting for
/// approval, or that failed validation, cannot be enabled (the server answers
/// `409`), so its switch is read-only.
const fn can_toggle(state: PluginStateDto) -> bool {
    matches!(
        state,
        PluginStateDto::Loaded | PluginStateDto::Enabled | PluginStateDto::Disabled | PluginStateDto::Failed
    )
}

/// One plugin with what the list loads for it besides the summary. Stats and
/// tasks are `None` when their request failed, as it does for a plugin that
/// never loaded.
#[derive(Clone, PartialEq)]
struct PluginEntry {
    summary: PluginSummaryDto,
    stats: Option<PluginStatsDto>,
    tasks: Option<Vec<PluginTaskDto>>,
}

async fn load_entries(service: Rc<PluginService>) -> Result<Vec<PluginEntry>, String> {
    let summaries = service.list().await.map_err(|err| err.to_string())?;
    let mut entries = Vec::with_capacity(summaries.len());
    for summary in summaries {
        let stats = service.get_stats(&summary.id).await.ok();
        let tasks = service.get_tasks(&summary.id).await.ok();
        entries.push(PluginEntry { summary, stats, tasks });
    }
    Ok(entries)
}

fn render_task_summary(translate: &YewI18n, tasks: &[PluginTaskDto]) -> Html {
    // A plugin without tasks has nothing to show here; saying so on every card only costs space.
    if tasks.is_empty() {
        return html! {};
    }
    html! {
        <div class="tp__plugins__tasks">
            <h4>{ format!("{} ({})", translate.t("LABEL.PLUGIN_TASKS"), tasks.len()) }</h4>
            <ul>
                { for tasks.iter().map(|task| html! {
                    <li key={task.id.clone()}>
                        { task.title.clone() }
                        if let Some(schedule) = task.default_schedule.as_ref() {
                            { " - " }<code>{ schedule.clone() }</code>
                        }
                    </li>
                }) }
            </ul>
        </div>
    }
}

crate::app::components::define_table_columns! {
    enum HookColumn {
        Hook => ("hook", "LABEL.PLUGIN_HOOK") { can_hide: false },
        Kept => ("kept", "LABEL.PLUGIN_KEPT"),
        Dropped => ("dropped", "LABEL.PLUGIN_DROPPED"),
        Modified => ("modified", "LABEL.PLUGIN_MODIFIED"),
        Errors => ("errors", "LABEL.PLUGIN_ERRORS"),
        Duration => ("duration_ms", "LABEL.PLUGIN_DURATION_MS"),
    }
}

#[derive(Properties, PartialEq)]
struct HookStatsTableProps {
    /// Keyed by the hook name (unique per plugin).
    hooks: Vec<PluginHookStats>,
}

/// The per-hook counters of one plugin, in the shared `Table`.
#[component]
fn HookStatsTable(props: &HookStatsTableProps) -> Html {
    let translate = use_translation();
    let columns = use_memo((), |()| HookColumn::columns());
    let render_header_cell = {
        let translate = translate.clone();
        Callback::<usize, Html>::from(move |col: usize| match HookColumn::from_index(col) {
            Some(column) => html! { {translate.t(column.header_label())} },
            None => html! {},
        })
    };
    let render_data_cell = Callback::<(usize, usize, Rc<PluginHookStats>), Html>::from(
        |(_, col, hook): (usize, usize, Rc<PluginHookStats>)| match HookColumn::from_index(col) {
            Some(HookColumn::Hook) => html! { {hook.hook.clone()} },
            Some(HookColumn::Kept) => html! { {hook.kept} },
            Some(HookColumn::Dropped) => html! { {hook.dropped} },
            Some(HookColumn::Modified) => html! { {hook.modified} },
            Some(HookColumn::Errors) => html! { {hook.errors} },
            Some(HookColumn::Duration) => html! { {hook.duration_ms} },
            None => html! {},
        },
    );
    let definition = use_memo(props.hooks.clone(), |hooks| TableDefinition::<PluginHookStats> {
        table_id: "plugins.hook_stats".into(),
        columns: columns.clone(),
        row_key: Callback::from(|(_, item): (usize, Rc<PluginHookStats>)| item.hook.clone().into()),
        items: if hooks.is_empty() { None } else { Some(Rc::new(hooks.iter().cloned().map(Rc::new).collect())) },
        on_sort: Callback::noop(),
        render_header_cell,
        render_data_cell,
    });
    html! { <Table::<PluginHookStats> definition={definition} /> }
}

fn render_stats(translate: &YewI18n, stats: Option<&PluginStatsDto>) -> Html {
    let Some(stats) = stats else {
        return html! { <span class="tp__plugins__muted">{ translate.t("MESSAGES.PLUGIN_STATS_UNAVAILABLE") }</span> };
    };
    html! {
        <div class="tp__plugins__stats">
            <div class="tp__plugins__counters">
                <span>{ format!("{}: {}", translate.t("LABEL.PLUGIN_CALLS"), stats.calls) }</span>
                <span>{ format!("{}: {}", translate.t("LABEL.PLUGIN_FAILURES"), stats.failures) }</span>
                <span>{ format!("{}: {}", translate.t("LABEL.PLUGIN_BUDGET_SKIPS"), stats.budget_skips) }</span>
            </div>
            if let Some(error) = stats.last_error.as_ref() {
                <div class="tp__plugins__last-error tp__input-error">
                    { format!("{}: {error}", translate.t("LABEL.PLUGIN_LAST_ERROR")) }
                </div>
            }
            if !stats.hooks.is_empty() {
                <HookStatsTable hooks={stats.hooks.clone()} />
            }
        </div>
    }
}

/// The capabilities an update asks for beyond the installed approval, for the
/// row's notice; `None` when it asks for nothing new.
fn added_capabilities_text(update: &PluginUpdateDto) -> Option<String> {
    (!update.added_capabilities.is_empty()).then(|| update.added_capabilities.join(", "))
}

/// What a row can do. The write controls (`can_write`) are shown only to users
/// with `SystemWrite`; `busy` locks them while an action runs. `generation`
/// changes after every action, so a switch the server refused snaps back.
struct RowActions<'a> {
    can_write: bool,
    busy: bool,
    generation: u32,
    on_settings: &'a Callback<(String, String)>,
    on_logs: &'a Callback<String>,
    on_toggle: &'a Callback<(String, bool)>,
    on_uninstall: &'a Callback<(String, String)>,
    /// Open the review of the update staged for this plugin id.
    on_review_update: &'a Callback<String>,
    /// Ask to roll back: plugin id, name and the version it would return to.
    on_rollback: &'a Callback<(String, String, String)>,
    /// The newer marketplace releases of this plugin the last check found: the
    /// newest, and before it the newest one the approval covers when the newest
    /// asks for more.
    candidates: Vec<&'a PluginUpdateCandidateDto>,
    /// Plugin id, release version and whether to activate at once.
    on_update: &'a Callback<(String, String, bool)>,
    on_policy: &'a Callback<(String, PluginUpdatePolicy)>,
}

fn render_entry(translate: &YewI18n, entry: &PluginEntry, actions: &RowActions<'_>, logs_open: bool) -> Html {
    let summary = &entry.summary;
    let on_logs = actions.on_logs;
    let on_settings = actions.on_settings;
    let toggle_logs = {
        let on_logs = on_logs.clone();
        let id = summary.id.clone();
        Callback::from(move |_: String| on_logs.emit(id.clone()))
    };
    let open_settings = {
        let on_settings = on_settings.clone();
        let id = summary.id.clone();
        let name = summary.name.clone();
        Callback::from(move |_: String| on_settings.emit((id.clone(), name.clone())))
    };
    let toggle = {
        let on_toggle = actions.on_toggle.clone();
        let id = summary.id.clone();
        Callback::from(move |enabled: bool| on_toggle.emit((id.clone(), enabled)))
    };
    let reenable = {
        let on_toggle = actions.on_toggle.clone();
        let id = summary.id.clone();
        Callback::from(move |_: String| on_toggle.emit((id.clone(), true)))
    };
    let uninstall = {
        let on_uninstall = actions.on_uninstall.clone();
        let id = summary.id.clone();
        let name = summary.name.clone();
        Callback::from(move |_: String| on_uninstall.emit((id.clone(), name.clone())))
    };
    let review_update = {
        let on_review_update = actions.on_review_update.clone();
        let id = summary.id.clone();
        Callback::from(move |_: String| on_review_update.emit(id.clone()))
    };
    let rollback = {
        let on_rollback = actions.on_rollback.clone();
        let id = summary.id.clone();
        let name = summary.name.clone();
        let version = summary.rollback.as_ref().map(|prior| prior.version.clone()).unwrap_or_default();
        Callback::from(move |_: String| on_rollback.emit((id.clone(), name.clone(), version.clone())))
    };
    html! {
        <Card key={summary.id.clone()} class={classes!("tp__plugins__card", logs_open.then_some("tp__plugins__card--wide"))}>
            <div class="tp__plugins__header">
                <div class="tp__plugins__title">
                    <h3>{ summary.name.clone() }</h3>
                    if actions.can_write {
                        <ToggleSwitch
                            key={format!("{}-{}", summary.id, actions.generation)}
                            value={summary.enabled}
                            readonly={actions.busy || !can_toggle(summary.state)}
                            aria_label={format!("{} {}", translate.t("LABEL.ENABLED"), summary.name)}
                            on_change={toggle}
                        />
                    }
                </div>
                <div class="tp__plugins__meta">
                    if let Some(version) = summary.version.as_ref() {
                        <span class="tp__plugins__version">{ format!("{} {version}", translate.t("LABEL.VERSION")) }</span>
                    }
                    <span class={classes!("tp__task-status", state_modifier(summary.state))}>
                        { translate.t(state_label_key(summary.state)) }
                    </span>
                    if let Some(category) = summary.category.as_ref() {
                        <span class="tp__plugin-badge">{ category_label(translate, category) }</span>
                    }
                    if let Some(origin) = summary.origin.as_ref() {
                        <span class="tp__plugins__origin">
                            { format!("{} {} @ {}", translate.t("LABEL.PLUGIN_MARKETPLACE_INSTALLED_FROM"), origin.source, origin.version) }
                        </span>
                    }
                </div>
                <div class="tp__plugins__actions">
                    <TextButton
                        name={format!("settings-{}", summary.id)}
                        icon="Settings"
                        class="tp__plugins__action"
                        title={translate.t("LABEL.SETTINGS")}
                        hint={Some(translate.t("LABEL.SETTINGS"))}
                        aria_label={format!("{} {}", translate.t("LABEL.SETTINGS"), summary.name)}
                        onclick={open_settings}
                    />
                    <TextButton
                        name={format!("logs-{}", summary.id)}
                        icon="Log"
                        class="tp__plugins__action"
                        title={translate.t("LABEL.LOGS")}
                        hint={Some(translate.t("LABEL.LOGS"))}
                        aria_label={format!("{} {}", translate.t("LABEL.LOGS"), summary.name)}
                        onclick={toggle_logs}
                    />
                    if actions.can_write {
                        if summary.state == PluginStateDto::Failed {
                            <TextButton
                                name={format!("reenable-{}", summary.id)}
                                icon="Refresh"
                                class="tp__plugins__action"
                                title={translate.t("LABEL.PLUGIN_REENABLE")}
                                hint={Some(translate.t("LABEL.PLUGIN_REENABLE"))}
                                aria_label={format!("{} {}", translate.t("LABEL.PLUGIN_REENABLE"), summary.name)}
                                disabled={actions.busy}
                                onclick={reenable}
                            />
                        }
                        if summary.rollback.is_some() {
                            <TextButton
                                name={format!("rollback-{}", summary.id)}
                                icon="Refresh"
                                class="tp__plugins__action"
                                title={translate.t("LABEL.PLUGIN_ROLLBACK")}
                                hint={Some(translate.t("LABEL.PLUGIN_ROLLBACK"))}
                                aria_label={format!("{} {}", translate.t("LABEL.PLUGIN_ROLLBACK"), summary.name)}
                                disabled={actions.busy}
                                onclick={rollback}
                            />
                        }
                        <TextButton
                            name={format!("uninstall-{}", summary.id)}
                            icon="Delete"
                            class="tp__plugins__action"
                            title={translate.t("LABEL.PLUGIN_UNINSTALL")}
                            hint={Some(translate.t("LABEL.PLUGIN_UNINSTALL"))}
                            aria_label={format!("{} {}", translate.t("LABEL.PLUGIN_UNINSTALL"), summary.name)}
                            disabled={actions.busy}
                            onclick={uninstall}
                        />
                    }
                </div>
            </div>
            if let Some(description) = summary.description.as_ref() {
                <p class="tp__plugins__description">{ description.clone() }</p>
            }
            if let Some(failure) = summary.failure.as_ref() {
                <div class="tp__plugins__failure tp__input-error" role="alert">{ failure.clone() }</div>
            }
            if let Some(reason) = summary.settings_review.as_ref() {
                <div class="tp__plugins__settings-review tp__input-error" role="status">
                    <strong>{ translate.t("LABEL.PLUGIN_SETTINGS_REVIEW") }</strong>{ ": " }{ reason.clone() }
                </div>
            }
            if let Some(update) = summary.update.as_ref().filter(|_| actions.can_write) {
                <div class="tp__plugins__update" role="status">
                    <strong>{ translate.t("LABEL.PLUGIN_UPDATE_AVAILABLE") }</strong>
                    { format!(" {} {}", translate.t("LABEL.VERSION"), update.version) }
                    if let Some(added) = added_capabilities_text(update) {
                        <span class="tp__plugins__update-added">
                            { format!(" - {}: {added}", translate.t("LABEL.PLUGIN_UPDATE_NEW_CAPABILITIES")) }
                        </span>
                    }
                    <TextButton
                        name={format!("review-update-{}", summary.id)}
                        icon="Visibility"
                        title={translate.t("LABEL.PLUGIN_UPDATE_REVIEW")}
                        disabled={actions.busy}
                        onclick={review_update}
                    />
                </div>
            }
            { for actions.candidates.iter().map(|candidate| render_update_candidate(translate, candidate, &CandidateActions {
                can_write: actions.can_write,
                busy: actions.busy,
                on_update: actions.on_update,
            })) }
            if summary.origin.is_some() {
                <div class="tp__plugins__update-policy-row">
                    { render_policy_selector(translate, &summary.id, summary.update_policy, actions.can_write,
                        actions.busy, actions.on_policy) }
                </div>
            }
            if !summary.hooks.is_empty() {
                <div class="tp__plugins__chips">
                    <span>{ translate.t("LABEL.PLUGIN_HOOKS") }</span>
                    { for summary.hooks.iter().map(|hook| html! { <Chip key={hook.clone()} label={hook.clone()} /> }) }
                </div>
            }
            { render_stats(translate, entry.stats.as_ref()) }
            if let Some(tasks) = entry.tasks.as_ref() {
                { render_task_summary(translate, tasks) }
            }
            if logs_open {
                <PluginLogs key={summary.id.clone()} plugin_id={summary.id.clone()} />
            }
        </Card>
    }
}

/// The i18n key of what a plugin reference is.
fn reference_kind_key(kind: PluginReferenceKind) -> &'static str {
    match kind {
        PluginReferenceKind::Input => "LABEL.PLUGIN_REFERENCE_INPUT",
        PluginReferenceKind::TargetOutput => "LABEL.PLUGIN_REFERENCE_TARGET_OUTPUT",
        PluginReferenceKind::TargetStrategy => "LABEL.PLUGIN_REFERENCE_TARGET_STRATEGY",
        PluginReferenceKind::InputManagedChannels => "LABEL.PLUGIN_REFERENCE_INPUT_MANAGED",
        PluginReferenceKind::TargetManagedChannels => "LABEL.PLUGIN_REFERENCE_TARGET_MANAGED",
        PluginReferenceKind::TargetFilter => "LABEL.PLUGIN_REFERENCE_TARGET_FILTER",
        PluginReferenceKind::Schedule => "LABEL.PLUGIN_REFERENCE_SCHEDULE",
    }
}

#[derive(Properties, Clone, PartialEq)]
struct PluginUninstallProps {
    plugin_id: String,
    plugin_name: String,
    /// `true` when the stored data should be deleted too (`purge_kv`).
    on_confirm: Callback<bool>,
    on_cancel: Callback<()>,
}

/// The uninstall confirmation, with the "also delete stored data" choice off by default. It lists what
/// still names the plugin; uninstalling stays allowed, those just go inactive.
#[component]
fn PluginUninstall(props: &PluginUninstallProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let purge = use_state(|| false);
    // Empty until loaded, and when the lookup fails: the warning is advice, never a blocker.
    let references = use_state(Vec::<PluginReferenceDto>::new);
    {
        let references = references.clone();
        let services = services.clone();
        use_effect_with(props.plugin_id.clone(), move |id| {
            let id = id.clone();
            spawn_local(async move {
                if let Ok(found) = services.plugins.references(&id).await {
                    references.set(found);
                }
            });
        });
    }
    let on_purge = {
        let purge = purge.clone();
        Callback::from(move |event: Event| {
            let input: HtmlInputElement = event.target_unchecked_into();
            purge.set(input.checked());
        })
    };
    let on_result = {
        let on_confirm = props.on_confirm.clone();
        let on_cancel = props.on_cancel.clone();
        let purge = purge.clone();
        Callback::from(move |result: DialogResult| match result {
            DialogResult::Ok => on_confirm.emit(*purge),
            DialogResult::Cancel => on_cancel.emit(()),
        })
    };
    let actions = DialogActions {
        left: Some(vec![DialogAction::new_focused(
            "cancel",
            "LABEL.CANCEL",
            DialogResult::Cancel,
            Some("Close".to_owned()),
            Some("secondary".to_owned()),
        )]),
        right: vec![DialogAction::new(
            "uninstall",
            "LABEL.PLUGIN_UNINSTALL",
            DialogResult::Ok,
            Some("Delete".to_owned()),
            Some("primary".to_owned()),
        )],
    };
    let content = html! {
        <div class="tp__plugin-uninstall">
            <h2>{ format!("{} - {}", translate.t("LABEL.PLUGIN_UNINSTALL"), props.plugin_name) }</h2>
            <p>{ translate.t("MESSAGES.PLUGIN_UNINSTALL_CONFIRM") }</p>
            if !references.is_empty() {
                <div class="tp__plugin-uninstall__in-use">
                    <p>{ translate.t("MESSAGES.PLUGIN_UNINSTALL_IN_USE") }</p>
                    <ul>
                        { for references.iter().map(|reference| html! {
                            <li>{ format!("{}: {}", translate.t(reference_kind_key(reference.kind)), reference.name) }</li>
                        }) }
                    </ul>
                </div>
            }
            <div class="tp__plugin-uninstall__purge">
                <Checkbox checked={*purge} aria_label={Some(translate.t("LABEL.PLUGIN_PURGE_DATA"))} onchange={on_purge} />
                <span>{ translate.t("LABEL.PLUGIN_PURGE_DATA") }</span>
            </div>
            <p class="tp__plugins__muted">{ translate.t("MESSAGES.PLUGIN_PURGE_HINT") }</p>
        </div>
    };
    html! {
        <ContentDialog
            content={content}
            actions={actions}
            close_on_backdrop_click={false}
            close_on_confirm={false}
            on_confirm={on_result}
        />
    }
}

/// The Plugin Manager page: every plugin with its state, hook statistics and
/// tasks, a settings dialog per plugin, and, for users with `SystemWrite`, the
/// install dialog, an enable switch and Uninstall per plugin.
#[component]
pub fn PluginsView() -> Html {
    let translate = use_translation();
    let services = use_service_context();

    let entries = use_state(|| None::<Rc<Vec<PluginEntry>>>);
    let loading = use_state(|| true);
    let error = use_state(|| None::<String>);
    let reload = use_state(|| 0_u32);
    let settings_for = use_state(|| None::<(String, String)>);
    let logs_for = use_state(|| None::<String>);
    let show_upload = use_state(|| false);
    let uninstall_for = use_state(|| None::<(String, String)>);
    // The staged update being reviewed, and the rollback awaiting confirmation (id, name, prior version).
    let update_review = use_state(|| None::<PluginUploadReviewDto>);
    let rollback_for = use_state(|| None::<(String, String, String)>);
    // The newer marketplace releases of installed plugins, from the last load or check.
    let updates = use_state(PluginUpdatesDto::default);
    // An action is in flight: the write controls wait for it.
    let acting = use_state(|| false);
    let can_write = services.auth.has_permission(Permission::SystemWrite);
    let plugin_ui = use_plugin_ui();

    {
        let entries = entries.clone();
        let loading = loading.clone();
        let error = error.clone();
        let updates = updates.clone();
        let service = services.plugins.clone();
        use_effect_with(*reload, move |generation| {
            loading.set(true);
            // After an install, enable, disable or uninstall the slots change too.
            let refresh_slots = (*generation > 0).then(|| plugin_ui.map(|ui| ui.refresh)).flatten();
            spawn_local(async move {
                // The candidates are an extra: the list works without them.
                if let Ok(found) = service.updates().await {
                    updates.set(found);
                }
                match load_entries(service).await {
                    Ok(loaded) => {
                        entries.set(Some(Rc::new(loaded)));
                        error.set(None);
                        if let Some(refresh) = refresh_slots {
                            refresh.emit(());
                        }
                    }
                    Err(message) => error.set(Some(message)),
                }
                loading.set(false);
            });
            || ()
        });
    }

    let on_refresh = {
        let reload = reload.clone();
        Callback::from(move |_: String| reload.set(*reload + 1))
    };
    let on_open_settings = {
        let settings_for = settings_for.clone();
        Callback::from(move |target: (String, String)| settings_for.set(Some(target)))
    };
    // One plugin's log is open at a time; its button toggles it.
    let on_toggle_logs = {
        let logs_for = logs_for.clone();
        Callback::from(move |id: String| {
            let open = logs_for.as_deref() == Some(id.as_str());
            logs_for.set((!open).then_some(id));
        })
    };
    let on_close_settings = {
        let settings_for = settings_for.clone();
        Callback::from(move |()| settings_for.set(None))
    };
    let on_toggle = {
        let services = services.clone();
        let translate = translate.clone();
        let acting = acting.clone();
        let reload = reload.clone();
        Callback::from(move |(id, enabled): (String, bool)| {
            if *acting {
                return;
            }
            acting.set(true);
            let next = *reload + 1;
            let services = services.clone();
            let translate = translate.clone();
            let acting = acting.clone();
            let reload = reload.clone();
            spawn_local(async move {
                match services.plugins.set_enabled(&id, enabled).await {
                    Ok(summary) => {
                        let key = if enabled { "MESSAGES.PLUGIN_ENABLED" } else { "MESSAGES.PLUGIN_DISABLED" };
                        services.toastr.success(format!("{}: {}", translate.t(key), summary.name));
                    }
                    Err(err) => services.toastr.error(err.message()),
                }
                acting.set(false);
                reload.set(next);
            });
        })
    };
    let on_uninstall = {
        let uninstall_for = uninstall_for.clone();
        Callback::from(move |target: (String, String)| uninstall_for.set(Some(target)))
    };
    let on_cancel_uninstall = {
        let uninstall_for = uninstall_for.clone();
        Callback::from(move |()| uninstall_for.set(None))
    };
    let on_confirm_uninstall = {
        let services = services.clone();
        let translate = translate.clone();
        let uninstall_for = uninstall_for.clone();
        let acting = acting.clone();
        let reload = reload.clone();
        Callback::from(move |purge_kv: bool| {
            let Some((id, name)) = (*uninstall_for).clone() else {
                return;
            };
            uninstall_for.set(None);
            acting.set(true);
            let next = *reload + 1;
            let services = services.clone();
            let translate = translate.clone();
            let acting = acting.clone();
            let reload = reload.clone();
            spawn_local(async move {
                match services.plugins.uninstall(&id, purge_kv).await {
                    Ok(()) => {
                        services.toastr.success(format!("{}: {name}", translate.t("MESSAGES.PLUGIN_UNINSTALLED")));
                    }
                    Err(err) => services.toastr.error(err.message()),
                }
                acting.set(false);
                reload.set(next);
            });
        })
    };
    let on_review_update = {
        let services = services.clone();
        let update_review = update_review.clone();
        let acting = acting.clone();
        Callback::from(move |id: String| {
            if *acting {
                return;
            }
            acting.set(true);
            let services = services.clone();
            let update_review = update_review.clone();
            let acting = acting.clone();
            spawn_local(async move {
                match services.plugins.staged_review(&id).await {
                    Ok(review) => update_review.set(Some(review)),
                    Err(err) => services.toastr.error(err.message()),
                }
                acting.set(false);
            });
        })
    };
    let on_close_update = {
        let update_review = update_review.clone();
        Callback::from(move |()| update_review.set(None))
    };
    let on_rollback = {
        let rollback_for = rollback_for.clone();
        Callback::from(move |target: (String, String, String)| rollback_for.set(Some(target)))
    };
    let on_confirm_rollback = {
        let services = services.clone();
        let translate = translate.clone();
        let rollback_for = rollback_for.clone();
        let acting = acting.clone();
        let reload = reload.clone();
        Callback::from(move |result: DialogResult| {
            let target = (*rollback_for).clone();
            rollback_for.set(None);
            let (DialogResult::Ok, Some((id, name, _))) = (result, target) else {
                return;
            };
            acting.set(true);
            let next = *reload + 1;
            let services = services.clone();
            let translate = translate.clone();
            let acting = acting.clone();
            let reload = reload.clone();
            spawn_local(async move {
                match services.plugins.rollback(&id).await {
                    Ok(summary) => {
                        let version = summary.version.unwrap_or_default();
                        services
                            .toastr
                            .success(format!("{}: {name} {version}", translate.t("MESSAGES.PLUGIN_ROLLED_BACK")));
                    }
                    Err(err) => services.toastr.error(err.message()),
                }
                acting.set(false);
                reload.set(next);
            });
        })
    };
    let on_update = {
        let services = services.clone();
        let translate = translate.clone();
        let acting = acting.clone();
        let reload = reload.clone();
        let update_review = update_review.clone();
        Callback::from(move |(id, version, apply): (String, String, bool)| {
            if *acting {
                return;
            }
            acting.set(true);
            let next = *reload + 1;
            let services = services.clone();
            let translate = translate.clone();
            let acting = acting.clone();
            let reload = reload.clone();
            let update_review = update_review.clone();
            spawn_local(async move {
                match services.plugins.update(&id, &PluginUpdateRequestDto { version, apply }).await {
                    Ok(result) => match result.result {
                        PluginUpdateResultKindDto::Applied => {
                            services.toastr.success(format!("{}: {id}", translate.t("MESSAGES.PLUGIN_UPDATE_APPLIED")));
                        }
                        PluginUpdateResultKindDto::Staged => match staged_review(result) {
                            Some(review) => update_review.set(Some(review)),
                            None => services.toastr.info(translate.t("MESSAGES.PLUGIN_UPDATE_STAGED")),
                        },
                        PluginUpdateResultKindDto::Failed => {
                            let reason = result.reason.unwrap_or_default();
                            services
                                .toastr
                                .error(format!("{}: {id} {reason}", translate.t("MESSAGES.PLUGIN_UPDATE_FAILED")));
                        }
                    },
                    Err(err) => services.toastr.error(err.message()),
                }
                acting.set(false);
                reload.set(next);
            });
        })
    };
    let on_policy = {
        let services = services.clone();
        let translate = translate.clone();
        let acting = acting.clone();
        let reload = reload.clone();
        Callback::from(move |(id, policy): (String, PluginUpdatePolicy)| {
            if *acting {
                return;
            }
            acting.set(true);
            let next = *reload + 1;
            let services = services.clone();
            let translate = translate.clone();
            let acting = acting.clone();
            let reload = reload.clone();
            spawn_local(async move {
                match services.plugins.set_update_policy(&id, policy).await {
                    Ok(()) => {
                        services
                            .toastr
                            .success(format!("{}: {id}", translate.t("MESSAGES.PLUGIN_UPDATE_POLICY_SAVED")));
                    }
                    Err(err) => services.toastr.error(err.message()),
                }
                acting.set(false);
                reload.set(next);
            });
        })
    };
    let on_check_updates = {
        let services = services.clone();
        let translate = translate.clone();
        let acting = acting.clone();
        let updates = updates.clone();
        Callback::from(move |_: String| {
            if *acting {
                return;
            }
            acting.set(true);
            let services = services.clone();
            let translate = translate.clone();
            let acting = acting.clone();
            let updates = updates.clone();
            spawn_local(async move {
                match services.plugins.check_updates().await {
                    Ok(found) => {
                        for failed in &found.errors {
                            services.toastr.warning(format!("{}: {}", failed.source, failed.error));
                        }
                        let message = if found.candidates.is_empty() {
                            translate.t("MESSAGES.PLUGIN_UPDATES_NONE")
                        } else {
                            format!("{}: {}", translate.t("MESSAGES.PLUGIN_UPDATES_FOUND"), found.candidates.len())
                        };
                        services.toastr.info(message);
                        updates.set(found);
                    }
                    Err(err) => services.toastr.error(err.message()),
                }
                acting.set(false);
            });
        })
    };
    let on_update_all = {
        let services = services.clone();
        let translate = translate.clone();
        let acting = acting.clone();
        let reload = reload.clone();
        let updates = updates.clone();
        Callback::from(move |_: String| {
            if *acting {
                return;
            }
            acting.set(true);
            let next = *reload + 1;
            let services = services.clone();
            let translate = translate.clone();
            let acting = acting.clone();
            let reload = reload.clone();
            let candidates = updates.candidates.clone();
            spawn_local(async move {
                let summary = update_all_eligible(services.plugins.clone(), candidates).await;
                let message = format!(
                    "{}: {} {}, {} {}, {} {}",
                    translate.t("MESSAGES.PLUGIN_UPDATE_ALL_DONE"),
                    summary.applied,
                    translate.t("LABEL.PLUGIN_UPDATE_APPLIED"),
                    summary.staged,
                    translate.t("LABEL.PLUGIN_UPDATE_AWAITING_APPROVAL"),
                    summary.failed,
                    translate.t("LABEL.PLUGIN_STATE_FAILED"),
                );
                if summary.failed > 0 {
                    services.toastr.warning(message);
                } else {
                    services.toastr.success(message);
                }
                acting.set(false);
                reload.set(next);
            });
        })
    };
    let on_open_upload = {
        let show_upload = show_upload.clone();
        Callback::from(move |_: String| show_upload.set(true))
    };
    let on_close_upload = {
        let show_upload = show_upload.clone();
        Callback::from(move |()| show_upload.set(false))
    };
    let on_installed = {
        let reload = reload.clone();
        Callback::from(move |()| reload.set(*reload + 1))
    };

    let list = if let Some(message) = error.as_ref() {
        html! {
            <div class="tp__plugins__error tp__input-error" role="alert">
                { format!("{} {message}", translate.t("MESSAGES.PLUGIN_LOAD_FAILED")) }
            </div>
        }
    } else if let Some(entries) = entries.as_ref() {
        if entries.is_empty() {
            html! { <NoContent text={translate.t("MESSAGES.PLUGIN_LIST_EMPTY")} /> }
        } else {
            html! {
                <div class="tp__plugins__grid">{ for entries.iter().map(|entry| {
                    let logs_open = logs_for.as_deref() == Some(entry.summary.id.as_str());
                    let actions = RowActions {
                        can_write,
                        busy: *acting,
                        generation: *reload,
                        on_settings: &on_open_settings,
                        on_logs: &on_toggle_logs,
                        on_toggle: &on_toggle,
                        on_uninstall: &on_uninstall,
                        on_review_update: &on_review_update,
                        on_rollback: &on_rollback,
                        candidates: updates
                            .candidates
                            .iter()
                            .filter(|candidate| candidate.id == entry.summary.id)
                            .collect(),
                        on_update: &on_update,
                        on_policy: &on_policy,
                    };
                    render_entry(&translate, entry, &actions, logs_open)
                }) }</div>
            }
        }
    } else {
        html! {}
    };

    let body = html! {
        <>
            <PluginUpdatePanel can_write={can_write} />
            { for updates.errors.iter().map(|failed| html! {
                <div key={failed.source.clone()} class="tp__plugins__error tp__input-error" role="alert">
                    { format!("{}: {}", failed.source, failed.error) }
                </div>
            }) }
            { list }
        </>
    };
    let eligible_count = updates.candidates.iter().filter(|candidate| is_update_all_candidate(candidate)).count();

    // The Marketplace tab loads when it is first opened, not with the page.
    let marketplace_opened = use_state(|| false);
    let on_tab_change = {
        let marketplace_opened = marketplace_opened.clone();
        Callback::from(move |tab: String| {
            if tab == TAB_MARKETPLACE {
                marketplace_opened.set(true);
            }
        })
    };
    let tabs = Rc::new(vec![
        TabItem {
            id: TAB_INSTALLED.to_owned(),
            title: translate.t("LABEL.PLUGIN_MARKETPLACE_TAB_INSTALLED"),
            icon: "Nodes".to_owned(),
            children: body,
            active_class: None,
            inactive_class: None,
        },
        TabItem {
            id: TAB_MARKETPLACE.to_owned(),
            title: translate.t("LABEL.PLUGIN_MARKETPLACE"),
            icon: "Download".to_owned(),
            children: if *marketplace_opened {
                html! { <MarketplaceView can_write={can_write} on_changed={on_installed.clone()} /> }
            } else {
                html! {}
            },
            active_class: None,
            inactive_class: None,
        },
    ]);

    html! {
        <div class="tp__config-view tp__plugins">
            <div class="tp__config-view__header">
                <h1>{ translate.t("LABEL.PLUGINS") }</h1>
                <div class="tp__config-view__header-tools">
                    if can_write {
                        <TextButton name="install" icon="Add" title={translate.t("LABEL.PLUGIN_INSTALL")}
                            disabled={*acting} onclick={on_open_upload} />
                    }
                    if can_write {
                        <TextButton name="check-updates" icon="Refresh" title={translate.t("LABEL.PLUGIN_UPDATE_CHECK")}
                            disabled={*acting} onclick={on_check_updates} />
                        <TextButton name="update-all" icon="Download" title={format!("{} ({eligible_count})", translate.t("LABEL.PLUGIN_UPDATE_ALL"))}
                            disabled={*acting || eligible_count == 0} onclick={on_update_all} />
                    }
                    <TextButton name="refresh" icon="Refresh" title={translate.t("LABEL.REFRESH")}
                        disabled={*loading} onclick={on_refresh} />
                </div>
            </div>
            <LoadingIndicator loading={*loading} />
            <div class="tp__config-view__body">
                <TabSet tabs={tabs} class="tp__config-view__tabset" on_tab_change={on_tab_change} />
            </div>
            if can_write && *show_upload {
                <PluginUpload on_close={on_close_upload} on_installed={on_installed.clone()} />
            }
            if let Some(review) = (*update_review).clone().filter(|_| can_write) {
                <PluginUpload key={format!("update-{}", review.id)} on_close={on_close_update}
                    on_installed={on_installed} initial_review={Some(review.clone())} />
            }
            if let Some((_, name, version)) = (*rollback_for).clone() {
                <ConfirmDialog
                    title={format!("{} {name}: {} {version}", translate.t("LABEL.PLUGIN_ROLLBACK"), translate.t("LABEL.VERSION"))}
                    ok_caption="LABEL.PLUGIN_ROLLBACK"
                    cancel_caption="LABEL.CANCEL"
                    on_confirm={on_confirm_rollback}
                    close_on_backdrop_click=false
                />
            }
            if let Some((plugin_id, plugin_name)) = (*uninstall_for).clone() {
                <PluginUninstall key={plugin_id.clone()} plugin_id={plugin_id.clone()} plugin_name={plugin_name}
                    on_confirm={on_confirm_uninstall}
                    on_cancel={on_cancel_uninstall} />
            }
            if let Some((plugin_id, plugin_name)) = (*settings_for).clone() {
                <PluginSettings key={plugin_id.clone()} plugin_id={plugin_id.clone()} plugin_name={plugin_name}
                    settings_review={entries.as_ref().and_then(|entries| entries.iter()
                        .find(|entry| entry.summary.id == plugin_id)
                        .and_then(|entry| entry.summary.settings_review.clone()))}
                    on_close={on_close_settings} />
            }
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_STATES: [PluginStateDto; 8] = [
        PluginStateDto::Discovered,
        PluginStateDto::Invalid,
        PluginStateDto::PendingApproval,
        PluginStateDto::Approved,
        PluginStateDto::Loaded,
        PluginStateDto::Enabled,
        PluginStateDto::Disabled,
        PluginStateDto::Failed,
    ];

    #[test]
    fn every_state_has_its_own_label_key() {
        let mut keys = ALL_STATES.iter().map(|state| state_label_key(*state)).collect::<Vec<_>>();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), ALL_STATES.len());
    }

    #[test]
    fn only_a_plugin_that_can_run_has_a_live_switch() {
        let live: Vec<_> = ALL_STATES.into_iter().filter(|state| can_toggle(*state)).collect();
        assert_eq!(
            live,
            [PluginStateDto::Loaded, PluginStateDto::Enabled, PluginStateDto::Disabled, PluginStateDto::Failed]
        );
        assert!(!can_toggle(PluginStateDto::PendingApproval) && !can_toggle(PluginStateDto::Invalid));
    }

    #[test]
    fn the_update_notice_lists_only_new_capabilities() {
        let mut update = PluginUpdateDto {
            version: "2.0.0".to_string(),
            added_capabilities: Vec::new(),
            removed_capabilities: vec!["storage".to_string()],
        };
        assert_eq!(added_capabilities_text(&update), None);
        update.added_capabilities = vec!["network".to_string(), "epg_write".to_string()];
        assert_eq!(added_capabilities_text(&update).as_deref(), Some("network, epg_write"));
    }

    #[test]
    fn failed_and_invalid_share_the_failure_style() {
        assert_eq!(state_modifier(PluginStateDto::Failed), state_modifier(PluginStateDto::Invalid));
        assert_ne!(state_modifier(PluginStateDto::Enabled), state_modifier(PluginStateDto::Failed));
    }
}
