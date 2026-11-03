use crate::{
    app::{
        components::{
            config::{
                config_page::{ConfigForm, ConfigPage},
                config_view_context::ConfigViewContext,
                use_emit_mapped,
            },
            Card, Chip, RadioButtonGroup,
        },
        ConfigContext,
    },
    config_field, config_field_child, edit_field_number_f64, edit_field_number_u64, generate_form_reducer,
    i18n::use_translation,
};
use shared::model::{DiskAlertConfigDto, MessagingConfigDto};
use std::rc::Rc;
use yew::prelude::*;

const LABEL_NOTIFY_ON: &str = "LABEL.NOTIFY_ON";
const LABEL_DISK_ALERT: &str = "LABEL.DISK_ALERT";
const LABEL_DISK_ALERT_WARN_PERCENT: &str = "LABEL.DISK_ALERT_WARN_PERCENT";
const LABEL_DISK_ALERT_CRITICAL_PERCENT: &str = "LABEL.DISK_ALERT_CRITICAL_PERCENT";
const LABEL_DISK_ALERT_REPEAT_INTERVAL_SECS: &str = "LABEL.DISK_ALERT_REPEAT_INTERVAL_SECS";

generate_form_reducer!(
    state: DiskAlertConfigFormState { form: DiskAlertConfigDto },
    action_name: DiskAlertConfigFormAction,
    fields {
        WarnPercent => warn_percent: f64,
        CriticalPercent => critical_percent: f64,
        RepeatIntervalSecs => repeat_interval_secs: u64,
    }
);

generate_form_reducer!(
    state: MessagingConfigFormState { form: MessagingConfigDto },
    action_name: MessagingConfigFormAction,
    fields {
        NotifyOn => notify_on: Vec<String>,
    }
);

/// Uppercased, underscore-separated form of an event id, for i18n keys and
/// field ids. `recording.completed` becomes `RECORDING_COMPLETED`, which is
/// also the legacy `MsgKind` suffix - so existing translations still resolve.
fn event_label_suffix(event: &str) -> String { event.replace('.', "_").to_uppercase() }

fn event_label_key(event: &str) -> String { format!("LABEL.MSG_KIND_{}", event_label_suffix(event)) }

#[component]
pub fn MessagingConfigView() -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let config_view_ctx = use_context::<ConfigViewContext>().expect("ConfigViewContext not found");

    let disk_alert_state =
        use_reducer(|| DiskAlertConfigFormState { form: DiskAlertConfigDto::default(), modified: false });

    let messaging_state =
        use_reducer(|| MessagingConfigFormState { form: MessagingConfigDto::default(), modified: false });

    // Driven by the event registry rather than a hardcoded list, so an
    // event added in the backend shows up here without a frontend change.
    let notify_on_options = use_memo((), |()| {
        shared::model::notification::registry::ALL
            .iter()
            .map(|descriptor| descriptor.id.as_str().to_string())
            .collect::<Vec<String>>()
    });

    let notify_on_labels = use_memo((notify_on_options.clone(), translate.clone()), |(options, tr)| {
        options.iter().map(|t| tr.t(&event_label_key(t))).collect::<Vec<String>>()
    });

    let notify_on_options_text = notify_on_options.clone();

    {
        let dependencies = (
            messaging_state.form.clone(),
            disk_alert_state.form.clone(),
            messaging_state.modified,
            disk_alert_state.modified,
        );
        use_emit_mapped(dependencies, config_view_ctx.on_form_change.clone(), |(m, da, mm, dam)| {
            let mut form = m;
            // The `DiskAlert` entry in `notify_on` is the on/off switch
            // for disk alerts: the runtime only fires alerts when
            // `notify_on` contains `DiskAlert` AND a `disk_alert` block
            // exists. Keep both in sync so checking the chip enables
            // alerts (default thresholds when untouched) and unchecking
            // removes the block.
            let subscribed = shared::model::notification::EventSubscription::parse(form.notify_on.iter())
                .matches(shared::model::notification::registry::SYSTEM_DISK_ALERT);
            form.disk_alert = if subscribed { Some(da) } else { None };

            let modified = mm || dam;
            ConfigForm::Messaging(modified, form)
        });
    }

    {
        let msg_state = messaging_state.clone();

        let msg_config: MessagingConfigDto = config_ctx
            .config
            .as_ref()
            .and_then(|c| c.config.messaging.as_ref())
            .map_or_else(MessagingConfigDto::default, std::clone::Clone::clone);

        let disk_alert_state = disk_alert_state.clone();
        let disk_alert_cfg =
            msg_config.disk_alert.as_ref().map_or_else(DiskAlertConfigDto::default, std::clone::Clone::clone);
        use_effect_with((disk_alert_cfg, *config_view_ctx.edit_mode), move |(disk_alert_cfg, _mode)| {
            disk_alert_state.dispatch(DiskAlertConfigFormAction::SetAll(disk_alert_cfg.clone()));
            || ()
        });

        use_effect_with((msg_config, *config_view_ctx.edit_mode), move |(msg_config, _mode)| {
            msg_state.dispatch(MessagingConfigFormAction::SetAll(msg_config.clone()));
            || ()
        });
    }

    let render_disk_alert = |disk_alert: Option<&DiskAlertConfigDto>| match disk_alert {
        Some(disk_alert) => html! {
            <Card class="tp__config-view__card">
                <h1>{translate.t(LABEL_DISK_ALERT)}</h1>
                { config_field!(disk_alert, translate.t(LABEL_DISK_ALERT_WARN_PERCENT), warn_percent) }
                { config_field!(disk_alert, translate.t(LABEL_DISK_ALERT_CRITICAL_PERCENT), critical_percent) }
                { config_field!(disk_alert, translate.t(LABEL_DISK_ALERT_REPEAT_INTERVAL_SECS), repeat_interval_secs) }
            </Card>
        },
        None => html! {},
    };

    let render_view_mode = || {
        let msg_state = messaging_state.clone();
        let notify_on_chips = notify_on_options
            .iter()
            .map(|t| {
                let is_selected = msg_state.form.notify_on.contains(t);
                let class = if is_selected { "tp__text-button primary" } else { "tp__text-button" };
                let label = translate.t(&event_label_key(t));
                html! { <Chip {label} class={class}/> }
            })
            .collect::<Html>();
        html! {
          <>
        <div class="tp__messaging-config-view__header tp__config-view-page__header">
          { config_field_child!(translate.t(LABEL_NOTIFY_ON), "MESSAGING_CONFIG.NOTIFY_ON", {
             html! { <div class="tp__messaging-config-view__notify-on">
                 { notify_on_chips }
                </div>
              }
          })}
        </div>
        <div class="tp__messaging-config-view__body tp__config-view-page__body">
          {render_disk_alert(msg_state.form.disk_alert.as_ref())}
        </div>
        </>
        }
    };

    let render_edit_mode = || {
        let msg_state = messaging_state.clone();
        let notify_on_selections = Rc::new(msg_state.form.notify_on.clone());
        html! {
            <>
            <div class="tp__messaging-config-view__header tp__config-view-page__header">
                { config_field_child!(translate.t(LABEL_NOTIFY_ON), "MESSAGING_CONFIG.NOTIFY_ON", {
                   let dispatch_handle = msg_state.clone();
                   html! { <RadioButtonGroup
                        multi_select={true} none_allowed={true} wrap={true}
                        labels={Some(notify_on_labels.clone())}
                        on_select={Callback::from(move |selections: Rc<Vec<String>>| {
                            dispatch_handle.dispatch(MessagingConfigFormAction::NotifyOn(
                                selections.as_ref().clone()));
                        })}
                        options={notify_on_options_text.clone()}
                        selected={notify_on_selections}
                    />
                }})}
            </div>
            <div class="tp__messaging-config-view__body tp__config-view-page__body">
                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_DISK_ALERT)}</h1>
                    { edit_field_number_f64!(disk_alert_state, translate.t(LABEL_DISK_ALERT_WARN_PERCENT), warn_percent, DiskAlertConfigFormAction::WarnPercent) }
                    { edit_field_number_f64!(disk_alert_state, translate.t(LABEL_DISK_ALERT_CRITICAL_PERCENT), critical_percent, DiskAlertConfigFormAction::CriticalPercent) }
                    { edit_field_number_u64!(disk_alert_state, translate.t(LABEL_DISK_ALERT_REPEAT_INTERVAL_SECS), repeat_interval_secs, DiskAlertConfigFormAction::RepeatIntervalSecs) }
                </Card>
            </div>
            </>
        }
    };

    html! {
        <div class="tp__messaging-config_view tp__config-view-page">
           <div class="tp__config-view-page__title">{translate.t(ConfigPage::Messaging.title_key())}</div>
            { if *config_view_ctx.edit_mode { render_edit_mode() } else { render_view_mode() } }
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::{event_label_key, event_label_suffix};
    use shared::model::notification::registry;

    #[test]
    fn event_label_suffix_formats_dotted_ids_to_upper_snake() {
        assert_eq!(event_label_suffix("system.info"), "SYSTEM_INFO");
        assert_eq!(event_label_suffix("recording.completed"), "RECORDING_COMPLETED");
        assert_eq!(event_label_suffix("provider.account.status_changed"), "PROVIDER_ACCOUNT_STATUS_CHANGED");
    }

    #[test]
    fn all_notification_event_keys_exist_in_every_locale() {
        const LOCALES: [(&str, &str); 3] = [
            ("en", include_str!("../../../../public/assets/i18n/en.json")),
            ("ar", include_str!("../../../../public/assets/i18n/ar.json")),
            ("ru", include_str!("../../../../public/assets/i18n/ru.json")),
        ];

        for (locale, json) in LOCALES {
            let parsed = serde_json::from_str::<serde_json::Value>(json);
            assert!(parsed.is_ok(), "{locale} locale must be valid JSON");
            let Ok(parsed) = parsed else { continue };

            for descriptor in registry::ALL {
                let full_key = event_label_key(descriptor.id.as_str());
                let value = full_key.split('.').try_fold(&parsed, |value, segment| value.get(segment));
                assert!(
                    value.and_then(serde_json::Value::as_str).is_some_and(|text| !text.trim().is_empty()),
                    "{locale} locale is missing non-empty translation key {full_key}"
                );
            }
        }
    }
}
