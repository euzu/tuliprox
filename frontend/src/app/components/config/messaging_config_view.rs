use crate::{
    app::{
        components::{
            config::{
                config_page::{ConfigForm, LABEL_MESSAGING_CONFIG},
                config_view_context::ConfigViewContext,
                use_emit_mapped,
            },
            Card, Chip, RadioButtonGroup, TextArea,
        },
        ConfigContext,
    },
    config_field, config_field_bool, config_field_bool_empty, config_field_child, config_field_custom,
    config_field_empty, config_field_hide, config_field_optional, config_field_tags, edit_field_bool, edit_field_list,
    edit_field_number_f64, edit_field_number_u64, edit_field_text, edit_field_text_option, generate_form_reducer,
    i18n::use_translation,
};
use shared::model::{
    DiscordMessagingConfigDto, DiskAlertConfigDto, MessagingConfigDto, PushoverMessagingConfigDto,
    RestMessagingConfigDto, TelegramMessagingConfigDto,
};
use std::rc::Rc;
use yew::prelude::*;

const LABEL_NOTIFY_ON: &str = "LABEL.NOTIFY_ON";
const LABEL_TELEGRAM: &str = "LABEL.TELEGRAM";
const LABEL_PUSHOVER: &str = "LABEL.PUSHOVER";
const LABEL_REST: &str = "LABEL.REST";
const LABEL_BOT_TOKEN: &str = "LABEL.BOT_TOKEN";
const LABEL_CHAT_IDS: &str = "LABEL.CHAT_IDS";
const LABEL_MARKDOWN: &str = "LABEL.MARKDOWN";
const LABEL_URL: &str = "LABEL.URL";
const LABEL_TOKEN: &str = "LABEL.TOKEN";
const LABEL_USER: &str = "LABEL.USER";
const LABEL_DISCORD: &str = "LABEL.DISCORD";
const LABEL_METHOD: &str = "LABEL.METHOD";
const LABEL_HEADERS: &str = "LABEL.HEADERS";
const LABEL_WEBHOOK_URL: &str = "LABEL.WEBHOOK_URL";
const LABEL_ADD_HEADER: &str = "LABEL.ADD_HEADER";
const LABEL_DISK_ALERT: &str = "LABEL.DISK_ALERT";
const LABEL_DISK_ALERT_WARN_PERCENT: &str = "LABEL.DISK_ALERT_WARN_PERCENT";
const LABEL_DISK_ALERT_CRITICAL_PERCENT: &str = "LABEL.DISK_ALERT_CRITICAL_PERCENT";
const LABEL_DISK_ALERT_REPEAT_INTERVAL_SECS: &str = "LABEL.DISK_ALERT_REPEAT_INTERVAL_SECS";

generate_form_reducer!(
    state: TelegramMessagingConfigFormState { form: TelegramMessagingConfigDto },
    action_name: TelegramMessagingConfigFormAction,
    fields {
        BotToken => bot_token: String,
        ChatIds => chat_ids: Vec<String>,
        Markdown => markdown: bool,
        Templates => templates: std::collections::HashMap<String, String>,
    }
);

generate_form_reducer!(
    state: RestMessagingConfigFormState { form: RestMessagingConfigDto },
    action_name: RestMessagingConfigFormAction,
    fields {
        Url => url: String,
        Method => method: Option<String>,
        Headers => headers: Vec<String>,
        Templates => templates: std::collections::HashMap<String, String>,
    }
);

generate_form_reducer!(
    state: DiscordMessagingConfigFormState { form: DiscordMessagingConfigDto },
    action_name: DiscordMessagingConfigFormAction,
    fields {
        Url => url: String,
        Templates => templates: std::collections::HashMap<String, String>,
    }
);

generate_form_reducer!(
    state: PushoverMessagingConfigFormState { form: PushoverMessagingConfigDto },
    action_name: PushoverMessagingConfigFormAction,
    fields {
        Url => url: Option<String>,
        Token => token: String,
        User => user: String,
    }
);

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

    let telegram_state = use_reducer(|| TelegramMessagingConfigFormState {
        form: TelegramMessagingConfigDto::default(),
        modified: false,
    });
    let rest_state =
        use_reducer(|| RestMessagingConfigFormState { form: RestMessagingConfigDto::default(), modified: false });

    let pushover_state = use_reducer(|| PushoverMessagingConfigFormState {
        form: PushoverMessagingConfigDto::default(),
        modified: false,
    });

    let discord_state =
        use_reducer(|| DiscordMessagingConfigFormState { form: DiscordMessagingConfigDto::default(), modified: false });

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

    let notify_on_options_text = notify_on_options.clone();

    {
        let dependencies = (
            messaging_state.form.clone(),
            telegram_state.form.clone(),
            rest_state.form.clone(),
            pushover_state.form.clone(),
            discord_state.form.clone(),
            disk_alert_state.form.clone(),
            messaging_state.modified,
            telegram_state.modified,
            rest_state.modified,
            pushover_state.modified,
            discord_state.modified,
            disk_alert_state.modified,
        );
        use_emit_mapped(
            dependencies,
            config_view_ctx.on_form_change.clone(),
            |(m, t, r, p, d, da, mm, tm, rm, pm, dm, dam)| {
                let mut form = m;
                form.telegram = Some(t);
                form.rest = Some(r);
                form.pushover = Some(p);
                form.discord = Some(d);
                // The `DiskAlert` entry in `notify_on` is the on/off switch
                // for disk alerts: the runtime only fires alerts when
                // `notify_on` contains `DiskAlert` AND a `disk_alert` block
                // exists. Keep both in sync so checking the chip enables
                // alerts (default thresholds when untouched) and unchecking
                // removes the block.
                let subscribed = shared::model::notification::EventSubscription::parse(form.notify_on.iter())
                    .matches(shared::model::notification::registry::SYSTEM_DISK_ALERT);
                form.disk_alert = if subscribed { Some(da) } else { None };

                let modified = mm || tm || rm || pm || dm || dam;
                ConfigForm::Messaging(modified, form)
            },
        );
    }

    {
        let msg_state = messaging_state.clone();
        let t_state = telegram_state.clone();
        let p_state = pushover_state.clone();
        let r_state = rest_state.clone();

        let msg_config: MessagingConfigDto = config_ctx
            .config
            .as_ref()
            .and_then(|c| c.config.messaging.as_ref())
            .map_or_else(MessagingConfigDto::default, std::clone::Clone::clone);

        let telegram_cfg =
            msg_config.telegram.as_ref().map_or_else(TelegramMessagingConfigDto::default, std::clone::Clone::clone);
        use_effect_with((telegram_cfg, *config_view_ctx.edit_mode), move |(telegram_cfg, _mode)| {
            t_state.dispatch(TelegramMessagingConfigFormAction::SetAll(telegram_cfg.clone()));
            || ()
        });

        let rest_cfg = msg_config.rest.as_ref().map_or_else(RestMessagingConfigDto::default, std::clone::Clone::clone);
        use_effect_with((rest_cfg, *config_view_ctx.edit_mode), move |(rest_cfg, _mode)| {
            r_state.dispatch(RestMessagingConfigFormAction::SetAll(rest_cfg.clone()));
            || ()
        });

        let pushover_cfg =
            msg_config.pushover.as_ref().map_or_else(PushoverMessagingConfigDto::default, std::clone::Clone::clone);
        use_effect_with((pushover_cfg, *config_view_ctx.edit_mode), move |(pushover_cfg, _mode)| {
            p_state.dispatch(PushoverMessagingConfigFormAction::SetAll(pushover_cfg.clone()));
            || ()
        });

        let discord_state = discord_state.clone();
        let discord_cfg =
            msg_config.discord.as_ref().map_or_else(DiscordMessagingConfigDto::default, std::clone::Clone::clone);
        use_effect_with((discord_cfg, *config_view_ctx.edit_mode), move |(discord_cfg, _mode)| {
            discord_state.dispatch(DiscordMessagingConfigFormAction::SetAll(discord_cfg.clone()));
            || ()
        });

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

    let render_templates_view = |templates: &std::collections::HashMap<String, String>| {
        if templates.is_empty() {
            html! {}
        } else {
            let template_fields = templates
                .iter()
                .map(|(kind, template)| config_field_custom!(translate.t(&event_label_key(kind)), template.clone()))
                .collect::<Html>();
            html! {
                <div class="tp__messaging-config__templates-view">
                    <h3>{translate.t("LABEL.TEMPLATES")}</h3>
                    { template_fields }
                </div>
            }
        }
    };

    let render_telegram = |telegram: Option<&TelegramMessagingConfigDto>| match telegram {
        Some(entry) => html! {
          <Card class="tp__config-view__card">
              <h1>{translate.t("LABEL.TELEGRAM")}</h1>
              { config_field_hide!(entry, translate.t(LABEL_BOT_TOKEN), bot_token) }
              { config_field_tags!(entry, translate.t(LABEL_CHAT_IDS), chat_ids, "MESSAGING_CONFIG.TELEGRAM_CHAT_IDS") }
             { config_field_bool!(entry, translate.t(LABEL_MARKDOWN), markdown) }
             { render_templates_view(&entry.templates) }
          </Card>
        },
        None => html! {
          <Card class="tp__config-view__card">
             <h1>{translate.t(LABEL_TELEGRAM)}</h1>
             { config_field_empty!(translate.t(LABEL_BOT_TOKEN), "TELEGRAM_CONFIG.BOT_TOKEN") }
             { config_field_empty!(translate.t(LABEL_CHAT_IDS), "MESSAGING_CONFIG.TELEGRAM_CHAT_IDS") }
             { config_field_bool_empty!(translate.t(LABEL_MARKDOWN), "TELEGRAM_CONFIG.MARKDOWN") }
          </Card>
        },
    };

    let render_rest = |rest: Option<&RestMessagingConfigDto>| match rest {
        Some(entry) => html! {
          <Card class="tp__config-view__card">
              <h1>{translate.t(LABEL_REST)}</h1>
              { config_field!(entry, translate.t(LABEL_URL), url) }
              { config_field_optional!(entry, translate.t(LABEL_METHOD), method) }
              { config_field_tags!(entry, translate.t(LABEL_HEADERS), headers, "MESSAGING_CONFIG.REST_HEADERS") }
              { render_templates_view(&entry.templates) }
          </Card>
        },
        None => html! {
          <Card class="tp__config-view__card">
              <h1>{translate.t(LABEL_REST)}</h1>
              { config_field_empty!(translate.t(LABEL_URL), "REST_MESSAGING_CONFIG.URL") }
          </Card>
        },
    };

    let render_discord = |discord: Option<&DiscordMessagingConfigDto>| match discord {
        Some(entry) => html! {
          <Card class="tp__config-view__card">
              <h1>{translate.t(LABEL_DISCORD)}</h1>
              { config_field_hide!(entry, translate.t(LABEL_WEBHOOK_URL), url) }
              { render_templates_view(&entry.templates) }
          </Card>
        },
        None => html! {
          <Card class="tp__config-view__card">
              <h1>{translate.t(LABEL_DISCORD)}</h1>
              { config_field_empty!(translate.t(LABEL_WEBHOOK_URL), "DISCORD_MESSAGING_CONFIG.URL") }
          </Card>
        },
    };

    let render_pushover = |pushover: Option<&PushoverMessagingConfigDto>| match pushover {
        Some(entry) => html! {
        <Card class="tp__config-view__card">
            <h1>{translate.t(LABEL_PUSHOVER)}</h1>
            { config_field_optional!(entry, translate.t(LABEL_URL), url) }
            { config_field_hide!(entry, translate.t(LABEL_TOKEN), token) }
            { config_field!(entry, translate.t(LABEL_USER), user) }
        </Card>
        },
        None => html! {
          <Card class="tp__config-view__card">
              <h1>{translate.t(LABEL_PUSHOVER)}</h1>
              { config_field_empty!(translate.t(LABEL_URL), "PUSHOVER_MESSAGING_CONFIG.URL") }
              { config_field_empty!(translate.t(LABEL_TOKEN), "PUSHOVER_MESSAGING_CONFIG.TOKEN") }
              { config_field_empty!(translate.t(LABEL_USER), "PUSHOVER_MESSAGING_CONFIG.USER") }
          </Card>
        },
    };

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
                html! { <Chip label={t.clone()} class={class}/> }
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
          {render_telegram(msg_state.form.telegram.as_ref())}
          {render_rest(msg_state.form.rest.as_ref())}
          {render_pushover(msg_state.form.pushover.as_ref())}
          {render_discord(msg_state.form.discord.as_ref())}
          {render_disk_alert(msg_state.form.disk_alert.as_ref())}
        </div>
        </>
        }
    };

    let render_edit_mode = || {
        let msg_state = messaging_state.clone();
        let notify_on_selections = Rc::new(msg_state.form.notify_on.clone());
        let telegram_template_fields = notify_on_options
            .iter()
            .map(|kind| {
                let kind_str = translate.t(&event_label_key(kind));
                let current_val = telegram_state.form.templates.get(kind).cloned().unwrap_or_default();
                let telegram_state = telegram_state.clone();
                let kind = kind.clone();
                html! {
                    <TextArea
                        label={kind_str}
                        field_id={Some(format!("TELEGRAM_CONFIG.TEMPLATES.{}", event_label_suffix(&kind)))}
                        value={current_val}
                        collapse_on_empty={true}
                        on_change={Callback::from(move |val: String| {
                            let mut updated = telegram_state.form.templates.clone();
                            if val.is_empty() {
                                updated.remove(&kind);
                            } else {
                                updated.insert(kind.clone(), val);
                            }
                            telegram_state.dispatch(TelegramMessagingConfigFormAction::Templates(updated));
                        })}
                    />
                }
            })
            .collect::<Html>();
        let rest_template_fields = notify_on_options
            .iter()
            .map(|kind| {
                let kind_str = translate.t(&event_label_key(kind));
                let current_val = rest_state.form.templates.get(kind).cloned().unwrap_or_default();
                let rest_state = rest_state.clone();
                let kind = kind.clone();
                html! {
                    <TextArea
                        label={kind_str}
                        field_id={Some(format!("REST_MESSAGING_CONFIG.TEMPLATES.{}", event_label_suffix(&kind)))}
                        value={current_val}
                        collapse_on_empty={true}
                        on_change={Callback::from(move |val: String| {
                            let mut updated = rest_state.form.templates.clone();
                            if val.is_empty() {
                                updated.remove(&kind);
                            } else {
                                updated.insert(kind.clone(), val);
                            }
                            rest_state.dispatch(RestMessagingConfigFormAction::Templates(updated));
                        })}
                    />
                }
            })
            .collect::<Html>();
        let discord_template_fields = notify_on_options
            .iter()
            .map(|kind| {
                let kind_str = translate.t(&event_label_key(kind));
                let current_val = discord_state.form.templates.get(kind).cloned().unwrap_or_default();
                let discord_state = discord_state.clone();
                let kind = kind.clone();
                html! {
                    <TextArea
                        label={kind_str}
                        field_id={Some(format!("DISCORD_MESSAGING_CONFIG.TEMPLATES.{}", event_label_suffix(&kind)))}
                        value={current_val}
                        collapse_on_empty={true}
                        on_change={Callback::from(move |val: String| {
                            let mut updated = discord_state.form.templates.clone();
                            if val.is_empty() {
                                updated.remove(&kind);
                            } else {
                                updated.insert(kind.clone(), val);
                            }
                            discord_state.dispatch(DiscordMessagingConfigFormAction::Templates(updated));
                        })}
                    />
                }
            })
            .collect::<Html>();
        html! {
            <>
            <div class="tp__messaging-config-view__header tp__config-view-page__header">
                { config_field_child!(translate.t("LABEL.NOTIFY_ON"), "MESSAGING_CONFIG.NOTIFY_ON", {
                   let dispatch_handle = msg_state.clone();
                   html! { <RadioButtonGroup
                        multi_select={true} none_allowed={true}
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
                    <h1>{translate.t(LABEL_TELEGRAM)}</h1>
                    { edit_field_text!(telegram_state, translate.t(LABEL_BOT_TOKEN), bot_token, TelegramMessagingConfigFormAction::BotToken, true) }
                    { edit_field_list!(telegram_state, translate.t(LABEL_CHAT_IDS), chat_ids, TelegramMessagingConfigFormAction::ChatIds, translate.t("LABEL.ADD_CHAT_ID")) }
                    { edit_field_bool!(telegram_state, translate.t(LABEL_MARKDOWN), markdown, TelegramMessagingConfigFormAction::Markdown) }
                    <div class="tp__messaging-config__templates">
                        <h3>{translate.t("LABEL.TEMPLATES")}</h3>
                        { telegram_template_fields }
                    </div>
                </Card>

                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_REST)}</h1>
                    { edit_field_text!(rest_state, translate.t(LABEL_URL), url, RestMessagingConfigFormAction::Url) }
                    { edit_field_text_option!(rest_state, translate.t(LABEL_METHOD), method, RestMessagingConfigFormAction::Method) }
                    { edit_field_list!(rest_state, translate.t(LABEL_HEADERS), headers, RestMessagingConfigFormAction::Headers, translate.t(LABEL_ADD_HEADER)) }
                    <div class="tp__messaging-config__templates">
                        <h3>{translate.t("LABEL.TEMPLATES")}</h3>
                        { rest_template_fields }
                    </div>
                </Card>

                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_PUSHOVER)}</h1>
                    { edit_field_text_option!(pushover_state, translate.t(LABEL_URL), url, PushoverMessagingConfigFormAction::Url) }
                    { edit_field_text!(pushover_state, translate.t(LABEL_TOKEN), token, PushoverMessagingConfigFormAction::Token, true) }
                    { edit_field_text!(pushover_state, translate.t(LABEL_USER), user, PushoverMessagingConfigFormAction::User) }
                </Card>

                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_DISCORD)}</h1>
                    { edit_field_text!(discord_state, translate.t(LABEL_WEBHOOK_URL), url, DiscordMessagingConfigFormAction::Url, true) }
                    <div class="tp__messaging-config__templates">
                        <h3>{translate.t("LABEL.TEMPLATES")}</h3>
                        { discord_template_fields }
                    </div>
                </Card>

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
           <div class="tp__config-view-page__title">{translate.t(LABEL_MESSAGING_CONFIG)}</div>
            { if *config_view_ctx.edit_mode { render_edit_mode() } else { render_view_mode() } }
        </div>
    }
}
