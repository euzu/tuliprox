use crate::{
    app::{
        components::{
            config::{
                config_page::{ConfigForm, LABEL_CHANNEL_GROUPS_CONFIG},
                config_view_context::ConfigViewContext,
                use_emit_config_form,
            },
            input::Input,
            number_input::NumberInput,
            select::Select,
            selection_first, Card, DropDownOption, DropDownSelection, FieldLabel, KeyValueEditor, Tag, TagList,
            ToggleSwitch,
        },
        context::ConfigContext,
    },
    generate_form_reducer,
    i18n::use_translation,
    use_default_form_reducer,
};
use shared::model::{
    ChannelGroupPolicy, ChannelGroupsConfig, ChannelMatchBy, ChannelMatchNormalize, EpgConflictMode,
    RuntimeAudioLanguage,
};
use std::{collections::HashMap, rc::Rc, str::FromStr};
use yew::prelude::*;

const LABEL_GROUPING: &str = "LABEL.CHANNEL_GROUPS_GROUPING";
const LABEL_START: &str = "LABEL.CHANNEL_GROUPS_START";
const LABEL_FAILOVER: &str = "LABEL.CHANNEL_GROUPS_FAILOVER";
const LABEL_CIRCUIT: &str = "LABEL.CHANNEL_GROUPS_CIRCUIT";
const INFO_CONFIG: &str = "INFO.CHANNEL_GROUPS_CONFIG";

generate_form_reducer!(
    state: ChannelGroupsConfigFormState { form: ChannelGroupsConfig },
    action_name: ChannelGroupsConfigFormAction,
    fields {
        MatchNormalize => match_normalize: ChannelMatchNormalize,
    }
);

/// One edit of the global channel group settings.
type Edit<T> = fn(&mut ChannelGroupsConfig, T);

/// Applies an edit to the form and marks it modified.
type Apply = dyn Fn(&dyn Fn(&mut ChannelGroupsConfig));

/// An empty value box, so a field without entries looks like the other fields in view mode.
fn empty_value() -> Html {
    html! { <span class="tp__form-field__value"></span> }
}

fn clamp_u8(value: u64) -> u8 { u8::try_from(value).unwrap_or(u8::MAX) }

fn choice<T>(values: &[T], value: T) -> Rc<Vec<DropDownOption>>
where
    T: Copy + PartialEq + ToString,
{
    Rc::new(
        values
            .iter()
            .map(|&option| DropDownOption {
                id: option.to_string(),
                label: html! { option.to_string() },
                selected: option == value,
            })
            .collect(),
    )
}

#[component]
pub fn ChannelGroupsConfigView() -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let config_view_ctx = use_context::<ConfigViewContext>().expect("ConfigViewContext not found");
    let edit_mode = *config_view_ctx.edit_mode;

    let form_state: UseReducerHandle<ChannelGroupsConfigFormState> =
        use_default_form_reducer!(ChannelGroupsConfigFormState { form: ChannelGroupsConfig::default() });

    use_emit_config_form(&form_state, config_view_ctx.on_form_change.clone(), ConfigForm::ChannelGroups);

    {
        let form_state = form_state.clone();
        let channel_groups = config_ctx.config.as_ref().and_then(|c| c.config.channel_groups.clone());
        use_effect_with((channel_groups, edit_mode), move |(channel_groups, _mode)| {
            form_state.dispatch(ChannelGroupsConfigFormAction::SetAll(channel_groups.clone().unwrap_or_default()));
            || ()
        });
    }

    let config = form_state.form.clone();
    let update: Rc<Apply> = {
        let form_state = form_state.clone();
        Rc::new(move |edit| {
            let mut config = form_state.form.clone();
            edit(&mut config);
            form_state.dispatch(ChannelGroupsConfigFormAction::SetAllEdited(config));
        })
    };

    let field = |label: &str, field_id: &str, body: Html| {
        html! {
            <div class="tp__form-field tp__form-field__text">
                <FieldLabel label={translate.t(label)} field_id={field_id.to_string()} />
                { body }
            </div>
        }
    };

    let toggle = |label: &str, field_id: &str, value: bool, edit: Edit<bool>| {
        let apply = Rc::clone(&update);
        html! {
            <div class="tp__form-field tp__form-field__bool">
                <ToggleSwitch value={value} readonly={!edit_mode}
                    on_change={Callback::from(move |value: bool| apply(&|config| edit(config, value)))} />
                <FieldLabel label={translate.t(label)} field_id={field_id.to_string()} />
            </div>
        }
    };

    let number = |label: &str, field_id: &str, value: u64, max: u64, edit: Edit<u64>| {
        if !edit_mode {
            return field(label, field_id, html! { <span class="tp__form-field__value">{ value }</span> });
        }
        let apply = Rc::clone(&update);
        html! {
            <div class="tp__form-field tp__form-field__number">
                <NumberInput
                    label={translate.t(label)}
                    name={field_id.to_string()}
                    field_id={Some(field_id.to_string())}
                    value={i64::try_from(value).ok()}
                    min_i64={Some(0)}
                    max_i64={i64::try_from(max).ok()}
                    on_change={Callback::from(move |value: Option<i64>| {
                        let value = value.and_then(|value| u64::try_from(value).ok()).unwrap_or_default().min(max);
                        apply(&|config| edit(config, value));
                    })}
                />
            </div>
        }
    };

    let select =
        |label: &str, field_id: &str, options: Rc<Vec<DropDownOption>>, shown: String, apply: Callback<String>| {
            let body = if edit_mode {
                html! {
                    <Select name={field_id.to_string()} multi_select={false} options={options}
                        on_select={Callback::from(move |(_, selection): (String, DropDownSelection)| {
                            if let Some(id) = selection_first(&selection) {
                                apply.emit(id.to_string());
                            }
                        })} />
                }
            } else {
                html! { <span class="tp__form-field__value">{ shown }</span> }
            };
            field(label, field_id, body)
        };

    macro_rules! enum_select {
        ($label:expr, $field_id:expr, $ty:ty, $value:expr, $edit:expr) => {{
            let value: $ty = $value;
            let edit: Edit<$ty> = $edit;
            let apply = Rc::clone(&update);
            select(
                $label,
                $field_id,
                choice(<$ty>::ALL, value),
                value.to_string(),
                Callback::from(move |id: String| {
                    if let Ok(value) = <$ty>::from_str(&id) {
                        apply(&|config| edit(config, value));
                    }
                }),
            )
        }};
    }

    let normalize = &config.match_normalize;
    let strip_regex = {
        let form_state = form_state.clone();
        let normalize = normalize.clone();
        let body = if edit_mode {
            html! {
                <Input name="strip_regex" autocomplete={true}
                    value={normalize.strip_regex.clone().unwrap_or_default()}
                    on_change={Callback::from(move |value: String| {
                        let mut normalize = form_state.form.match_normalize.clone();
                        normalize.strip_regex = (!value.trim().is_empty()).then_some(value);
                        form_state.dispatch(ChannelGroupsConfigFormAction::MatchNormalize(normalize));
                    })} />
            }
        } else {
            html! { <span class="tp__form-field__value">{ normalize.strip_regex.clone().unwrap_or_default() }</span> }
        };
        field("LABEL.CHANNEL_GROUPS_STRIP_REGEX", "CHANNEL_MATCH_NORMALIZE.STRIP_REGEX", body)
    };
    let ignore_tokens = {
        let form_state = form_state.clone();
        let tags: Vec<Rc<Tag>> =
            normalize.ignore_tokens.iter().map(|token| Rc::new(Tag { label: token.clone(), class: None })).collect();
        field(
            "LABEL.CHANNEL_GROUPS_IGNORE_TOKENS",
            "CHANNEL_MATCH_NORMALIZE.IGNORE_TOKENS",
            if !edit_mode && tags.is_empty() {
                empty_value()
            } else {
                html! {
                    <TagList tags={tags} readonly={!edit_mode} placeholder={translate.t("LABEL.CHANNEL_GROUPS_IGNORE_TOKEN")}
                        on_change={Callback::from(move |tags: Vec<Rc<Tag>>| {
                            let mut normalize = form_state.form.match_normalize.clone();
                            normalize.ignore_tokens = tags.iter().map(|tag| tag.label.clone()).collect();
                            form_state.dispatch(ChannelGroupsConfigFormAction::MatchNormalize(normalize));
                        })} />
                }
            },
        )
    };
    let aliases = {
        let form_state = form_state.clone();
        let entries: HashMap<String, String> =
            normalize.aliases.iter().map(|(key, value)| (key.clone(), value.clone())).collect();
        field(
            "LABEL.CHANNEL_GROUPS_ALIASES",
            "CHANNEL_MATCH_NORMALIZE.ALIASES",
            if !edit_mode && entries.is_empty() {
                empty_value()
            } else {
                html! {
                    <KeyValueEditor entries={entries} readonly={!edit_mode}
                        key_placeholder={translate.t("LABEL.CHANNEL_GROUPS_ALIAS_NAME")}
                        value_placeholder={translate.t("LABEL.CHANNEL_GROUPS_ALIAS_TARGET")}
                        on_change={Callback::from(move |entries: HashMap<String, String>| {
                            let mut normalize = form_state.form.match_normalize.clone();
                            normalize.aliases = entries.into_iter().collect();
                            form_state.dispatch(ChannelGroupsConfigFormAction::MatchNormalize(normalize));
                        })} />
                }
            },
        )
    };

    let start = config.start;
    let failover = config.failover;
    let circuit = config.circuit;

    html! {
        <div class="tp__channel-groups-config-view tp__config-view-page">
            <div class="tp__config-view-page__title">{ translate.t(LABEL_CHANNEL_GROUPS_CONFIG) }</div>
            <div class="tp__channel-groups-config-view__info">{ translate.t(INFO_CONFIG) }</div>
            <div class="tp__channel-groups-config-view__body tp__config-view-page__body">
                <Card class="tp__config-view__card">
                    <h1>{ translate.t(LABEL_GROUPING) }</h1>
                    { enum_select!("LABEL.CHANNEL_GROUPS_MATCH_BY", "CHANNEL_GROUPS_CONFIG.MATCH_BY", ChannelMatchBy,
                        config.match_by, |config, value| config.match_by = value) }
                    { toggle("LABEL.CHANNEL_GROUPS_MATCH_AS_ASCII", "CHANNEL_GROUPS_CONFIG.MATCH_AS_ASCII",
                        config.match_as_ascii, |config, value| config.match_as_ascii = value) }
                    { strip_regex }
                    { ignore_tokens }
                    { aliases }
                    { enum_select!("LABEL.CHANNEL_GROUPS_EPG_CONFLICT", "CHANNEL_GROUPS_CONFIG.EPG_CONFLICT", EpgConflictMode,
                        config.epg_conflict, |config, value| config.epg_conflict = value) }
                    { toggle("LABEL.CHANNEL_GROUPS_NAME_STRIP_QUALITY", "CHANNEL_GROUPS_CONFIG.NAME_STRIP_QUALITY",
                        config.name_strip_quality, |config, value| config.name_strip_quality = value) }
                    { enum_select!("LABEL.CHANNEL_GROUPS_POLICY", "CHANNEL_GROUPS_CONFIG.POLICY", ChannelGroupPolicy,
                        config.policy, |config, value| config.policy = value) }
                </Card>
                <Card class="tp__config-view__card">
                    <h1>{ translate.t(LABEL_START) }</h1>
                    { toggle("LABEL.CHANNEL_GROUPS_ALLOW_PROVIDER_GRACE", "CHANNEL_GROUPS_CONFIG.ALLOW_PROVIDER_GRACE",
                        config.allow_provider_grace, |config, value| config.allow_provider_grace = value) }
                    { number("LABEL.CHANNEL_GROUPS_START_TIMEOUT_SECS", "CHANNEL_GROUP_START_CONFIG.TIMEOUT_SECS",
                        start.timeout_secs, 3_600, |config, value| config.start.timeout_secs = value) }
                    { number("LABEL.CHANNEL_GROUPS_START_MAX_ATTEMPTS", "CHANNEL_GROUP_START_CONFIG.MAX_ATTEMPTS",
                        u64::from(start.max_attempts), u64::from(u8::MAX),
                        |config, value| config.start.max_attempts = clamp_u8(value.max(1))) }
                </Card>
                <Card class="tp__config-view__card">
                    <h1>{ translate.t(LABEL_FAILOVER) }</h1>
                    { toggle("LABEL.ENABLED", "CHANNEL_GROUP_FAILOVER_CONFIG.ENABLED",
                        failover.enabled, |config, value| config.failover.enabled = value) }
                    { number("LABEL.CHANNEL_GROUPS_NO_DATA_TIMEOUT_SECS", "CHANNEL_GROUP_FAILOVER_CONFIG.NO_DATA_TIMEOUT_SECS",
                        failover.no_data_timeout_secs, 3_600, |config, value| config.failover.no_data_timeout_secs = value) }
                    { number("LABEL.CHANNEL_GROUPS_SLOW_SOURCE_PCT", "CHANNEL_GROUP_FAILOVER_CONFIG.SLOW_SOURCE_PCT",
                        u64::from(failover.slow_source_pct), 100, |config, value| config.failover.slow_source_pct = clamp_u8(value)) }
                    { number("LABEL.CHANNEL_GROUPS_SLOW_SOURCE_SECS", "CHANNEL_GROUP_FAILOVER_CONFIG.SLOW_SOURCE_SECS",
                        failover.slow_source_secs, 3_600, |config, value| config.failover.slow_source_secs = value) }
                    { number("LABEL.CHANNEL_GROUPS_MAX_OUTAGE_SECS", "CHANNEL_GROUP_FAILOVER_CONFIG.MAX_OUTAGE_SECS",
                        failover.max_outage_secs, 3_600, |config, value| config.failover.max_outage_secs = value.max(1)) }
                    { number("LABEL.CHANNEL_GROUPS_SAME_SOURCE_RETRIES", "CHANNEL_GROUP_FAILOVER_CONFIG.SAME_SOURCE_RETRIES",
                        u64::from(failover.same_source_retries), u64::from(u8::MAX),
                        |config, value| config.failover.same_source_retries = clamp_u8(value)) }
                    { number("LABEL.CHANNEL_GROUPS_STABLE_AFTER_SECS", "CHANNEL_GROUP_FAILOVER_CONFIG.STABLE_AFTER_SECS",
                        failover.stable_after_secs, 86_400, |config, value| config.failover.stable_after_secs = value) }
                    { number("LABEL.CHANNEL_GROUPS_MAX_SWITCHES", "CHANNEL_GROUP_FAILOVER_CONFIG.MAX_SWITCHES",
                        u64::from(failover.max_switches), u64::from(u8::MAX),
                        |config, value| config.failover.max_switches = clamp_u8(value)) }
                    { number("LABEL.CHANNEL_GROUPS_MAX_SWITCHES_WINDOW_SECS", "CHANNEL_GROUP_FAILOVER_CONFIG.MAX_SWITCHES_WINDOW_SECS",
                        failover.max_switches_window_secs, 86_400, |config, value| config.failover.max_switches_window_secs = value) }
                    { enum_select!("LABEL.CHANNEL_GROUPS_AUDIO_LANGUAGE", "CHANNEL_GROUP_FAILOVER_CONFIG.AUDIO_LANGUAGE",
                        RuntimeAudioLanguage, failover.audio_language, |config, value| config.failover.audio_language = value) }
                </Card>
                <Card class="tp__config-view__card">
                    <h1>{ translate.t(LABEL_CIRCUIT) }</h1>
                    { number("LABEL.CHANNEL_GROUPS_SOURCE_FAILURES", "CHANNEL_GROUP_CIRCUIT_CONFIG.SOURCE_FAILURES",
                        u64::from(circuit.source_failures), u64::from(u8::MAX),
                        |config, value| config.circuit.source_failures = clamp_u8(value.max(1))) }
                    { number("LABEL.CHANNEL_GROUPS_COOLDOWN_SECS", "CHANNEL_GROUP_CIRCUIT_CONFIG.COOLDOWN_SECS",
                        circuit.cooldown_secs, 86_400, |config, value| config.circuit.cooldown_secs = value) }
                    { number("LABEL.CHANNEL_GROUPS_COOLDOWN_MAX_SECS", "CHANNEL_GROUP_CIRCUIT_CONFIG.COOLDOWN_MAX_SECS",
                        circuit.cooldown_max_secs, 86_400, |config, value| config.circuit.cooldown_max_secs = value) }
                    { number("LABEL.CHANNEL_GROUPS_INPUT_FAILURES", "CHANNEL_GROUP_CIRCUIT_CONFIG.INPUT_FAILURES",
                        u64::from(circuit.input_failures), u64::from(u8::MAX),
                        |config, value| config.circuit.input_failures = clamp_u8(value.max(2))) }
                    { number("LABEL.CHANNEL_GROUPS_INPUT_WINDOW_SECS", "CHANNEL_GROUP_CIRCUIT_CONFIG.INPUT_WINDOW_SECS",
                        circuit.input_window_secs, 86_400, |config, value| config.circuit.input_window_secs = value) }
                </Card>
            </div>
        </div>
    }
}
