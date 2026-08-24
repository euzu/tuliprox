//! Recurring-rule view.
//!
//! The view lists and mutates recurring rules, shows matching limitations, and
//! gates shared controls to administrators.

use super::recording_rule_form::RecordingRuleForm;
use crate::{
    app::{
        components::{text_button::TextButton, Table, TableDefinition, ToggleSwitch},
        ConfigContext,
    },
    hooks::use_service_context,
    i18n::{use_translation, YewI18n},
    model::{DialogResult, EventMessage},
    services::{
        DialogService, EditRecordingRuleRequest, RecordingError, RecordingRuleResponse, RecordingRuleSnapshot,
        RecordingService,
    },
};
use shared::model::{recording_rule::RuleBody, ConfigSourceDto, SortOrder};
use std::{cell::Cell, rc::Rc};
use yew::prelude::*;

/// Permission gate: may the principal see the recurring-rule
/// section at all? Any user with `recording.read` can list rules; creation
/// needs `recording.write`.
#[allow(dead_code)]
pub fn can_show_rules_section(has_recording_read: bool) -> bool { has_recording_read }

/// Permission gate: may the principal create new rules? Owners
/// can create private rules; only administrators can create shared
/// rules.
#[allow(dead_code)]
pub fn can_create_rule(has_recording_write: bool) -> bool { has_recording_write }

/// Permission gate: may the principal create a *shared* rule?
/// Administrators with `recording.write` only.
#[allow(dead_code)]
pub fn can_create_shared_rule(has_recording_write: bool, is_admin_role: bool) -> bool {
    has_recording_write && is_admin_role
}

/// Permission gate: may the principal edit this rule?
/// - Private rule: owner with `recording.write`.
/// - Shared rule: administrator with `recording.write`.
#[allow(dead_code)]
pub fn can_edit_rule(has_recording_write: bool, is_admin_role: bool, is_owner: bool, is_shared: bool) -> bool {
    if !has_recording_write {
        return false;
    }
    if is_shared {
        is_admin_role
    } else {
        is_admin_role || is_owner
    }
}

/// Permission gate: may the principal delete this rule?
/// Same matrix as edit.
#[allow(dead_code)]
pub fn can_delete_rule(has_recording_write: bool, is_admin_role: bool, is_owner: bool, is_shared: bool) -> bool {
    can_edit_rule(has_recording_write, is_admin_role, is_owner, is_shared)
}

/// The delete future-policy options exposed to the user. The API
/// requires `future=retain|cancel`; the UI mirrors that with two
/// radio buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum DeleteFuture {
    Retain,
    Cancel,
}

#[allow(dead_code)]
impl DeleteFuture {
    pub fn wire(self) -> &'static str {
        match self {
            Self::Retain => "retain",
            Self::Cancel => "cancel",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "retain" => Some(Self::Retain),
            "cancel" => Some(Self::Cancel),
            _ => None,
        }
    }
}

/// A short, user-facing note for recurring-rule limitations.
/// calls out. The form's text surfaces these alongside the
/// matching-field inputs.
#[allow(dead_code)]
pub fn new_episode_limitations_text() -> &'static str {
    "When the EPG does not publish a stable series id, the rule falls back to the title. \
     Title fallback may record reruns when provider metadata is incomplete."
}

/// DST + timezone explanation for weekly rules. The form's text
/// surfaces this next to the timezone input.
#[allow(dead_code)]
pub fn weekly_timezone_hint_text() -> &'static str {
    "Local wall-clock time. Daylight-saving transitions follow the timezone: \
     ambiguous times (fall-back) pick the earlier instant; nonexistent times \
     (spring-forward) advance to the next valid instant."
}

fn recording_rule_form_key(existing: Option<&RecordingRuleSnapshot>) -> String {
    existing.map_or_else(|| "new".to_string(), |rule| rule.id.clone())
}

/// Map a backend reconciliation error to a stable i18n key the
/// form can render. A failed request may have applied a partial
/// change on the server, so the message tells the user what
/// state the system is in.
#[allow(dead_code)]
pub fn reconciliation_error_to_i18n_key(primary: &str, secondary: &str) -> String {
    format!("MESSAGES.RECORDING.PARTIAL_OPERATION/{primary}/{secondary}")
}

const RULE_HEADERS: &[&str] = &[
    "LABEL.RECORDING_RULE_COLUMN_TARGET",
    "LABEL.RECORDING_RULE_COLUMN_VISIBILITY",
    "LABEL.RECORDING_RULE_COLUMN_SCHEDULE",
    "LABEL.RECORDING_RULE_COLUMN_ENABLED",
    "LABEL.RECORDING_COLUMN_ACTIONS",
];

pub fn rule_summary(rule: &RecordingRuleResponse) -> String {
    let s = &rule.rule.source;
    format!("{} / {} / {}", s.target_id, s.virtual_id, s.input_name)
}

/// i18n key for a rule's visibility. The label used to be a hardcoded
/// English literal, so it stayed in English whatever the UI language.
pub fn rule_visibility_key(rule: &RecordingRuleResponse) -> &'static str {
    use shared::model::recording_rule::RuleVisibility;
    match rule.rule.visibility {
        RuleVisibility::Shared => "LABEL.RECORDING_VISIBILITY_SHARED",
        RuleVisibility::Private => "LABEL.RECORDING_VISIBILITY_PRIVATE",
    }
}

pub fn rule_visibility_label(translate: &YewI18n, rule: &RecordingRuleResponse) -> String {
    translate.t(rule_visibility_key(rule))
}

/// The language-independent part of a weekly rule's schedule: weekday
/// number, local start time, and duration in whole minutes. Split out of
/// [`rule_schedule_label`] so it can be tested without an i18n context.
pub fn rule_weekly_schedule_text(weekday: u8, local_start_time: &str, duration_secs: u64) -> String {
    format!("W{weekday} {local_start_time} ({}m)", duration_secs / 60)
}

pub fn rule_schedule_label(translate: &YewI18n, rule: &RecordingRuleResponse) -> String {
    match &rule.rule.body {
        RuleBody::NewEpisode { .. } => translate.t("LABEL.RECORDING_RULE_KIND_NEW_EPISODE"),
        RuleBody::WeeklyTimeslot { weekday, local_start_time, duration_secs, .. } => {
            rule_weekly_schedule_text(*weekday, local_start_time, *duration_secs)
        }
    }
}

/// Translate a rule-service failure for display.
fn error_message(translate: &YewI18n, error: &RecordingError) -> String { translate.t(error.i18n_key()) }

#[function_component(RecordingRulesView)]
pub fn recording_rules_view() -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let dialog = use_context::<DialogService>();
    let rules = use_state(|| Rc::new(Vec::<RecordingRuleResponse>::new()));
    let editing = use_state(|| None::<RecordingRuleSnapshot>);
    let creating = use_state(|| false);

    {
        let rules = rules.clone();
        let svc = services.clone();
        use_effect_with((), move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(r) = RecordingService::new().list_rules().await {
                    rules.set(Rc::new(r));
                }
            });
            let _ = svc; // suppress unused
            || {}
        });
    }

    // Live updates: the backend broadcasts `RecordingRulesChanged`
    // when the rule repository mutates. Subscribe to it directly
    // — no per-recording-snapshot refetch. The initial fetch above
    // is what the user sees until the first mutation arrives.
    {
        let rules = rules.clone();
        let svc = services.clone();
        use_effect_with((), move |_| {
            let sid = svc.event.subscribe(move |msg| {
                if matches!(msg, EventMessage::RecordingRulesChanged) {
                    let rules = rules.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        if let Ok(r) = RecordingService::new().list_rules().await {
                            rules.set(Rc::new(r));
                        }
                    });
                }
            });
            move || svc.event.unsubscribe(sid)
        });
    }

    let _ = EventMessage::Unauthorized; // ensure EventMessage is referenced for future WS subscription

    let on_done_callback = {
        let rules = rules.clone();
        let editing = editing.clone();
        let creating = creating.clone();
        Callback::from(move |_: ()| {
            editing.set(None);
            creating.set(false);
            let rules = rules.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(r) = RecordingService::new().list_rules().await {
                    rules.set(Rc::new(r));
                }
            });
        })
    };

    let edit_id_click = {
        let editing_outer = editing.clone();
        let rules_outer = rules.clone();
        move |id: String| {
            let editing = editing_outer.clone();
            let rules = rules_outer.clone();
            Callback::from(move |_: String| {
                if let Some(rule) = (*rules).iter().find(|r| r.rule.id == id).map(|r| r.rule.clone()) {
                    editing.set(Some(rule));
                }
            })
        }
    };

    // Deleting a rule is two decisions, not one: drop the rule, and
    // decide what happens to the occurrences it already scheduled. The
    // backend has supported `future=retain|cancel` all along but the UI
    // hardcoded `retain`, so there was no way to stop upcoming recordings
    // from a deleted rule. The dialog now asks.
    let delete_id_click = {
        let rules_outer = rules.clone();
        let dialog_outer = dialog.clone();
        let services_outer = services.clone();
        let translate_outer = translate.clone();
        move |id: String| {
            let rules = rules_outer.clone();
            let dialog = dialog_outer.clone();
            let services = services_outer.clone();
            let translate = translate_outer.clone();
            Callback::from(move |_: String| {
                let Some(dialog) = dialog.clone() else { return };
                let id = id.clone();
                let rules = rules.clone();
                let services = services.clone();
                let translate = translate.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    // Shared with the checkbox in the dialog body: the
                    // dialog itself only reports Ok/Cancel, so the policy
                    // travels out of band.
                    let cancel_future = Rc::new(Cell::new(false));
                    let content = {
                        let cancel_future = cancel_future.clone();
                        let on_change = Callback::from(move |value: bool| cancel_future.set(value));
                        html! {
                            <div class="tp__recording-rule-delete">
                                <p>{ translate.t("LABEL.RECORDING_FORM_RULE_DELETE_CONFIRM") }</p>
                                <label class="tp__recording-rule-delete__option">
                                    <ToggleSwitch value={false} on_change={on_change} />
                                    <span>{ translate.t("LABEL.RECORDING_RULE_DELETE_CANCEL") }</span>
                                </label>
                                <p class="tp__recording-rule-delete__hint">
                                    { translate.t("LABEL.RECORDING_RULE_DELETE_RETAIN") }
                                </p>
                            </div>
                        }
                    };
                    if dialog.content(content, None, true).await != DialogResult::Ok {
                        return;
                    }
                    let future = if cancel_future.get() { "cancel" } else { "retain" };
                    let service = RecordingService::new();
                    match service.delete_rule(&id, future).await {
                        Ok(()) => services.toastr.success(translate.t("MESSAGES.RECORDING.RULE_DELETED")),
                        Err(error) => {
                            log::error!("rule delete failed ({future}): {error}");
                            services.toastr.error(error_message(&translate, &error));
                        }
                    }
                    if let Ok(r) = service.list_rules().await {
                        rules.set(Rc::new(r));
                    }
                });
            })
        }
    };

    // Enabling / disabling a rule was only possible by opening the edit
    // form; the list now carries the switch. Disabling is the reversible
    // way to stop a rule, so it should be the cheapest action available.
    let toggle_enabled_click = {
        let rules_outer = rules.clone();
        let services_outer = services.clone();
        let translate_outer = translate.clone();
        move |id: String, enabled: bool| {
            let rules = rules_outer.clone();
            let services = services_outer.clone();
            let translate = translate_outer.clone();
            Callback::from(move |_: bool| {
                let id = id.clone();
                let rules = rules.clone();
                let services = services.clone();
                let translate = translate.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let request = EditRecordingRuleRequest {
                        body: None,
                        channel_id: None,
                        clear_channel_id: false,
                        pre_roll_secs: None,
                        post_roll_secs: None,
                        visibility: None,
                        enabled: Some(!enabled),
                    };
                    let service = RecordingService::new();
                    match service.edit_rule(&id, request).await {
                        // The switch moves optimistically, so success and
                        // failure look the same for a moment. Say which
                        // one happened.
                        Ok(_) => services.toastr.success(translate.t("MESSAGES.RECORDING.RULE_UPDATED")),
                        Err(error) => {
                            log::error!("rule enable toggle failed: {error}");
                            services.toastr.error(error_message(&translate, &error));
                        }
                    }
                    // Refetch either way: on failure the switch has to
                    // snap back to the server's answer.
                    if let Ok(r) = service.list_rules().await {
                        rules.set(Rc::new(r));
                    }
                });
            })
        }
    };

    let on_create_click = {
        let creating = creating.clone();
        let services = services.clone();
        let translate = translate.clone();
        Callback::from(move |_: String| {
            let creating = creating.clone();
            let services = services.clone();
            let translate = translate.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if super::ensure_recording_available(&services, &translate).await {
                    creating.set(true);
                }
            });
        })
    };

    let config_ctx = use_context::<ConfigContext>();
    let sources: Rc<Vec<Rc<ConfigSourceDto>>> = match config_ctx {
        Some(ctx) => match ctx.config {
            Some(c) => Rc::new(c.sources.sources.iter().cloned().map(Rc::new).collect()),
            None => Rc::new(Vec::new()),
        },
        None => Rc::new(Vec::new()),
    };

    let form = if let Some(rule) = editing.as_ref().as_ref() {
        let rule_value: RecordingRuleSnapshot = (*rule).clone();
        let form_key = recording_rule_form_key(Some(&rule_value));
        html! {
            <RecordingRuleForm
                key={form_key}
                existing={Some(rule_value)}
                sources={sources.clone()}
                on_done={on_done_callback.clone()}
            />
        }
    } else if *creating {
        html! {
            <RecordingRuleForm
                key={recording_rule_form_key(None)}
                existing={Option::<RecordingRuleSnapshot>::None}
                sources={sources.clone()}
                on_done={on_done_callback.clone()}
            />
        }
    } else {
        html! { <></> }
    };

    let headers: Vec<String> = RULE_HEADERS.iter().map(|h| translate.t(h)).collect();
    let render_header = Callback::from(move |col: usize| {
        let headers = headers.clone();
        let col_text = headers.get(col).cloned().unwrap_or_default();
        html! { <>{ col_text }</> }
    });

    // The enabled column is a switch, not text: sorting by it would
    // reorder rows under the pointer mid-click.
    let is_sortable = Callback::from(|col: usize| matches!(col, 0..=2));
    let on_sort = Callback::from(|_: Option<(usize, SortOrder)>| {});

    let rules_items: Rc<Vec<Rc<RecordingRuleResponse>>> = Rc::new((*rules).iter().cloned().map(Rc::new).collect());
    let is_empty = rules_items.is_empty();
    let render_data = {
        let translate = translate.clone();
        Callback::from(move |(_row, col, rule): (usize, usize, Rc<RecordingRuleResponse>)| match col {
            0 => html! { <>{ rule_summary(&rule) }</> },
            1 => html! { <>{ rule_visibility_label(&translate, &rule) }</> },
            2 => html! { <>{ rule_schedule_label(&translate, &rule) }</> },
            3 => {
                let enabled = rule.rule.enabled;
                let on_change = toggle_enabled_click(rule.rule.id.clone(), enabled);
                let label = translate.t(if enabled {
                    "LABEL.RECORDING_RULE_ACTION_DISABLE"
                } else {
                    "LABEL.RECORDING_RULE_ACTION_ENABLE"
                });
                html! {
                    <span class="tp__recording-rule-enabled" title={label.clone()} aria-label={label}>
                        <ToggleSwitch value={enabled} compact={true} on_change={on_change} />
                    </span>
                }
            }
            _ => {
                let id = rule.rule.id.clone();
                let on_edit = edit_id_click(id.clone());
                let on_delete = delete_id_click(id.clone());
                let edit_label = translate.t("LABEL.RECORDING_ACTION_EDIT");
                let delete_label = translate.t("LABEL.RECORDING_ACTION_DELETE");
                let row = rule_summary(&rule);
                html! {
                    <div class="tp__recording-rule-row-actions">
                        <TextButton name="rule_edit" icon="" title={edit_label.clone()}
                            aria_label={format!("{edit_label}: {row}")} onclick={on_edit} />
                        <TextButton name="rule_delete" icon="" class="tp__button--danger" title={delete_label.clone()}
                            aria_label={format!("{delete_label}: {row}")} onclick={on_delete} />
                    </div>
                }
            }
        })
    };

    let table_def = Rc::new(TableDefinition::<RecordingRuleResponse> {
        items: Some(rules_items),
        num_cols: RULE_HEADERS.len(),
        is_sortable,
        render_header_cell: render_header,
        render_data_cell: render_data,
        on_sort,
    });

    html! {
        <div class="tp__recording-rules-view tp__list-view">
            <div class="tp__recording-rules-view__body tp__list-view__body">
                <div class="tp__recording-rules tp__list-list">
                    <div class="tp__recording-rules__header tp__list-list__header">
                        <h1>{ translate.t("LABEL.RECORDING_RULES") }</h1>
                        <TextButton name="rule_create" icon="" title={format!("+ {}", translate.t("LABEL.RECORDING_RULES"))} onclick={on_create_click} />
                    </div>
                    <div class="tp__recording-rules__body tp__list-list__body">
                        { form }
                        if is_empty {
                            <p class="tp__recording-list__empty">
                                { translate.t("MESSAGES.RECORDING.EMPTY_RULES") }
                            </p>
                        } else {
                            <Table::<RecordingRuleResponse> definition={table_def} />
                        }
                    </div>
                </div>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::recording_rule::{RuleSource, RuleVisibility};

    #[test]
    fn can_show_rules_section_requires_recording_read() {
        assert!(!can_show_rules_section(false));
        assert!(can_show_rules_section(true));
    }

    #[test]
    fn recording_rule_form_key_changes_with_rule_id() {
        let first = dummy_rule(
            RuleBody::NewEpisode { series_id: Some("s1".into()), title_pattern: None, exclude_repeat: true },
            RuleVisibility::Private,
            true,
        );
        let mut second = first.rule.clone();
        second.id = "r2".to_string();

        assert_eq!(recording_rule_form_key(Some(&first.rule)), "r1");
        assert_eq!(recording_rule_form_key(Some(&second)), "r2");
        assert_eq!(recording_rule_form_key(None), "new");
    }

    #[test]
    fn can_create_rule_requires_recording_write() {
        assert!(!can_create_rule(false));
        assert!(can_create_rule(true));
    }

    #[test]
    fn can_create_shared_rule_requires_admin() {
        assert!(!can_create_shared_rule(false, false));
        assert!(!can_create_shared_rule(false, true));
        assert!(!can_create_shared_rule(true, false));
        assert!(can_create_shared_rule(true, true));
    }

    #[test]
    fn can_edit_private_rule_owner_or_admin() {
        assert!(!can_edit_rule(false, false, true, false));
        assert!(can_edit_rule(true, false, true, false));
        assert!(!can_edit_rule(true, false, false, false));
        assert!(can_edit_rule(true, true, false, false));
    }

    #[test]
    fn can_edit_shared_rule_only_admin() {
        assert!(!can_edit_rule(true, false, true, true));
        assert!(can_edit_rule(true, true, false, true));
    }

    #[test]
    fn can_delete_rule_matches_edit_rule() {
        for (a, b, c, d) in [(false, false, true, false), (true, true, false, true), (true, false, true, false)] {
            assert_eq!(can_edit_rule(a, b, c, d), can_delete_rule(a, b, c, d));
        }
    }

    #[test]
    fn delete_future_round_trip() {
        assert_eq!(DeleteFuture::from_wire("retain"), Some(DeleteFuture::Retain));
        assert_eq!(DeleteFuture::from_wire("cancel"), Some(DeleteFuture::Cancel));
        assert_eq!(DeleteFuture::from_wire("bogus"), None);
        assert_eq!(DeleteFuture::Retain.wire(), "retain");
        assert_eq!(DeleteFuture::Cancel.wire(), "cancel");
    }

    #[test]
    fn new_episode_limitations_text_mentions_title_fallback() {
        assert!(new_episode_limitations_text().contains("title"));
    }

    #[test]
    fn weekly_timezone_hint_text_mentions_dst() {
        assert!(weekly_timezone_hint_text().to_lowercase().contains("daylight"));
    }

    #[test]
    fn reconciliation_error_to_i18n_key_carries_both_labels() {
        let k = reconciliation_error_to_i18n_key("rule", "tombstone");
        assert!(k.contains("rule"));
        assert!(k.contains("tombstone"));
    }

    fn dummy_rule(body: RuleBody, visibility: RuleVisibility, enabled: bool) -> RecordingRuleResponse {
        RecordingRuleResponse {
            revision: 0,
            rule: RecordingRuleSnapshot {
                id: "r1".to_string(),
                owner_id: "u1".to_string(),
                visibility,
                enabled,
                source: RuleSource::new("tgt", "vid", "input"),
                channel_id: None,
                body,
                pre_roll_secs: 0,
                post_roll_secs: 0,
                created_at: 0,
                updated_at: 0,
            },
        }
    }

    #[test]
    fn rule_summary_includes_target_virtual_and_input() {
        let r = dummy_rule(
            RuleBody::WeeklyTimeslot {
                weekday: 1,
                local_start_time: "20:00".into(),
                duration_secs: 3600,
                timezone: "UTC".into(),
            },
            RuleVisibility::Private,
            true,
        );
        let s = rule_summary(&r);
        assert!(s.contains("tgt"), "missing target_id: {s}");
        assert!(s.contains("vid"), "missing virtual_id: {s}");
        assert!(s.contains("input"), "missing input_name: {s}");
    }

    #[test]
    fn rule_visibility_label_maps_wire_to_label() {
        let r_shared = dummy_rule(
            RuleBody::WeeklyTimeslot {
                weekday: 1,
                local_start_time: "20:00".into(),
                duration_secs: 3600,
                timezone: "UTC".into(),
            },
            RuleVisibility::Shared,
            true,
        );
        let r_priv = dummy_rule(
            RuleBody::WeeklyTimeslot {
                weekday: 1,
                local_start_time: "20:00".into(),
                duration_secs: 3600,
                timezone: "UTC".into(),
            },
            RuleVisibility::Private,
            true,
        );
        // Assert on the i18n key, not the rendered text: the text is
        // whatever the active language says, the key is the contract.
        assert_eq!(rule_visibility_key(&r_shared), "LABEL.RECORDING_VISIBILITY_SHARED");
        assert_eq!(rule_visibility_key(&r_priv), "LABEL.RECORDING_VISIBILITY_PRIVATE");
        assert_ne!(rule_visibility_key(&r_shared), rule_visibility_key(&r_priv));
    }

    #[test]
    fn rule_schedule_label_weekly_format() {
        let s = rule_weekly_schedule_text(3, "21:30", 1800);
        assert!(s.contains("W3"), "missing weekday: {s}");
        assert!(s.contains("21:30"), "missing start: {s}");
        assert!(s.contains("30m"), "missing duration: {s}");
    }

    #[test]
    fn rule_weekly_schedule_rounds_down_to_whole_minutes() {
        assert!(rule_weekly_schedule_text(1, "20:00", 90).contains("(1m)"));
        assert!(rule_weekly_schedule_text(1, "20:00", 59).contains("(0m)"));
    }
}
