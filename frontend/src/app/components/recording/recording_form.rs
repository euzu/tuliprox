//! Shared recording form used by the playlist explorer and the EPG view.
//!
//! The form collects padding and visibility only; the caller receives a
//! fully-populated `CreateRecordingTaskRequest` via `on_submit` and decides
//! what to do with it. The server is the source of truth for validation,
//! source resolution, path reservation, and quota admission; every client
//! calculation is a preview.

use crate::{
    app::components::{number_input::NumberInput, DateTimeInput, RadioButtonGroup},
    i18n::use_translation,
    services::{
        ConflictSeverity, CreateRecordingTaskRequest, PreviewCandidateDto, PreviewConflictsRequest, PreviewSourceDto,
        RecordingConflictPreview, RecordingService, RecordingSourceInput,
    },
};
use gloo_timers::future::TimeoutFuture;
#[cfg(test)]
use shared::model::permission::Permission;
use shared::model::recording::EpgEpisodeMetadata;
use std::rc::Rc;
use yew::prelude::*;

/// How long the form waits after the last edit before asking the server
/// for a conflict preview. Long enough that dragging a duration spinner
/// does not fire a request per keystroke; short enough to feel live.
const CONFLICT_PREVIEW_DEBOUNCE_MS: u32 = 400;

/// Configured padding bounds. Mirrors the `RecordingConfigDto` fields
/// the server already validates. The frontend uses the upper bound for
/// input validation; the server is authoritative.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaddingBounds {
    pub default_pre_roll_secs: u64,
    pub max_pre_roll_secs: u64,
    pub default_post_roll_secs: u64,
    pub max_post_roll_secs: u64,
}

/// The data the form needs to pre-populate. All identifiers are
/// server-owned; the form never accepts a URL from the caller.
#[derive(Clone, PartialEq)]
pub struct RecordingFormPrefill {
    pub source: RecordingSourceInput,
    pub program_title: String,
    /// Original programme interval (Unix seconds). The form always
    /// displays this. The server is the source of truth.
    pub program_start: i64,
    pub program_end: i64,
    pub channel_id: Option<String>,
    pub channel_name: Option<String>,
    pub epg: Option<EpgEpisodeMetadata>,
    pub padding: PaddingBounds,
}

impl RecordingFormPrefill {
    /// Build a prefill from the minimum surface every caller has. The
    /// `channel_id` / `channel_name` / epg are optional; the form stores
    /// them so the server can use them for visibility and matching.
    pub fn new(
        source: RecordingSourceInput,
        program_title: impl Into<String>,
        program_start: i64,
        program_end: i64,
        padding: PaddingBounds,
    ) -> Self {
        Self {
            source,
            program_title: program_title.into(),
            program_start,
            program_end,
            channel_id: None,
            channel_name: None,
            epg: None,
            padding,
        }
    }

    /// Builder-style channel id. The Playlist Explorer wires this in
    /// from the selected `ChannelSelection`; the EPG view wires it from
    /// the programme metadata.
    pub fn with_channel_id(mut self, channel_id: impl Into<String>) -> Self {
        self.channel_id = Some(channel_id.into());
        self
    }

    /// Builder-style channel name.
    pub fn with_channel_name(mut self, channel_name: impl Into<String>) -> Self {
        self.channel_name = Some(channel_name.into());
        self
    }

    /// Builder-style EPG metadata. The EPG view wires this in from
    /// the programme metadata.
    pub fn with_epg(mut self, epg: EpgEpisodeMetadata) -> Self {
        self.epg = Some(epg);
        self
    }
}

/// Pure: compute the padded scheduled interval from the original
/// programme interval and the user's padding choice. Saturates on
/// overflow so the rendered previews never panic.
pub fn compute_scheduled_interval(
    program_start: i64,
    program_end: i64,
    pre_roll_secs: u64,
    post_roll_secs: u64,
) -> (i64, i64) {
    let scheduled_start = program_start.saturating_sub(pre_roll_secs as i64);
    let scheduled_end = program_end.saturating_add(post_roll_secs as i64);
    (scheduled_start, scheduled_end)
}

/// Pure: validate the user's padding against the configured bounds.
/// The maximums the server enforces are authoritative; the frontend
/// uses the same bounds so the submit button never enables a value the
/// server will reject.
pub fn validate_padding(pre_roll_secs: u64, post_roll_secs: u64, bounds: &PaddingBounds) -> Result<(), String> {
    if pre_roll_secs > bounds.max_pre_roll_secs {
        return Err(format!(
            "Pre-roll ({pre_roll_secs}s) exceeds the configured maximum ({}s)",
            bounds.max_pre_roll_secs
        ));
    }
    if post_roll_secs > bounds.max_post_roll_secs {
        return Err(format!(
            "Post-roll ({post_roll_secs}s) exceeds the configured maximum ({}s)",
            bounds.max_post_roll_secs
        ));
    }
    Ok(())
}

/// Pure: render a filename preview for the form. Mirrors the
/// placeholder discipline the server's filename template uses so the
/// preview is consistent with the server-rendered name. The result is
/// purely advisory — the server may resolve a different filename for
/// collision, owner templates, or sanitization rules.
pub fn render_filename_preview(prefill: &RecordingFormPrefill, _pre_roll_secs: u64, _post_roll_secs: u64) -> String {
    // Preview only — the server's filename template is authoritative.
    let channel = prefill.channel_name.as_deref().unwrap_or("channel");
    let title = prefill.program_title.trim();
    let safe_channel = sanitize_preview_token(channel);
    let safe_title = if title.is_empty() { "program".to_string() } else { sanitize_preview_token(title) };
    let start = format_timestamp_for_filename(prefill.program_start);
    format!("{safe_channel}_{safe_title}_{start}.ts")
}

fn sanitize_preview_token(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_underscore = false;
    for c in s.chars() {
        let mapped = match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => c,
            ' ' | '\t' => '_',
            _ => '_',
        };
        if mapped == '_' {
            if !prev_underscore {
                out.push(mapped);
            }
            prev_underscore = true;
        } else {
            out.push(mapped);
            prev_underscore = false;
        }
    }
    out.trim_matches('_').to_string()
}

fn format_timestamp_for_filename(ts: i64) -> String {
    let Some(naive) = chrono::DateTime::from_timestamp(ts, 0) else {
        return "0000-00-00_00-00".to_string();
    };
    naive.with_timezone(&chrono::Utc).format("%Y-%m-%d_%H-%M").to_string()
}

/// Show the Shared visibility option only to administrators with
/// `recording.write`. Non-admins can only record privately.
pub fn can_pick_shared(has_recording_write: bool, is_admin_role: bool) -> bool { has_recording_write && is_admin_role }

pub fn target_name_for_id(
    sources: &shared::model::SourcesConfigDto,
    target_id: u16,
    input_name: Option<&str>,
) -> Option<String> {
    sources.sources.iter().find_map(|source| {
        if input_name.is_some_and(|name| !source.inputs.iter().any(|configured| configured.as_ref() == name)) {
            return None;
        }
        source.targets.iter().find(|target| target.id == target_id).map(|target| target.name.clone())
    })
}

/// Translate the user's form choice into the wire enum. The form only
/// ever emits `private` or `shared`; the server's
/// `recording_shared_not_administrator` code path is the authoritative
/// forge defense.
pub fn visibility_to_wire(picked_shared: bool) -> &'static str {
    if picked_shared {
        "shared"
    } else {
        "private"
    }
}

/// Build a `CreateRecordingTaskRequest` from the form's prefill +
/// user-controlled padding + visibility. When `override_start` and
/// `override_duration_minutes` are both provided, the request's
/// `program_start` / `program_end` reflect the user's editable window;
/// otherwise the prefill window is passed through unchanged. Either
/// `override_*` being `None` falls back to the prefill's matching field
/// (so a half-set override cannot produce a corrupt interval).
pub fn build_request(
    prefill: &RecordingFormPrefill,
    pre_roll_secs: u64,
    post_roll_secs: u64,
    picked_shared: bool,
    override_start: Option<i64>,
    override_duration_minutes: Option<u64>,
) -> CreateRecordingTaskRequest {
    let (program_start, program_end) = match (override_start, override_duration_minutes) {
        (Some(start), Some(minutes)) => {
            let minutes_i64 = i64::try_from(minutes).unwrap_or(i64::MAX);
            let end = start.saturating_add(minutes_i64.saturating_mul(60));
            (start, end)
        }
        (Some(start), None) => (start, prefill.program_end),
        (None, Some(_)) => (prefill.program_start, prefill.program_end),
        (None, None) => (prefill.program_start, prefill.program_end),
    };
    CreateRecordingTaskRequest {
        source: prefill.source.clone(),
        program_title: prefill.program_title.clone(),
        program_start: Some(program_start),
        program_end: Some(program_end),
        pre_roll_secs: Some(pre_roll_secs),
        post_roll_secs: Some(post_roll_secs),
        visibility: visibility_to_wire(picked_shared).to_string(),
        channel_id: prefill.channel_id.clone(),
        channel_name: prefill.channel_name.clone(),
        epg: prefill.epg.clone(),
    }
}

/// Format a Unix timestamp for display in the form. The form shows the
/// original (`program`) and the padded (`scheduled`) intervals as a
/// short local wall-clock time (`HH:MM`) so the user reads the
/// display the same way they typed the start time into the
/// `DateTimeInput` field. Cross-midnight end times show as `HH:MM`
/// without a date marker — a verbose date stamp crowds the dialog.
pub fn format_interval_for_display(ts: i64) -> String {
    let Some(dt) = chrono::DateTime::from_timestamp(ts, 0) else {
        return "<invalid>".to_string();
    };
    dt.with_timezone(&chrono::Local).format("%H:%M").to_string()
}

/// Convenience accessor for callers that track a `PermissionSet`.
#[cfg(test)]
pub fn has_recording_write(permissions: &shared::model::permission::PermissionSet) -> bool {
    permissions.contains(Permission::RecordingWrite)
}

/// Properties for the recording form component.
#[derive(Properties, Clone, PartialEq)]
pub struct RecordingFormProps {
    pub prefill: RecordingFormPrefill,
    /// `recording.write` permission.
    pub has_recording_write: bool,
    /// Whether the principal carries the built-in administrator role.
    pub is_admin_role: bool,
    /// Submit callback. Fires with a fully populated
    /// `CreateRecordingTaskRequest` once the form is valid.
    pub on_submit: Callback<CreateRecordingTaskRequest>,
    /// Cancel callback. The caller decides what to do (close dialog).
    pub on_cancel: Callback<()>,
}

#[component]
pub fn RecordingForm(props: &RecordingFormProps) -> Html {
    let translate = use_translation();
    let prefill = &props.prefill;
    let pre_state = use_state(|| prefill.padding.default_pre_roll_secs);
    let post_state = use_state(|| prefill.padding.default_post_roll_secs);
    let shared_offered = can_pick_shared(props.has_recording_write, props.is_admin_role);
    let shared_state = use_state(|| false);
    let start_state = use_state(|| prefill.program_start);
    let duration_minutes_state = use_state(|| {
        // Floor: a recording shorter than a minute is never useful.
        let secs = (prefill.program_end - prefill.program_start).max(60);
        (secs / 60) as u64
    });

    let on_pre_input = {
        let pre_state = pre_state.clone();
        Callback::from(move |value: Option<i64>| {
            if let Some(v) = value {
                if v >= 0 {
                    pre_state.set(v as u64);
                }
            }
        })
    };

    let on_post_input = {
        let post_state = post_state.clone();
        Callback::from(move |value: Option<i64>| {
            if let Some(v) = value {
                if v >= 0 {
                    post_state.set(v as u64);
                }
            }
        })
    };

    let on_start_change = {
        let start_state = start_state.clone();
        Callback::from(move |value: Option<i64>| {
            if let Some(ts) = value {
                start_state.set(ts);
            }
            // None = invalid input. The DateTimeInput component already
            // re-displays the last valid value in that case; we just
            // don't touch state.
        })
    };

    let on_duration_change = {
        let duration_minutes_state = duration_minutes_state.clone();
        Callback::from(move |value: Option<i64>| {
            if let Some(v) = value {
                if v >= 1 {
                    duration_minutes_state.set(v as u64);
                }
            }
        })
    };

    let visibility_options: Rc<Vec<String>> = if shared_offered {
        Rc::new(vec!["private".to_string(), "shared".to_string()])
    } else {
        Rc::new(vec!["private".to_string()])
    };
    let visibility_labels: Rc<Vec<String>> = Rc::new(vec![translate.t("LABEL.PRIVATE"), translate.t("LABEL.SHARED")]);
    let visibility_selected: Rc<Vec<String>> =
        if *shared_state { Rc::new(vec!["shared".to_string()]) } else { Rc::new(vec!["private".to_string()]) };
    let on_visibility_select = {
        let shared_state = shared_state.clone();
        Callback::from(move |selections: Rc<Vec<String>>| {
            let picked = selections.iter().next().map_or("private", String::as_str);
            shared_state.set(picked == "shared");
        })
    };

    // Mirror the latest valid request into the parent slot on every state change so
    // the dialog's own Cancel/Record buttons stay the only way to close.
    {
        let pre_state = pre_state.clone();
        let post_state = post_state.clone();
        let shared_state = shared_state.clone();
        let start_state = start_state.clone();
        let duration_minutes_state = duration_minutes_state.clone();
        let on_submit = props.on_submit.clone();
        let prefill = prefill.clone();
        use_effect_with(
            (*pre_state, *post_state, *shared_state, *start_state, *duration_minutes_state),
            move |(pre, post, shared, start, duration_minutes)| {
                let emit_empty = || {
                    // Sentinel request: `program_start == 0 && program_end == 0`
                    // signals "no valid submission yet" so the parent
                    // can disable the action button without falling
                    // back to the previous good request.
                    on_submit.emit(CreateRecordingTaskRequest {
                        source: prefill.source.clone(),
                        program_title: prefill.program_title.clone(),
                        program_start: Some(0),
                        program_end: Some(0),
                        pre_roll_secs: Some(0),
                        post_roll_secs: Some(0),
                        visibility: if *shared { "shared".to_string() } else { "private".to_string() },
                        channel_id: None,
                        channel_name: None,
                        epg: None,
                    });
                };
                if *duration_minutes < 1 {
                    emit_empty();
                    return;
                }
                match validate_padding(*pre, *post, &prefill.padding) {
                    Ok(()) => {
                        let request =
                            build_request(&prefill, *pre, *post, *shared, Some(*start), Some(*duration_minutes));
                        on_submit.emit(request);
                    }
                    Err(_) => emit_empty(),
                }
            },
        );
    }

    let effective_start = *start_state;
    let effective_end = effective_start.saturating_add((*duration_minutes_state as i64).saturating_mul(60));
    let (scheduled_start, scheduled_end) =
        compute_scheduled_interval(effective_start, effective_end, *pre_state, *post_state);
    let preview_filename = render_filename_preview(prefill, *pre_state, *post_state);
    let padding_error = validate_padding(*pre_state, *post_state, &prefill.padding).err();

    // Live conflict preview. The endpoint existed but nothing called it,
    // so users scheduled blind and only found out a recording had lost
    // its provider slot after it failed. The request is debounced and
    // keyed on the padded interval, so editing the form is cheap.
    let conflict = use_state(|| None::<RecordingConflictPreview>);
    let conflict_pending = use_state(|| false);
    {
        let conflict = conflict.clone();
        let conflict_pending = conflict_pending.clone();
        let source = prefill.source.clone();
        let pre = *pre_state;
        let post = *post_state;
        use_effect_with((scheduled_start, scheduled_end, pre, post), move |_| {
            // Set on teardown, checked after the debounce and after the
            // response: an edit that supersedes this one must not let a
            // stale answer land.
            let cancelled = Rc::new(std::cell::Cell::new(false));
            if scheduled_end <= scheduled_start {
                // Interval not valid yet — nothing to preview.
                conflict.set(None);
                conflict_pending.set(false);
            } else {
                conflict_pending.set(true);
                let superseded = cancelled.clone();
                let request = PreviewConflictsRequest {
                    source: PreviewSourceDto {
                        target_name: source.target_id.clone(),
                        virtual_id: source.virtual_id.clone(),
                        input_name: source.input_name.clone(),
                    },
                    candidate: PreviewCandidateDto {
                        padded_start: scheduled_start,
                        padded_end: scheduled_end,
                        pre_roll_secs: pre,
                        post_roll_secs: post,
                        priority: 0,
                    },
                };
                wasm_bindgen_futures::spawn_local(async move {
                    TimeoutFuture::new(CONFLICT_PREVIEW_DEBOUNCE_MS).await;
                    if superseded.get() {
                        return;
                    }
                    let result = RecordingService::new().preview_conflicts(&request).await;
                    if superseded.get() {
                        return;
                    }
                    conflict_pending.set(false);
                    match result {
                        Ok(preview) => conflict.set(Some(preview)),
                        // Advisory only: a failed preview must never block
                        // the form or shout at the user. Drop the badge.
                        Err(error) => {
                            log::debug!("conflict preview unavailable: {error}");
                            conflict.set(None);
                        }
                    }
                });
            }
            move || cancelled.set(true)
        });
    }
    let conflict_badge = if *conflict_pending {
        html! {
            <span class="tp__conflict-badge tp__conflict-badge--pending">
                { translate.t("LABEL.CONFLICT_CHECKING") }
            </span>
        }
    } else if let Some(preview) = conflict.as_ref() {
        let label = translate.t(severity_i18n_key(&preview.severity));
        // The tooltip names how many other recordings overlap, never
        // which ones: the preview is anonymized server-side and must
        // stay that way.
        let overlaps = preview.overlap_segments.len();
        let detail = if overlaps > 0 {
            translate.t("LABEL.CONFLICT_OVERLAP_COUNT").replace("{count}", &overlaps.to_string())
        } else {
            label.clone()
        };
        html! {
            <span
                class={classes!("tp__conflict-badge", severity_modifier(&preview.severity))}
                title={detail}
                aria-label={label.clone()}
            >
                { label }
            </span>
        }
    } else {
        html! { <></> }
    };

    let programme_label = translate.t("LABEL.PROGRAMME");
    let original_label = translate.t("LABEL.ORIGINAL_INTERVAL");
    let scheduled_label = translate.t("LABEL.SCHEDULED_INTERVAL");
    let start_label = translate.t("LABEL.START_TIME");
    let duration_label = translate.t("LABEL.DURATION_MINUTES");
    let pre_roll_label = format!("{} (max {}s)", translate.t("LABEL.PRE_ROLL"), prefill.padding.max_pre_roll_secs);
    let post_roll_label = format!("{} (max {}s)", translate.t("LABEL.POST_ROLL"), prefill.padding.max_post_roll_secs);
    let visibility_label = translate.t("LABEL.VISIBILITY");
    let filename_preview_label = translate.t("LABEL.FILENAME_PREVIEW");
    let original_interval_value = format!(
        "{} — {}",
        format_interval_for_display(prefill.program_start),
        format_interval_for_display(prefill.program_end)
    );
    let scheduled_interval_value =
        format!("{} — {}", format_interval_for_display(scheduled_start), format_interval_for_display(scheduled_end));

    html! {
        <div class="tp__recording-form">
            <div class="tp__recording-form__program">
                <div class="tp__recording-form__row">
                    <span class="tp__recording-form__heading">{ programme_label }</span>
                    <span class="tp__value">{ prefill.program_title.clone() }</span>
                </div>
                <div class="tp__recording-form__row">
                    <span class="tp__label">{ original_label }</span>
                    <span class="tp__value">{ original_interval_value }</span>
                </div>
                <div class="tp__recording-form__row">
                    <span class="tp__label">{ start_label }</span>
                    <DateTimeInput
                        name="start_at"
                        value={ Some(*start_state) }
                        on_change={ on_start_change }
                    />
                </div>
                <NumberInput
                    name="duration_minutes"
                    label={ duration_label.clone() }
                    value={ Some(*duration_minutes_state as i64) }
                    on_change={ on_duration_change }
                />
                <div class="tp__recording-form__row">
                    <span class="tp__label">{ scheduled_label }</span>
                    <span class="tp__value">{ scheduled_interval_value }</span>
                    { conflict_badge }
                </div>
            </div>
            <NumberInput
                name="pre_roll_secs"
                label={pre_roll_label}
                placeholder={prefill.padding.default_pre_roll_secs.to_string()}
                value={Some(*pre_state as i64)}
                on_change={on_pre_input}
            />
            <NumberInput
                name="post_roll_secs"
                label={post_roll_label}
                placeholder={prefill.padding.default_post_roll_secs.to_string()}
                value={Some(*post_state as i64)}
                on_change={on_post_input}
            />
            if let Some(err) = padding_error {
                <div class="tp__recording-form__error">{ err }</div>
            }
            <div class="tp__recording-form__visibility">
                <span class="tp__label">{ visibility_label }</span>
                <RadioButtonGroup
                    options={visibility_options}
                    selected={visibility_selected}
                    labels={Some(visibility_labels)}
                    on_select={on_visibility_select}
                />
            </div>
            <div class="tp__recording-form__filename">
                <span class="tp__label">{ filename_preview_label }</span>
                <span class="tp__value">{ preview_filename }</span>
            </div>
        </div>
    }
}

/// Build a `RecordingFormPrefill` from a single source-id tuple. The
/// helper is the minimal builder callers need; the richer
/// `RecordingFormPrefill` builder methods cover the optional fields.
#[cfg(test)]
pub fn prefill_from_source(
    source: RecordingSourceInput,
    program_title: impl Into<String>,
    program_start: i64,
    program_end: i64,
    padding: PaddingBounds,
) -> RecordingFormPrefill {
    RecordingFormPrefill::new(source, program_title, program_start, program_end, padding)
}

pub struct EpgProgrammePrefillInput {
    pub source: RecordingSourceInput,
    pub channel_id: Option<String>,
    pub channel_name: Option<String>,
    pub programme_title: String,
    pub programme_start: i64,
    pub programme_end: i64,
    pub padding: PaddingBounds,
    pub episode: Option<EpgEpisodeMetadata>,
}

/// Build a `RecordingFormPrefill` from an EPG programme and a
/// channel id / name. The EPG view's Record action uses this
/// helper; the backend revalidates the window when the request arrives.
///
/// `channel_id` and `channel_name` are optional. `episode` is also
/// optional — when the EPG payload carries episode data, the form
/// forwards it as `EpgEpisodeMetadata` so the server can use it for
/// matching.
pub fn epg_programme_to_prefill(input: EpgProgrammePrefillInput) -> RecordingFormPrefill {
    let mut prefill = RecordingFormPrefill::new(
        input.source,
        input.programme_title,
        input.programme_start,
        input.programme_end,
        input.padding,
    );
    if let Some(id) = input.channel_id {
        prefill = prefill.with_channel_id(id);
    }
    if let Some(name) = input.channel_name {
        prefill = prefill.with_channel_name(name);
    }
    if let Some(ep) = input.episode {
        prefill = prefill.with_epg(ep);
    }
    prefill
}

/// i18n key for a conflict severity.
///
/// Conflict previews are advisory, so they are a separate surface from
/// `RecordingError`: a conflict never blocks a submission, it only warns
/// that the recording may wait for a slot or miss its window.
pub fn severity_i18n_key(severity: &ConflictSeverity) -> &'static str {
    match severity {
        ConflictSeverity::NoKnownConflict => "LABEL.CONFLICT_NO_KNOWN_CONFLICT",
        ConflictSeverity::PossibleCapacityWait => "LABEL.CONFLICT_POSSIBLE_CAPACITY_WAIT",
        ConflictSeverity::LikelyMissedWindow => "LABEL.CONFLICT_LIKELY_MISSED_WINDOW",
    }
}

/// CSS modifier for a conflict severity. Reuses the task-status pill
/// families so a "likely to miss" badge looks like the failure states
/// elsewhere in the UI rather than inventing a third palette.
pub fn severity_modifier(severity: &ConflictSeverity) -> &'static str {
    match severity {
        ConflictSeverity::NoKnownConflict => "tp__conflict-badge--ok",
        ConflictSeverity::PossibleCapacityWait => "tp__conflict-badge--warn",
        ConflictSeverity::LikelyMissedWindow => "tp__conflict-badge--danger",
    }
}

/// Tests for the pure helpers. The component itself is exercised by
/// `trunk build` and the integration smoke.
#[cfg(test)]
mod tests {
    use super::*;

    fn bounds() -> PaddingBounds {
        PaddingBounds {
            default_pre_roll_secs: 0,
            max_pre_roll_secs: 900,
            default_post_roll_secs: 0,
            max_post_roll_secs: 1800,
        }
    }

    fn source() -> RecordingSourceInput {
        RecordingSourceInput {
            target_id: "default".to_string(),
            virtual_id: "virt-1".to_string(),
            cluster: shared::model::XtreamCluster::Live,
            input_name: "input-1".to_string(),
        }
    }

    #[test]
    fn target_name_lookup_uses_current_id_and_optional_input_source() {
        let target =
            |id, name: &str| shared::model::ConfigTargetDto { id, name: name.to_string(), ..Default::default() };
        let sources = shared::model::SourcesConfigDto {
            sources: vec![
                shared::model::ConfigSourceDto {
                    inputs: vec!["input-a".to_string().into()],
                    targets: vec![target(11, "target-a")],
                },
                shared::model::ConfigSourceDto {
                    inputs: vec!["input-b".to_string().into()],
                    targets: vec![target(12, "stable-target")],
                },
            ],
            ..Default::default()
        };

        assert_eq!(target_name_for_id(&sources, 12, Some("input-b")).as_deref(), Some("stable-target"));
        assert!(target_name_for_id(&sources, 12, Some("input-a")).is_none());
    }

    #[test]
    fn prefill_constructor_uses_provided_values() {
        let prefill = prefill_from_source(source(), "Title", 1_700_000_000, 1_700_003_600, bounds());
        assert_eq!(prefill.program_title, "Title");
        assert_eq!(prefill.program_start, 1_700_000_000);
        assert_eq!(prefill.program_end, 1_700_003_600);
        assert_eq!(prefill.padding.max_pre_roll_secs, 900);
        assert_eq!(prefill.padding.max_post_roll_secs, 1800);
        assert!(prefill.channel_id.is_none());
        assert!(prefill.channel_name.is_none());
        assert!(prefill.epg.is_none());
    }

    #[test]
    fn prefill_builders_store_optional_fields() {
        let prefill = RecordingFormPrefill::new(source(), "Title", 1, 2, bounds())
            .with_channel_id("ch-1")
            .with_channel_name("Channel One");
        assert_eq!(prefill.channel_id.as_deref(), Some("ch-1"));
        assert_eq!(prefill.channel_name.as_deref(), Some("Channel One"));
    }

    #[test]
    fn compute_scheduled_interval_subtracts_pre_roll_and_adds_post_roll() {
        let (start, end) = compute_scheduled_interval(1_700_000_000, 1_700_003_600, 60, 120);
        assert_eq!(start, 1_700_000_000 - 60);
        assert_eq!(end, 1_700_003_600 + 120);
    }

    #[test]
    fn compute_scheduled_interval_saturates_on_overflow() {
        let (start, end) = compute_scheduled_interval(i64::MIN, i64::MAX, 60, 60);
        // Saturating subtraction on i64::MIN should not panic and should clamp.
        assert_eq!(start, i64::MIN);
        assert_eq!(end, i64::MAX);
    }

    #[test]
    fn validate_padding_accepts_values_within_bounds() {
        assert!(validate_padding(0, 0, &bounds()).is_ok());
        assert!(validate_padding(900, 1800, &bounds()).is_ok());
    }

    #[test]
    fn validate_padding_rejects_pre_roll_above_max() {
        let err = validate_padding(901, 0, &bounds()).unwrap_err();
        assert!(err.contains("Pre-roll"));
    }

    #[test]
    fn validate_padding_rejects_post_roll_above_max() {
        let err = validate_padding(0, 1801, &bounds()).unwrap_err();
        assert!(err.contains("Post-roll"));
    }

    #[test]
    fn can_pick_shared_requires_recording_write_and_admin() {
        assert!(!can_pick_shared(false, true));
        assert!(!can_pick_shared(true, false));
        assert!(can_pick_shared(true, true));
    }

    #[test]
    fn visibility_to_wire_stable_strings() {
        assert_eq!(visibility_to_wire(false), "private");
        assert_eq!(visibility_to_wire(true), "shared");
    }

    #[test]
    fn render_filename_preview_is_derived_from_prefill() {
        let prefill = RecordingFormPrefill::new(source(), "My Title", 1_700_000_000, 1_700_003_600, bounds())
            .with_channel_name("Channel-1");
        let preview = render_filename_preview(&prefill, 0, 0);
        assert!(preview.starts_with("Channel-1_My_Title_"));
        assert!(preview.ends_with(".ts"));
    }

    #[test]
    fn render_filename_preview_falls_back_when_title_blank() {
        let prefill = RecordingFormPrefill::new(source(), "   ", 1_700_000_000, 1_700_003_600, bounds())
            .with_channel_name("Channel-1");
        let preview = render_filename_preview(&prefill, 0, 0);
        assert!(preview.contains("program"));
    }

    #[test]
    fn render_filename_preview_replaces_unsafe_chars() {
        let prefill =
            RecordingFormPrefill::new(source(), "Title/With:Bad?Chars", 1_700_000_000, 1_700_003_600, bounds())
                .with_channel_name("ch");
        let preview = render_filename_preview(&prefill, 0, 0);
        assert!(preview.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-'));
    }

    #[test]
    fn build_request_carries_source_padding_and_visibility() {
        let prefill = RecordingFormPrefill::new(source(), "Title", 100, 200, bounds());
        let request = build_request(&prefill, 60, 30, false, None, None);
        assert_eq!(request.source.target_id, "default");
        assert_eq!(request.source.virtual_id, "virt-1");
        assert_eq!(request.source.input_name, "input-1");
        assert_eq!(request.pre_roll_secs, Some(60));
        assert_eq!(request.post_roll_secs, Some(30));
        assert_eq!(request.visibility, "private");
    }

    #[test]
    fn build_request_with_shared_visibility() {
        let prefill = RecordingFormPrefill::new(source(), "Title", 100, 200, bounds());
        let request = build_request(&prefill, 0, 0, true, None, None);
        assert_eq!(request.visibility, "shared");
    }

    #[test]
    fn build_request_does_not_silently_truncate_forged_padding() {
        // Even if the caller somehow passes a value above the bounds,
        // the request is built but the server is authoritative. The
        // frontend's padding input enforces the bound; this test
        // documents that the helper does not silently truncate.
        let prefill = RecordingFormPrefill::new(source(), "Title", 100, 200, bounds());
        let request = build_request(&prefill, 9999, 9999, false, None, None);
        assert_eq!(request.pre_roll_secs, Some(9999));
        assert_eq!(request.post_roll_secs, Some(9999));
    }

    #[test]
    fn build_request_emits_override_start_and_duration() {
        let prefill = RecordingFormPrefill::new(source(), "Title", 100, 200, bounds());
        let request = build_request(&prefill, 0, 0, false, Some(500), Some(15));
        assert_eq!(request.program_start, Some(500));
        assert_eq!(request.program_end, Some(500 + 15 * 60));
    }

    #[test]
    fn build_request_falls_back_to_prefill_when_overrides_are_none() {
        let prefill = RecordingFormPrefill::new(source(), "Title", 100, 200, bounds());
        let request = build_request(&prefill, 0, 0, false, None, None);
        assert_eq!(request.program_start, Some(100));
        assert_eq!(request.program_end, Some(200));
    }

    #[test]
    fn build_request_partial_override_falls_back_to_prefill() {
        let prefill = RecordingFormPrefill::new(source(), "Title", 100, 200, bounds());
        // Only start provided — duration falls back; end reverts to prefill end.
        let request = build_request(&prefill, 0, 0, false, Some(500), None);
        assert_eq!(request.program_start, Some(500));
        assert_eq!(request.program_end, Some(200));
    }

    #[test]
    fn build_request_saturates_when_start_plus_duration_would_overflow() {
        let prefill = RecordingFormPrefill::new(source(), "Title", 100, 200, bounds());
        let request = build_request(&prefill, 0, 0, false, Some(i64::MAX), Some(u64::MAX));
        // Must not panic.
        assert!(request.program_end >= request.program_start);
    }

    #[test]
    fn has_recording_write_respects_permission_set() {
        let perms: shared::model::permission::PermissionSet = Permission::RecordingWrite.into();
        assert!(has_recording_write(&perms));
        let none = shared::model::permission::PermissionSet::new();
        assert!(!has_recording_write(&none));
    }

    #[test]
    fn epg_programme_to_prefill_passes_through_source_and_padding() {
        let prefill = epg_programme_to_prefill(EpgProgrammePrefillInput {
            source: source(),
            channel_id: Some("ch-1".into()),
            channel_name: Some("Channel 1".into()),
            programme_title: "Programme".to_string(),
            programme_start: 1_700_000_000,
            programme_end: 1_700_003_600,
            padding: bounds(),
            episode: None,
        });
        assert_eq!(prefill.source.target_id, "default");
        assert_eq!(prefill.channel_id.as_deref(), Some("ch-1"));
        assert_eq!(prefill.channel_name.as_deref(), Some("Channel 1"));
        assert_eq!(prefill.program_title, "Programme");
        assert_eq!(prefill.program_start, 1_700_000_000);
        assert_eq!(prefill.program_end, 1_700_003_600);
        assert!(prefill.epg.is_none());
    }

    #[test]
    fn epg_programme_to_prefill_includes_episode_when_provided() {
        let episode = EpgEpisodeMetadata::default();
        let prefill = epg_programme_to_prefill(EpgProgrammePrefillInput {
            source: source(),
            channel_id: None,
            channel_name: None,
            programme_title: "Programme".to_string(),
            programme_start: 1_700_000_000,
            programme_end: 1_700_003_600,
            padding: bounds(),
            episode: Some(episode),
        });
        assert!(prefill.epg.is_some());
        assert!(prefill.channel_id.is_none());
        assert!(prefill.channel_name.is_none());
    }

    #[test]
    fn every_severity_has_a_distinct_key_and_modifier() {
        // The old mapper took a wire *string* and had a catch-all arm, so
        // a renamed severity silently rendered "unknown". Matching on the
        // typed enum makes a new variant a compile error instead.
        let all = [
            ConflictSeverity::NoKnownConflict,
            ConflictSeverity::PossibleCapacityWait,
            ConflictSeverity::LikelyMissedWindow,
        ];
        let mut keys: Vec<&str> = all.iter().map(severity_i18n_key).collect();
        let mut modifiers: Vec<&str> = all.iter().map(severity_modifier).collect();
        let total = all.len();
        keys.sort_unstable();
        keys.dedup();
        modifiers.sort_unstable();
        modifiers.dedup();
        assert_eq!(keys.len(), total, "two severities share an i18n key");
        assert_eq!(modifiers.len(), total, "two severities share a colour");
    }

    #[test]
    fn severity_deserializes_from_the_wire_names() {
        for (wire, expected) in [
            ("no_known_conflict", ConflictSeverity::NoKnownConflict),
            ("possible_capacity_wait", ConflictSeverity::PossibleCapacityWait),
            ("likely_missed_window", ConflictSeverity::LikelyMissedWindow),
        ] {
            let parsed: ConflictSeverity = serde_json::from_str(&format!("\"{wire}\"")).expect("severity deserializes");
            assert_eq!(parsed, expected);
        }
    }
}
