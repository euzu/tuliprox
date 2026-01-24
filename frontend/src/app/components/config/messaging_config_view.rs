use crate::app::components::config::config_page::{ConfigForm, LABEL_MESSAGING_CONFIG};
use crate::app::components::config::config_view_context::ConfigViewContext;
use crate::app::components::{Card, Chip, RadioButtonGroup, TextArea};
use crate::app::ConfigContext;
use crate::{config_field, config_field_bool, config_field_bool_empty, config_field_child, config_field_custom, config_field_empty, config_field_hide, config_field_optional, edit_field_bool, edit_field_list, edit_field_text, edit_field_text_option, generate_form_reducer};
use shared::model::{DiscordMessagingConfigDto, MessagingConfigDto, MsgKind, PushoverMessagingConfigDto,
                    RestMessagingConfigDto, TelegramMessagingConfigDto};
use std::rc::Rc;
use std::str::FromStr;
use yew::prelude::*;
use yew_i18n::use_translation;

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

generate_form_reducer!(
    state: TelegramMessagingConfigFormState { form: TelegramMessagingConfigDto },
    action_name: TelegramMessagingConfigFormAction,
    fields {
        BotToken => bot_token: String,
        ChatIds => chat_ids: Vec<String>,
        Markdown => markdown: bool,
        Templates => templates: std::collections::HashMap<MsgKind, String>,
    }
);

generate_form_reducer!(
    state: RestMessagingConfigFormState { form: RestMessagingConfigDto },
    action_name: RestMessagingConfigFormAction,
    fields {
        Url => url: String,
        Method => method: Option<String>,
        Headers => headers: Vec<String>,
        Templates => templates: std::collections::HashMap<MsgKind, String>,
    }
);

generate_form_reducer!(
    state: DiscordMessagingConfigFormState { form: DiscordMessagingConfigDto },
    action_name: DiscordMessagingConfigFormAction,
    fields {
        Url => url: String,
        Templates => templates: std::collections::HashMap<MsgKind, String>,
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
    state: MessagingConfigFormState { form: MessagingConfigDto },
    action_name: MessagingConfigFormAction,
    fields {
        NotifyOn => notify_on: Vec<MsgKind>,
    }
);

#[function_component]
pub fn MessagingConfigView() -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let config_view_ctx = use_context::<ConfigViewContext>().expect("ConfigViewContext not found");

    let telegram_state =
        use_reducer(|| TelegramMessagingConfigFormState {
            form: TelegramMessagingConfigDto::default(),
            modified: false,
        });
    let rest_state =
        use_reducer(|| RestMessagingConfigFormState {
            form: RestMessagingConfigDto::default(),
            modified: false,
        });

    let pushover_state =
        use_reducer(|| PushoverMessagingConfigFormState {
            form: PushoverMessagingConfigDto::default(),
            modified: false,
        });

    let discord_state =
        use_reducer(|| DiscordMessagingConfigFormState {
            form: DiscordMessagingConfigDto::default(),
            modified: false,
        });

    let messaging_state =
        use_reducer(|| MessagingConfigFormState {
            form: MessagingConfigDto::default(),
            modified: false,
        });

    let notify_on_options = use_memo((), |_| {
        vec![
            MsgKind::Info,
            MsgKind::Stats,
            MsgKind::Error,
            MsgKind::Watch,
        ]
    });

    let notify_on_options_text = use_memo((*notify_on_options).clone(), |options: &Vec<MsgKind>| {
        options.iter().map(ToString::to_string).collect::<Vec<String>>()
    });

    {
        let on_form_change = config_view_ctx.on_form_change.clone();
        let messaging_state = messaging_state.clone();
        let telegram_state = telegram_state.clone();
        let rest_state = rest_state.clone();
        let pushover_state = pushover_state.clone();
        let discord_state = discord_state.clone();

        let dependencies = (
            messaging_state.modified,
            telegram_state.modified,
            rest_state.modified,
            pushover_state.modified,
            discord_state.modified,
            messaging_state,
            telegram_state,
            rest_state,
            pushover_state,
            discord_state,
        );
        use_effect_with(dependencies, move |(mm, tm, rm, pm, dm, m, t, r, p, d)| {
            let mut form = m.form.clone();
            form.telegram = Some(t.form.clone());
            form.rest = Some(r.form.clone());
            form.pushover = Some(p.form.clone());
            form.discord = Some(d.form.clone());

            let modified = *mm || *tm || *rm || *pm || *dm;
            on_form_change.emit(ConfigForm::Messaging(modified, form));
        });
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
            .map_or_else(MessagingConfigDto::default, |m| m.clone());

        let telegram_cfg = msg_config.telegram.as_ref().map_or_else(TelegramMessagingConfigDto::default, |t| t.clone());
        use_effect_with((telegram_cfg, config_view_ctx.edit_mode.clone()), move |(telegram_cfg, _mode)| {
            t_state.dispatch(TelegramMessagingConfigFormAction::SetAll(telegram_cfg.clone()));
            || ()
        });

        let rest_cfg = msg_config.rest.as_ref().map_or_else(RestMessagingConfigDto::default, |t| t.clone());
        use_effect_with((rest_cfg, config_view_ctx.edit_mode.clone()), move |(rest_cfg, _mode)| {
            r_state.dispatch(RestMessagingConfigFormAction::SetAll(rest_cfg.clone()));
            || ()
        });

        let pushover_cfg = msg_config.pushover.as_ref().map_or_else(PushoverMessagingConfigDto::default, |t| t.clone());
        use_effect_with((pushover_cfg, config_view_ctx.edit_mode.clone()), move |(pushover_cfg, _mode)| {
            p_state.dispatch(PushoverMessagingConfigFormAction::SetAll(pushover_cfg.clone()));
            || ()
        });

        let discord_state = discord_state.clone();
        let discord_cfg = msg_config.discord.as_ref().map_or_else(DiscordMessagingConfigDto::default, |t| t.clone());
        use_effect_with((discord_cfg, config_view_ctx.edit_mode.clone()), move |(discord_cfg, _mode)| {
            discord_state.dispatch(DiscordMessagingConfigFormAction::SetAll(discord_cfg.clone()));
            || ()
        });

        use_effect_with((msg_config, config_view_ctx.edit_mode.clone()), move |(msg_config, _mode)| {
            msg_state.dispatch(MessagingConfigFormAction::SetAll(msg_config.clone()));
            || ()
        });
    }

    let render_templates_view = |templates: &std::collections::HashMap<MsgKind, String>| {
        if templates.is_empty() {
            html! {}
        } else {
            html! {
                <div class="tp__messaging-config__templates-view">
                    <h3>{translate.t("LABEL.TEMPLATES")}</h3>
                    { for templates.iter().map(|(kind, template)| {
                        html! {
                             { config_field_custom!(translate.t(&format!("LABEL.MSG_KIND_{}", kind.to_string().to_uppercase())), template.clone()) }
                        }
                    })}
                </div>
            }
        }
    };

    let render_telegram = |telegram: Option<&TelegramMessagingConfigDto>| match telegram {
        Some(entry) => html! {
          <Card class="tp__config-view__card">
              <h1>{translate.t("LABEL.TELEGRAM")}</h1>
              { config_field_hide!(entry, translate.t(LABEL_BOT_TOKEN), bot_token) }
              { config_field_child!(translate.t(LABEL_CHAT_IDS), {
                  html! {
                      <div class="tp__config-view__tags">
                          {
                              if entry.chat_ids.is_empty() {
                                  html! {}
                              } else {
                                  html! { for entry.chat_ids.iter().map(|t| html! { <Chip label={t.clone()} /> }) }
                              }
                          }
                      </div>
                  }
              })}
             { config_field_bool!(entry, translate.t(LABEL_MARKDOWN), markdown) }
             { render_templates_view(&entry.templates) }
          </Card>
        },
        None => html! {
          <Card class="tp__config-view__card">
             <h1>{translate.t(LABEL_TELEGRAM)}</h1>
             { config_field_empty!(translate.t(LABEL_BOT_TOKEN)) }
             { config_field_empty!(translate.t(LABEL_CHAT_IDS)) }
             { config_field_bool_empty!(translate.t(LABEL_MARKDOWN)) }
          </Card>
        },
    };

    let render_rest = |rest: Option<&RestMessagingConfigDto>| match rest {
        Some(entry) => html! {
          <Card class="tp__config-view__card">
              <h1>{translate.t(LABEL_REST)}</h1>
              { config_field!(entry, translate.t(LABEL_URL), url) }
              { config_field_optional!(entry, translate.t(LABEL_METHOD), method) }
              { config_field_child!(translate.t(LABEL_HEADERS), {
                  html! {
                      <div class="tp__config-view__tags">
                          {
                              if entry.headers.is_empty() {
                                  html! {}
                              } else {
                                  html! { for entry.headers.iter().map(|h| html! { <Chip label={h.clone()} /> }) }
                              }
                          }
                      </div>
                  }
              })}
              { render_templates_view(&entry.templates) }
          </Card>
        },
        None => html! {
          <Card class="tp__config-view__card">
              <h1>{translate.t(LABEL_REST)}</h1>
              { config_field_empty!(translate.t(LABEL_URL)) }
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
              { config_field_empty!(translate.t(LABEL_WEBHOOK_URL)) }
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
              { config_field_empty!(translate.t(LABEL_URL)) }
              { config_field_empty!(translate.t(LABEL_TOKEN)) }
              { config_field_empty!(translate.t(LABEL_USER)) }
          </Card>
        },
    };

    let render_view_mode = || {
        let msg_state = messaging_state.clone();
        html! {
          <>
        <div class="tp__messaging-config-view__header tp__config-view-page__header">
          { config_field_child!(translate.t(LABEL_NOTIFY_ON), {
             html! { <div class="tp__messaging-config-view__notify-on">
                 { for  notify_on_options.iter().map(|t| {
                     let is_selected = msg_state.form.notify_on.contains(t);
                      let class = if is_selected { "tp__text-button tp__button-primary" } else { "tp__text-button" };
                     html! {
                     <Chip label={t.to_string()} class={class}/>
                 }}) }
                </div>
              }
          })}
        </div>
        <div class="tp__messaging-config-view__body tp__config-view-page__body">
          {render_telegram(msg_state.form.telegram.as_ref())}
          {render_rest(msg_state.form.rest.as_ref())}
          {render_pushover(msg_state.form.pushover.as_ref())}
          {render_discord(msg_state.form.discord.as_ref())}
        </div>
        </>
        }
    };

    let render_edit_mode = || {
        let msg_state = messaging_state.clone();
        let notify_on_selections = Rc::new(
            msg_state
                .form
                .notify_on
                .iter()
                .map(ToString::to_string)
                .collect(),
        );
        html! {
            <>
            <div class="tp__messaging-config-view__header tp__config-view-page__header">
                { config_field_child!(translate.t("LABEL.NOTIFY_ON"), {
                   html! { <RadioButtonGroup
                        multi_select={true} none_allowed={true}
                        on_select={Callback::from(move |selections: Rc<Vec<String>>| {
                            msg_state.dispatch(MessagingConfigFormAction::NotifyOn(
                                selections.iter().filter_map(|s| MsgKind::from_str(s).ok()).collect()));
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
                        { for notify_on_options.iter().map(|kind| {
                            let kind_str = translate.t(&format!("LABEL.MSG_KIND_{}", kind.to_string().to_uppercase()));
                            let current_val = telegram_state.form.templates.get(kind).cloned().unwrap_or_default();
                            let telegram_state = telegram_state.clone();
                            let kind = *kind;
                            html! {
                                <TextArea 
                                    label={kind_str}
                                    value={current_val}
                                    collapse_on_empty={true}
                                    on_change={Callback::from(move |val: String| {
                                        let mut updated = telegram_state.form.templates.clone();
                                        if val.is_empty() {
                                            updated.remove(&kind);
                                        } else {
                                            updated.insert(kind, val);
                                        }
                                        telegram_state.dispatch(TelegramMessagingConfigFormAction::Templates(updated));
                                    })}
                                />
                            }
                        })}
                    </div>
                </Card>

                <Card class="tp__config-view__card">
                    <h1>{translate.t(LABEL_REST)}</h1>
                    { edit_field_text!(rest_state, translate.t(LABEL_URL), url, RestMessagingConfigFormAction::Url) }
                    { edit_field_text_option!(rest_state, translate.t(LABEL_METHOD), method, RestMessagingConfigFormAction::Method) }
                    { edit_field_list!(rest_state, translate.t(LABEL_HEADERS), headers, RestMessagingConfigFormAction::Headers, translate.t(LABEL_ADD_HEADER)) }
                    <div class="tp__messaging-config__templates">
                        <h3>{translate.t("LABEL.TEMPLATES")}</h3>
                        { for notify_on_options.iter().map(|kind| {
                            let kind_str = translate.t(&format!("LABEL.MSG_KIND_{}", kind.to_string().to_uppercase()));
                            let current_val = rest_state.form.templates.get(kind).cloned().unwrap_or_default();
                            let rest_state = rest_state.clone();
                            let kind = *kind;
                            html! {
                                <TextArea 
                                    label={kind_str}
                                    value={current_val}
                                    collapse_on_empty={true}
                                    on_change={Callback::from(move |val: String| {
                                        let mut updated = rest_state.form.templates.clone();
                                        if val.is_empty() {
                                            updated.remove(&kind);
                                        } else {
                                            updated.insert(kind, val);
                                        }
                                        rest_state.dispatch(RestMessagingConfigFormAction::Templates(updated));
                                    })}
                                />
                            }
                        })}
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
                        { for notify_on_options.iter().map(|kind| {
                            let kind_str = translate.t(&format!("LABEL.MSG_KIND_{}", kind.to_string().to_uppercase()));
                            let current_val = discord_state.form.templates.get(kind).cloned().unwrap_or_default();
                            let discord_state = discord_state.clone();
                            let kind = *kind;
                            html! {
                                <TextArea 
                                    label={kind_str}
                                    value={current_val}
                                    collapse_on_empty={true}
                                    on_change={Callback::from(move |val: String| {
                                        let mut updated = discord_state.form.templates.clone();
                                        if val.is_empty() {
                                            updated.remove(&kind);
                                        } else {
                                            updated.insert(kind, val);
                                        }
                                        discord_state.dispatch(DiscordMessagingConfigFormAction::Templates(updated));
                                    })}
                                />
                            }
                        })}
                    </div>
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
