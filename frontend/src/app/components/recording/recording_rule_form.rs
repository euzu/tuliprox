//! Form to create or edit a recurring recording rule.
//!
//! Renders inside a dedicated `RecordingRuleFormView` panel. The
//! panel-based layout is the standard master-detail pattern: the
//! rule list is in `RecordingRulesView`, the form is in its own
//! panel, save/cancel switches back. Submit calls
//! `RecordingService::create_rule` or `RecordingService::edit_rule`
//! depending on whether an `existing` rule is provided.
//!
//! Uses the shared form-reducer macros (`generate_form_reducer!` +
//! `edit_field_*!`) so all typed fields share one diffing source of
//! truth. Target and input are `Select` widgets over the config
//! targets / inputs lists — the user only sees the durable target
//! and input names; the target name is the stable wire value. The target
//! dropdown is filtered to the sources that contain the selected
//! input, so picking an input narrows the available targets.

use crate::{
    app::components::{
        config::HasFormData, select::Select, selection_first_owned, DropDownOption, DropDownSelection, TextButton,
    },
    config_field_child, edit_field_bool, edit_field_number_u64, edit_field_number_u8, edit_field_text,
    edit_field_text_option, generate_form_reducer,
    hooks::use_service_context,
    i18n::use_translation,
    services::{CreateRecordingRuleRequest, EditRecordingRuleRequest, RecordingRuleSnapshot, RecordingService},
};
use shared::model::{
    recording_rule::{RuleBody, RuleVisibility},
    ConfigSourceDto, ConfigTargetDto,
};
use std::rc::Rc;
use yew::prelude::*;

/// Flat form DTO. All fields are tracked through the reducer so
/// the diffing + modification flag stay consistent.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RuleFormDto {
    pub target_id: Option<String>,
    pub virtual_id: String,
    pub input_name: String,
    pub channel_id: Option<String>,
    /// `"weekly"` or `"new_episode"`.
    pub kind: String,
    // Weekly fields.
    pub weekday: u8,
    pub start_time: String,
    pub duration_secs: u64,
    pub timezone: String,
    // NewEpisode fields.
    pub series_id: Option<String>,
    pub title_pattern: Option<String>,
    pub exclude_repeat: bool,
    // Padding.
    pub pre_roll: u64,
    pub post_roll: u64,
    // Visibility + enabled.
    pub visibility: String,
    pub enabled: bool,
}

generate_form_reducer!(
    state: RuleFormState { form: RuleFormDto },
    action_name: RuleFormAction,
    fields {
        TargetId => target_id: Option<String>,
        VirtualId => virtual_id: String,
        InputName => input_name: String,
        ChannelId => channel_id: Option<String>,
        Kind => kind: String,
        Weekday => weekday: u8,
        StartTime => start_time: String,
        DurationSecs => duration_secs: u64,
        Timezone => timezone: String,
        SeriesId => series_id: Option<String>,
        TitlePattern => title_pattern: Option<String>,
        ExcludeRepeat => exclude_repeat: bool,
        PreRoll => pre_roll: u64,
        PostRoll => post_roll: u64,
        Visibility => visibility: String,
        Enabled => enabled: bool,
    }
);

fn dto_from_existing(existing: &RecordingRuleSnapshot) -> RuleFormDto {
    let mut dto = RuleFormDto {
        target_id: Some(existing.source.target_id.clone()),
        virtual_id: existing.source.virtual_id.clone(),
        input_name: existing.source.input_name.clone(),
        channel_id: existing.channel_id.clone(),
        kind: match &existing.body {
            RuleBody::NewEpisode { .. } => "new_episode".to_string(),
            RuleBody::WeeklyTimeslot { .. } => "weekly".to_string(),
        },
        pre_roll: existing.pre_roll_secs,
        post_roll: existing.post_roll_secs,
        visibility: match existing.visibility {
            RuleVisibility::Shared => "shared".to_string(),
            RuleVisibility::Private => "private".to_string(),
        },
        enabled: existing.enabled,
        ..Default::default()
    };
    match &existing.body {
        RuleBody::NewEpisode { series_id, title_pattern, exclude_repeat } => {
            dto.series_id = series_id.clone();
            dto.title_pattern = title_pattern.clone();
            dto.exclude_repeat = *exclude_repeat;
        }
        RuleBody::WeeklyTimeslot { weekday, local_start_time, duration_secs, timezone } => {
            dto.weekday = *weekday;
            dto.start_time = local_start_time.clone();
            dto.duration_secs = *duration_secs;
            dto.timezone = timezone.clone();
        }
    }
    dto
}

fn dto_defaults() -> RuleFormDto {
    RuleFormDto {
        target_id: None,
        virtual_id: String::new(),
        input_name: String::new(),
        channel_id: None,
        kind: "weekly".to_string(),
        weekday: 1,
        start_time: "20:00".to_string(),
        duration_secs: 3600,
        timezone: "UTC".to_string(),
        series_id: None,
        title_pattern: None,
        exclude_repeat: true,
        pre_roll: 60,
        post_roll: 120,
        visibility: "private".to_string(),
        enabled: true,
    }
}

/// Build a `CreateRecordingRuleRequest` from the form's collected
/// state. Pure — unit-testable.
pub fn build_create_request(form: &RuleFormDto, visibility: RuleVisibility) -> Option<CreateRecordingRuleRequest> {
    let channel_id_opt = form.channel_id.clone().filter(|s| !s.trim().is_empty());
    let body = if form.kind == "new_episode" {
        RuleBody::NewEpisode {
            series_id: form.series_id.clone().filter(|s| !s.trim().is_empty()),
            title_pattern: form.title_pattern.clone().filter(|s| !s.trim().is_empty()),
            exclude_repeat: form.exclude_repeat,
        }
    } else {
        RuleBody::WeeklyTimeslot {
            weekday: form.weekday,
            local_start_time: form.start_time.clone(),
            duration_secs: form.duration_secs,
            timezone: form.timezone.clone(),
        }
    };
    Some(CreateRecordingRuleRequest {
        target_id: form.target_id.clone()?,
        virtual_id: form.virtual_id.clone(),
        input_name: form.input_name.clone(),
        body,
        channel_id: channel_id_opt,
        pre_roll_secs: form.pre_roll,
        post_roll_secs: form.post_roll,
        visibility,
    })
}

fn show_source_controls(existing: bool) -> bool { !existing }

fn channel_id_patch(existing: Option<&str>, current: Option<&str>) -> (Option<String>, bool) {
    let current = current.filter(|value| !value.trim().is_empty());
    match (existing, current) {
        (Some(old), Some(new)) if old == new => (None, false),
        (Some(_), None) => (None, true),
        (None, None) => (None, false),
        (_, Some(new)) => (Some(new.to_string()), false),
    }
}

#[derive(Clone, PartialEq, Properties)]
pub struct RuleFormProps {
    pub existing: Option<RecordingRuleSnapshot>,
    pub sources: Rc<Vec<Rc<ConfigSourceDto>>>,
    pub on_done: Option<Callback<()>>,
}

/// Filter the source list to those that contain the given input
/// name. Returns the union of their `targets`.
fn targets_for_input(sources: &[Rc<ConfigSourceDto>], input_name: &str) -> Vec<Rc<ConfigTargetDto>> {
    if input_name.trim().is_empty() {
        return sources.iter().flat_map(|s| s.targets.iter().cloned()).map(Rc::new).collect();
    }
    sources
        .iter()
        .filter(|s| s.inputs.iter().any(|i| i.as_ref() == input_name))
        .flat_map(|s| s.targets.iter().cloned())
        .map(Rc::new)
        .collect()
}

/// Collect every input name across all sources, de-duplicated.
fn all_input_names(sources: &[Rc<ConfigSourceDto>]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for s in sources {
        for name in &s.inputs {
            let name_str: String = name.to_string();
            if !seen.contains(&name_str) {
                seen.push(name_str);
            }
        }
    }
    seen
}

#[function_component(RecordingRuleForm)]
pub fn recording_rule_form(props: &RuleFormProps) -> Html {
    let services = use_service_context();
    let translate = use_translation();

    let initial = props.existing.as_ref().map(dto_from_existing).unwrap_or_else(dto_defaults);
    let form_state: UseReducerHandle<RuleFormState> = use_reducer(|| RuleFormState { form: initial, modified: false });

    let inputs = use_memo(props.sources.clone(), |sources| all_input_names(sources));

    let filtered_targets =
        use_memo((props.sources.clone(), form_state.form.input_name.clone()), |(sources, input_name)| {
            targets_for_input(sources, input_name)
        });

    let input_options = use_memo((inputs.clone(), form_state.form.input_name.clone()), |(inputs, selected)| {
        inputs
            .iter()
            .map(|name_str| DropDownOption {
                id: name_str.clone(),
                label: html! { name_str.clone() },
                selected: name_str == selected,
            })
            .collect::<Vec<DropDownOption>>()
    });

    let target_options =
        use_memo((filtered_targets.clone(), form_state.form.target_id.clone()), |(targets, selected)| {
            targets
                .iter()
                .map(|t| {
                    let name_str: String = t.name.to_string();
                    DropDownOption {
                        id: name_str.clone(),
                        label: html! { name_str.clone() },
                        selected: selected.as_deref() == Some(name_str.as_str()),
                    }
                })
                .collect::<Vec<DropDownOption>>()
        });

    let kind_options = use_memo(form_state.form.kind.clone(), |kind| {
        vec![
            DropDownOption {
                id: "weekly".to_string(),
                label: html! { translate.t("LABEL.RECORDING_RULE_KIND_WEEKLY") },
                selected: kind == "weekly",
            },
            DropDownOption {
                id: "new_episode".to_string(),
                label: html! { translate.t("LABEL.RECORDING_RULE_KIND_NEW_EPISODE") },
                selected: kind == "new_episode",
            },
        ]
    });

    let visibility_options = use_memo(form_state.form.visibility.clone(), |vis| {
        vec![
            DropDownOption { id: "private".to_string(), label: html! { "Private" }, selected: vis == "private" },
            DropDownOption { id: "shared".to_string(), label: html! { "Shared" }, selected: vis == "shared" },
        ]
    });

    let on_cancel = {
        let on_done = props.on_done.clone();
        Callback::from(move |_: String| {
            if let Some(cb) = on_done.clone() {
                cb.emit(());
            }
        })
    };

    let on_save = {
        let form_state = form_state.clone();
        let existing_id = props.existing.as_ref().map(|r| r.id.clone());
        let existing_channel_id = props.existing.as_ref().and_then(|rule| rule.channel_id.clone());
        let services = services.clone();
        let on_done = props.on_done.clone();
        let translate = translate.clone();
        Callback::from(move |_: String| {
            let form = form_state.data().clone();
            if existing_id.is_none() && form.target_id.is_none() {
                services.toastr.error(translate.t("MESSAGES.RECORDING.NO_TARGET"));
                return;
            }
            if existing_id.is_none() && form.input_name.trim().is_empty() {
                services.toastr.error(translate.t("MESSAGES.RECORDING.NO_INPUT"));
                return;
            }
            let visibility = match form.visibility.as_str() {
                "shared" => RuleVisibility::Shared,
                _ => RuleVisibility::Private,
            };
            let rule_body = if form.kind == "new_episode" {
                RuleBody::NewEpisode {
                    series_id: form.series_id.clone().filter(|s| !s.trim().is_empty()),
                    title_pattern: form.title_pattern.clone().filter(|s| !s.trim().is_empty()),
                    exclude_repeat: form.exclude_repeat,
                }
            } else {
                RuleBody::WeeklyTimeslot {
                    weekday: form.weekday,
                    local_start_time: form.start_time.clone(),
                    duration_secs: form.duration_secs,
                    timezone: form.timezone.clone(),
                }
            };
            let svc = services.clone();
            let on_done = on_done.clone();
            let existing_id_for_async = existing_id.clone();
            let existing_channel_id = existing_channel_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let res = if let Some(id) = existing_id_for_async {
                    let (channel_id, clear_channel_id) =
                        channel_id_patch(existing_channel_id.as_deref(), form.channel_id.as_deref());
                    let request = EditRecordingRuleRequest {
                        body: Some(rule_body),
                        channel_id,
                        clear_channel_id,
                        pre_roll_secs: Some(form.pre_roll),
                        post_roll_secs: Some(form.post_roll),
                        visibility: Some(visibility),
                        enabled: Some(form.enabled),
                    };
                    RecordingService::new().edit_rule(&id, request).await
                } else {
                    let Some(request) = build_create_request(&form, visibility) else {
                        return;
                    };
                    RecordingService::new().create_rule(request).await
                };
                match res {
                    Ok(_) => {
                        svc.toastr.success("Rule saved");
                        if let Some(cb) = on_done {
                            cb.emit(());
                        }
                    }
                    Err(e) => svc.toastr.error(format!("Save failed: {}", e)),
                }
            });
        })
    };

    let kind = form_state.form.kind.clone();
    let weekly_fields = if kind == "weekly" {
        html! {
            <>
                { edit_field_number_u8!(form_state, translate.t("LABEL.RECORDING_FORM_WEEKDAY"), weekday, RuleFormAction::Weekday) }
                { edit_field_text!(form_state, translate.t("LABEL.RECORDING_FORM_START_TIME"), start_time, RuleFormAction::StartTime) }
                { edit_field_number_u64!(form_state, translate.t("LABEL.RECORDING_FORM_DURATION"), duration_secs, RuleFormAction::DurationSecs) }
                { edit_field_text!(form_state, translate.t("LABEL.RECORDING_FORM_TIMEZONE"), timezone, RuleFormAction::Timezone) }
            </>
        }
    } else {
        html! {
            <>
                { edit_field_text_option!(form_state, translate.t("LABEL.RECORDING_RULE_SERIES_ID"), series_id, RuleFormAction::SeriesId) }
                { edit_field_text_option!(form_state, translate.t("LABEL.RECORDING_RULE_TITLE_PATTERN"), title_pattern, RuleFormAction::TitlePattern) }
                { edit_field_bool!(form_state, translate.t("LABEL.RECORDING_RULE_EXCLUDE_REPEAT"), exclude_repeat, RuleFormAction::ExcludeRepeat) }
            </>
        }
    };

    let input_select = {
        let form_state = form_state.clone();
        html! {
            <Select name="input_name"
                multi_select={false}
                on_select={Callback::from(move |(_, selections): (String, DropDownSelection)| {
                    if let Some(name) = selection_first_owned(selections) {
                        // Switching the input may invalidate the
                        // current target; if the target isn't in
                        // the filtered list, clear it.
                        form_state.dispatch(RuleFormAction::InputName(name.clone()));
                        form_state.dispatch(RuleFormAction::TargetId(None));
                    }
                })}
                options={input_options.clone()}
            />
        }
    };

    let target_select = {
        let form_state = form_state.clone();
        html! {
            <Select name="target_id"
                multi_select={false}
                on_select={Callback::from(move |(_, selections): (String, DropDownSelection)| {
                    if let Some(name) = selection_first_owned(selections) {
                        form_state.dispatch(RuleFormAction::TargetId(Some(name)));
                    }
                })}
                options={target_options.clone()}
            />
        }
    };

    let kind_select = {
        let form_state = form_state.clone();
        html! {
            <Select name="kind"
                multi_select={false}
                on_select={Callback::from(move |(_, selections): (String, DropDownSelection)| {
                    if let Some(next) = selection_first_owned(selections) {
                        form_state.dispatch(RuleFormAction::Kind(next));
                    }
                })}
                options={kind_options.clone()}
            />
        }
    };

    let visibility_select = {
        let form_state = form_state.clone();
        html! {
            <Select name="visibility"
                multi_select={false}
                on_select={Callback::from(move |(_, selections): (String, DropDownSelection)| {
                    if let Some(vis) = selection_first_owned(selections) {
                        form_state.dispatch(RuleFormAction::Visibility(vis));
                    }
                })}
                options={visibility_options.clone()}
            />
        }
    };

    let source_fields = if show_source_controls(props.existing.is_some()) {
        html! {
            <>
                { config_field_child!(translate.t("LABEL.RECORDING_FORM_INPUT_NAME"), "rule_input_name", {
                    { input_select }
                }) }
                { config_field_child!(translate.t("LABEL.RECORDING_FORM_TARGET_ID"), "rule_target_id", {
                    { target_select }
                }) }
                { edit_field_text!(form_state, translate.t("LABEL.RECORDING_FORM_VIRTUAL_ID"), virtual_id, RuleFormAction::VirtualId) }
            </>
        }
    } else {
        html! {}
    };

    html! {
        <div class="tp__rule-form tp__form-page">
            <div class="tp__rule-form__body tp__form-page__body">
                { source_fields }
                { edit_field_text_option!(form_state, translate.t("LABEL.RECORDING_FORM_CHANNEL_ID"), channel_id, RuleFormAction::ChannelId) }
                { config_field_child!(translate.t("LABEL.RECORDING_RULE_KIND"), "rule_kind", {
                    { kind_select }
                }) }
                { weekly_fields }
                { edit_field_number_u64!(form_state, translate.t("LABEL.RECORDING_FORM_PRE_ROLL"), pre_roll, RuleFormAction::PreRoll) }
                { edit_field_number_u64!(form_state, translate.t("LABEL.RECORDING_FORM_POST_ROLL"), post_roll, RuleFormAction::PostRoll) }
                { config_field_child!("Visibility", "rule_visibility", {
                    { visibility_select }
                }) }
                { edit_field_bool!(form_state, translate.t("LABEL.RECORDING_FORM_ENABLED"), enabled, RuleFormAction::Enabled) }
            </div>
            <div class="tp__rule-form__toolbar tp__form-page__toolbar tp__form-page__toolbar--right">
                <TextButton
                    class="secondary"
                    name="rule_cancel"
                    icon="Cancel"
                    title={ translate.t("LABEL.RECORDING_FORM_CANCEL") }
                    onclick={on_cancel}
                />
                <TextButton
                    class="primary"
                    name="rule_save"
                    icon="Save"
                    title={ translate.t("LABEL.RECORDING_FORM_SAVE") }
                    onclick={on_save}
                />
            </div>
        </div>
    }
}

// The form takes sources as a prop. The home.rs panel reads the
// config context and passes the list to the form.

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_form() -> RuleFormDto {
        RuleFormDto {
            target_id: Some("default".to_string()),
            virtual_id: "1".to_string(),
            input_name: "inp".to_string(),
            ..dto_defaults()
        }
    }

    fn mk_source(inputs: &[&str], targets: &[&str]) -> ConfigSourceDto {
        ConfigSourceDto {
            inputs: inputs.iter().map(|s| (*s).to_string().into()).collect(),
            targets: targets
                .iter()
                .map(|s| ConfigTargetDto { id: 0, name: (*s).to_string(), ..Default::default() })
                .collect(),
        }
    }

    #[test]
    fn targets_for_input_returns_empty_when_input_not_in_any_source() {
        let s = mk_source(&["inp-a"], &["tgt-a"]);
        let sources: Vec<Rc<ConfigSourceDto>> = vec![Rc::new(s)];
        let result = targets_for_input(&sources, "missing");
        assert!(result.is_empty());
    }

    #[test]
    fn targets_for_input_filters_to_matching_source() {
        let s1 = mk_source(&["inp-a"], &["tgt-a"]);
        let s2 = mk_source(&["inp-b"], &["tgt-b"]);
        let sources: Vec<Rc<ConfigSourceDto>> = vec![Rc::new(s1), Rc::new(s2)];
        let result = targets_for_input(&sources, "inp-a");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "tgt-a");
    }

    #[test]
    fn targets_for_input_with_empty_input_returns_all_targets() {
        let s1 = mk_source(&["inp-a"], &["tgt-a"]);
        let s2 = mk_source(&["inp-b"], &["tgt-b"]);
        let sources: Vec<Rc<ConfigSourceDto>> = vec![Rc::new(s1), Rc::new(s2)];
        let result = targets_for_input(&sources, "");
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn all_input_names_deduplicates() {
        let s1 = mk_source(&["inp-a", "inp-b"], &["tgt-a"]);
        let s2 = mk_source(&["inp-a", "inp-c"], &["tgt-b"]);
        let sources: Vec<Rc<ConfigSourceDto>> = vec![Rc::new(s1), Rc::new(s2)];
        let result = all_input_names(&sources);
        assert_eq!(result.len(), 3);
        assert!(result.contains(&"inp-a".to_string()));
        assert!(result.contains(&"inp-b".to_string()));
        assert!(result.contains(&"inp-c".to_string()));
    }

    #[test]
    fn build_create_request_new_episode_sets_three_fields_only() {
        let mut f = empty_form();
        f.kind = "new_episode".to_string();
        f.series_id = Some("series-1".to_string());
        f.title_pattern = Some("Title".to_string());
        f.exclude_repeat = true;
        let req = build_create_request(&f, RuleVisibility::Private).expect("selected target");
        assert_eq!(req.target_id, "default");
        assert_eq!(req.virtual_id, "1");
        assert_eq!(req.input_name, "inp");
        assert!(matches!(
            req.body,
            RuleBody::NewEpisode {
                series_id: Some(ref series_id),
                title_pattern: Some(ref title),
                exclude_repeat: true,
            } if series_id == "series-1" && title == "Title"
        ));
    }

    #[test]
    fn build_create_request_weekly_sets_weekly_fields_only() {
        let mut form = empty_form();
        form.weekday = 3;
        let req = build_create_request(&form, RuleVisibility::Private).expect("selected target");
        assert!(matches!(
            req.body,
            RuleBody::WeeklyTimeslot {
                weekday: 3,
                ref local_start_time,
                duration_secs: 3600,
                ref timezone,
            } if local_start_time == "20:00" && timezone == "UTC"
        ));
    }

    #[test]
    fn build_create_request_omits_blank_channel_id() {
        let mut f = empty_form();
        f.channel_id = Some("   ".to_string());
        let req = build_create_request(&f, RuleVisibility::Private).expect("selected target");
        assert!(req.channel_id.is_none());
    }

    #[test]
    fn build_create_request_keeps_non_empty_channel_id() {
        let mut f = empty_form();
        f.channel_id = Some("chan-1".to_string());
        let req = build_create_request(&f, RuleVisibility::Private).expect("selected target");
        assert_eq!(req.channel_id.as_deref(), Some("chan-1"));
    }

    #[test]
    fn existing_target_name_is_restored() {
        let existing = RecordingRuleSnapshot {
            id: "rule-1".to_string(),
            owner_id: "web:alice".to_string(),
            visibility: RuleVisibility::Private,
            enabled: true,
            source: shared::model::recording_rule::RuleSource::new("legacy-target", "42", "inp"),
            channel_id: None,
            body: RuleBody::WeeklyTimeslot {
                weekday: 1,
                local_start_time: "20:00".to_string(),
                duration_secs: 3600,
                timezone: "UTC".to_string(),
            },
            pre_roll_secs: 0,
            post_roll_secs: 0,
            created_at: 1,
            updated_at: 1,
        };

        assert_eq!(dto_from_existing(&existing).target_id.as_deref(), Some("legacy-target"));
    }

    #[test]
    fn existing_rule_source_controls_are_not_editable() {
        assert!(!show_source_controls(true));
        assert!(show_source_controls(false));
    }

    #[test]
    fn blank_existing_channel_requests_clear_but_absent_channel_does_not() {
        assert_eq!(channel_id_patch(Some("channel-1"), None), (None, true));
        assert_eq!(channel_id_patch(None, None), (None, false));
        assert_eq!(channel_id_patch(Some("channel-1"), Some("channel-1")), (None, false));
        assert_eq!(channel_id_patch(Some("channel-1"), Some("channel-2")), (Some("channel-2".to_string()), false));
    }
}
