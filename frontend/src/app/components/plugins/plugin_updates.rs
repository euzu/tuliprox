//! Plugin updates in the Plugin Manager.
//!
//! The update badge of a plugin row (eligible, or awaiting approval with the reasons), the policy
//! selector, "Update all eligible", the global boot-update settings (also shown on the config
//! page) and the report of the last boot update run. An update that needs a review opens the
//! existing upload review; nothing here approves a package by itself.
use crate::{
    app::components::{
        input::Input, Card, Chip, DropDownOption, DropDownSelection, LoadingIndicator, Select, TextButton, ToggleSwitch,
    },
    hooks::use_service_context,
    i18n::{use_translation, YewI18n},
    services::PluginService,
};
use js_sys::Date;
use shared::model::{
    PluginBootReportDto, PluginBootUpdateOutcomeDto, PluginUpdateBlockerDto, PluginUpdateCandidateDto,
    PluginUpdatePolicy, PluginUpdateRequestDto, PluginUpdateResultKindDto, PluginUpdateSettingsDto,
    PluginUploadReviewDto,
};
use std::rc::Rc;
use wasm_bindgen::JsValue;
use yew::{classes, component, html, platform::spawn_local, use_effect_with, use_state, Callback, Html, Properties};

const POLICIES: [PluginUpdatePolicy; 3] =
    [PluginUpdatePolicy::Auto, PluginUpdatePolicy::Manual, PluginUpdatePolicy::Pinned];

/// The i18n key of an update policy.
const fn policy_label_key(policy: PluginUpdatePolicy) -> &'static str {
    match policy {
        PluginUpdatePolicy::Auto => "LABEL.PLUGIN_UPDATE_POLICY_AUTO",
        PluginUpdatePolicy::Manual => "LABEL.PLUGIN_UPDATE_POLICY_MANUAL",
        PluginUpdatePolicy::Pinned => "LABEL.PLUGIN_UPDATE_POLICY_PINNED",
    }
}

/// The wire name of a policy, which is also the id of its select option.
const fn policy_id(policy: PluginUpdatePolicy) -> &'static str {
    match policy {
        PluginUpdatePolicy::Auto => "auto",
        PluginUpdatePolicy::Manual => "manual",
        PluginUpdatePolicy::Pinned => "pinned",
    }
}

fn policy_from_id(id: &str) -> Option<PluginUpdatePolicy> {
    POLICIES.into_iter().find(|policy| policy_id(*policy) == id)
}

/// The i18n key that words one blocker.
const fn blocker_label_key(blocker: &PluginUpdateBlockerDto) -> &'static str {
    match blocker {
        PluginUpdateBlockerDto::CapabilitiesAdded { .. } => "LABEL.PLUGIN_UPDATE_BLOCKER_CAPABILITIES_ADDED",
        PluginUpdateBlockerDto::McpExposureWidened { .. } => "LABEL.PLUGIN_UPDATE_BLOCKER_MCP_WIDENED",
        PluginUpdateBlockerDto::SettingsMigrationMissing { .. } => "LABEL.PLUGIN_UPDATE_BLOCKER_MIGRATION_MISSING",
        PluginUpdateBlockerDto::SettingsMigrationFailed { .. } => "LABEL.PLUGIN_UPDATE_BLOCKER_MIGRATION_FAILED",
        PluginUpdateBlockerDto::SettingsVersionDowngrade { .. } => "LABEL.PLUGIN_UPDATE_BLOCKER_DOWNGRADE",
    }
}

/// What a blocker adds to its wording: the names, the schema versions, the reason.
fn blocker_detail(blocker: &PluginUpdateBlockerDto) -> String {
    match blocker {
        PluginUpdateBlockerDto::CapabilitiesAdded { capabilities } => capabilities.join(", "),
        PluginUpdateBlockerDto::McpExposureWidened { exports } => exports.join(", "),
        PluginUpdateBlockerDto::SettingsMigrationMissing { from, to }
        | PluginUpdateBlockerDto::SettingsVersionDowngrade { from, to } => format!("v{from} -> v{to}"),
        PluginUpdateBlockerDto::SettingsMigrationFailed { from, to, reason } => format!("v{from} -> v{to}: {reason}"),
    }
}

fn blocker_text(translate: &YewI18n, blocker: &PluginUpdateBlockerDto) -> String {
    let detail = blocker_detail(blocker);
    let label = translate.t(blocker_label_key(blocker));
    if detail.is_empty() {
        label
    } else {
        format!("{label}: {detail}")
    }
}

/// "Update all eligible" takes eligible updates whose policy is not `pinned`.
pub fn is_update_all_candidate(candidate: &PluginUpdateCandidateDto) -> bool {
    candidate.eligible && candidate.policy != PluginUpdatePolicy::Pinned
}

const fn outcome_label_key(outcome: PluginBootUpdateOutcomeDto) -> &'static str {
    match outcome {
        PluginBootUpdateOutcomeDto::UpToDate => "LABEL.PLUGIN_BOOT_UP_TO_DATE",
        PluginBootUpdateOutcomeDto::Applied => "LABEL.PLUGIN_UPDATE_APPLIED",
        PluginBootUpdateOutcomeDto::AwaitingApproval => "LABEL.PLUGIN_UPDATE_AWAITING_APPROVAL",
        PluginBootUpdateOutcomeDto::Failed => "LABEL.PLUGIN_STATE_FAILED",
        PluginBootUpdateOutcomeDto::Skipped => "LABEL.PLUGIN_BOOT_SKIPPED",
    }
}

const fn outcome_modifier(outcome: PluginBootUpdateOutcomeDto) -> &'static str {
    match outcome {
        PluginBootUpdateOutcomeDto::Applied => "tp__task-status--active",
        PluginBootUpdateOutcomeDto::UpToDate => "tp__task-status--done",
        PluginBootUpdateOutcomeDto::AwaitingApproval => "tp__task-status--pending",
        PluginBootUpdateOutcomeDto::Skipped => "tp__task-status--paused",
        PluginBootUpdateOutcomeDto::Failed => "tp__task-status--failed",
    }
}

/// Local time of a unix-seconds stamp as `YYYY-MM-DD HH:MM:SS`.
fn format_unix_secs(secs: i64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let date = Date::new(&JsValue::from_f64(secs as f64 * 1_000.0));
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        date.get_full_year(),
        date.get_month() + 1,
        date.get_date(),
        date.get_hours(),
        date.get_minutes(),
        date.get_seconds()
    )
}

/// What the badge of one row can do.
pub struct CandidateActions<'a> {
    pub can_write: bool,
    pub busy: bool,
    /// Plugin id, release version and whether to activate at once.
    pub on_update: &'a Callback<(String, String, bool)>,
}

fn policy_options(translate: &YewI18n, current: PluginUpdatePolicy) -> Rc<Vec<DropDownOption>> {
    Rc::new(
        POLICIES
            .into_iter()
            .map(|policy| {
                DropDownOption::new(
                    policy_id(policy),
                    html! { {translate.t(policy_label_key(policy))} },
                    policy == current,
                )
            })
            .collect(),
    )
}

/// The update policy of a marketplace-installed plugin: a selector for users with `SystemWrite`,
/// a chip for the others.
pub fn render_policy_selector(
    translate: &YewI18n,
    id: &str,
    policy: PluginUpdatePolicy,
    can_write: bool,
    busy: bool,
    on_policy: &Callback<(String, PluginUpdatePolicy)>,
) -> Html {
    let on_policy = {
        let on_policy = on_policy.clone();
        let id = id.to_owned();
        Callback::from(move |(_, selection): (String, DropDownSelection)| {
            if let Some(policy) = match selection {
                DropDownSelection::Single(value) => policy_from_id(&value),
                _ => None,
            } {
                on_policy.emit((id.clone(), policy));
            }
        })
    };
    if !can_write {
        return html! {
            <Chip label={format!("{}: {}", translate.t("LABEL.PLUGIN_UPDATE_POLICY"), translate.t(policy_label_key(policy)))} />
        };
    }
    html! {
        <span class="tp__plugins__update-policy">
            <label>{ translate.t("LABEL.PLUGIN_UPDATE_POLICY") }</label>
            <Select key={format!("{id}-{}", policy_id(policy))} name={format!("update-policy-{id}")}
                options={policy_options(translate, policy)} on_select={on_policy} />
            if busy {
                <span class="tp__plugins__muted">{ "…" }</span>
            }
        </span>
    }
}

/// The update notice of a plugin row: the new version, whether it can be applied as is or waits
/// for an admin (with every reason), where it comes from, the policy and the Update button.
pub fn render_update_candidate(
    translate: &YewI18n,
    candidate: &PluginUpdateCandidateDto,
    actions: &CandidateActions<'_>,
) -> Html {
    let update = {
        let on_update = actions.on_update.clone();
        let id = candidate.id.clone();
        let version = candidate.version.clone();
        let apply = candidate.eligible;
        Callback::from(move |_: String| on_update.emit((id.clone(), version.clone(), apply)))
    };
    let (status_key, status_modifier) = if candidate.eligible {
        ("LABEL.PLUGIN_UPDATE_ELIGIBLE", "tp__task-status--active")
    } else {
        ("LABEL.PLUGIN_UPDATE_AWAITING_APPROVAL", "tp__task-status--pending")
    };
    let hint_key = if candidate.verified { "LABEL.PLUGIN_UPDATE_VERIFIED" } else { "LABEL.PLUGIN_UPDATE_FROM_CATALOG" };
    let added = &candidate.capability_diff.added;
    html! {
        <div class="tp__plugins__update tp__plugins__update-candidate" role="status">
            <strong>{ translate.t("LABEL.PLUGIN_UPDATE_NEWER_VERSION") }</strong>
            { format!(" {} -> {}", candidate.installed_version, candidate.version) }
            <span class={classes!("tp__task-status", status_modifier)}>{ translate.t(status_key) }</span>
            <span class="tp__plugins__muted" title={translate.t("MESSAGES.PLUGIN_UPDATE_VERIFIED_HINT")}>
                { translate.t(hint_key) }
            </span>
            <span class="tp__plugins__origin">{ format!("{}: {}", translate.t("LABEL.SOURCE"), candidate.source) }</span>
            if actions.can_write {
                <TextButton
                    name={format!("update-{}", candidate.id)}
                    icon="Download"
                    title={translate.t("LABEL.PLUGIN_UPDATE_NOW")}
                    aria_label={format!("{} {}", translate.t("LABEL.PLUGIN_UPDATE_NOW"), candidate.name)}
                    disabled={actions.busy}
                    onclick={update}
                />
            }
            if !candidate.blockers.is_empty() {
                <ul class="tp__plugins__update-blockers">
                    { for candidate.blockers.iter().map(|blocker| html! {
                        <li class="tp__plugins__update-added">{ blocker_text(translate, blocker) }</li>
                    }) }
                </ul>
            }
            if !added.is_empty() {
                <span class="tp__plugins__update-added">
                    { format!("{}: {}", translate.t("LABEL.PLUGIN_UPDATE_NEW_CAPABILITIES"), added.join(", ")) }
                </span>
            }
            if let Some(changelog) = candidate.changelog.as_ref().filter(|text| !text.trim().is_empty()) {
                <details class="tp__plugins__update-changelog">
                    <summary>{ translate.t("LABEL.PLUGIN_MARKETPLACE_CHANGELOG") }</summary>
                    <pre>{ changelog.clone() }</pre>
                </details>
            }
        </div>
    }
}

/// What "Update all eligible" did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UpdateAllSummary {
    pub applied: u32,
    pub staged: u32,
    pub failed: u32,
}

/// Applies the eligible, not pinned updates one after the other (a reload cancels running tasks,
/// so they never overlap). A package that turns out to need a review stays staged.
pub async fn update_all_eligible(
    service: Rc<PluginService>,
    candidates: Vec<PluginUpdateCandidateDto>,
) -> UpdateAllSummary {
    let mut summary = UpdateAllSummary::default();
    for candidate in candidates.into_iter().filter(is_update_all_candidate) {
        let request = PluginUpdateRequestDto { version: candidate.version.clone(), apply: true };
        match service.update(&candidate.id, &request).await {
            Ok(result) => match result.result {
                PluginUpdateResultKindDto::Applied => summary.applied += 1,
                PluginUpdateResultKindDto::Staged => summary.staged += 1,
                PluginUpdateResultKindDto::Failed => summary.failed += 1,
            },
            Err(_) => summary.failed += 1,
        }
    }
    summary
}

/// The review a `staged` answer carries, to open in the existing review dialog.
pub fn staged_review(result: shared::model::PluginUpdateResultDto) -> Option<PluginUploadReviewDto> {
    match result.result {
        PluginUpdateResultKindDto::Staged => result.review,
        _ => None,
    }
}

#[derive(Properties, PartialEq)]
pub struct PluginUpdateSettingsProps {
    /// Only users with `SystemWrite` can change the fields.
    pub can_write: bool,
}

/// The global update switch and boot budget (`plugins.auto_update_on_boot`,
/// `plugins.boot_update_budget`). Shown in the Plugin Manager and on the config page; both read
/// and write the same persisted fields through `…/updates/settings`.
#[component]
pub fn PluginUpdateSettings(props: &PluginUpdateSettingsProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let settings = use_state(|| None::<PluginUpdateSettingsDto>);
    let saved = use_state(|| None::<PluginUpdateSettingsDto>);
    let error = use_state(|| None::<String>);
    let plugins_off = use_state(|| false);
    let saving = use_state(|| false);

    {
        let settings = settings.clone();
        let saved = saved.clone();
        let error = error.clone();
        let plugins_off = plugins_off.clone();
        let service = services.plugins.clone();
        use_effect_with((), move |()| {
            spawn_local(async move {
                match service.update_settings().await {
                    Ok(loaded) => {
                        saved.set(Some(loaded.clone()));
                        settings.set(Some(loaded));
                    }
                    // The routes answer 404 while the plugin system is off.
                    Err(err) if err.status == 404 => plugins_off.set(true),
                    Err(err) => error.set(Some(err.message())),
                }
            });
            || ()
        });
    }

    let on_toggle = {
        let settings = settings.clone();
        Callback::from(move |enabled: bool| {
            if let Some(current) = (*settings).clone() {
                settings.set(Some(PluginUpdateSettingsDto { auto_update_on_boot: enabled, ..current }));
            }
        })
    };
    let on_budget = {
        let settings = settings.clone();
        Callback::from(move |budget: String| {
            if let Some(current) = (*settings).clone() {
                settings.set(Some(PluginUpdateSettingsDto { boot_update_budget: budget, ..current }));
            }
        })
    };
    let on_directory = {
        let settings = settings.clone();
        Callback::from(move |directory: String| {
            if let Some(current) = (*settings).clone() {
                settings.set(Some(PluginUpdateSettingsDto { directory, ..current }));
            }
        })
    };
    let on_save = {
        let settings = settings.clone();
        let saved = saved.clone();
        let saving = saving.clone();
        let services = services.clone();
        let translate = translate.clone();
        Callback::from(move |_: String| {
            let Some(current) = (*settings).clone() else { return };
            saving.set(true);
            let saved = saved.clone();
            let settings = settings.clone();
            let saving = saving.clone();
            let services = services.clone();
            let translate = translate.clone();
            spawn_local(async move {
                match services.plugins.save_update_settings(&current).await {
                    Ok(stored) => {
                        services.toastr.success(translate.t("MESSAGES.PLUGIN_UPDATE_SETTINGS_SAVED"));
                        saved.set(Some(stored.clone()));
                        settings.set(Some(stored));
                    }
                    Err(err) => services.toastr.error(err.message()),
                }
                saving.set(false);
            });
        })
    };

    let Some(current) = (*settings).clone() else {
        if *plugins_off {
            return html! { <p class="tp__plugins__muted">{ translate.t("MESSAGES.PLUGIN_UPDATES_DISABLED") }</p> };
        }
        return match error.as_ref() {
            Some(message) => html! {
                <div class="tp__plugins__error tp__input-error" role="alert">
                    { format!("{} {message}", translate.t("MESSAGES.PLUGIN_UPDATE_SETTINGS_LOAD_FAILED")) }
                </div>
            },
            None => html! {},
        };
    };
    let changed = saved.as_ref() != Some(&current);
    let search_dirs = if current.search_directories.is_empty() {
        if current.effective_directory.is_empty() {
            String::new()
        } else {
            current.effective_directory.clone()
        }
    } else {
        current.search_directories.join(", ")
    };
    html! {
        <div class="tp__plugins__update-settings">
            <fieldset class="tp__plugins__update-settings-fields" disabled={!props.can_write || *saving}>
                <label class="tp__plugins__update-toggle">
                    <ToggleSwitch
                        value={current.auto_update_on_boot}
                        readonly={!props.can_write || *saving}
                        aria_label={translate.t("LABEL.PLUGIN_UPDATE_ON_BOOT")}
                        on_change={on_toggle}
                    />
                    { " " }{ translate.t("LABEL.PLUGIN_UPDATE_ON_BOOT") }
                </label>
                <Input name="plugin_boot_update_budget"
                    label={Some(translate.t("LABEL.PLUGIN_UPDATE_BOOT_BUDGET"))}
                    value={current.boot_update_budget.clone()}
                    on_change={Some(on_budget)} />
                <Input name="plugin_directory"
                    label={Some(translate.t("LABEL.PLUGIN_DIRECTORY"))}
                    value={current.directory.clone()}
                    placeholder={Some("plugins".to_string())}
                    hint_key={Some("PLUGIN_DIRECTORY".to_string())}
                    readonly={current.directory_overridden}
                    on_change={(!current.directory_overridden).then_some(on_directory)} />
            </fieldset>
            <p class="tp__plugins__muted">{ translate.t("MESSAGES.PLUGIN_UPDATE_ON_BOOT_HINT") }</p>
            <p class="tp__plugins__muted">{ translate.t("MESSAGES.PLUGIN_DIRECTORY_HINT") }</p>
            if current.directory_overridden {
                <p class="tp__plugins__muted">{ translate.t("MESSAGES.PLUGIN_DIRECTORY_OVERRIDDEN") }</p>
            }
            if !search_dirs.is_empty() {
                <p class="tp__plugins__muted">
                    { format!("{}: {search_dirs}", translate.t("LABEL.PLUGIN_SEARCH_DIRECTORIES")) }
                </p>
            }
            if props.can_write {
                <div class="tp__plugins__update-settings__toolbar">
                    <TextButton name="save-update-settings" class="primary" icon="Save" title={translate.t("LABEL.SAVE")}
                        disabled={*saving || !changed} onclick={on_save} />
                </div>
            }
        </div>
    }
}

fn render_boot_report(translate: &YewI18n, report: &PluginBootReportDto) -> Html {
    let Some(started) = report.started_at else {
        return html! { <p class="tp__plugins__muted">{ translate.t("MESSAGES.PLUGIN_BOOT_REPORT_NONE") }</p> };
    };
    html! {
        <div class="tp__plugins__boot-report">
            <p>
                { format!("{}: {}", translate.t("LABEL.PLUGIN_BOOT_STARTED"), format_unix_secs(started)) }
                if let Some(finished) = report.finished_at {
                    { format!(" - {}: {}", translate.t("LABEL.PLUGIN_BOOT_FINISHED"), format_unix_secs(finished)) }
                }
                { format!(" - {}: {} ms", translate.t("LABEL.PLUGIN_UPDATE_BOOT_BUDGET"), report.budget_ms) }
            </p>
            if report.budget_exceeded {
                <p class="tp__plugins__update-added" role="alert">{ translate.t("MESSAGES.PLUGIN_BOOT_BUDGET_EXCEEDED") }</p>
            }
            if !report.recovered.is_empty() {
                <p class="tp__plugins__update-added" role="alert">
                    { format!("{}: {}", translate.t("LABEL.PLUGIN_BOOT_RECOVERED"), report.recovered.join(", ")) }
                </p>
                <p class="tp__plugins__muted">{ translate.t("MESSAGES.PLUGIN_BOOT_RECOVERED_HINT") }</p>
            }
            if report.entries.is_empty() {
                <p class="tp__plugins__muted">{ translate.t("MESSAGES.PLUGIN_BOOT_REPORT_EMPTY") }</p>
            } else {
                <ul class="tp__plugins__boot-entries">
                    { for report.entries.iter().map(|entry| html! {
                        <li key={entry.id.clone()}>
                            <strong>{ entry.id.clone() }</strong>
                            { format!(" {}", entry.from) }
                            if let Some(to) = entry.to.as_ref() {
                                { format!(" -> {to}") }
                            }
                            { " " }
                            <span class={classes!("tp__task-status", outcome_modifier(entry.outcome))}>
                                { translate.t(outcome_label_key(entry.outcome)) }
                            </span>
                            if let Some(source) = entry.source.as_ref() {
                                <span class="tp__plugins__origin">{ source.clone() }</span>
                            }
                            if let Some(reason) = entry.reason.as_ref() {
                                <span class="tp__plugins__muted">{ format!(" {reason}") }</span>
                            }
                            <span class="tp__plugins__muted">{ format!(" ({})", format_unix_secs(entry.attempted_at)) }</span>
                        </li>
                    }) }
                </ul>
            }
        </div>
    }
}

/// The last boot update run: when it ran, whether the budget ran out, the plugins that were
/// recovered from an interrupted swap, and each plugin's outcome with its reason.
#[component]
pub fn PluginBootReport() -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let report = use_state(|| None::<PluginBootReportDto>);
    let error = use_state(|| None::<String>);
    let plugins_off = use_state(|| false);
    {
        let report = report.clone();
        let error = error.clone();
        let plugins_off = plugins_off.clone();
        let service = services.plugins.clone();
        use_effect_with((), move |()| {
            spawn_local(async move {
                match service.boot_report().await {
                    Ok(loaded) => report.set(Some(loaded)),
                    // The settings above already say the plugin system is off.
                    Err(err) if err.status == 404 => plugins_off.set(true),
                    Err(err) => error.set(Some(err.message())),
                }
            });
            || ()
        });
    }
    if *plugins_off {
        return html! {};
    }
    let body = match (report.as_ref(), error.as_ref()) {
        (Some(report), _) => render_boot_report(&translate, report),
        (None, Some(message)) => html! {
            <div class="tp__plugins__error tp__input-error" role="alert">
                { format!("{} {message}", translate.t("MESSAGES.PLUGIN_BOOT_REPORT_LOAD_FAILED")) }
            </div>
        },
        (None, None) => html! { <LoadingIndicator loading={true} /> },
    };
    html! {
        <details class="tp__plugins__boot-details">
            <summary>{ translate.t("LABEL.PLUGIN_BOOT_REPORT") }</summary>
            { body }
        </details>
    }
}

/// The updates panel above the plugin list: the global settings and the boot report.
#[derive(Properties, PartialEq)]
pub struct PluginUpdatePanelProps {
    pub can_write: bool,
}

#[component]
pub fn PluginUpdatePanel(props: &PluginUpdatePanelProps) -> Html {
    let translate = use_translation();
    html! {
        <Card class="tp__plugins__update-panel">
            <h3>{ translate.t("LABEL.PLUGIN_UPDATES") }</h3>
            <PluginUpdateSettings can_write={props.can_write} />
            <PluginBootReport />
        </Card>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::PluginCapabilityDiffDto;

    fn candidate(eligible: bool, policy: PluginUpdatePolicy) -> PluginUpdateCandidateDto {
        PluginUpdateCandidateDto {
            id: "sports".to_string(),
            name: "Sports".to_string(),
            installed_version: "1.0.0".to_string(),
            version: "1.1.0".to_string(),
            source: "community".to_string(),
            policy,
            enabled: true,
            eligible,
            verified: false,
            blockers: Vec::new(),
            capability_diff: PluginCapabilityDiffDto {
                added: Vec::new(),
                removed: Vec::new(),
                unchanged: Vec::new(),
                mcp_was_approved: false,
            },
            changelog: None,
        }
    }

    #[test]
    fn every_blocker_has_its_own_wording() {
        let blockers = [
            PluginUpdateBlockerDto::CapabilitiesAdded { capabilities: vec!["network".to_string()] },
            PluginUpdateBlockerDto::McpExposureWidened { exports: vec!["search".to_string()] },
            PluginUpdateBlockerDto::SettingsMigrationMissing { from: 1, to: 2 },
            PluginUpdateBlockerDto::SettingsMigrationFailed { from: 1, to: 2, reason: "bad".to_string() },
            PluginUpdateBlockerDto::SettingsVersionDowngrade { from: 2, to: 1 },
        ];
        let mut keys = blockers.iter().map(blocker_label_key).collect::<Vec<_>>();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), blockers.len());
    }

    #[test]
    fn blocker_details_carry_names_versions_and_reasons() {
        assert_eq!(
            blocker_detail(&PluginUpdateBlockerDto::CapabilitiesAdded {
                capabilities: vec!["network".to_string(), "storage".to_string()]
            }),
            "network, storage"
        );
        assert_eq!(blocker_detail(&PluginUpdateBlockerDto::SettingsMigrationMissing { from: 1, to: 3 }), "v1 -> v3");
        assert_eq!(
            blocker_detail(&PluginUpdateBlockerDto::SettingsMigrationFailed {
                from: 1,
                to: 2,
                reason: "bad".to_string()
            }),
            "v1 -> v2: bad"
        );
    }

    #[test]
    fn update_all_takes_eligible_updates_that_are_not_pinned() {
        assert!(is_update_all_candidate(&candidate(true, PluginUpdatePolicy::Auto)));
        assert!(is_update_all_candidate(&candidate(true, PluginUpdatePolicy::Manual)));
        assert!(!is_update_all_candidate(&candidate(true, PluginUpdatePolicy::Pinned)));
        assert!(!is_update_all_candidate(&candidate(false, PluginUpdatePolicy::Auto)));
    }

    #[test]
    fn policies_round_trip_through_their_option_ids() {
        for policy in POLICIES {
            assert_eq!(policy_from_id(policy_id(policy)), Some(policy));
        }
        assert_eq!(policy_from_id("other"), None);
        let mut keys = POLICIES.iter().map(|policy| policy_label_key(*policy)).collect::<Vec<_>>();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), POLICIES.len());
    }

    #[test]
    fn only_a_staged_answer_opens_the_review() {
        use shared::model::PluginUpdateResultDto;
        let answer = |result| PluginUpdateResultDto { result, review: None, blockers: Vec::new(), reason: None };
        assert!(staged_review(answer(PluginUpdateResultKindDto::Applied)).is_none());
        assert!(staged_review(answer(PluginUpdateResultKindDto::Failed)).is_none());
        assert!(staged_review(answer(PluginUpdateResultKindDto::Staged)).is_none(), "no review to show");
    }

    #[test]
    fn boot_outcomes_have_distinct_labels() {
        let outcomes = [
            PluginBootUpdateOutcomeDto::UpToDate,
            PluginBootUpdateOutcomeDto::Applied,
            PluginBootUpdateOutcomeDto::AwaitingApproval,
            PluginBootUpdateOutcomeDto::Failed,
            PluginBootUpdateOutcomeDto::Skipped,
        ];
        let mut keys = outcomes.iter().map(|outcome| outcome_label_key(*outcome)).collect::<Vec<_>>();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), outcomes.len());
    }

    #[test]
    fn update_settings_dto_serde_defaults_and_backward_compatibility() {
        let legacy_json = r#"{"autoUpdateOnBoot":true,"bootUpdateBudget":"30s"}"#;
        let dto: PluginUpdateSettingsDto = serde_json::from_str(legacy_json).unwrap();
        assert!(dto.auto_update_on_boot);
        assert_eq!(dto.boot_update_budget, "30s");
        assert_eq!(dto.directory, "");
        assert_eq!(dto.effective_directory, "");
        assert!(!dto.directory_overridden);
        assert_eq!(dto.search_directories, Vec::<String>::new());

        let extended_json = r#"{
            "autoUpdateOnBoot": false,
            "bootUpdateBudget": "45s",
            "directory": "custom-plugins",
            "effectiveDirectory": "/var/tuliprox/custom-plugins",
            "directoryOverridden": true,
            "searchDirectories": ["/app/plugins", "/var/tuliprox/custom-plugins"]
        }"#;
        let dto2: PluginUpdateSettingsDto = serde_json::from_str(extended_json).unwrap();
        assert_eq!(dto2.directory, "custom-plugins");
        assert_eq!(dto2.effective_directory, "/var/tuliprox/custom-plugins");
        assert!(dto2.directory_overridden);
        assert_eq!(dto2.search_directories, vec!["/app/plugins", "/var/tuliprox/custom-plugins"]);

        let alias_json = r#"{
            "autoUpdateOnBoot": true,
            "bootUpdateBudget": "15s",
            "resolvedDirectory": "/app/plugins",
            "overridden": true,
            "searchDirectories": ["/app/plugins"]
        }"#;
        let dto_alias: PluginUpdateSettingsDto = serde_json::from_str(alias_json).unwrap();
        assert_eq!(dto_alias.effective_directory, "/app/plugins");
        assert!(dto_alias.directory_overridden);
        assert_eq!(dto_alias.search_directories, vec!["/app/plugins"]);
    }

    #[test]
    fn update_settings_change_detection_includes_directory() {
        let saved = PluginUpdateSettingsDto {
            auto_update_on_boot: false,
            boot_update_budget: "30s".to_string(),
            directory: String::new(),
            effective_directory: "/app/config/plugins".to_string(),
            directory_overridden: false,
            search_directories: vec!["/app/plugins".to_string(), "/app/config/plugins".to_string()],
        };
        let mut edited = saved.clone();
        assert_eq!(saved, edited);
        edited.directory = "my-plugins".to_string();
        assert_ne!(saved, edited);
    }

    #[test]
    fn update_settings_scss_toolbar_is_right_aligned() {
        let scss = include_str!("../../../../scss/app/components/_plugins.scss");
        let (_, settings) = scss.split_once(".tp__plugins__update-settings {").expect("settings scss");
        let (_, rest) = settings.split_once("&__toolbar {").expect("toolbar rule");
        let (toolbar, _) = rest.split_once('}').expect("toolbar rule end");
        assert!(toolbar.contains("justify-content: flex-end;"));
        assert!(toolbar.contains("gap: var(--margin-small);"));
    }
}
