use crate::{
    app::{
        components::{
            config::HasFormData, select::Select, userlist::proxy_type_input::ProxyTypeInput, DropDownOption,
            DropDownSelection, TextButton, UserStatus,
        },
        TargetUser,
    },
    config_field_child, config_field_custom, edit_field_bool, edit_field_date, edit_field_number,
    edit_field_number_i8, edit_field_text, edit_field_text_option, generate_form_reducer,
    hooks::use_service_context,
    i18n::use_translation,
};
use chrono::{Duration, Utc};
use shared::{
    model::{ApiProxyServerInfoDto, ConfigTargetDto, ProxyType, ProxyUserCredentialsDto, ProxyUserStatus},
    utils::generate_random_string,
};
use std::rc::Rc;
use yew::prelude::*;

const DEFAULT_MAX_CONNECTIONS: u32 = 1;
const DEFAULT_EXPIRATION_DAYS: i64 = 365;

generate_form_reducer!(
    state: UserFormState { form: ProxyUserCredentialsDto },
    action_name: UserFormAction,
    fields {
        Username => username: String,
        Password => password: String,
        Token => token: Option<String>,
        Proxy => proxy: ProxyType,
        Server => server: Option<String>,
        Status => status: Option<ProxyUserStatus>,
        MaxConnections => max_connections: u32,
        Priority => priority: i8,
        ExpDate => exp_date: Option<i64>,
        UiEnabled => ui_enabled: bool,
        EpgTimeshift => epg_timeshift: Option<String>,
        EpgRequestTimeshift => epg_request_timeshift: Option<String>,
        Comment => comment: Option<String>,
    }
);

#[derive(Properties, PartialEq, Clone)]
pub struct ProxyUserCredentialsFormProps {
    pub user: Option<Rc<TargetUser>>,
    pub targets: Rc<Vec<Rc<ConfigTargetDto>>>,
    pub server: Rc<Vec<ApiProxyServerInfoDto>>,
    pub on_save: Callback<(bool, String, ProxyUserCredentialsDto)>,
    pub on_cancel: Callback<()>,
}

#[component]
pub fn ProxyUserCredentialsForm(props: &ProxyUserCredentialsFormProps) -> Html {
    let translate = use_translation();
    let service_ctx = use_service_context();
    let selected_target = use_state(|| None);
    let update = use_state(|| false);

    let form_state: UseReducerHandle<UserFormState> =
        use_reducer(|| UserFormState { form: ProxyUserCredentialsDto::default(), modified: false });

    let proxy_user_status = use_memo(form_state.data().status, |status| {
        [
            ProxyUserStatus::Active,
            ProxyUserStatus::Expired,
            ProxyUserStatus::Banned,
            ProxyUserStatus::Trial,
            ProxyUserStatus::Disabled,
            ProxyUserStatus::Pending,
        ]
        .iter()
        .map(|s| DropDownOption {
            id: s.to_string(),
            label: html! { <UserStatus status={Some(*s)} /> },
            selected: status.as_ref() == Some(s),
        })
        .collect::<Vec<DropDownOption>>()
    });

    let targets = use_memo((props.targets.clone(), (*selected_target).clone()), |(targets, selected)| {
        targets
            .iter()
            .map(|t| DropDownOption {
                id: t.name.clone(),
                label: html! { t.name.clone() },
                selected: selected.as_ref().is_some_and(|ut: &String| ut == &t.name),
            })
            .collect::<Vec<DropDownOption>>()
    });

    let server = use_memo((props.server.clone(), form_state.data().server.clone()), |(server_list, user_server)| {
        server_list
            .iter()
            .map(|s| DropDownOption {
                id: s.name.to_string(),
                label: html! { s.name.clone() },
                selected: user_server.as_ref() == Some(&s.name),
            })
            .collect::<Vec<DropDownOption>>()
    });

    {
        let form_state = form_state.clone();
        let set_selected_target = selected_target.clone();
        let set_update = update.clone();
        use_effect_with((props.user.clone(), props.server.clone()), move |(user, server)| {
            if let Some(u) = user.clone() {
                set_update.set(true);
                set_selected_target.set(Some(u.target.clone()));
                form_state.dispatch(UserFormAction::SetAll((*u.credentials).clone()));
            } else {
                set_update.set(false);
                set_selected_target.set(None);
                let mut user = ProxyUserCredentialsDto::default();
                if let Some(api_server) = (*server).first() {
                    user.server = Some(api_server.name.clone());
                }
                user.max_connections = DEFAULT_MAX_CONNECTIONS;
                user.proxy = ProxyType::Reverse(None);
                user.status = Some(ProxyUserStatus::Active);
                user.ui_enabled = true;
                let now = Utc::now();
                user.created_at = Some(now.timestamp());
                let in_one_year = now + Duration::days(DEFAULT_EXPIRATION_DAYS);
                user.exp_date = Some(in_one_year.timestamp());

                if user.username.is_empty() {
                    user.username = generate_random_string(6).to_uppercase();
                }
                if user.password.is_empty() {
                    user.password = generate_random_string(6).to_uppercase();
                }
                user.token = Some(generate_random_string(6));

                form_state.dispatch(UserFormAction::SetAll(user));
            }
        });
    }

    let handle_cancel = {
        let oncancel = props.on_cancel.clone();
        Callback::from(move |_| {
            oncancel.emit(());
        })
    };

    let handle_save_user = {
        let user = form_state.clone();
        let original = props.user.clone();
        let services = service_ctx.clone();
        let translate_clone = translate.clone();
        let target = selected_target.clone();
        let onsave = props.on_save.clone();
        let is_update = update.clone();
        Callback::from(move |_| {
            let nothing_to_save = || services.toastr.warning(translate_clone.t("MESSAGES.SAVE.USER.NOTHING_TO_SAVE"));
            if let Some(target_name) = (*target).as_ref().cloned() {
                let original_target = original.as_ref().map(|u| u.target.clone()).unwrap_or_default();
                let target_changed = target_name != original_target;
                if target_changed || user.modified() {
                    let user = user.data();
                    if let Err(err) = user.validate() {
                        services.toastr.error(err.to_string());
                    } else {
                        match original.as_ref().map(|t| t.credentials.clone()) {
                            None => onsave.emit((*is_update, target_name, user.clone())),
                            Some(original_user) => {
                                if target_changed || &(*original_user) != user {
                                    onsave.emit((*is_update, target_name, user.clone()));
                                } else {
                                    nothing_to_save();
                                }
                            }
                        };
                    }
                } else {
                    nothing_to_save();
                }
            } else {
                services.toastr.error(translate_clone.t("MESSAGES.SAVE.USER.TARGET_NOT_SELECTED"));
            }
        })
    };

    let set_selected_target = selected_target.clone();
    let server_list = server.clone();
    let instance_status = form_state.clone();
    let instance_proxy = form_state.clone();
    let instance_server = form_state.clone();
    html! {
        <div class="tp__proxy-user-credentials-form tp__form-page">
          <div class="tp__proxy-user-credentials-form__body tp__form-page__body">
            { config_field_child!(translate.t("LABEL.PLAYLIST"), "PROXY_USER_CREDENTIALS.PLAYLIST", {
               html! { <Select name="target"
                    multi_select={false}
                    on_select={Callback::from(move |(_name, selections):(String, DropDownSelection)| {
                      let target = match selections {
                        DropDownSelection::Empty => None,
                        DropDownSelection::Single(option) => Some(option),
                        DropDownSelection::Multi(options) => options.first().cloned(),
                        };
                        set_selected_target.set(target);
                    })}
                    options={targets.clone()}
                />
            }})}
            { config_field_child!(translate.t("LABEL.STATUS"), "PROXY_USER_CREDENTIALS.STATUS", {
               html! { <Select name="status"
                    multi_select={false}
                    on_select={Callback::from(move |(_name, selections):(String, DropDownSelection)| {
                        let status = match selections {
                            DropDownSelection::Empty => None,
                            DropDownSelection::Single(option) => option.parse::<ProxyUserStatus>().ok(),
                            DropDownSelection::Multi(options) => options.first().as_ref().and_then(|f| f.parse::<ProxyUserStatus>().ok())
                           };
                        instance_status.dispatch(UserFormAction::Status(status));
                    })}
                    options={proxy_user_status.clone()}
                />
            }})}
            { if *update {
                  config_field_custom!(translate.t("LABEL.USERNAME"), form_state.data().username.clone())
                } else {
                  edit_field_text!(form_state, translate.t("LABEL.USERNAME"), username, UserFormAction::Username)
               }
            }
            { edit_field_text!(form_state, translate.t("LABEL.PASSWORD"), password, UserFormAction::Password, true) }
            { edit_field_text_option!(form_state,  translate.t("LABEL.TOKEN"), token, UserFormAction::Token, true) }
            { config_field_child!(translate.t("LABEL.PROXY"), "PROXY_USER_CREDENTIALS.PROXY", {
               html! {
                     <ProxyTypeInput value={form_state.data().proxy}
                        on_change={Callback::from(move |proxy_type: ProxyType| {
                         instance_proxy.dispatch(UserFormAction::Proxy(proxy_type));
                        }
                    )}/>
            }})}
            { config_field_child!(translate.t("LABEL.SERVER"), "PROXY_USER_CREDENTIALS.SERVER", {
               html! {
                <Select name="server"
                    multi_select={false}
                    on_select={Callback::from(move |(_name, selections):(String, DropDownSelection)| {
                        let server = match selections {
                            DropDownSelection::Empty => None,
                            DropDownSelection::Single(option) => Some(option.clone()),
                            DropDownSelection::Multi(options) => options.first().cloned(),
                           };
                        instance_server.dispatch(UserFormAction::Server(server));
                    })}
                    options={server_list.clone()}
                />
            }})}
            { edit_field_number!(form_state,  translate.t("LABEL.MAX_CONNECTIONS"), max_connections, UserFormAction::MaxConnections) }
            { edit_field_number_i8!(form_state, translate.t("LABEL.PRIORITY"), priority, UserFormAction::Priority) }
            { edit_field_date!(form_state,  translate.t("LABEL.EXP_DATE"), exp_date, UserFormAction::ExpDate) }
            { edit_field_text_option!(form_state,  translate.t("LABEL.EPG_TIMESHIFT"), epg_timeshift, UserFormAction::EpgTimeshift) }
            { edit_field_text_option!(form_state,  translate.t("LABEL.EPG_REQUEST_TIMESHIFT"), epg_request_timeshift, UserFormAction::EpgRequestTimeshift) }
            { edit_field_bool!(form_state,  translate.t("LABEL.USER_UI_ENABLED"), ui_enabled, UserFormAction::UiEnabled) }
            { edit_field_text_option!(form_state,  translate.t("LABEL.COMMENT"), comment, UserFormAction::Comment) }

          </div>
          <div class="tp__proxy-user-credentials-form__toolbar tp__form-page__toolbar">
             <TextButton class="secondary" name="cancel_user"
                icon="Cancel"
                title={ translate.t("LABEL.CANCEL")}
                onclick={handle_cancel}></TextButton>
             <TextButton class="primary" name="save_user"
                icon="Save"
                title={ translate.t("LABEL.SAVE")}
                onclick={handle_save_user}></TextButton>
          </div>
        </div>
    }
}
