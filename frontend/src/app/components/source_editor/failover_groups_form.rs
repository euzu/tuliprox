//! Failover group page of a target: the enable switch and the overrides of the global settings.

use crate::{
    app::{
        components::{select::Select, selection_first, DropDownOption, DropDownSelection, FieldLabel, ToggleSwitch},
        ConfigContext,
    },
    i18n::use_translation,
};
use shared::model::{
    ChannelMatchBy, EpgConflictMode, FailoverGroupPolicy, FailoverGroupsConfig, FailoverGroupsTargetConfig,
};
use std::{rc::Rc, str::FromStr};
use yew::{component, html, use_context, Callback, Html, Properties};

const LABEL_ENABLED: &str = "LABEL.ENABLED";
const LABEL_DISABLED: &str = "LABEL.DISABLED";
const LABEL_GLOBAL: &str = "LABEL.FAILOVER_GROUPS_GLOBAL";
const INFO_TARGET: &str = "INFO.FAILOVER_GROUPS_TARGET";

/// Select id of the entry that keeps the global value.
const GLOBAL: &str = "global";

/// The target block after the enable toggle; a block without settings is dropped so the target
/// YAML stays clean.
pub(super) fn toggle_failover_groups(
    current: Option<&FailoverGroupsTargetConfig>,
    enabled: bool,
) -> Option<FailoverGroupsTargetConfig> {
    edit_failover_groups(current, |config| config.enabled = enabled)
}

/// The target block with one setting changed.
pub(super) fn edit_failover_groups(
    current: Option<&FailoverGroupsTargetConfig>,
    edit: impl FnOnce(&mut FailoverGroupsTargetConfig),
) -> Option<FailoverGroupsTargetConfig> {
    let mut config = current.copied().unwrap_or_default();
    edit(&mut config);
    (!config.is_default()).then_some(config)
}

/// The override a select entry stands for; the global entry is `None`.
fn parse_override<T: FromStr>(id: &str) -> Option<T> {
    if id == GLOBAL {
        None
    } else {
        id.parse().ok()
    }
}

/// One override setting: its values, how they read and where they live in the target block.
struct OverrideField<T: 'static> {
    label: &'static str,
    field_id: &'static str,
    values: &'static [T],
    get: fn(&FailoverGroupsTargetConfig) -> Option<T>,
    set: fn(&mut FailoverGroupsTargetConfig, Option<T>),
    global: fn(&FailoverGroupsConfig) -> T,
}

#[derive(Properties, PartialEq)]
pub(super) struct TargetFailoverGroupsViewProps {
    pub config: Option<FailoverGroupsTargetConfig>,
    pub readonly: bool,
    pub on_change: Callback<Option<FailoverGroupsTargetConfig>>,
}

#[component]
pub(super) fn TargetFailoverGroupsView(props: &TargetFailoverGroupsViewProps) -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>();
    let global = config_ctx
        .as_ref()
        .and_then(|ctx| ctx.config.as_ref())
        .and_then(|app_config| app_config.config.failover_groups.clone())
        .unwrap_or_default();
    let current = props.config.unwrap_or_default();
    let readonly = props.readonly;

    let on_toggle = {
        let on_change = props.on_change.clone();
        let config = props.config;
        Callback::from(move |enabled: bool| on_change.emit(toggle_failover_groups(config.as_ref(), enabled)))
    };

    let bool_label = |value: bool| translate.t(if value { LABEL_ENABLED } else { LABEL_DISABLED });
    let global_label = translate.t(LABEL_GLOBAL);

    let render = |label: &str,
                  field_id: &str,
                  options: Vec<DropDownOption>,
                  shown: String,
                  on_select: Callback<(String, DropDownSelection)>| {
        html! {
            <div class="tp__form-field tp__form-field__text">
                <FieldLabel label={translate.t(label)} field_id={field_id.to_string()} />
                if readonly {
                    <span class="tp__form-field__value">{ shown }</span>
                } else {
                    <Select name={field_id.to_string()} multi_select={false} on_select={on_select} options={Rc::new(options)} />
                }
            </div>
        }
    };

    macro_rules! override_row {
        ($field:expr, $label_of:expr) => {{
            let field = $field;
            let label_of = $label_of;
            let value = (field.get)(&current);
            let global_value = (field.global)(&global);
            let global_entry = format!("{global_label} ({})", label_of(global_value));
            let shown = value.map_or_else(|| global_entry.clone(), |value| label_of(value));
            let mut options = vec![DropDownOption {
                id: GLOBAL.to_string(),
                label: html! { global_entry },
                selected: value.is_none(),
            }];
            options.extend(field.values.iter().map(|&option| DropDownOption {
                id: option.to_string(),
                label: html! { label_of(option) },
                selected: value == Some(option),
            }));
            let on_select = {
                let on_change = props.on_change.clone();
                let config = props.config;
                let set = field.set;
                Callback::from(move |(_, selection): (String, DropDownSelection)| {
                    let Some(id) = selection_first(&selection) else { return };
                    let value = parse_override(id);
                    on_change.emit(edit_failover_groups(config.as_ref(), |config| set(config, value)));
                })
            };
            render(field.label, field.field_id, options, shown, on_select)
        }};
    }

    let config_name = |value: &dyn ToString| value.to_string();
    let rows = html! {
        <>
            { override_row!(OverrideField::<ChannelMatchBy> {
                label: "LABEL.FAILOVER_GROUPS_MATCH_BY",
                field_id: "FAILOVER_GROUPS_CONFIG.MATCH_BY",
                values: ChannelMatchBy::ALL,
                get: |config| config.match_by,
                set: |config, value| config.match_by = value,
                global: |global| global.match_by,
            }, |value: ChannelMatchBy| config_name(&value)) }
            { override_row!(OverrideField::<bool> {
                label: "LABEL.FAILOVER_GROUPS_MATCH_AS_ASCII",
                field_id: "FAILOVER_GROUPS_CONFIG.MATCH_AS_ASCII",
                values: &[true, false],
                get: |config| config.match_as_ascii,
                set: |config, value| config.match_as_ascii = value,
                global: |global| global.match_as_ascii,
            }, bool_label) }
            { override_row!(OverrideField::<EpgConflictMode> {
                label: "LABEL.FAILOVER_GROUPS_EPG_CONFLICT",
                field_id: "FAILOVER_GROUPS_CONFIG.EPG_CONFLICT",
                values: EpgConflictMode::ALL,
                get: |config| config.epg_conflict,
                set: |config, value| config.epg_conflict = value,
                global: |global| global.epg_conflict,
            }, |value: EpgConflictMode| config_name(&value)) }
            { override_row!(OverrideField::<bool> {
                label: "LABEL.FAILOVER_GROUPS_NAME_STRIP_QUALITY",
                field_id: "FAILOVER_GROUPS_CONFIG.NAME_STRIP_QUALITY",
                values: &[true, false],
                get: |config| config.name_strip_quality,
                set: |config, value| config.name_strip_quality = value,
                global: |global| global.name_strip_quality,
            }, bool_label) }
            { override_row!(OverrideField::<FailoverGroupPolicy> {
                label: "LABEL.FAILOVER_GROUPS_POLICY",
                field_id: "FAILOVER_GROUPS_CONFIG.POLICY",
                values: FailoverGroupPolicy::ALL,
                get: |config| config.policy,
                set: |config, value| config.policy = value,
                global: |global| global.policy,
            }, |value: FailoverGroupPolicy| config_name(&value)) }
            { override_row!(OverrideField::<bool> {
                label: "LABEL.FAILOVER_GROUPS_ALLOW_PROVIDER_GRACE",
                field_id: "FAILOVER_GROUPS_CONFIG.ALLOW_PROVIDER_GRACE",
                values: &[true, false],
                get: |config| config.allow_provider_grace,
                set: |config, value| config.allow_provider_grace = value,
                global: |global| global.allow_provider_grace,
            }, bool_label) }
            { override_row!(OverrideField::<bool> {
                label: "LABEL.FAILOVER_GROUPS_RUNTIME",
                field_id: "FAILOVER_GROUP_RUNTIME_CONFIG.ENABLED",
                values: &[true, false],
                get: |config| config.failover,
                set: |config, value| config.failover = value,
                global: |global| global.failover.enabled,
            }, bool_label) }
        </>
    };

    html! {
        <div class="tp__target-options">
            <div class="tp__target-options__hint">{ translate.t(INFO_TARGET) }</div>
            <div class="tp__form-field tp__form-field__bool tp__target-options__toggle-row">
                <ToggleSwitch value={current.enabled} readonly={readonly} on_change={on_toggle} />
                <FieldLabel label={translate.t(LABEL_ENABLED)} field_id={"FAILOVER_GROUPS_TARGET_CONFIG.ENABLED".to_string()} />
            </div>
            <div class="tp__target-options__children">{ rows }</div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::{edit_failover_groups, parse_override, toggle_failover_groups, GLOBAL};
    use shared::model::{ChannelMatchBy, FailoverGroupsTargetConfig};

    #[test]
    fn enabling_creates_a_block_without_overrides() {
        let config = toggle_failover_groups(None, true).expect("block");
        assert_eq!(config, FailoverGroupsTargetConfig { enabled: true, ..FailoverGroupsTargetConfig::default() });
    }

    #[test]
    fn disabling_drops_a_block_without_overrides() {
        let enabled = FailoverGroupsTargetConfig { enabled: true, ..FailoverGroupsTargetConfig::default() };
        assert_eq!(toggle_failover_groups(Some(&enabled), false), None);
    }

    #[test]
    fn disabling_keeps_overrides() {
        let customized = FailoverGroupsTargetConfig {
            enabled: true,
            match_by: Some(ChannelMatchBy::Name),
            ..FailoverGroupsTargetConfig::default()
        };
        let disabled = toggle_failover_groups(Some(&customized), false).expect("overrides kept");
        assert!(!disabled.enabled);
        assert_eq!(disabled.match_by, Some(ChannelMatchBy::Name));
    }

    #[test]
    fn editing_changes_only_the_edited_override() {
        let current = FailoverGroupsTargetConfig {
            enabled: true,
            failover: Some(false),
            ..FailoverGroupsTargetConfig::default()
        };
        let edited = edit_failover_groups(Some(&current), |config| config.match_by = Some(ChannelMatchBy::Title))
            .expect("block");
        assert_eq!((edited.failover, edited.match_by), (Some(false), Some(ChannelMatchBy::Title)));
        assert!(edited.enabled);
    }

    #[test]
    fn global_entry_clears_the_override() {
        assert_eq!(parse_override::<ChannelMatchBy>(GLOBAL), None);
        assert_eq!(parse_override::<ChannelMatchBy>("epg_channel_id"), Some(ChannelMatchBy::EpgChannelId));
        assert_eq!(parse_override::<bool>("false"), Some(false));
    }
}
