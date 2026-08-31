use crate::{
    app::{
        components::{
            config::HasFormData,
            input::Input,
            select::Select,
            selection_first_owned, selection_parse_first,
            userlist::{page::UserlistPage, proxy_type_input::ProxyTypeInput, ProxyTypeView},
            ClusterFlagsInput, ClusterFlagsInputMode, DropDownOption, DropDownSelection, Tag, TextButton, UserStatus,
        },
        TargetUser,
    },
    config_field_child, config_field_custom, edit_field_bool, edit_field_date, edit_field_list_option,
    edit_field_number, edit_field_number_i8, edit_field_number_u16, edit_field_text_option, generate_form_reducer,
    hooks::{use_clipboard_copy, use_service_context},
    html_if,
    i18n::use_translation,
};
use chrono::{Duration, Utc};
use shared::{
    model::{
        permission::Permission, ApiProxyServerInfoDto, ClusterFlags, ConfigTargetDto, NetworkAccessDto, ProxyType,
        ProxyUserCredentialsDto, ProxyUserStatus, UserPlanDto,
    },
    utils::generate_random_string,
};
use std::{net::IpAddr, rc::Rc};
use strum::IntoEnumIterator;
use yew::prelude::*;

const DEFAULT_EXPIRATION_DAYS: i64 = 365;
const DEFAULT_MAX_CONNECTIONS: u32 = 1;

#[derive(Clone, PartialEq, Default)]
struct UserFormFieldErrors {
    username: Option<String>,
    password: Option<String>,
    target: Option<String>,
}

impl UserFormFieldErrors {
    fn has_errors(&self) -> bool { self.username.is_some() || self.password.is_some() || self.target.is_some() }
}

fn cluster_flags_label(flags: ClusterFlags, t: impl Fn(&str) -> String) -> String {
    let mut parts = Vec::new();
    if flags.contains(ClusterFlags::Live) {
        parts.push(t("LABEL.LIVE_SHORT"));
    }
    if flags.contains(ClusterFlags::Vod) {
        parts.push(t("LABEL.VOD_SHORT"));
    }
    if flags.contains(ClusterFlags::Series) {
        parts.push(t("LABEL.SERIES_SHORT"));
    }
    parts.join(", ")
}

fn plan_hint_html(
    inherited: bool,
    inherited_key: &str,
    override_key: &str,
    plan_label: String,
    t: impl Fn(&str) -> String,
) -> Html {
    if inherited {
        html! {
            <div class="tp__form-field__plan-hint tp__form-field__plan-hint--inherited">
                <span class="tp__form-field__plan-hint-badge">{ t(inherited_key) }</span>
                <span class="tp__form-field__plan-hint-text">{ plan_label }</span>
            </div>
        }
    } else {
        html! {
            <div class="tp__form-field__plan-hint tp__form-field__plan-hint--override">
                <span class="tp__form-field__plan-hint-badge">{ t(override_key) }</span>
                <span class="tp__form-field__plan-hint-text">{ format!("(Plan: {plan_label})") }</span>
            </div>
        }
    }
}

fn normalize_country_entry(input: &str) -> Result<String, &'static str> {
    let normalized = input.trim().to_ascii_uppercase();
    if normalized.len() == 2 && normalized.chars().all(|ch| ch.is_ascii_alphabetic()) {
        Ok(normalized)
    } else {
        Err("MESSAGES.VALIDATION.NETWORK_ACCESS_COUNTRIES")
    }
}

fn normalize_network_entry(input: &str) -> Result<String, &'static str> {
    let normalized = input.trim().to_string();
    let Some((address, prefix)) = normalized.split_once('/') else {
        return Err("MESSAGES.VALIDATION.NETWORK_ACCESS_NETWORKS");
    };
    let address = address.trim().to_ascii_lowercase();
    let prefix = prefix.trim();
    let Ok(ip) = address.parse::<IpAddr>() else {
        return Err("MESSAGES.VALIDATION.NETWORK_ACCESS_NETWORKS");
    };
    let Ok(prefix) = prefix.parse::<u8>() else {
        return Err("MESSAGES.VALIDATION.NETWORK_ACCESS_NETWORKS");
    };
    let prefix_valid = match ip {
        IpAddr::V4(_) => prefix <= 32,
        IpAddr::V6(_) => prefix <= 128,
    };
    if prefix_valid {
        Ok(normalized)
    } else {
        Err("MESSAGES.VALIDATION.NETWORK_ACCESS_NETWORKS")
    }
}

fn build_network_access(countries: &[String], networks: &[String]) -> Result<Option<NetworkAccessDto>, String> {
    let countries_list = countries.iter().filter(|s| !s.trim().is_empty()).cloned().collect::<Vec<_>>();
    let networks_list = networks.iter().filter(|s| !s.trim().is_empty()).cloned().collect::<Vec<_>>();
    if countries_list.is_empty() && networks_list.is_empty() {
        return Ok(None);
    }
    let mut dto = NetworkAccessDto {
        allowed_countries: if countries_list.is_empty() { None } else { Some(countries_list) },
        allowed_networks: if networks_list.is_empty() { None } else { Some(networks_list) },
    };
    dto.prepare().map_err(|e| e.to_string())?;
    Ok(Some(dto))
}

fn network_access_changed(
    original: Option<&NetworkAccessDto>,
    countries: &[String],
    networks: &[String],
) -> Result<bool, String> {
    let built = build_network_access(countries, networks)?;
    Ok(original != built.as_ref())
}

fn validate_network_access(countries: &[String], networks: &[String]) -> Result<(), &'static str> {
    for country in countries {
        normalize_country_entry(country)?;
    }
    for network in networks {
        normalize_network_entry(network)?;
    }
    Ok(())
}

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
        OutputClusters => output_clusters: Option<ClusterFlags>,
        ExpDate => exp_date: Option<i64>,
        UiEnabled => ui_enabled: bool,
        EpgTimeshift => epg_timeshift: Option<String>,
        EpgRequestTimeshift => epg_request_timeshift: Option<String>,
        Comment => comment: Option<String>,
        Plan => plan: Option<String>,
        Filter => filter: Option<String>,
        MaxConnections => max_connections: u32,
        SoftConnections => soft_connections: u16,
        Priority => priority: i8,
        SoftPriority => soft_priority: i8,
    }
);

#[derive(Properties, PartialEq, Clone)]
pub struct ProxyUserCredentialsFormProps {
    pub user: Option<Rc<TargetUser>>,
    pub targets: Rc<Vec<Rc<ConfigTargetDto>>>,
    pub server: Rc<Vec<ApiProxyServerInfoDto>>,
    #[prop_or_default]
    pub plans: Rc<Vec<UserPlanDto>>,
    #[prop_or_default]
    pub active_page: Option<UserlistPage>,
    pub on_save: Callback<(bool, String, ProxyUserCredentialsDto)>,
    pub on_cancel: Callback<()>,
}

#[component]
pub fn ProxyUserCredentialsForm(props: &ProxyUserCredentialsFormProps) -> Html {
    let translate = use_translation();
    let service_ctx = use_service_context();
    let selected_target = use_state(|| None);
    let update = use_state(|| false);
    let allowed_countries = use_state(Vec::<Rc<Tag>>::new);
    let allowed_networks = use_state(Vec::<Rc<Tag>>::new);
    let field_errors = use_state(UserFormFieldErrors::default);

    let form_state: UseReducerHandle<UserFormState> =
        use_reducer(|| UserFormState { form: ProxyUserCredentialsDto::default(), modified: false });

    let proxy_user_status = use_memo(form_state.data().status, |status| {
        ProxyUserStatus::iter()
            .map(|s| DropDownOption {
                id: s.to_string(),
                label: html! { <UserStatus status={Some(s)} /> },
                selected: status.as_ref() == Some(&s),
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
                id: s.name.clone(),
                label: html! { s.name.clone() },
                selected: user_server.as_ref() == Some(&s.name),
            })
            .collect::<Vec<DropDownOption>>()
    });

    let plan_options = use_memo((props.plans.clone(), form_state.data().plan.clone()), |(plans, user_plan)| {
        let mut options =
            vec![DropDownOption { id: String::new(), label: html! { "—" }, selected: user_plan.is_none() }];
        options.extend(plans.iter().map(|p| DropDownOption {
            id: p.name.clone(),
            label: html! { p.name.clone() },
            selected: user_plan.as_ref() == Some(&p.name),
        }));
        options
    });

    {
        let form_state = form_state.clone();
        let set_selected_target = selected_target.clone();
        let set_update = update.clone();
        let set_allowed_countries = allowed_countries.clone();
        let set_allowed_networks = allowed_networks.clone();
        use_effect_with(
            (props.active_page, props.user.clone(), props.server.clone()),
            move |(active_page, user, server)| {
                if active_page.is_none() || *active_page == Some(UserlistPage::Edit) {
                    if let Some(u) = user.clone() {
                        set_update.set(true);
                        set_selected_target.set(Some(u.target.clone()));
                        let creds = (*u.credentials).clone();
                        if let Some(na) = &creds.network_access {
                            set_allowed_countries.set(na.allowed_countries.as_ref().map_or_else(
                                Vec::new,
                                |countries| {
                                    countries
                                        .iter()
                                        .map(|country| Rc::new(Tag { label: country.clone(), class: None }))
                                        .collect()
                                },
                            ));
                            set_allowed_networks.set(na.allowed_networks.as_ref().map_or_else(Vec::new, |networks| {
                                networks
                                    .iter()
                                    .map(|network| Rc::new(Tag { label: network.clone(), class: None }))
                                    .collect()
                            }));
                        } else {
                            set_allowed_countries.set(Vec::new());
                            set_allowed_networks.set(Vec::new());
                        }
                        form_state.dispatch(UserFormAction::SetAll(creds));
                    } else {
                        set_update.set(false);
                        set_selected_target.set(None);
                        set_allowed_countries.set(Vec::new());
                        set_allowed_networks.set(Vec::new());
                        let mut user = ProxyUserCredentialsDto::default();
                        if let Some(api_server) = (*server).first() {
                            user.server = Some(api_server.name.clone());
                        }
                        user.max_connections = DEFAULT_MAX_CONNECTIONS;
                        user.proxy = ProxyType::Reverse(None);
                        user.status = Some(ProxyUserStatus::Active);
                        user.output_clusters = None;
                        user.ui_enabled = true;
                        let now = Utc::now();
                        user.created_at = Some(now.timestamp());
                        let in_one_year = now + Duration::days(DEFAULT_EXPIRATION_DAYS);
                        user.exp_date = Some(in_one_year.timestamp());

                        user.username = generate_random_string(6).to_uppercase();
                        user.password = generate_random_string(6).to_uppercase();
                        user.token = Some(generate_random_string(6));

                        form_state.dispatch(UserFormAction::SetAll(user));
                    }
                }
            },
        );
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
        let countries = allowed_countries.clone();
        let networks = allowed_networks.clone();
        let field_errors = field_errors.clone();
        Callback::from(move |_| {
            let nothing_to_save = || services.toastr.warning(translate_clone.t("MESSAGES.SAVE.USER.NOTHING_TO_SAVE"));
            // Inline field validation before the save flow
            let mut errors = UserFormFieldErrors::default();
            if (*target).is_none() {
                errors.target = Some(translate_clone.t("MESSAGES.SAVE.USER.TARGET_NOT_SELECTED"));
            }
            {
                let data = user.data();
                if data.username.trim().is_empty() {
                    errors.username = Some(translate_clone.t("MESSAGES.VALIDATION.REQUIRED"));
                }
                if data.password.trim().is_empty() {
                    errors.password = Some(translate_clone.t("MESSAGES.VALIDATION.REQUIRED"));
                }
            }
            let has_errors = errors.has_errors();
            field_errors.set(errors);
            if has_errors {
                services.toastr.error(translate_clone.t("MESSAGES.SAVE.USER.FAIL"));
                return;
            }
            if let Some(target_name) = (*target).clone() {
                let original_target = original.as_ref().map(|u| u.target.clone()).unwrap_or_default();
                let target_changed = target_name != original_target;
                let countries_value = countries.iter().map(|tag| tag.label.clone()).collect::<Vec<_>>();
                let networks_value = networks.iter().map(|tag| tag.label.clone()).collect::<Vec<_>>();
                let network_access_changed = match network_access_changed(
                    original.as_ref().and_then(|u| u.credentials.network_access.as_ref()),
                    &countries_value,
                    &networks_value,
                ) {
                    Ok(changed) => changed,
                    Err(err) => {
                        services.toastr.error(err);
                        return;
                    }
                };
                if target_changed || user.modified() || network_access_changed {
                    let mut user = user.data().clone();
                    if let Err(message_key) = validate_network_access(&countries_value, &networks_value) {
                        services.toastr.error(translate_clone.t(message_key));
                        return;
                    }
                    user.network_access = match build_network_access(&countries_value, &networks_value) {
                        Ok(na) => na,
                        Err(err) => {
                            services.toastr.error(err);
                            return;
                        }
                    };
                    if let Err(err) = user.validate() {
                        services.toastr.error(err.to_string());
                    } else {
                        match original.as_ref().map(|t| t.credentials.clone()) {
                            None => onsave.emit((*is_update, target_name, user)),
                            Some(original_user) => {
                                if target_changed || (*original_user) != user {
                                    onsave.emit((*is_update, target_name, user));
                                } else {
                                    nothing_to_save();
                                }
                            }
                        }
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
    let instance_plan = form_state.clone();
    let plan_list = props.plans.clone();
    let plan_is_update = update.clone();
    let instance_output_clusters = form_state.clone();
    let active_plan = form_state.data().plan.as_ref().and_then(|name| props.plans.iter().find(|p| &p.name == name));
    let country_services = service_ctx.clone();
    let country_translate = translate.clone();
    let create_country_tag = Callback::from(move |value: String| match normalize_country_entry(&value) {
        Ok(normalized) => Some(Tag { label: normalized, class: None }),
        Err(message_key) => {
            country_services.toastr.error(country_translate.t(message_key));
            None
        }
    });
    let network_services = service_ctx.clone();
    let network_translate = translate.clone();
    let create_network_tag = Callback::from(move |value: String| match normalize_network_entry(&value) {
        Ok(normalized) => Some(Tag { label: normalized, class: None }),
        Err(message_key) => {
            network_services.toastr.error(network_translate.t(message_key));
            None
        }
    });
    let copy_to_clipboard = use_clipboard_copy();
    let handle_copy_credentials = {
        let form_state = form_state.clone();
        let copy_to_clipboard = copy_to_clipboard.clone();
        Callback::from(move |_: String| {
            let data = form_state.data();
            let text = format!(
                "username: {} password: {} token: {}",
                data.username,
                data.password,
                data.token.as_ref().map_or_else(String::new, ToString::to_string)
            );
            copy_to_clipboard.emit(text);
        })
    };
    html! {
        <div class="tp__proxy-user-credentials-form tp__form-page">
          <div class="tp__proxy-user-credentials-form__body tp__form-page__body">
            { config_field_child!(translate.t("LABEL.PLAYLIST"), "PROXY_USER_CREDENTIALS.PLAYLIST", {
               html! {
                    <div class="tp__proxy-user-credentials-form__playlist-row">
                        <div class="tp__proxy-user-credentials-form__playlist-target">
                                <Select name="target"
                                multi_select={false}
                                required={true}
                                error={field_errors.target.clone()}
                                on_select={Callback::from(move |(_, selections): (String, DropDownSelection)| {
                                  let target = selection_first_owned(selections);
                                    set_selected_target.set(target);
                                })}
                                options={targets.clone()}
                            />
                        </div>
                        <div>
                            <ClusterFlagsInput
                                name="output_clusters"
                                value={form_state.data().output_clusters}
                                mode={ClusterFlagsInputMode::NoneIsAll}
                                short_labels={true}
                                on_change={Callback::from(move |(_, flags): (String, Option<ClusterFlags>)| {
                                    instance_output_clusters.dispatch(UserFormAction::OutputClusters(flags));
                                })}
                            />
                            { if let Some(plan) = active_plan {
                                if let Some(plan_clusters) = plan.output_clusters {
                                    let label = cluster_flags_label(plan_clusters, |k| translate.t(k));
                                    let inherited = form_state.data().output_clusters.is_none();
                                    { plan_hint_html(inherited, "LABEL.PLAN_INHERITED", "LABEL.PLAN_OVERRIDE", label, |k| translate.t(k)) }
                                } else { html! {} }
                            } else { html! {} } }
                        </div>
                    </div>
                }
            }) }
            { config_field_child!(translate.t("LABEL.STATUS"), "PROXY_USER_CREDENTIALS.STATUS", {
               html! { <Select name="status"
                    multi_select={false}
                    on_select={Callback::from(move |(_, selections): (String, DropDownSelection)| {
                        let status = selection_parse_first::<ProxyUserStatus>(&selections);
                        instance_status.dispatch(UserFormAction::Status(status));
                    })}
                    options={proxy_user_status.clone()}
                />
            }})}
            { if *update {
                  config_field_custom!(translate.t("LABEL.USERNAME"), form_state.data().username.clone())
                } else {
                  html! {
                    <div class="tp__form-field tp__form-field__text">
                        <Input
                            label={translate.t("LABEL.USERNAME")}
                            name="username"
                            autocomplete={true}
                            required={true}
                            error={field_errors.username.clone()}
                            value={form_state.data().username.clone()}
                            on_change={{
                                let form_state = form_state.clone();
                                Callback::from(move |value: String| form_state.dispatch(UserFormAction::Username(value)))
                            }}
                        />
                    </div>
                  }
               }
            }
            {
                html! {
                    <div class="tp__form-field tp__form-field__text">
                        <Input
                            label={translate.t("LABEL.PASSWORD")}
                            name="password"
                            hidden={true}
                            autocomplete={false}
                            required={true}
                            error={field_errors.password.clone()}
                            value={form_state.data().password.clone()}
                            on_change={{
                                let form_state = form_state.clone();
                                Callback::from(move |value: String| form_state.dispatch(UserFormAction::Password(value)))
                            }}
                        />
                    </div>
                }
            }
            { edit_field_text_option!(form_state,  translate.t("LABEL.TOKEN"), token, UserFormAction::Token, true) }
            { config_field_child!(translate.t("LABEL.PROXY"), "PROXY_USER_CREDENTIALS.PROXY", {
               html! {
                    <div>
                        <ProxyTypeInput value={form_state.data().proxy}
                            on_change={Callback::from(move |proxy_type: ProxyType| {
                                instance_proxy.dispatch(UserFormAction::Proxy(proxy_type));
                            }
                        )}/>
                        { if let Some(plan) = active_plan {
                            if let Some(plan_proxy) = plan.proxy {
                                // "Inherited" means the user's proxy value matches the plan's
                                // value exactly — not just that it equals ProxyType::default().
                                // New users start with Reverse(None) which is distinct from
                                // Redirect, so we must compare against the actual plan value.
                                let inherited = form_state.data().proxy == plan_proxy;
                                { plan_hint_html(inherited, "LABEL.PLAN_INHERITED", "LABEL.PLAN_OVERRIDE", plan_proxy.to_string(), |k| translate.t(k)) }
                            } else { html! {} }
                        } else { html! {} } }

                    </div>
            }})}
            { config_field_child!(translate.t("LABEL.SERVER"), "PROXY_USER_CREDENTIALS.SERVER", {
               html! {
                <Select name="server"
                    multi_select={false}
                    on_select={Callback::from(move |(_, selections): (String, DropDownSelection)| {
                        let server = selection_first_owned(selections);
                        instance_server.dispatch(UserFormAction::Server(server));
                    })}
                    options={server_list.clone()}
                />
            }})}
            { config_field_child!(translate.t("LABEL.PLAN"), "PROXY_USER_CREDENTIALS.PLAN", {
               html! {
                <Select name="plan"
                    multi_select={false}
                    on_select={Callback::from(move |(_, selections): (String, DropDownSelection)| {
                        let plan = selection_first_owned(selections).filter(|value| !value.is_empty());
                        // Snapshot state once to avoid reading stale pre-dispatch values later.
                        let current = instance_plan.data().clone();
                        let is_new = !*plan_is_update;
                        if let Some(plan_name) = &plan {
                            if let Some(p) = plan_list.iter().find(|p| &p.name == plan_name) {
                                if is_new && current.max_connections == DEFAULT_MAX_CONNECTIONS {
                                    instance_plan.dispatch(UserFormAction::MaxConnections(0));
                                }
                                // Trial window is computed only when creating a new user — never
                                // overwrite an existing user's expiry or status automatically.
                                if is_new {
                                    let trial_secs = p.trial.as_ref().and_then(shared::model::UserPlanTrialDto::duration_secs);
                                    if let Some(trial_secs) = trial_secs {
                                        let expires = Utc::now().timestamp().saturating_add(i64::try_from(trial_secs).unwrap_or(i64::MAX));
                                        instance_plan.dispatch(UserFormAction::ExpDate(Some(expires)));
                                        instance_plan.dispatch(UserFormAction::Status(Some(ProxyUserStatus::Trial)));
                                    } else if current.status == Some(ProxyUserStatus::Trial) {
                                        // Switched from a trial-plan to a non-trial plan: restore the
                                        // default one-year expiry and active status.
                                        let default_exp = Utc::now().timestamp().saturating_add(DEFAULT_EXPIRATION_DAYS * 86_400);
                                        instance_plan.dispatch(UserFormAction::ExpDate(Some(default_exp)));
                                        instance_plan.dispatch(UserFormAction::Status(Some(ProxyUserStatus::Active)));
                                    }
                                }
                            }
                        } else {
                            if is_new && current.max_connections == 0 {
                                instance_plan.dispatch(UserFormAction::MaxConnections(DEFAULT_MAX_CONNECTIONS));
                            }
                            // Plan cleared: if status was Trial (set by previous plan selection),
                            // restore default state.
                            if is_new && current.status == Some(ProxyUserStatus::Trial) {
                                let default_exp = Utc::now().timestamp().saturating_add(DEFAULT_EXPIRATION_DAYS * 86_400);
                                instance_plan.dispatch(UserFormAction::ExpDate(Some(default_exp)));
                                instance_plan.dispatch(UserFormAction::Status(Some(ProxyUserStatus::Active)));
                            }
                        }
                        instance_plan.dispatch(UserFormAction::Plan(plan));
                    })}
                    options={plan_options.clone()}
                />
            }})}

            { if let Some(plan) = active_plan {
                let max_con_label = if plan.max_connections == 0 {
                    translate.t("LABEL.UNLIMITED")
                } else {
                    plan.max_connections.to_string()
                };
                html! {
                    <div class="tp__proxy-user-credentials-form__plan-summary">
                        <div class="tp__proxy-user-credentials-form__plan-summary-header">
                            <span class="tp__proxy-user-credentials-form__plan-summary-title">{ translate.t("LABEL.PLAN_DETAILS") }</span>
                            <span class="tp__proxy-user-credentials-form__plan-summary-name">{ &plan.name }</span>
                        </div>
                        <div class="tp__proxy-user-credentials-form__plan-summary-body">
                            <div class="tp__proxy-user-credentials-form__plan-summary-chip">
                                <span class="tp__proxy-user-credentials-form__plan-summary-chip-label">{ translate.t("LABEL.MAX_CON") }</span>
                                <span class="tp__proxy-user-credentials-form__plan-summary-chip-val">{ max_con_label }</span>
                            </div>
                            { if plan.soft_connections > 0 {
                                html! {
                                    <div class="tp__proxy-user-credentials-form__plan-summary-chip">
                                        <span class="tp__proxy-user-credentials-form__plan-summary-chip-label">{ translate.t("LABEL.SOFT_CON") }</span>
                                        <span class="tp__proxy-user-credentials-form__plan-summary-chip-val">{ plan.soft_connections }</span>
                                    </div>
                                }
                            } else { html! {} } }
                            { if let Some(clusters) = plan.output_clusters {
                                html! {
                                    <div class="tp__proxy-user-credentials-form__plan-summary-chip">
                                        <span class="tp__proxy-user-credentials-form__plan-summary-chip-label">{ translate.t("LABEL.CLUSTER") }</span>
                                        <span class="tp__proxy-user-credentials-form__plan-summary-chip-val">{ cluster_flags_label(clusters, |k| translate.t(k)) }</span>
                                    </div>
                                }
                            } else { html! {} } }
                            { if let Some(proxy) = plan.proxy {
                                html! {
                                    <div class="tp__proxy-user-credentials-form__plan-summary-chip">
                                        <span class="tp__proxy-user-credentials-form__plan-summary-chip-label">{ translate.t("LABEL.PROXY") }</span>
                                        <span class="tp__proxy-user-credentials-form__plan-summary-chip-val"><ProxyTypeView value={proxy} /></span>
                                    </div>
                                }
                            } else { html! {} } }
                            { if let Some(trial) = &plan.trial {
                                html! {
                                    <div class="tp__proxy-user-credentials-form__plan-summary-chip">
                                        <span class="tp__proxy-user-credentials-form__plan-summary-chip-label">{ translate.t("LABEL.TRIAL") }</span>
                                        <span class="tp__proxy-user-credentials-form__plan-summary-chip-val">{ &trial.duration }</span>
                                    </div>
                                }
                            } else { html! {} } }
                        </div>
                    </div>
                }
            } else {
                html! {}
            } }
            { edit_field_text_option!(form_state,  translate.t("LABEL.FILTER"), filter, UserFormAction::Filter) }
            { if let Some(plan) = active_plan {
                if let Some(plan_filter) = &plan.filter {
                    html! {
                        <div class="tp__form-field__plan-filter-notice">
                            <div class="tp__form-field__plan-filter-notice-header">
                                <span class="tp__form-field__plan-filter-notice-title">{ translate.t("LABEL.PLAN_FILTER_ACTIVE") }</span>
                                <span class="tp__form-field__plan-filter-notice-sub">{ translate.t("LABEL.PLAN_FILTER_COMBINED_NOTICE") }</span>
                            </div>
                            <code class="tp__form-field__plan-filter-notice-code">{ plan_filter }</code>
                        </div>
                    }
                } else { html! {} }
            } else { html! {} } }
            { edit_field_date!(form_state,  translate.t("LABEL.EXP_DATE"), exp_date, UserFormAction::ExpDate) }
            <div>
                { edit_field_number!(form_state,  translate.t("LABEL.MAX_CONNECTIONS"), max_connections, UserFormAction::MaxConnections) }
                { if let Some(plan) = active_plan {
                    let plan_max_str = if plan.max_connections == 0 {
                        translate.t("LABEL.UNLIMITED")
                    } else {
                        plan.max_connections.to_string()
                    };
                    let inherited = form_state.data().max_connections == 0;
                    { plan_hint_html(inherited, "LABEL.PLAN_INHERITED", "LABEL.PLAN_OVERRIDE", plan_max_str, |k| translate.t(k)) }
                } else { html! {} } }
            </div>
            <div>
                { edit_field_number_u16!(form_state,  translate.t("LABEL.SOFT_CONNECTIONS"), soft_connections, UserFormAction::SoftConnections) }
                { if let Some(plan) = active_plan {
                    if plan.soft_connections > 0 {
                        let inherited = form_state.data().soft_connections == 0;
                        { plan_hint_html(inherited, "LABEL.PLAN_INHERITED", "LABEL.PLAN_OVERRIDE", plan.soft_connections.to_string(), |k| translate.t(k)) }
                    } else { html! {} }
                } else { html! {} } }
            </div>
            { edit_field_number_i8!(form_state, translate.t("LABEL.PRIORITY"), priority, UserFormAction::Priority) }
            { edit_field_number_i8!(form_state, translate.t("LABEL.SOFT_PRIORITY"), soft_priority, UserFormAction::SoftPriority) }
            { edit_field_text_option!(form_state,  translate.t("LABEL.EPG_TIMESHIFT"), epg_timeshift, UserFormAction::EpgTimeshift) }
            { edit_field_text_option!(form_state,  translate.t("LABEL.EPG_REQUEST_TIMESHIFT"), epg_request_timeshift, UserFormAction::EpgRequestTimeshift) }
            { edit_field_bool!(form_state,  translate.t("LABEL.USER_UI_ENABLED"), ui_enabled, UserFormAction::UiEnabled) }
            { edit_field_text_option!(form_state,  translate.t("LABEL.COMMENT"), comment, UserFormAction::Comment) }
            { edit_field_list_option!(allowed_countries, translate.t("LABEL.ALLOWED_COUNTRIES"), "PROXY_USER_CREDENTIALS.NETWORK_ACCESS_COUNTRIES", translate.t("LABEL.ADD_COUNTRY"), create_country_tag) }
            { edit_field_list_option!(allowed_networks, translate.t("LABEL.ALLOWED_NETWORKS"), "PROXY_USER_CREDENTIALS.NETWORK_ACCESS_NETWORKS", translate.t("LABEL.ADD_NETWORK"), create_network_tag) }

          </div>
          <div class="tp__proxy-user-credentials-form__toolbar tp__form-page__toolbar">
             <TextButton class="secondary" name="cancel_user"
                icon="Cancel"
                title={ translate.t("LABEL.CANCEL")}
                onclick={handle_cancel}></TextButton>
             <TextButton class="secondary" name="copy_credentials"
                icon="Clipboard"
                title={ translate.t("LABEL.COPY_CREDENTIALS")}
                onclick={handle_copy_credentials}></TextButton>
             { html_if!(service_ctx.auth.has_permission(Permission::UserWrite), {
                 <TextButton class="primary" name="save_user"
                    icon="Save"
                    title={ translate.t("LABEL.SAVE")}
                    onclick={handle_save_user}></TextButton>
             })}
          </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_network_access_returns_none_for_empty_inputs() {
        assert_eq!(build_network_access(&[], &[]), Ok(None));
    }

    #[test]
    fn network_access_changed_detects_network_only_edit() {
        let original = None;
        assert!(network_access_changed(original, &["DE".to_string()], &[]).unwrap());
    }

    #[test]
    fn network_access_changed_is_false_for_equivalent_values() {
        let original = Some(&NetworkAccessDto {
            allowed_countries: Some(vec!["DE".to_string(), "AT".to_string()]),
            allowed_networks: Some(vec!["10.0.0.0/8".to_string()]),
        });
        assert!(!network_access_changed(original, &["DE".to_string(), "AT".to_string()], &["10.0.0.0/8".to_string()])
            .unwrap());
    }

    #[test]
    fn build_network_access_prepares_countries_for_storage() {
        let result = build_network_access(&["de".to_string(), "DE".to_string()], &[]);
        let dto = result.unwrap().unwrap();
        assert_eq!(dto.allowed_countries, Some(vec!["DE".to_string()]));
    }

    #[test]
    fn build_network_access_propagates_invalid_cidr_error() {
        let result = build_network_access(&[], &["not-a-cidr".to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn validate_network_access_rejects_invalid_country_codes() {
        assert_eq!(
            validate_network_access(&["DEU".to_string()], &[]),
            Err("MESSAGES.VALIDATION.NETWORK_ACCESS_COUNTRIES")
        );
    }

    #[test]
    fn validate_network_access_rejects_invalid_networks() {
        assert_eq!(
            validate_network_access(&[], &["192.168.1.1".to_string()]),
            Err("MESSAGES.VALIDATION.NETWORK_ACCESS_NETWORKS")
        );
    }

    #[test]
    fn validate_network_access_accepts_basic_cidr_lists() {
        assert_eq!(
            validate_network_access(
                &["de".to_string(), "at".to_string()],
                &["192.168.0.0/16".to_string(), "2001:db8::/32".to_string()],
            ),
            Ok(())
        );
    }

    #[test]
    fn normalize_country_entry_uppercases_valid_values() {
        assert_eq!(normalize_country_entry("de"), Ok("DE".to_string()));
    }

    #[test]
    fn normalize_network_entry_accepts_basic_cidr() {
        assert_eq!(normalize_network_entry("192.168.0.0/16"), Ok("192.168.0.0/16".to_string()));
    }

    #[test]
    fn normalize_network_entry_rejects_invalid_prefix_range() {
        assert_eq!(normalize_network_entry("192.168.0.0/33"), Err("MESSAGES.VALIDATION.NETWORK_ACCESS_NETWORKS"));
    }
}
