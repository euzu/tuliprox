use super::{
    common::CommonInputForm, libraries_from_text, libraries_to_text, mutate_media_server, ConfigInputFormAction,
    ConfigInputFormState, LABEL_ACCOUNT_TOKEN, LABEL_ALLOW_RELAY, LABEL_API_KEY, LABEL_CATALOG_OPTIONS,
    LABEL_INCLUDE_MEDIA_SOURCES, LABEL_LIBRARIES, LABEL_MEDIA_SERVER, LABEL_PAGE_SIZE, LABEL_PREFER_HTTPS,
    LABEL_REQUEST_DELAY_MS, LABEL_SERVER_ID, LABEL_SERVER_NAME, LABEL_TOKEN, LABEL_USER_ID,
};
use crate::{
    app::components::{
        input::Input, number_input::NumberInput, selection_parse_first, Card, DropDownOption, DropDownSelection,
        HideContent, Select, TitledCard, ToggleSwitch,
    },
    config_field_child, config_field_custom,
    i18n::{use_translation, YewI18n},
};
use shared::model::{ConfigInputDto, InputType, MediaServerInputConfigDto};
use std::rc::Rc;
use yew::{component, html, Callback, Html, Properties, UseReducerHandle};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaServerVisibleFields {
    pub url: bool,
    pub credentials: bool,
    pub token: bool,
    pub api_key: bool,
    pub user_id: bool,
    pub account_token: bool,
    pub server_id: bool,
    pub server_name: bool,
    pub prefer_https: bool,
    pub allow_relay: bool,
    pub libraries: bool,
    pub catalog_options: bool,
    pub include_media_sources: bool,
    pub silo_url_hint: bool,
}

pub fn media_server_visible_fields(input_type: InputType) -> MediaServerVisibleFields {
    match input_type {
        InputType::Plex => MediaServerVisibleFields {
            url: true,
            credentials: false,
            token: true,
            api_key: false,
            user_id: false,
            account_token: true,
            server_id: true,
            server_name: true,
            prefer_https: true,
            allow_relay: true,
            libraries: true,
            catalog_options: true,
            include_media_sources: false,
            silo_url_hint: false,
        },
        InputType::Emby | InputType::Jellyfin => MediaServerVisibleFields {
            url: true,
            credentials: true,
            token: true,
            api_key: true,
            user_id: true,
            account_token: false,
            server_id: false,
            server_name: false,
            prefer_https: false,
            allow_relay: false,
            libraries: true,
            catalog_options: true,
            include_media_sources: true,
            silo_url_hint: false,
        },
        InputType::Silo => MediaServerVisibleFields {
            url: true,
            credentials: true,
            token: true,
            api_key: true,
            user_id: true,
            account_token: false,
            server_id: false,
            server_name: false,
            prefer_https: false,
            allow_relay: false,
            libraries: true,
            catalog_options: true,
            include_media_sources: true,
            silo_url_hint: true,
        },
        _ => MediaServerVisibleFields {
            url: false,
            credentials: false,
            token: false,
            api_key: false,
            user_id: false,
            account_token: false,
            server_id: false,
            server_name: false,
            prefer_https: false,
            allow_relay: false,
            libraries: false,
            catalog_options: false,
            include_media_sources: false,
            silo_url_hint: false,
        },
    }
}

pub fn clear_foreign_media_server_fields(media_server: &mut MediaServerInputConfigDto, target_flavor: InputType) {
    if target_flavor == InputType::Plex {
        media_server.api_key = None;
        media_server.user_id = None;
        media_server.catalog.include_media_sources = true;
    } else {
        media_server.account_token = None;
        media_server.server_id = None;
        media_server.server_name = None;
        media_server.prefer_https = true;
        media_server.allow_relay = false;
    }
}

pub fn switch_media_server_flavor(input: &mut ConfigInputDto, new_flavor: InputType) {
    if input.input_type == new_flavor {
        return;
    }
    input.input_type = new_flavor;
    if new_flavor == InputType::Plex {
        input.username = None;
        input.password = None;
    }
    if let Some(media_server) = input.media_server.as_mut() {
        clear_foreign_media_server_fields(media_server, new_flavor);
    }
}

pub fn media_server_flavor_label_key(flavor: InputType) -> &'static str {
    match flavor {
        InputType::Emby => "LABEL.EMBY",
        InputType::Jellyfin => "LABEL.JELLYFIN",
        InputType::Silo => "LABEL.SILO",
        InputType::Plex => "LABEL.PLEX",
        _ => "LABEL.MEDIA_SERVER",
    }
}

pub fn media_server_flavor_options(selected: InputType, translate: &YewI18n) -> Rc<Vec<DropDownOption>> {
    let flavors = [InputType::Jellyfin, InputType::Emby, InputType::Plex, InputType::Silo];
    Rc::new(
        flavors
            .iter()
            .map(|&flavor| {
                let id = flavor.to_string();
                let label = translate.t(media_server_flavor_label_key(flavor));
                DropDownOption::new(&id, html! { label }, flavor == selected)
            })
            .collect(),
    )
}

#[derive(Properties, Clone)]
pub(super) struct MediaServerInputFormProps {
    pub state: UseReducerHandle<ConfigInputFormState>,
    pub allow_write: bool,
}

impl PartialEq for MediaServerInputFormProps {
    fn eq(&self, _other: &Self) -> bool { false }
}

#[component]
pub(super) fn MediaServerInputForm(props: &MediaServerInputFormProps) -> Html {
    let translate = use_translation();
    let state = props.state.clone();
    let input_type = state.form.input_type;
    let visible = media_server_visible_fields(input_type);

    let flavor_field = if props.allow_write {
        let options = media_server_flavor_options(input_type, &translate);
        let on_flavor_select = {
            let state = state.clone();
            Callback::from(move |(_, selection): (String, DropDownSelection)| {
                if let Some(new_flavor) = selection_parse_first::<InputType>(&selection) {
                    if new_flavor.is_media_server() && new_flavor != state.form.input_type {
                        let mut form = state.form.clone();
                        switch_media_server_flavor(&mut form, new_flavor);
                        state.dispatch(ConfigInputFormAction::InputType(new_flavor));
                        state.dispatch(ConfigInputFormAction::MediaServer(form.media_server));
                        if new_flavor == InputType::Plex {
                            state.dispatch(ConfigInputFormAction::Username(None));
                            state.dispatch(ConfigInputFormAction::Password(None));
                        }
                    }
                }
            })
        };
        config_field_child!(translate.t(LABEL_MEDIA_SERVER), "INPUT_FORM.MEDIA_SERVER_FLAVOR", {
            html! {
                <Select name="media_server_flavor" multi_select={false} required={true}
                    options={options} on_select={on_flavor_select} />
            }
        })
    } else {
        config_field_custom!(translate.t(LABEL_MEDIA_SERVER), translate.t(media_server_flavor_label_key(input_type)))
    };

    let lead = html! {
        <div class="tp__config-view__cols-2">
            { flavor_field }
        </div>
    };

    let url_hint_key = if visible.silo_url_hint { Some("INPUT_FORM.SILO_URL_HINT".to_string()) } else { None };

    html! {
        <CommonInputForm state={props.state.clone()} allow_write={props.allow_write} simple_url={true}
            credentials={visible.credentials} connection={true} cache_duration={true}
            lead={lead} {url_hint_key} />
    }
}

#[derive(Properties, Clone)]
pub(super) struct MediaServerSettingsFormProps {
    pub state: UseReducerHandle<ConfigInputFormState>,
    pub allow_write: bool,
}

impl PartialEq for MediaServerSettingsFormProps {
    fn eq(&self, _other: &Self) -> bool { false }
}

#[component]
pub(super) fn MediaServerSettingsForm(props: &MediaServerSettingsFormProps) -> Html {
    let translate = use_translation();
    let state = props.state.clone();
    let visible = media_server_visible_fields(state.form.input_type);

    let media_server = state.form.media_server.clone().unwrap_or_default();
    let token = media_server.token.clone().unwrap_or_default();
    let api_key = media_server.api_key.clone().unwrap_or_default();
    let user_id = media_server.user_id.clone().unwrap_or_default();
    let account_token = media_server.account_token.clone().unwrap_or_default();
    let server_id = media_server.server_id.clone().unwrap_or_default();
    let server_name = media_server.server_name.clone().unwrap_or_default();
    let libraries = libraries_to_text(&media_server.libraries);
    let page_size = media_server.catalog.page_size;
    let request_delay_ms = media_server.catalog.request_delay_ms;
    let include_media_sources = media_server.catalog.include_media_sources;

    if !props.allow_write {
        return html! {
            <>
                <Card class="tp__config-view__card">
                    <TitledCard title={translate.t(LABEL_MEDIA_SERVER)}>
                        if visible.api_key {
                            <div class="tp__config-view__cols-2">
                                { secret_field(translate.t(LABEL_TOKEN), "MEDIA_SERVER.TOKEN", token) }
                                { secret_field(translate.t(LABEL_API_KEY), "MEDIA_SERVER.API_KEY", api_key) }
                            </div>
                            <div class="tp__config-view__cols-2">
                                { text_field(translate.t(LABEL_USER_ID), "MEDIA_SERVER.USER_ID", user_id) }
                            </div>
                        } else {
                            <div class="tp__config-view__cols-2">
                                { secret_field(translate.t(LABEL_TOKEN), "MEDIA_SERVER.TOKEN", token) }
                                { secret_field(translate.t(LABEL_ACCOUNT_TOKEN), "MEDIA_SERVER.ACCOUNT_TOKEN", account_token) }
                            </div>
                            <div class="tp__config-view__cols-2">
                                { text_field(translate.t(LABEL_SERVER_ID), "MEDIA_SERVER.SERVER_ID", server_id) }
                                { text_field(translate.t(LABEL_SERVER_NAME), "MEDIA_SERVER.SERVER_NAME", server_name) }
                            </div>
                            <div class="tp__config-view__cols-2">
                                { config_field_child!(translate.t(LABEL_PREFER_HTTPS), "MEDIA_SERVER.PREFER_HTTPS", {
                                    html! { <ToggleSwitch value={media_server.prefer_https} readonly={true} /> }
                                })}
                                { config_field_child!(translate.t(LABEL_ALLOW_RELAY), "MEDIA_SERVER.ALLOW_RELAY", {
                                    html! { <ToggleSwitch value={media_server.allow_relay} readonly={true} /> }
                                })}
                            </div>
                        }
                    </TitledCard>
                </Card>
                if visible.libraries {
                    <Card class="tp__config-view__card">
                        { text_field(translate.t(LABEL_LIBRARIES), "MEDIA_SERVER.LIBRARIES", libraries) }
                    </Card>
                }
                if visible.catalog_options {
                    <Card class="tp__config-view__card">
                        <TitledCard title={translate.t(LABEL_CATALOG_OPTIONS)}>
                            <div class="tp__config-view__cols-2">
                                { config_field_custom!(translate.t(LABEL_PAGE_SIZE), page_size.to_string()) }
                                { config_field_custom!(translate.t(LABEL_REQUEST_DELAY_MS), request_delay_ms.to_string()) }
                            </div>
                            if visible.include_media_sources {
                                <div class="tp__config-view__cols-2">
                                    { config_field_child!(translate.t(LABEL_INCLUDE_MEDIA_SOURCES), "MEDIA_SERVER.INCLUDE_MEDIA_SOURCES", {
                                        html! { <ToggleSwitch value={include_media_sources} readonly={true} /> }
                                    })}
                                </div>
                            }
                        </TitledCard>
                    </Card>
                }
            </>
        };
    }

    let text_callback = |update: fn(&mut MediaServerInputConfigDto, Option<String>)| {
        let state = state.clone();
        Callback::from(move |value: String| {
            let value = (!value.is_empty()).then_some(value);
            let media_server = mutate_media_server(&state.form.media_server, |config| update(config, value));
            state.dispatch(ConfigInputFormAction::MediaServer(media_server));
        })
    };
    let bool_callback = |update: fn(&mut MediaServerInputConfigDto, bool)| {
        let state = state.clone();
        Callback::from(move |value| {
            let media_server = mutate_media_server(&state.form.media_server, |config| update(config, value));
            state.dispatch(ConfigInputFormAction::MediaServer(media_server));
        })
    };
    let on_libraries = {
        let state = state.clone();
        Callback::from(move |value: String| {
            let media_server = mutate_media_server(&state.form.media_server, |config| {
                config.libraries = libraries_from_text(&value, &config.libraries);
            });
            state.dispatch(ConfigInputFormAction::MediaServer(media_server));
        })
    };

    html! {
        <>
            <Card class="tp__config-view__card">
                <TitledCard title={translate.t(LABEL_MEDIA_SERVER)}>
                    if visible.api_key {
                        <div class="tp__config-view__cols-2">
                            <Input name="media_server_token" field_id={Some("MEDIA_SERVER.TOKEN".to_string())}
                                label={Some(translate.t(LABEL_TOKEN))} value={token} hidden={true}
                                on_change={Some(text_callback(|config, value| config.token = value))} />
                            <Input name="media_server_api_key" field_id={Some("MEDIA_SERVER.API_KEY".to_string())}
                                label={Some(translate.t(LABEL_API_KEY))} value={api_key} hidden={true}
                                on_change={Some(text_callback(|config, value| config.api_key = value))} />
                        </div>
                        <div class="tp__config-view__cols-2">
                            <Input name="media_server_user_id" field_id={Some("MEDIA_SERVER.USER_ID".to_string())}
                                label={Some(translate.t(LABEL_USER_ID))} value={user_id}
                                on_change={Some(text_callback(|config, value| config.user_id = value))} />
                        </div>
                    } else {
                        <div class="tp__config-view__cols-2">
                            <Input name="media_server_token" field_id={Some("MEDIA_SERVER.TOKEN".to_string())}
                                label={Some(translate.t(LABEL_TOKEN))} value={token} hidden={true}
                                on_change={Some(text_callback(|config, value| config.token = value))} />
                            <Input name="media_server_account_token" field_id={Some("MEDIA_SERVER.ACCOUNT_TOKEN".to_string())}
                                label={Some(translate.t(LABEL_ACCOUNT_TOKEN))} value={account_token} hidden={true}
                                on_change={Some(text_callback(|config, value| config.account_token = value))} />
                        </div>
                        <div class="tp__config-view__cols-2">
                            <Input name="media_server_server_id" field_id={Some("MEDIA_SERVER.SERVER_ID".to_string())}
                                label={Some(translate.t(LABEL_SERVER_ID))} value={server_id}
                                on_change={Some(text_callback(|config, value| config.server_id = value))} />
                            <Input name="media_server_server_name" field_id={Some("MEDIA_SERVER.SERVER_NAME".to_string())}
                                label={Some(translate.t(LABEL_SERVER_NAME))} value={server_name}
                                on_change={Some(text_callback(|config, value| config.server_name = value))} />
                        </div>
                        <div class="tp__config-view__cols-2">
                            { config_field_child!(translate.t(LABEL_PREFER_HTTPS), "MEDIA_SERVER.PREFER_HTTPS", {
                                html! { <ToggleSwitch value={media_server.prefer_https}
                                    on_change={bool_callback(|config, value| config.prefer_https = value)} /> }
                            })}
                            { config_field_child!(translate.t(LABEL_ALLOW_RELAY), "MEDIA_SERVER.ALLOW_RELAY", {
                                html! { <ToggleSwitch value={media_server.allow_relay}
                                    on_change={bool_callback(|config, value| config.allow_relay = value)} /> }
                            })}
                        </div>
                    }
                </TitledCard>
            </Card>
            if visible.libraries {
                <Card class="tp__config-view__card">
                    <Input name="media_server_libraries" field_id={Some("MEDIA_SERVER.LIBRARIES".to_string())}
                        label={Some(translate.t(LABEL_LIBRARIES))} value={libraries} on_change={Some(on_libraries)} />
                </Card>
            }
            if visible.catalog_options {
                <Card class="tp__config-view__card">
                    <TitledCard title={translate.t(LABEL_CATALOG_OPTIONS)}>
                        <div class="tp__config-view__cols-2">
                            <NumberInput name="media_server_page_size" field_id={Some("MEDIA_SERVER.PAGE_SIZE".to_string())}
                                label={Some(translate.t(LABEL_PAGE_SIZE))} value={Some(i64::from(page_size))}
                                min_i64={Some(1)} max_i64={Some(i64::from(u16::MAX))}
                                on_change={Callback::from({
                                    let state = state.clone();
                                    move |value: Option<i64>| {
                                        if let Some(value) = value.and_then(|v| u16::try_from(v).ok()).filter(|&v| v > 0) {
                                            let media_server = mutate_media_server(&state.form.media_server, |config| {
                                                config.catalog.page_size = value;
                                            });
                                            state.dispatch(ConfigInputFormAction::MediaServer(media_server));
                                        }
                                    }
                                })} />
                            <NumberInput name="media_server_request_delay_ms" field_id={Some("MEDIA_SERVER.REQUEST_DELAY_MS".to_string())}
                                label={Some(translate.t(LABEL_REQUEST_DELAY_MS))} value={Some(i64::try_from(request_delay_ms).unwrap_or(0))}
                                min_i64={Some(0)}
                                on_change={Callback::from({
                                    let state = state.clone();
                                    move |value: Option<i64>| {
                                        if let Some(value) = value.and_then(|v| u64::try_from(v).ok()) {
                                            let media_server = mutate_media_server(&state.form.media_server, |config| {
                                                config.catalog.request_delay_ms = value;
                                            });
                                            state.dispatch(ConfigInputFormAction::MediaServer(media_server));
                                        }
                                    }
                                })} />
                        </div>
                        if visible.include_media_sources {
                            <div class="tp__config-view__cols-2">
                                { config_field_child!(translate.t(LABEL_INCLUDE_MEDIA_SOURCES), "MEDIA_SERVER.INCLUDE_MEDIA_SOURCES", {
                                    html! { <ToggleSwitch value={include_media_sources}
                                        on_change={bool_callback(|config, value| config.catalog.include_media_sources = value)} /> }
                                })}
                            </div>
                        }
                    </TitledCard>
                </Card>
            }
        </>
    }
}

fn secret_field(label: String, field_id: &'static str, value: String) -> Html {
    config_field_child!(label, field_id, {
        html! { <span class="tp__form-field__value"><HideContent content={value} /></span> }
    })
}

fn text_field(label: String, field_id: &'static str, value: String) -> Html {
    config_field_child!(label, field_id, {
        html! { <span class="tp__form-field__value">{value}</span> }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::MediaServerLibrarySelector;

    #[test]
    fn visible_fields_per_flavor() {
        let plex = media_server_visible_fields(InputType::Plex);
        assert!(plex.url);
        assert!(!plex.credentials);
        assert!(plex.token);
        assert!(!plex.api_key);
        assert!(!plex.user_id);
        assert!(plex.account_token);
        assert!(plex.server_id);
        assert!(plex.server_name);
        assert!(plex.prefer_https);
        assert!(plex.allow_relay);
        assert!(plex.libraries);
        assert!(plex.catalog_options);
        assert!(!plex.include_media_sources);
        assert!(!plex.silo_url_hint);

        for flavor in [InputType::Emby, InputType::Jellyfin] {
            let fields = media_server_visible_fields(flavor);
            assert!(fields.url);
            assert!(fields.credentials);
            assert!(fields.token);
            assert!(fields.api_key);
            assert!(fields.user_id);
            assert!(!fields.account_token);
            assert!(!fields.server_id);
            assert!(!fields.server_name);
            assert!(!fields.prefer_https);
            assert!(!fields.allow_relay);
            assert!(fields.libraries);
            assert!(fields.catalog_options);
            assert!(fields.include_media_sources);
            assert!(!fields.silo_url_hint);
        }

        let silo = media_server_visible_fields(InputType::Silo);
        assert!(silo.url);
        assert!(silo.credentials);
        assert!(silo.token);
        assert!(silo.api_key);
        assert!(silo.user_id);
        assert!(!silo.account_token);
        assert!(!silo.server_id);
        assert!(!silo.server_name);
        assert!(!silo.prefer_https);
        assert!(!silo.allow_relay);
        assert!(silo.libraries);
        assert!(silo.catalog_options);
        assert!(silo.include_media_sources);
        assert!(silo.silo_url_hint);
    }

    #[test]
    fn flavor_switch_clears_foreign_fields_and_keeps_shared_ones() {
        // Switching from Jellyfin to Plex
        let mut input = ConfigInputDto {
            name: "My Server".into(),
            url: "http://192.168.1.50:8096".to_string(),
            username: Some("admin".to_string()),
            password: Some("secret".to_string()),
            media_server: Some(MediaServerInputConfigDto {
                token: Some("my-token".to_string()),
                api_key: Some("api-key-123".to_string()),
                user_id: Some("user-456".to_string()),
                libraries: vec![MediaServerLibrarySelector::Name("Movies".to_string())],
                catalog: shared::model::MediaServerCatalogConfigDto {
                    page_size: 50,
                    request_delay_ms: 100,
                    include_media_sources: false,
                },
                ..MediaServerInputConfigDto::default()
            }),
            ..ConfigInputDto::new_with_type(InputType::Jellyfin)
        };

        switch_media_server_flavor(&mut input, InputType::Plex);

        assert_eq!(input.input_type, InputType::Plex);
        assert_eq!(input.name.as_ref(), "My Server");
        assert_eq!(input.url, "http://192.168.1.50:8096");
        // Credentials cleared for Plex
        assert_eq!(input.username, None);
        assert_eq!(input.password, None);

        let ms = input.media_server.as_ref().unwrap();
        // Emby/Jellyfin-only fields cleared
        assert_eq!(ms.api_key, None);
        assert_eq!(ms.user_id, None);
        // Shared fields kept
        assert_eq!(ms.token.as_deref(), Some("my-token"));
        assert_eq!(ms.libraries.len(), 1);
        assert_eq!(ms.catalog.page_size, 50);
        assert_eq!(ms.catalog.request_delay_ms, 100);
        assert!(ms.catalog.include_media_sources); // reset to default for Plex

        // Now populate Plex-specific fields and switch to Silo
        let ms_mut = input.media_server.as_mut().unwrap();
        ms_mut.account_token = Some("act-tok".to_string());
        ms_mut.server_id = Some("srv-id-789".to_string());
        ms_mut.server_name = Some("Living Room".to_string());
        ms_mut.prefer_https = false;
        ms_mut.allow_relay = true;

        switch_media_server_flavor(&mut input, InputType::Silo);

        assert_eq!(input.input_type, InputType::Silo);
        let ms_silo = input.media_server.as_ref().unwrap();
        // Plex-specific fields cleared / reset to defaults
        assert_eq!(ms_silo.account_token, None);
        assert_eq!(ms_silo.server_id, None);
        assert_eq!(ms_silo.server_name, None);
        assert!(ms_silo.prefer_https);
        assert!(!ms_silo.allow_relay);
        // Shared fields kept
        assert_eq!(ms_silo.token.as_deref(), Some("my-token"));
        assert_eq!(ms_silo.libraries.len(), 1);
        assert_eq!(ms_silo.catalog.page_size, 50);
        assert_eq!(ms_silo.catalog.request_delay_ms, 100);
    }
}
