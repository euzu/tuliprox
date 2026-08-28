use crate::{
    app::components::{
        config::use_emit_mapped, input::Input, number_input::UnsignedInputError, Card, DropDownOption,
        DropDownSelection, FieldLabel, KeyValueEditor, Select, ToggleSwitch,
    },
    generate_form_reducer,
    i18n::use_translation,
    use_default_form_reducer,
};
use shared::model::{
    RecordingConfigDto, RecordingContainerFormat, RecordingDiskConfigDto, RecordingNotificationConfigDto,
    RecordingQuotaConfigDto, RecordingRetentionConfigDto,
};
use std::{collections::HashMap, rc::Rc};
use yew::prelude::*;

generate_form_reducer!(
    state: RecordingConfigFormState { form: RecordingConfigDto },
    action_name: RecordingConfigFormAction,
    fields {
        Enabled => enabled: bool,
        Priority => priority: i8,
        ContainerFormat => container_format: RecordingContainerFormat,
        Directory => directory: Option<String>,
        Timezone => timezone: Option<String>,
        FilenameTemplate => filename_template: Option<String>,
        DefaultPreRollSecs => default_pre_roll_secs: Option<u64>,
        MaxPreRollSecs => max_pre_roll_secs: u64,
        DefaultPostRollSecs => default_post_roll_secs: Option<u64>,
        MaxPostRollSecs => max_post_roll_secs: u64,
        FallbackBytesPerMinute => fallback_bytes_per_minute: u64,
    }
);

generate_form_reducer!(
    state: RecordingRetentionFormState { form: RecordingRetentionConfigDto },
    action_name: RecordingRetentionFormAction,
    fields {
        KeepLastPerChannel => keep_last_per_channel: Option<u32>,
        DeleteAfterDays => delete_after_days: Option<u32>,
        SweepIntervalSecs => sweep_interval_secs: u64,
    }
);

generate_form_reducer!(
    state: RecordingDiskFormState { form: RecordingDiskConfigDto },
    action_name: RecordingDiskFormAction,
    fields {
        HighWaterPercent => high_water_percent: Option<u8>,
        LowWaterPercent => low_water_percent: Option<u8>,
        CleanupIntervalSecs => cleanup_interval_secs: Option<u64>,
        SafetyBytes => safety_bytes: Option<u64>,
    }
);

generate_form_reducer!(
    state: RecordingQuotaFormState { form: RecordingQuotaConfigDto },
    action_name: RecordingQuotaFormAction,
    fields {
        DefaultPrivateBytes => default_private_bytes: Option<u64>,
        PerUserBytes => per_user_bytes: HashMap<String, u64>,
        SharedBytes => shared_bytes: Option<u64>,
    }
);

generate_form_reducer!(
    state: RecordingNotificationFormState { form: RecordingNotificationConfigDto },
    action_name: RecordingNotificationFormAction,
    fields {
        OutboxBuffer => outbox_buffer: usize,
        MaxAttempts => max_attempts: u32,
        BackoffInitialSecs => backoff_initial_secs: u64,
        BackoffMaxSecs => backoff_max_secs: u64,
    }
);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecordingSectionChanges {
    pub retention: bool,
    pub disk: bool,
    pub quota: bool,
    pub notifications: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotaEntryParseError {
    pub user: String,
}

pub fn assemble_recording_config(
    mut direct: RecordingConfigDto,
    retention: RecordingRetentionConfigDto,
    disk: RecordingDiskConfigDto,
    quota: RecordingQuotaConfigDto,
    notifications: RecordingNotificationConfigDto,
    changes: RecordingSectionChanges,
) -> RecordingConfigDto {
    if changes.retention {
        direct.retention = (!retention.is_empty()).then_some(retention);
    }
    if changes.disk {
        direct.disk = (!disk.is_empty()).then_some(disk);
    }
    if changes.quota {
        direct.quota = (!quota.is_empty()).then_some(quota);
    }
    if changes.notifications {
        direct.notifications = (!notifications.is_empty()).then_some(notifications);
    }
    direct
}

pub fn quota_entries_to_strings(entries: &HashMap<String, u64>) -> HashMap<String, String> {
    entries.iter().map(|(user, bytes)| (user.clone(), bytes.to_string())).collect()
}

pub fn quota_entries_from_strings(
    entries: &HashMap<String, String>,
) -> Result<HashMap<String, u64>, QuotaEntryParseError> {
    entries
        .iter()
        .map(|(user, bytes)| {
            bytes
                .trim()
                .parse::<u64>()
                .map(|bytes| (user.clone(), bytes))
                .map_err(|_| QuotaEntryParseError { user: user.clone() })
        })
        .collect()
}

pub const fn recording_container_id(container: RecordingContainerFormat) -> &'static str {
    match container {
        RecordingContainerFormat::Mpegts => "mpegts",
        RecordingContainerFormat::Matroska => "matroska",
        RecordingContainerFormat::Mp4 => "mp4",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordingCardDescriptor {
    pub id: &'static str,
    pub label: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordingFieldDescriptor {
    pub id: &'static str,
    pub label: &'static str,
    pub card: usize,
}

pub const RECORDING_CARDS: [RecordingCardDescriptor; 5] = [
    RecordingCardDescriptor { id: "recording-general", label: "LABEL.RECORDING_GENERAL" },
    RecordingCardDescriptor { id: "recording-padding", label: "LABEL.RECORDING_PADDING" },
    RecordingCardDescriptor { id: "recording-retention-disk", label: "LABEL.RECORDING_RETENTION_DISK" },
    RecordingCardDescriptor { id: "recording-quotas", label: "LABEL.RECORDING_QUOTAS" },
    RecordingCardDescriptor { id: "recording-notifications", label: "LABEL.RECORDING_NOTIFICATIONS" },
];

pub const RECORDING_FIELDS: [RecordingFieldDescriptor; 25] = [
    RecordingFieldDescriptor { id: "enabled", label: "LABEL.RECORDING_ENABLED", card: 0 },
    RecordingFieldDescriptor { id: "priority", label: "LABEL.PRIORITY", card: 0 },
    RecordingFieldDescriptor { id: "container_format", label: "LABEL.CONTAINER", card: 0 },
    RecordingFieldDescriptor { id: "directory", label: "LABEL.DIRECTORY", card: 0 },
    RecordingFieldDescriptor { id: "timezone", label: "LABEL.TIMEZONE", card: 0 },
    RecordingFieldDescriptor { id: "filename_template", label: "LABEL.RECORDING_FILENAME_TEMPLATE", card: 0 },
    RecordingFieldDescriptor { id: "default_pre_roll_secs", label: "LABEL.RECORDING_DEFAULT_PRE_ROLL_SECS", card: 1 },
    RecordingFieldDescriptor { id: "max_pre_roll_secs", label: "LABEL.RECORDING_MAX_PRE_ROLL_SECS", card: 1 },
    RecordingFieldDescriptor { id: "default_post_roll_secs", label: "LABEL.RECORDING_DEFAULT_POST_ROLL_SECS", card: 1 },
    RecordingFieldDescriptor { id: "max_post_roll_secs", label: "LABEL.RECORDING_MAX_POST_ROLL_SECS", card: 1 },
    RecordingFieldDescriptor { id: "keep_last_per_channel", label: "LABEL.RECORDING_KEEP_LAST_PER_CHANNEL", card: 2 },
    RecordingFieldDescriptor { id: "delete_after_days", label: "LABEL.RECORDING_DELETE_AFTER_DAYS", card: 2 },
    RecordingFieldDescriptor { id: "sweep_interval_secs", label: "LABEL.RECORDING_SWEEP_INTERVAL_SECS", card: 2 },
    RecordingFieldDescriptor { id: "high_water_percent", label: "LABEL.RECORDING_HIGH_WATER_PERCENT", card: 2 },
    RecordingFieldDescriptor { id: "low_water_percent", label: "LABEL.RECORDING_LOW_WATER_PERCENT", card: 2 },
    RecordingFieldDescriptor { id: "cleanup_interval_secs", label: "LABEL.RECORDING_CLEANUP_INTERVAL_SECS", card: 2 },
    RecordingFieldDescriptor { id: "safety_bytes", label: "LABEL.RECORDING_SAFETY_BYTES", card: 2 },
    RecordingFieldDescriptor {
        id: "fallback_bytes_per_minute",
        label: "LABEL.RECORDING_FALLBACK_BYTES_PER_MINUTE",
        card: 2,
    },
    RecordingFieldDescriptor { id: "default_private_bytes", label: "LABEL.RECORDING_DEFAULT_PRIVATE_BYTES", card: 3 },
    RecordingFieldDescriptor { id: "per_user_bytes", label: "LABEL.RECORDING_PER_USER_BYTES", card: 3 },
    RecordingFieldDescriptor { id: "shared_bytes", label: "LABEL.RECORDING_SHARED_BYTES", card: 3 },
    RecordingFieldDescriptor { id: "outbox_buffer", label: "LABEL.RECORDING_OUTBOX_BUFFER", card: 4 },
    RecordingFieldDescriptor { id: "max_attempts", label: "LABEL.RECORDING_MAX_ATTEMPTS", card: 4 },
    RecordingFieldDescriptor { id: "backoff_initial_secs", label: "LABEL.RECORDING_BACKOFF_INITIAL_SECS", card: 4 },
    RecordingFieldDescriptor { id: "backoff_max_secs", label: "LABEL.RECORDING_BACKOFF_MAX_SECS", card: 4 },
];

pub fn recording_container_from_id(id: &str) -> Option<RecordingContainerFormat> {
    match id {
        "mpegts" => Some(RecordingContainerFormat::Mpegts),
        "matroska" => Some(RecordingContainerFormat::Matroska),
        "mp4" => Some(RecordingContainerFormat::Mp4),
        _ => None,
    }
}

#[derive(Properties, Clone, PartialEq)]
pub struct RecordingConfigCardsProps {
    pub recording: RecordingConfigDto,
    pub reload_generation: u64,
    pub edit_mode: bool,
    pub on_change: Callback<(bool, RecordingConfigDto)>,
    pub on_error: Callback<String>,
}

#[component]
pub fn RecordingConfigCards(props: &RecordingConfigCardsProps) -> Html {
    let translate = use_translation();
    let direct_state: UseReducerHandle<RecordingConfigFormState> =
        use_default_form_reducer!(RecordingConfigFormState { form: props.recording.clone() });
    let retention_state: UseReducerHandle<RecordingRetentionFormState> =
        use_default_form_reducer!(RecordingRetentionFormState {
            form: props.recording.retention.clone().unwrap_or_default()
        });
    let disk_state: UseReducerHandle<RecordingDiskFormState> =
        use_default_form_reducer!(RecordingDiskFormState { form: props.recording.disk.clone().unwrap_or_default() });
    let quota_state: UseReducerHandle<RecordingQuotaFormState> =
        use_default_form_reducer!(RecordingQuotaFormState { form: props.recording.quota.clone().unwrap_or_default() });
    let notification_state: UseReducerHandle<RecordingNotificationFormState> =
        use_default_form_reducer!(RecordingNotificationFormState {
            form: props.recording.notifications.clone().unwrap_or_default()
        });

    {
        let direct_state = direct_state.clone();
        let retention_state = retention_state.clone();
        let disk_state = disk_state.clone();
        let quota_state = quota_state.clone();
        let notification_state = notification_state.clone();
        let recording = props.recording.clone();
        use_effect_with(props.reload_generation, move |_| {
            direct_state.dispatch(RecordingConfigFormAction::SetAll(recording.clone()));
            retention_state
                .dispatch(RecordingRetentionFormAction::SetAll(recording.retention.clone().unwrap_or_default()));
            disk_state.dispatch(RecordingDiskFormAction::SetAll(recording.disk.clone().unwrap_or_default()));
            quota_state.dispatch(RecordingQuotaFormAction::SetAll(recording.quota.clone().unwrap_or_default()));
            notification_state
                .dispatch(RecordingNotificationFormAction::SetAll(recording.notifications.clone().unwrap_or_default()));
            || ()
        });
    }

    use_emit_mapped(
        (
            direct_state.form.clone(),
            retention_state.form.clone(),
            disk_state.form.clone(),
            quota_state.form.clone(),
            notification_state.form.clone(),
            direct_state.modified(),
            retention_state.modified(),
            disk_state.modified(),
            quota_state.modified(),
            notification_state.modified(),
        ),
        props.on_change.clone(),
        |(
            direct,
            retention,
            disk,
            quota,
            notifications,
            direct_changed,
            retention_changed,
            disk_changed,
            quota_changed,
            notifications_changed,
        )| {
            let changes = RecordingSectionChanges {
                retention: retention_changed,
                disk: disk_changed,
                quota: quota_changed,
                notifications: notifications_changed,
            };
            (
                direct_changed || retention_changed || disk_changed || quota_changed || notifications_changed,
                assemble_recording_config(direct, retention, disk, quota, notifications, changes),
            )
        },
    );

    let invalid_unsigned = {
        let on_error = props.on_error.clone();
        let translate = translate.clone();
        Callback::from(move |error: UnsignedInputError| {
            let key = match error {
                UnsignedInputError::SyntaxOrOverflow => "MESSAGES.RECORDING.INVALID_UNSIGNED",
                UnsignedInputError::Range => "MESSAGES.RECORDING.INVALID_RANGE",
            };
            on_error.emit(translate.t(key));
        })
    };

    let render_view_field = |field: &RecordingFieldDescriptor| {
        let label = translate.t(field.label);
        let value = match field.id {
            "priority" => direct_state.form.priority.to_string(),
            "container_format" => recording_container_id(direct_state.form.container_format).to_string(),
            "directory" => direct_state.form.directory.clone().unwrap_or_default(),
            "timezone" => direct_state.form.timezone.clone().unwrap_or_default(),
            "filename_template" => direct_state.form.filename_template.clone().unwrap_or_default(),
            "default_pre_roll_secs" => {
                direct_state.form.default_pre_roll_secs.map(|v| v.to_string()).unwrap_or_default()
            }
            "max_pre_roll_secs" => direct_state.form.max_pre_roll_secs.to_string(),
            "default_post_roll_secs" => {
                direct_state.form.default_post_roll_secs.map(|v| v.to_string()).unwrap_or_default()
            }
            "max_post_roll_secs" => direct_state.form.max_post_roll_secs.to_string(),
            "keep_last_per_channel" => {
                retention_state.form.keep_last_per_channel.map(|v| v.to_string()).unwrap_or_default()
            }
            "delete_after_days" => retention_state.form.delete_after_days.map(|v| v.to_string()).unwrap_or_default(),
            "sweep_interval_secs" => retention_state.form.sweep_interval_secs.to_string(),
            "high_water_percent" => disk_state.form.high_water_percent.map(|v| v.to_string()).unwrap_or_default(),
            "low_water_percent" => disk_state.form.low_water_percent.map(|v| v.to_string()).unwrap_or_default(),
            "cleanup_interval_secs" => disk_state.form.cleanup_interval_secs.map(|v| v.to_string()).unwrap_or_default(),
            "safety_bytes" => disk_state.form.safety_bytes.map(|v| v.to_string()).unwrap_or_default(),
            "fallback_bytes_per_minute" => direct_state.form.fallback_bytes_per_minute.to_string(),
            "default_private_bytes" => {
                quota_state.form.default_private_bytes.map(|v| v.to_string()).unwrap_or_default()
            }
            "shared_bytes" => quota_state.form.shared_bytes.map(|v| v.to_string()).unwrap_or_default(),
            "outbox_buffer" => notification_state.form.outbox_buffer.to_string(),
            "max_attempts" => notification_state.form.max_attempts.to_string(),
            "backoff_initial_secs" => notification_state.form.backoff_initial_secs.to_string(),
            "backoff_max_secs" => notification_state.form.backoff_max_secs.to_string(),
            _ => String::new(),
        };
        let content = match field.id {
            "enabled" => html! {
                <div class="tp__form-field tp__form-field__bool">
                    <ToggleSwitch value={direct_state.form.enabled} readonly={true} />
                    <FieldLabel label={label} field_id={field.id} />
                </div>
            },
            "per_user_bytes" => html! {
                <KeyValueEditor label={Some(label)} entries={quota_entries_to_strings(&quota_state.form.per_user_bytes)} />
            },
            _ => html! {
                <div class="tp__form-field tp__form-field__text">
                    <FieldLabel label={label} field_id={field.id} />
                    <span class="tp__form-field__value">{value}</span>
                </div>
            },
        };
        html! { <div data-recording-field={field.id}>{content}</div> }
    };

    let render_edit_field = |field: &RecordingFieldDescriptor| {
        let label = translate.t(field.label);
        let invalid = invalid_unsigned.clone();
        let unsigned = |value: Option<u64>, allow_empty: bool, min: Option<u64>, max: Option<u64>, on_change| {
            html! {
                <crate::app::components::number_input::NumberInput
                    name={field.id}
                    label={Some(label.clone())}
                    u64_value={value}
                    on_change_u64={Some(on_change)}
                    allow_empty_u64={allow_empty}
                    min_u64={min}
                    max_u64={max}
                    on_invalid_u64={Some(invalid.clone())}
                />
            }
        };
        let content = match field.id {
            "enabled" => {
                let state = direct_state.clone();
                html! { <div class="tp__form-field tp__form-field__bool"><ToggleSwitch value={state.form.enabled} readonly={false} on_change={Callback::from(move |value| state.dispatch(RecordingConfigFormAction::Enabled(value)))} /><FieldLabel label={label} field_id={field.id} /></div> }
            }
            "priority" => {
                let state = direct_state.clone();
                html! { <crate::app::components::number_input::NumberInput name={field.id} label={Some(label)} value={Some(i64::from(state.form.priority))} min_i64={Some(i64::from(i8::MIN))} max_i64={Some(i64::from(i8::MAX))} on_change={Callback::from(move |value: Option<i64>| { if let Some(value) = value.and_then(|value| i8::try_from(value).ok()) { state.dispatch(RecordingConfigFormAction::Priority(value)); } })} /> }
            }
            "container_format" => {
                let state = direct_state.clone();
                let selected = state.form.container_format;
                let options = Rc::new(
                    [
                        RecordingContainerFormat::Mpegts,
                        RecordingContainerFormat::Matroska,
                        RecordingContainerFormat::Mp4,
                    ]
                    .into_iter()
                    .map(|container| {
                        DropDownOption::new(
                            recording_container_id(container),
                            html! { recording_container_id(container) },
                            container == selected,
                        )
                    })
                    .collect(),
                );
                html! { <><FieldLabel label={label} field_id={field.id} /><Select name={field.id} options={options} on_select={Callback::from(move |(_, selection)| { if let DropDownSelection::Single(id) = selection { if let Some(container) = recording_container_from_id(&id) { state.dispatch(RecordingConfigFormAction::ContainerFormat(container)); } } })} /></> }
            }
            "directory" | "timezone" | "filename_template" => {
                let state = direct_state.clone();
                let value = match field.id {
                    "directory" => state.form.directory.clone(),
                    "timezone" => state.form.timezone.clone(),
                    _ => state.form.filename_template.clone(),
                }
                .unwrap_or_default();
                let id = field.id;
                html! { <Input name={id} label={Some(label)} value={value} hint_key={(id == "filename_template").then(|| "VIDEO_CONFIG.RECORDING_FILENAME_TEMPLATE".to_string())} on_change={Some(Callback::from(move |value: String| { let value = (!value.is_empty()).then_some(value); match id { "directory" => state.dispatch(RecordingConfigFormAction::Directory(value)), "timezone" => state.dispatch(RecordingConfigFormAction::Timezone(value)), _ => state.dispatch(RecordingConfigFormAction::FilenameTemplate(value)), } }))} /> }
            }
            "default_pre_roll_secs" => {
                let state = direct_state.clone();
                unsigned(
                    state.form.default_pre_roll_secs,
                    true,
                    None,
                    None,
                    Callback::from(move |v| state.dispatch(RecordingConfigFormAction::DefaultPreRollSecs(v))),
                )
            }
            "max_pre_roll_secs" => {
                let state = direct_state.clone();
                unsigned(
                    Some(state.form.max_pre_roll_secs),
                    false,
                    None,
                    None,
                    Callback::from(move |v| {
                        if let Some(v) = v {
                            state.dispatch(RecordingConfigFormAction::MaxPreRollSecs(v));
                        }
                    }),
                )
            }
            "default_post_roll_secs" => {
                let state = direct_state.clone();
                unsigned(
                    state.form.default_post_roll_secs,
                    true,
                    None,
                    None,
                    Callback::from(move |v| state.dispatch(RecordingConfigFormAction::DefaultPostRollSecs(v))),
                )
            }
            "max_post_roll_secs" => {
                let state = direct_state.clone();
                unsigned(
                    Some(state.form.max_post_roll_secs),
                    false,
                    None,
                    None,
                    Callback::from(move |v| {
                        if let Some(v) = v {
                            state.dispatch(RecordingConfigFormAction::MaxPostRollSecs(v));
                        }
                    }),
                )
            }
            "keep_last_per_channel" => {
                let state = retention_state.clone();
                unsigned(
                    state.form.keep_last_per_channel.map(u64::from),
                    true,
                    Some(1),
                    Some(u64::from(u32::MAX)),
                    Callback::from(move |v| {
                        if let Some(v) = v {
                            if let Ok(v) = u32::try_from(v) {
                                state.dispatch(RecordingRetentionFormAction::KeepLastPerChannel(Some(v)));
                            }
                        } else {
                            state.dispatch(RecordingRetentionFormAction::KeepLastPerChannel(None));
                        }
                    }),
                )
            }
            "delete_after_days" => {
                let state = retention_state.clone();
                unsigned(
                    state.form.delete_after_days.map(u64::from),
                    true,
                    Some(1),
                    Some(u64::from(u32::MAX)),
                    Callback::from(move |v| {
                        if let Some(v) = v {
                            if let Ok(v) = u32::try_from(v) {
                                state.dispatch(RecordingRetentionFormAction::DeleteAfterDays(Some(v)));
                            }
                        } else {
                            state.dispatch(RecordingRetentionFormAction::DeleteAfterDays(None));
                        }
                    }),
                )
            }
            "sweep_interval_secs" => {
                let state = retention_state.clone();
                unsigned(
                    Some(state.form.sweep_interval_secs),
                    false,
                    Some(1),
                    None,
                    Callback::from(move |v| {
                        if let Some(v) = v {
                            state.dispatch(RecordingRetentionFormAction::SweepIntervalSecs(v));
                        }
                    }),
                )
            }
            "high_water_percent" => {
                let state = disk_state.clone();
                unsigned(
                    state.form.high_water_percent.map(u64::from),
                    true,
                    None,
                    Some(100),
                    Callback::from(move |v| {
                        if let Some(v) = v {
                            if let Ok(v) = u8::try_from(v) {
                                state.dispatch(RecordingDiskFormAction::HighWaterPercent(Some(v)));
                            }
                        } else {
                            state.dispatch(RecordingDiskFormAction::HighWaterPercent(None));
                        }
                    }),
                )
            }
            "low_water_percent" => {
                let state = disk_state.clone();
                unsigned(
                    state.form.low_water_percent.map(u64::from),
                    true,
                    None,
                    Some(100),
                    Callback::from(move |v| {
                        if let Some(v) = v {
                            if let Ok(v) = u8::try_from(v) {
                                state.dispatch(RecordingDiskFormAction::LowWaterPercent(Some(v)));
                            }
                        } else {
                            state.dispatch(RecordingDiskFormAction::LowWaterPercent(None));
                        }
                    }),
                )
            }
            "cleanup_interval_secs" => {
                let state = disk_state.clone();
                unsigned(
                    state.form.cleanup_interval_secs,
                    true,
                    Some(1),
                    None,
                    Callback::from(move |v| state.dispatch(RecordingDiskFormAction::CleanupIntervalSecs(v))),
                )
            }
            "safety_bytes" => {
                let state = disk_state.clone();
                unsigned(
                    state.form.safety_bytes,
                    true,
                    Some(1),
                    None,
                    Callback::from(move |v| state.dispatch(RecordingDiskFormAction::SafetyBytes(v))),
                )
            }
            "fallback_bytes_per_minute" => {
                let state = direct_state.clone();
                unsigned(
                    Some(state.form.fallback_bytes_per_minute),
                    false,
                    Some(1),
                    None,
                    Callback::from(move |v| {
                        if let Some(v) = v {
                            state.dispatch(RecordingConfigFormAction::FallbackBytesPerMinute(v));
                        }
                    }),
                )
            }
            "default_private_bytes" => {
                let state = quota_state.clone();
                unsigned(
                    state.form.default_private_bytes,
                    true,
                    None,
                    None,
                    Callback::from(move |v| state.dispatch(RecordingQuotaFormAction::DefaultPrivateBytes(v))),
                )
            }
            "shared_bytes" => {
                let state = quota_state.clone();
                unsigned(
                    state.form.shared_bytes,
                    true,
                    None,
                    None,
                    Callback::from(move |v| state.dispatch(RecordingQuotaFormAction::SharedBytes(v))),
                )
            }
            "per_user_bytes" => {
                let state = quota_state.clone();
                let on_error = props.on_error.clone();
                let quota_error = translate.t("MESSAGES.RECORDING.INVALID_QUOTA");
                html! { <KeyValueEditor label={Some(label)} entries={quota_entries_to_strings(&state.form.per_user_bytes)} readonly={false} key_placeholder={translate.t("LABEL.RECORDING_QUOTA_USER")} value_placeholder={translate.t("LABEL.RECORDING_QUOTA_BYTES")} validate_entry={Some(Callback::from(move |(_, value): (String, String)| { if value.trim().parse::<u64>().is_ok() { true } else { on_error.emit(quota_error.clone()); false } }))} on_change={Callback::from(move |entries| { if let Ok(entries) = quota_entries_from_strings(&entries) { state.dispatch(RecordingQuotaFormAction::PerUserBytes(entries)); } })} /> }
            }
            "outbox_buffer" => {
                let state = notification_state.clone();
                unsigned(
                    Some(state.form.outbox_buffer as u64),
                    false,
                    Some(1),
                    Some(usize::MAX as u64),
                    Callback::from(move |v| {
                        if let Some(v) = v {
                            if let Ok(v) = usize::try_from(v) {
                                state.dispatch(RecordingNotificationFormAction::OutboxBuffer(v));
                            }
                        }
                    }),
                )
            }
            "max_attempts" => {
                let state = notification_state.clone();
                unsigned(
                    Some(u64::from(state.form.max_attempts)),
                    false,
                    Some(1),
                    Some(u64::from(u32::MAX)),
                    Callback::from(move |v| {
                        if let Some(v) = v {
                            if let Ok(v) = u32::try_from(v) {
                                state.dispatch(RecordingNotificationFormAction::MaxAttempts(v));
                            }
                        }
                    }),
                )
            }
            "backoff_initial_secs" => {
                let state = notification_state.clone();
                unsigned(
                    Some(state.form.backoff_initial_secs),
                    false,
                    Some(1),
                    None,
                    Callback::from(move |v| {
                        if let Some(v) = v {
                            state.dispatch(RecordingNotificationFormAction::BackoffInitialSecs(v));
                        }
                    }),
                )
            }
            "backoff_max_secs" => {
                let state = notification_state.clone();
                unsigned(
                    Some(state.form.backoff_max_secs),
                    false,
                    Some(1),
                    None,
                    Callback::from(move |v| {
                        if let Some(v) = v {
                            state.dispatch(RecordingNotificationFormAction::BackoffMaxSecs(v));
                        }
                    }),
                )
            }
            _ => Html::default(),
        };
        html! { <div data-recording-field={field.id}>{content}</div> }
    };

    html! {
        <>
            { for RECORDING_CARDS.iter().enumerate().map(|(card_index, card)| html! {
                <div id={card.id}>
                    <Card class="tp__config-view__card">
                        <h1>{translate.t(card.label)}</h1>
                        { for RECORDING_FIELDS.iter().filter(|field| field.card == card_index).map(|field| if props.edit_mode { render_edit_field(field) } else { render_view_field(field) }) }
                    </Card>
                </div>
            }) }
        </>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{
        RecordingConfigDto, RecordingContainerFormat, RecordingDiskConfigDto, RecordingNotificationConfigDto,
        RecordingQuotaConfigDto, RecordingRetentionConfigDto,
    };
    use std::collections::HashMap;

    pub(super) fn populated_recording() -> RecordingConfigDto {
        RecordingConfigDto {
            enabled: false,
            container_format: RecordingContainerFormat::Mp4,
            directory: Some("recordings".to_string()),
            timezone: Some("Europe/Berlin".to_string()),
            filename_template: Some("{channel}-{start_time}".to_string()),
            default_pre_roll_secs: Some(7),
            max_pre_roll_secs: 11,
            default_post_roll_secs: Some(13),
            max_post_roll_secs: 17,
            retention: Some(RecordingRetentionConfigDto {
                keep_last_per_channel: Some(2),
                delete_after_days: Some(30),
                sweep_interval_secs: 19,
            }),
            disk: Some(RecordingDiskConfigDto {
                high_water_percent: Some(90),
                low_water_percent: Some(80),
                cleanup_interval_secs: Some(23),
                safety_bytes: Some(29),
            }),
            quota: Some(RecordingQuotaConfigDto {
                default_private_bytes: Some(31),
                per_user_bytes: HashMap::from([("alice".to_string(), 37)]),
                shared_bytes: Some(41),
            }),
            notifications: Some(RecordingNotificationConfigDto {
                outbox_buffer: 43,
                max_attempts: 47,
                backoff_initial_secs: 53,
                backoff_max_secs: 59,
            }),
            fallback_bytes_per_minute: 61,
            ..RecordingConfigDto::default()
        }
    }

    #[test]
    fn unchanged_populated_recording_assembles_exactly() {
        let original = populated_recording();

        let assembled = assemble_recording_config(
            original.clone(),
            original.retention.clone().unwrap_or_default(),
            original.disk.clone().unwrap_or_default(),
            original.quota.clone().unwrap_or_default(),
            original.notifications.clone().unwrap_or_default(),
            RecordingSectionChanges::default(),
        );

        assert_eq!(assembled, original);
    }

    #[test]
    fn direct_recording_merge_preserves_all_nested_sections() {
        let original = populated_recording();
        let mut direct = original.clone();
        direct.timezone = Some("UTC".to_string());

        let merged = assemble_recording_config(
            direct,
            original.retention.clone().unwrap_or_default(),
            original.disk.clone().unwrap_or_default(),
            original.quota.clone().unwrap_or_default(),
            original.notifications.clone().unwrap_or_default(),
            RecordingSectionChanges::default(),
        );

        assert_eq!(merged.timezone.as_deref(), Some("UTC"));
        assert_eq!(merged.retention, original.retention);
        assert_eq!(merged.disk, original.disk);
        assert_eq!(merged.quota, original.quota);
        assert_eq!(merged.notifications, original.notifications);
    }

    #[test]
    fn isolated_nested_recording_merge_preserves_siblings() {
        let original = populated_recording();
        let retention = RecordingRetentionConfigDto { keep_last_per_channel: Some(99), ..Default::default() };

        let merged = assemble_recording_config(
            original.clone(),
            retention.clone(),
            original.disk.clone().unwrap_or_default(),
            original.quota.clone().unwrap_or_default(),
            original.notifications.clone().unwrap_or_default(),
            RecordingSectionChanges { retention: true, ..Default::default() },
        );

        assert_eq!(merged.retention, Some(retention));
        assert_eq!(merged.disk, original.disk);
        assert_eq!(merged.quota, original.quota);
        assert_eq!(merged.notifications, original.notifications);
    }

    #[test]
    fn clearing_last_nested_value_removes_only_changed_section() {
        let original = populated_recording();
        let cases = [
            RecordingSectionChanges { retention: true, ..Default::default() },
            RecordingSectionChanges { disk: true, ..Default::default() },
            RecordingSectionChanges { quota: true, ..Default::default() },
            RecordingSectionChanges { notifications: true, ..Default::default() },
        ];

        for changes in cases {
            let merged = assemble_recording_config(
                original.clone(),
                RecordingRetentionConfigDto::default(),
                RecordingDiskConfigDto::default(),
                RecordingQuotaConfigDto::default(),
                RecordingNotificationConfigDto::default(),
                changes,
            );
            assert_eq!(merged.retention.is_none(), changes.retention);
            assert_eq!(merged.disk.is_none(), changes.disk);
            assert_eq!(merged.quota.is_none(), changes.quota);
            assert_eq!(merged.notifications.is_none(), changes.notifications);
        }
    }

    #[test]
    fn quota_string_entries_round_trip_full_u64_range() {
        let entries = HashMap::from([("zero".to_string(), 0), ("max".to_string(), u64::MAX)]);
        let strings = quota_entries_to_strings(&entries);

        assert_eq!(quota_entries_from_strings(&strings), Ok(entries));
    }

    #[test]
    fn invalid_quota_entries_fail_atomically() {
        let entries = HashMap::from([
            ("valid".to_string(), "42".to_string()),
            ("invalid".to_string(), "not-a-number".to_string()),
        ]);

        assert!(quota_entries_from_strings(&entries).is_err());
    }

    #[test]
    fn recording_container_ids_round_trip_wire_values() {
        let cases = [
            ("mpegts", RecordingContainerFormat::Mpegts),
            ("matroska", RecordingContainerFormat::Matroska),
            ("mp4", RecordingContainerFormat::Mp4),
        ];

        for (id, container) in cases {
            assert_eq!(recording_container_from_id(id), Some(container));
            assert_eq!(recording_container_id(container), id);
        }
    }

    #[test]
    fn recording_descriptor_inventory_covers_every_field_once() {
        let actual = RECORDING_FIELDS.iter().map(|field| field.id).collect::<std::collections::HashSet<_>>();
        let expected = std::collections::HashSet::from([
            "enabled",
            "priority",
            "container_format",
            "directory",
            "timezone",
            "filename_template",
            "default_pre_roll_secs",
            "max_pre_roll_secs",
            "default_post_roll_secs",
            "max_post_roll_secs",
            "keep_last_per_channel",
            "delete_after_days",
            "sweep_interval_secs",
            "high_water_percent",
            "low_water_percent",
            "cleanup_interval_secs",
            "safety_bytes",
            "fallback_bytes_per_minute",
            "default_private_bytes",
            "per_user_bytes",
            "shared_bytes",
            "outbox_buffer",
            "max_attempts",
            "backoff_initial_secs",
            "backoff_max_secs",
        ]);

        assert_eq!(actual, expected);
        assert_eq!(RECORDING_FIELDS.len(), expected.len());
        assert_eq!(RECORDING_CARDS.len(), 5);
    }

    #[test]
    fn recording_ui_translation_keys_exist_in_every_locale() {
        const KEYS: [&str; 31] = [
            "LABEL.RECORDING_GENERAL",
            "LABEL.RECORDING_PADDING",
            "LABEL.RECORDING_RETENTION_DISK",
            "LABEL.RECORDING_QUOTAS",
            "LABEL.RECORDING_NOTIFICATIONS",
            "LABEL.RECORDING_FILENAME_TEMPLATE",
            "LABEL.RECORDING_DEFAULT_PRE_ROLL_SECS",
            "LABEL.RECORDING_MAX_PRE_ROLL_SECS",
            "LABEL.RECORDING_DEFAULT_POST_ROLL_SECS",
            "LABEL.RECORDING_MAX_POST_ROLL_SECS",
            "LABEL.RECORDING_KEEP_LAST_PER_CHANNEL",
            "LABEL.RECORDING_DELETE_AFTER_DAYS",
            "LABEL.RECORDING_SWEEP_INTERVAL_SECS",
            "LABEL.RECORDING_HIGH_WATER_PERCENT",
            "LABEL.RECORDING_LOW_WATER_PERCENT",
            "LABEL.RECORDING_CLEANUP_INTERVAL_SECS",
            "LABEL.RECORDING_SAFETY_BYTES",
            "LABEL.RECORDING_FALLBACK_BYTES_PER_MINUTE",
            "LABEL.RECORDING_DEFAULT_PRIVATE_BYTES",
            "LABEL.RECORDING_PER_USER_BYTES",
            "LABEL.RECORDING_SHARED_BYTES",
            "LABEL.RECORDING_OUTBOX_BUFFER",
            "LABEL.RECORDING_MAX_ATTEMPTS",
            "LABEL.RECORDING_BACKOFF_INITIAL_SECS",
            "LABEL.RECORDING_BACKOFF_MAX_SECS",
            "LABEL.RECORDING_QUOTA_USER",
            "LABEL.RECORDING_QUOTA_BYTES",
            "EXPLANATION.VIDEO_CONFIG.RECORDING_FILENAME_TEMPLATE",
            "MESSAGES.RECORDING.INVALID_UNSIGNED",
            "MESSAGES.RECORDING.INVALID_RANGE",
            "MESSAGES.RECORDING.INVALID_QUOTA",
        ];
        const LOCALES: [(&str, &str); 3] = [
            ("en", include_str!("../../../../public/assets/i18n/en.json")),
            ("ar", include_str!("../../../../public/assets/i18n/ar.json")),
            ("ru", include_str!("../../../../public/assets/i18n/ru.json")),
        ];

        for (locale, json) in LOCALES {
            let parsed = serde_json::from_str::<serde_json::Value>(json);
            assert!(parsed.is_ok(), "{locale} locale must be valid JSON");
            let Ok(parsed) = parsed else { continue };
            for key in KEYS {
                let value = key.split('.').try_fold(&parsed, |value, segment| value.get(segment));
                assert!(
                    value.and_then(serde_json::Value::as_str).is_some_and(|text| !text.trim().is_empty()),
                    "{locale} locale is missing non-empty key {key}"
                );
            }
        }
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod browser_tests {
    use super::*;
    use crate::{
        app::{
            components::config::{ConfigForm, ConfigViewContext, RecordingConfigView},
            ConfigContext,
        },
        i18n::I18nProvider,
        model::WebConfig,
        provider::{DialogProvider, IconContextProvider, ServiceContextProvider},
    };
    use gloo_timers::future::TimeoutFuture;
    use std::{cell::RefCell, collections::HashSet, rc::Rc};
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};
    use web_sys::{Element, Event, HtmlInputElement};
    use yew::{AppHandle, Renderer};

    wasm_bindgen_test_configure!(run_in_browser);

    #[derive(Properties, Clone, PartialEq)]
    struct HarnessProps {
        recording: RecordingConfigDto,
        priority: i8,
        generation: u64,
        edit_mode: bool,
        emissions: Rc<RefCell<Vec<(bool, RecordingConfigDto)>>>,
    }

    #[derive(Properties, Clone, PartialEq)]
    struct RecordingHarnessProps {
        recording: RecordingConfigDto,
        emissions: Rc<RefCell<Vec<ConfigForm>>>,
    }

    #[component]
    fn Harness(props: &HarnessProps) -> Html {
        let emissions = Rc::clone(&props.emissions);
        html! {
            <I18nProvider>
                <IconContextProvider icons={vec![]}>
                    <DialogProvider>
                        <RecordingConfigCards
                            recording={props.recording.clone()}
                            reload_generation={props.generation}
                            edit_mode={props.edit_mode}
                            on_change={Callback::from(move |value| emissions.borrow_mut().push(value))}
                            on_error={Callback::noop()}
                        />
                    </DialogProvider>
                </IconContextProvider>
            </I18nProvider>
        }
    }

    #[component]
    fn RecordingHarness(props: &RecordingHarnessProps) -> Html {
        let edit_mode = use_state(|| true);
        let emissions = Rc::clone(&props.emissions);
        let config = shared::model::AppConfigDto {
            config: shared::model::ConfigDto {
                video: Some(shared::model::VideoConfigDto {
                    extensions: Vec::new(),
                    web_search: None,
                    recording: Some(props.recording.clone()),
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let config_ctx = ConfigContext { config: Some(Rc::new(config)), api_proxy: None };
        let view_ctx = ConfigViewContext {
            edit_mode,
            show_restart_notice: false,
            on_form_change: Callback::from(move |form| emissions.borrow_mut().push(form)),
        };

        html! {
            <I18nProvider>
                <IconContextProvider icons={vec![]}>
                    <DialogProvider>
                        <ServiceContextProvider config={WebConfig::default()}>
                            <ContextProvider<ConfigContext> context={config_ctx}>
                                <ContextProvider<ConfigViewContext> context={view_ctx}>
                                    <RecordingConfigView />
                                </ContextProvider<ConfigViewContext>>
                            </ContextProvider<ConfigContext>>
                        </ServiceContextProvider>
                    </DialogProvider>
                </IconContextProvider>
            </I18nProvider>
        }
    }

    async fn settle() {
        for _ in 0..8 {
            TimeoutFuture::new(0).await;
        }
    }

    async fn render(props: HarnessProps) -> Result<(Element, AppHandle<Harness>), wasm_bindgen::JsValue> {
        let document = gloo_utils::document();
        let body = document.body().ok_or_else(|| wasm_bindgen::JsValue::from_str("test document has no body"))?;
        let root = document.create_element("div")?;
        body.append_child(&root)?;
        let handle = Renderer::<Harness>::with_root_and_props(root.clone(), props).render();
        settle().await;
        Ok((root, handle))
    }

    fn input(root: &Element, field: &str) -> Result<HtmlInputElement, wasm_bindgen::JsValue> {
        root.query_selector(&format!("[data-recording-field=\"{field}\"] input"))?
            .ok_or_else(|| wasm_bindgen::JsValue::from_str("recording input was not rendered"))?
            .dyn_into::<HtmlInputElement>()
            .map_err(|_| wasm_bindgen::JsValue::from_str("recording field is not an input"))
    }

    fn set_input(input: &HtmlInputElement, value: &str) -> Result<(), wasm_bindgen::JsValue> {
        input.set_value(value);
        input.dispatch_event(&Event::new("input")?)?;
        Ok(())
    }

    fn rendered_fields(root: &Element) -> Result<HashSet<String>, wasm_bindgen::JsValue> {
        let nodes = root.query_selector_all("[data-recording-field]")?;
        let mut fields = HashSet::new();
        for index in 0..nodes.length() {
            let Some(node) = nodes.item(index) else { continue };
            let element = node
                .dyn_into::<Element>()
                .map_err(|_| wasm_bindgen::JsValue::from_str("recording field node is not an element"))?;
            if let Some(field) = element.get_attribute("data-recording-field") {
                fields.insert(field);
            }
        }
        Ok(fields)
    }

    fn assert_real_field(
        root: &Element,
        card: &RecordingCardDescriptor,
        field: &RecordingFieldDescriptor,
        edit_mode: bool,
    ) -> Result<(), wasm_bindgen::JsValue> {
        let wrapper = root
            .query_selector(&format!("#{} [data-recording-field=\"{}\"]", card.id, field.id))?
            .ok_or_else(|| wasm_bindgen::JsValue::from_str("recording field is not in its descriptor card"))?;
        match (edit_mode, field.id) {
            (true, "enabled") => assert!(wrapper.query_selector("input[type=\"checkbox\"]")?.is_some()),
            (true, "container_format") => assert!(wrapper.query_selector(".tp__select")?.is_some()),
            (true, "per_user_bytes") => {
                let entries = wrapper
                    .query_selector(".tp__keyvalue-editor__entries")?
                    .and_then(|entries| entries.text_content())
                    .unwrap_or_default();
                assert!(!entries.trim().is_empty());
            }
            (true, _) => {
                let value = wrapper
                    .query_selector("input")?
                    .ok_or_else(|| wasm_bindgen::JsValue::from_str("recording edit control is missing"))?
                    .dyn_into::<HtmlInputElement>()
                    .map_err(|_| wasm_bindgen::JsValue::from_str("recording edit control is not an input"))?
                    .value();
                assert!(!value.is_empty());
            }
            (false, "enabled") => assert!(wrapper.query_selector("input[type=\"checkbox\"]:disabled")?.is_some()),
            (false, "per_user_bytes") => {
                let value = wrapper
                    .query_selector(".tp__keyvalue-editor")?
                    .and_then(|entries| entries.text_content())
                    .unwrap_or_default();
                assert!(!value.trim().is_empty());
            }
            (false, _) => {
                let value = wrapper
                    .query_selector(".tp__form-field__value")?
                    .and_then(|value| value.text_content())
                    .unwrap_or_default();
                assert!(!value.trim().is_empty());
            }
        }
        Ok(())
    }

    #[wasm_bindgen_test(async)]
    async fn both_modes_render_the_complete_descriptor_inventory() -> Result<(), wasm_bindgen::JsValue> {
        let expected = RECORDING_FIELDS.iter().map(|field| field.id.to_string()).collect::<HashSet<_>>();
        let emissions = Rc::new(RefCell::new(Vec::new()));
        let props = HarnessProps {
            recording: super::tests::populated_recording(),
            priority: 3,
            generation: 0,
            edit_mode: false,
            emissions,
        };
        let (root, mut handle) = render(props.clone()).await?;

        assert_eq!(rendered_fields(&root)?, expected);
        for card in RECORDING_CARDS {
            assert!(root.query_selector(&format!("#{}", card.id))?.is_some());
        }
        for field in RECORDING_FIELDS {
            assert_real_field(&root, &RECORDING_CARDS[field.card], &field, false)?;
        }

        handle.update(HarnessProps { edit_mode: true, ..props });
        settle().await;
        assert_eq!(rendered_fields(&root)?, expected);
        for field in RECORDING_FIELDS {
            assert_real_field(&root, &RECORDING_CARDS[field.card], &field, true)?;
        }

        handle.destroy();
        root.remove();
        Ok(())
    }

    #[wasm_bindgen_test(async)]
    async fn reload_resets_dirty_state_and_emits_only_config_b() -> Result<(), wasm_bindgen::JsValue> {
        let emissions = Rc::new(RefCell::new(Vec::new()));
        let config_a = RecordingConfigDto { timezone: Some("A".to_string()), ..Default::default() };
        let config_b = RecordingConfigDto {
            timezone: Some("B".to_string()),
            retention: Some(RecordingRetentionConfigDto { delete_after_days: Some(12), ..Default::default() }),
            ..Default::default()
        };
        let props = HarnessProps {
            recording: config_a,
            priority: 0,
            generation: 0,
            edit_mode: true,
            emissions: Rc::clone(&emissions),
        };
        let (root, mut handle) = render(props.clone()).await?;
        set_input(&input(&root, "timezone")?, "dirty")?;
        settle().await;
        emissions.borrow_mut().clear();

        handle.update(HarnessProps { recording: config_b.clone(), generation: 1, ..props });
        settle().await;
        let last = emissions.borrow().last().cloned();

        assert_eq!(last, Some((false, config_b)));
        assert_eq!(input(&root, "timezone")?.value(), "B");
        assert_eq!(input(&root, "delete_after_days")?.value(), "12");
        emissions.borrow_mut().clear();
        set_input(&input(&root, "timezone")?, "after-reload")?;
        settle().await;
        let last = emissions.borrow().last().cloned();
        assert!(last.is_some_and(|(modified, recording)| {
            modified
                && recording.timezone.as_deref() == Some("after-reload")
                && recording.retention.as_ref().and_then(|retention| retention.delete_after_days) == Some(12)
        }));
        handle.destroy();
        root.remove();
        Ok(())
    }

    #[wasm_bindgen_test(async)]
    async fn recording_and_priority_controls_reach_outer_video_form_together() -> Result<(), wasm_bindgen::JsValue> {
        let emissions = Rc::new(RefCell::new(Vec::<ConfigForm>::new()));
        let recording = RecordingConfigDto::default();
        let document = gloo_utils::document();
        let body = document.body().ok_or_else(|| wasm_bindgen::JsValue::from_str("test document has no body"))?;
        let root = document.create_element("div")?;
        body.append_child(&root)?;
        let handle = Renderer::<RecordingHarness>::with_root_and_props(
            root.clone(),
            RecordingHarnessProps { recording, emissions: Rc::clone(&emissions) },
        )
        .render();
        settle().await;
        emissions.borrow_mut().clear();

        set_input(&input(&root, "timezone")?, "UTC")?;
        set_input(&input(&root, "priority")?, "9")?;
        settle().await;
        let last = emissions.borrow().last().cloned();

        assert!(last.is_some_and(|form| match form {
            ConfigForm::Recording(modified, video) => {
                modified
                    && video.recording.as_ref().is_some_and(|recording| {
                        recording.priority == 9 && recording.timezone.as_deref() == Some("UTC")
                    })
            }
            _ => false,
        }));
        handle.destroy();
        root.remove();
        Ok(())
    }
}
