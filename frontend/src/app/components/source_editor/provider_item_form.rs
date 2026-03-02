use crate::{
    app::components::{select::Select, Card, DropDownOption, DropDownSelection, Tag, TagList, TextButton},
    config_field_child, edit_field_bool, edit_field_number_u64, edit_field_text, generate_form_reducer,
    i18n::use_translation,
};
use shared::{
    model::{ConfigProviderDto, DnsPrefer, DnsScheme, OnConnectErrorPolicy, OnResolveErrorPolicy, ProviderDnsDto},
    utils::Internable,
};
use std::{collections::HashSet, rc::Rc, sync::Arc};
use yew::{component, html, use_reducer, use_state, Callback, Html, Properties, UseReducerHandle};

const LABEL_PROVIDER_NAME: &str = "LABEL.PROVIDER_NAME";
const LABEL_PROVIDER_URLS: &str = "LABEL.PROVIDER_URLS";
const LABEL_ADD_URL: &str = "LABEL.ADD_URL";
const LABEL_PROVIDER_DNS: &str = "LABEL.PROVIDER_DNS";
const LABEL_DNS_ENABLED: &str = "LABEL.DNS_ENABLED";
const LABEL_DNS_REFRESH_SECS: &str = "LABEL.DNS_REFRESH_SECS";
const LABEL_DNS_PREFER: &str = "LABEL.DNS_PREFER";
const LABEL_DNS_MAX_ADDRS: &str = "LABEL.DNS_MAX_ADDRS";
const LABEL_DNS_SCHEMES: &str = "LABEL.DNS_SCHEMES";
const LABEL_DNS_KEEP_VHOST: &str = "LABEL.DNS_KEEP_VHOST";
const LABEL_DNS_ON_RESOLVE_ERROR: &str = "LABEL.DNS_ON_RESOLVE_ERROR";
const LABEL_DNS_ON_CONNECT_ERROR: &str = "LABEL.DNS_ON_CONNECT_ERROR";

const DNS_PREFER_IPV4: &str = "ipv4";
const DNS_PREFER_IPV6: &str = "ipv6";
const DNS_PREFER_SYSTEM: &str = "system";
const DNS_SCHEME_HTTP: &str = "http";
const DNS_SCHEME_HTTPS: &str = "https";
const DNS_RESOLVE_KEEP_LAST_GOOD: &str = "keep_last_good";
const DNS_RESOLVE_FALLBACK_TO_HOSTNAME: &str = "fallback_to_hostname";
const DNS_CONNECT_TRY_NEXT_IP: &str = "try_next_ip";
const DNS_CONNECT_ROTATE_PROVIDER_URL: &str = "rotate_provider_url";

generate_form_reducer!(
    state: ProviderFormState { form: ConfigProviderDto },
    action_name: ProviderFormAction,
    fields {
        Name => name: Arc<str>,
    }
);

generate_form_reducer!(
    state: ProviderDnsFormState { form: ProviderDnsDto },
    action_name: ProviderDnsFormAction,
    fields {
        Enabled => enabled: bool,
        RefreshSecs => refresh_secs: u64,
        Prefer => prefer: DnsPrefer,
        MaxAddrs => max_addrs: Option<usize>,
        Schemes => schemes: Option<Vec<DnsScheme>>,
        KeepVhost => keep_vhost: bool,
        OnResolveError => on_resolve_error: OnResolveErrorPolicy,
        OnConnectError => on_connect_error: OnConnectErrorPolicy,
    }
);

fn dns_prefer_to_id(prefer: DnsPrefer) -> &'static str {
    match prefer {
        DnsPrefer::Ipv4 => DNS_PREFER_IPV4,
        DnsPrefer::Ipv6 => DNS_PREFER_IPV6,
        DnsPrefer::System => DNS_PREFER_SYSTEM,
    }
}

fn dns_prefer_from_id(id: &str) -> DnsPrefer {
    match id {
        DNS_PREFER_IPV4 => DnsPrefer::Ipv4,
        DNS_PREFER_IPV6 => DnsPrefer::Ipv6,
        _ => DnsPrefer::System,
    }
}

fn on_resolve_error_to_id(policy: OnResolveErrorPolicy) -> &'static str {
    match policy {
        OnResolveErrorPolicy::KeepLastGood => DNS_RESOLVE_KEEP_LAST_GOOD,
        OnResolveErrorPolicy::FallbackToHostname => DNS_RESOLVE_FALLBACK_TO_HOSTNAME,
    }
}

fn on_resolve_error_from_id(id: &str) -> OnResolveErrorPolicy {
    match id {
        DNS_RESOLVE_FALLBACK_TO_HOSTNAME => OnResolveErrorPolicy::FallbackToHostname,
        _ => OnResolveErrorPolicy::KeepLastGood,
    }
}

fn on_connect_error_to_id(policy: OnConnectErrorPolicy) -> &'static str {
    match policy {
        OnConnectErrorPolicy::TryNextIp => DNS_CONNECT_TRY_NEXT_IP,
        OnConnectErrorPolicy::RotateProviderUrl => DNS_CONNECT_ROTATE_PROVIDER_URL,
    }
}

fn on_connect_error_from_id(id: &str) -> OnConnectErrorPolicy {
    match id {
        DNS_CONNECT_ROTATE_PROVIDER_URL => OnConnectErrorPolicy::RotateProviderUrl,
        _ => OnConnectErrorPolicy::TryNextIp,
    }
}

fn scheme_to_id(scheme: DnsScheme) -> &'static str {
    match scheme {
        DnsScheme::Http => DNS_SCHEME_HTTP,
        DnsScheme::Https => DNS_SCHEME_HTTPS,
    }
}

fn schemes_from_ids(ids: Vec<String>) -> Option<Vec<DnsScheme>> {
    let selected = ids.into_iter().collect::<HashSet<_>>();
    let mut schemes = Vec::new();
    if selected.contains(DNS_SCHEME_HTTP) {
        schemes.push(DnsScheme::Http);
    }
    if selected.contains(DNS_SCHEME_HTTPS) {
        schemes.push(DnsScheme::Https);
    }
    if schemes.is_empty() {
        None
    } else {
        Some(schemes)
    }
}

#[derive(Properties, PartialEq, Clone)]
pub struct ProviderItemFormProps {
    pub on_submit: Callback<ConfigProviderDto>,
    pub on_cancel: Callback<()>,
    #[prop_or_default]
    pub initial: Option<ConfigProviderDto>,
}

#[component]
pub fn ProviderItemForm(props: &ProviderItemFormProps) -> Html {
    let translate = use_translation();

    let initial =
        props.initial.clone().unwrap_or_else(|| ConfigProviderDto { name: "".intern(), urls: Vec::new(), dns: None });

    let form_state: UseReducerHandle<ProviderFormState> =
        use_reducer(|| ProviderFormState { form: initial.clone(), modified: false });
    let dns_state: UseReducerHandle<ProviderDnsFormState> =
        use_reducer(|| ProviderDnsFormState { form: initial.dns.clone().unwrap_or_default(), modified: false });

    let urls_state = use_state(|| {
        initial.urls.iter().map(|u| Rc::new(Tag { label: u.to_string(), class: None })).collect::<Vec<_>>()
    });

    let handle_urls_change = {
        let urls_state = urls_state.clone();
        Callback::from(move |tags: Vec<Rc<Tag>>| {
            urls_state.set(tags);
        })
    };

    let handle_submit = {
        let form_state = form_state.clone();
        let dns_state = dns_state.clone();
        let urls_state = urls_state.clone();
        let on_submit = props.on_submit.clone();
        Callback::from(move |_| {
            let mut data = form_state.form.clone();
            data.urls = (*urls_state)
                .iter()
                .map(|t| t.label.trim().to_string().intern())
                .filter(|u: &Arc<str>| !u.is_empty())
                .collect();
            data.dns = Some(dns_state.form.clone());
            if !data.name.trim().is_empty() && !data.urls.is_empty() {
                on_submit.emit(data);
            }
        })
    };

    let handle_cancel = {
        let on_cancel = props.on_cancel.clone();
        Callback::from(move |_| {
            on_cancel.emit(());
        })
    };

    let dns_prefer_options = Rc::new(vec![
        DropDownOption::new(
            DNS_PREFER_SYSTEM,
            html! { "system" },
            dns_prefer_to_id(dns_state.form.prefer) == DNS_PREFER_SYSTEM,
        ),
        DropDownOption::new(
            DNS_PREFER_IPV4,
            html! { "ipv4" },
            dns_prefer_to_id(dns_state.form.prefer) == DNS_PREFER_IPV4,
        ),
        DropDownOption::new(
            DNS_PREFER_IPV6,
            html! { "ipv6" },
            dns_prefer_to_id(dns_state.form.prefer) == DNS_PREFER_IPV6,
        ),
    ]);
    let dns_prefer_state = dns_state.clone();
    let handle_dns_prefer_select = Callback::from(move |(_, selection): (String, DropDownSelection)| {
        let prefer = match selection {
            DropDownSelection::Single(id) => dns_prefer_from_id(&id),
            DropDownSelection::Multi(ids) => ids.first().map_or(DnsPrefer::default(), |id| dns_prefer_from_id(id)),
            DropDownSelection::Empty => DnsPrefer::default(),
        };
        dns_prefer_state.dispatch(ProviderDnsFormAction::Prefer(prefer));
    });

    let selected_dns_schemes = dns_state
        .form
        .schemes
        .as_ref()
        .map_or_else(HashSet::new, |schemes| schemes.iter().copied().map(scheme_to_id).collect::<HashSet<_>>());
    let dns_scheme_options = Rc::new(vec![
        DropDownOption::new(DNS_SCHEME_HTTP, html! { "http" }, selected_dns_schemes.contains(DNS_SCHEME_HTTP)),
        DropDownOption::new(DNS_SCHEME_HTTPS, html! { "https" }, selected_dns_schemes.contains(DNS_SCHEME_HTTPS)),
    ]);
    let dns_schemes_state = dns_state.clone();
    let handle_dns_schemes_select = Callback::from(move |(_, selection): (String, DropDownSelection)| {
        let schemes = match selection {
            DropDownSelection::Single(id) => schemes_from_ids(vec![id]),
            DropDownSelection::Multi(ids) => schemes_from_ids(ids),
            DropDownSelection::Empty => None,
        };
        dns_schemes_state.dispatch(ProviderDnsFormAction::Schemes(schemes));
    });

    let dns_on_resolve_error_options = Rc::new(vec![
        DropDownOption::new(
            DNS_RESOLVE_KEEP_LAST_GOOD,
            html! { "keep_last_good" },
            on_resolve_error_to_id(dns_state.form.on_resolve_error) == DNS_RESOLVE_KEEP_LAST_GOOD,
        ),
        DropDownOption::new(
            DNS_RESOLVE_FALLBACK_TO_HOSTNAME,
            html! { "fallback_to_hostname" },
            on_resolve_error_to_id(dns_state.form.on_resolve_error) == DNS_RESOLVE_FALLBACK_TO_HOSTNAME,
        ),
    ]);
    let dns_on_resolve_error_state = dns_state.clone();
    let handle_dns_on_resolve_error_select = Callback::from(move |(_, selection): (String, DropDownSelection)| {
        let policy = match selection {
            DropDownSelection::Single(id) => on_resolve_error_from_id(&id),
            DropDownSelection::Multi(ids) => {
                ids.first().map_or(OnResolveErrorPolicy::default(), |id| on_resolve_error_from_id(id))
            }
            DropDownSelection::Empty => OnResolveErrorPolicy::default(),
        };
        dns_on_resolve_error_state.dispatch(ProviderDnsFormAction::OnResolveError(policy));
    });

    let dns_on_connect_error_options = Rc::new(vec![
        DropDownOption::new(
            DNS_CONNECT_TRY_NEXT_IP,
            html! { "try_next_ip" },
            on_connect_error_to_id(dns_state.form.on_connect_error) == DNS_CONNECT_TRY_NEXT_IP,
        ),
        DropDownOption::new(
            DNS_CONNECT_ROTATE_PROVIDER_URL,
            html! { "rotate_provider_url" },
            on_connect_error_to_id(dns_state.form.on_connect_error) == DNS_CONNECT_ROTATE_PROVIDER_URL,
        ),
    ]);
    let dns_on_connect_error_state = dns_state.clone();
    let handle_dns_on_connect_error_select = Callback::from(move |(_, selection): (String, DropDownSelection)| {
        let policy = match selection {
            DropDownSelection::Single(id) => on_connect_error_from_id(&id),
            DropDownSelection::Multi(ids) => {
                ids.first().map_or(OnConnectErrorPolicy::default(), |id| on_connect_error_from_id(id))
            }
            DropDownSelection::Empty => OnConnectErrorPolicy::default(),
        };
        dns_on_connect_error_state.dispatch(ProviderDnsFormAction::OnConnectError(policy));
    });

    html! {
        <Card class="tp__config-view__card tp__item-form">
            { edit_field_text!(form_state, translate.t(LABEL_PROVIDER_NAME), name, ProviderFormAction::Name) }
            { config_field_child!(translate.t(LABEL_PROVIDER_URLS), "PROVIDER_FORM.URLS", {
                html! {
                    <TagList
                        tags={(*urls_state).clone()}
                        readonly={false}
                        placeholder={translate.t(LABEL_ADD_URL)}
                        on_change={handle_urls_change}
                    />
                }
            })}

            <h1> { translate.t(LABEL_PROVIDER_DNS) } </h1>
            { edit_field_bool!(dns_state, translate.t(LABEL_DNS_ENABLED), enabled, ProviderDnsFormAction::Enabled) }
            <div class="tp__config-view__cols-2">
                { edit_field_number_u64!(dns_state, translate.t(LABEL_DNS_REFRESH_SECS), refresh_secs, ProviderDnsFormAction::RefreshSecs) }
                <div class="tp__form-field tp__form-field__number">
                    <crate::app::components::number_input::NumberInput
                        label={Some(translate.t(LABEL_DNS_MAX_ADDRS))}
                        name={"dns_max_addrs"}
                        value={dns_state.form.max_addrs.map(|v| i64::try_from(v).unwrap_or(i64::MAX))}
                        on_change={Callback::from({
                            let dns_state = dns_state.clone();
                            move |value: Option<i64>| {
                                let parsed = value.and_then(|v| usize::try_from(v).ok());
                                dns_state.dispatch(ProviderDnsFormAction::MaxAddrs(parsed));
                            }
                        })}
                    />
                </div>
            </div>
            { config_field_child!(translate.t(LABEL_DNS_PREFER), "PROVIDER_FORM.DNS.PREFER", {
                html! {
                    <Select
                        name={"provider_dns_prefer"}
                        multi_select={false}
                        on_select={handle_dns_prefer_select}
                        options={dns_prefer_options}
                    />
                }
            })}
            { config_field_child!(translate.t(LABEL_DNS_SCHEMES), "PROVIDER_FORM.DNS.SCHEMES", {
                html! {
                    <Select
                        name={"provider_dns_schemes"}
                        multi_select={true}
                        on_select={handle_dns_schemes_select}
                        options={dns_scheme_options}
                    />
                }
            })}
            { edit_field_bool!(dns_state, translate.t(LABEL_DNS_KEEP_VHOST), keep_vhost, ProviderDnsFormAction::KeepVhost) }
            { config_field_child!(translate.t(LABEL_DNS_ON_RESOLVE_ERROR), "PROVIDER_FORM.DNS.ON_RESOLVE_ERROR", {
                html! {
                    <Select
                        name={"provider_dns_on_resolve_error"}
                        multi_select={false}
                        on_select={handle_dns_on_resolve_error_select}
                        options={dns_on_resolve_error_options}
                    />
                }
            })}
            { config_field_child!(translate.t(LABEL_DNS_ON_CONNECT_ERROR), "PROVIDER_FORM.DNS.ON_CONNECT_ERROR", {
                html! {
                    <Select
                        name={"provider_dns_on_connect_error"}
                        multi_select={false}
                        on_select={handle_dns_on_connect_error_select}
                        options={dns_on_connect_error_options}
                    />
                }
            })}
            <div class="tp__form-page__toolbar">
                <TextButton
                    class="secondary"
                    name="cancel_provider"
                    icon="Cancel"
                    title={translate.t("LABEL.CANCEL")}
                    onclick={handle_cancel}
                />
                <TextButton
                    class="primary"
                    name="submit_provider"
                    icon="Accept"
                    title={translate.t("LABEL.SUBMIT")}
                    onclick={handle_submit}
                />
            </div>
        </Card>
    }
}
