use super::{
    common::CommonInputForm, libraries_from_text, libraries_to_text, mutate_media_server, ConfigInputFormAction,
    ConfigInputFormState, LABEL_ACCOUNT_TOKEN, LABEL_ALLOW_RELAY, LABEL_API_KEY, LABEL_LIBRARIES, LABEL_MEDIA_SERVER,
    LABEL_PREFER_HTTPS, LABEL_SERVER_ID, LABEL_SERVER_NAME, LABEL_TOKEN, LABEL_USER_ID,
};
use crate::{
    app::components::{input::Input, Card, HideContent, TitledCard, ToggleSwitch},
    config_field_child,
    i18n::use_translation,
};
use shared::model::MediaServerInputConfigDto;
use yew::{component, html, Callback, Html, Properties, UseReducerHandle};

#[derive(Properties, Clone)]
pub(super) struct MediaServerInputFormProps {
    pub state: UseReducerHandle<ConfigInputFormState>,
    pub allow_write: bool,
}

impl PartialEq for MediaServerInputFormProps {
    fn eq(&self, _other: &Self) -> bool {
        false
    }
}

#[component]
pub(super) fn MediaServerInputForm(props: &MediaServerInputFormProps) -> Html {
    html! {
        <CommonInputForm state={props.state.clone()} allow_write={props.allow_write} simple_url={true}
            credentials={true} connection={true} cache_duration={true} />
    }
}

#[derive(Properties, Clone)]
pub(super) struct MediaServerSettingsFormProps {
    pub state: UseReducerHandle<ConfigInputFormState>,
    pub allow_write: bool,
}

impl PartialEq for MediaServerSettingsFormProps {
    fn eq(&self, _other: &Self) -> bool {
        false
    }
}

#[component]
pub(super) fn MediaServerSettingsForm(props: &MediaServerSettingsFormProps) -> Html {
    let translate = use_translation();
    let state = props.state.clone();
    let media_server = state.form.media_server.clone().unwrap_or_default();
    let token = media_server.token.clone().unwrap_or_default();
    let api_key = media_server.api_key.clone().unwrap_or_default();
    let user_id = media_server.user_id.clone().unwrap_or_default();
    let account_token = media_server.account_token.clone().unwrap_or_default();
    let server_id = media_server.server_id.clone().unwrap_or_default();
    let server_name = media_server.server_name.clone().unwrap_or_default();
    let libraries = libraries_to_text(&media_server.libraries);

    if !props.allow_write {
        return html! {
            <>
                <Card class="tp__config-view__card">
                    <TitledCard title={translate.t(LABEL_MEDIA_SERVER)}>
                        <div class="tp__config-view__cols-2">
                            { secret_field(translate.t(LABEL_TOKEN), "MEDIA_SERVER.TOKEN", token) }
                            { secret_field(translate.t(LABEL_API_KEY), "MEDIA_SERVER.API_KEY", api_key) }
                        </div>
                        <div class="tp__config-view__cols-2">
                            { text_field(translate.t(LABEL_USER_ID), "MEDIA_SERVER.USER_ID", user_id) }
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
                    </TitledCard>
                </Card>
                <Card class="tp__config-view__card">
                    { text_field(translate.t(LABEL_LIBRARIES), "MEDIA_SERVER.LIBRARIES", libraries) }
                </Card>
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
                </TitledCard>
            </Card>
            <Card class="tp__config-view__card">
                <Input name="media_server_libraries" field_id={Some("MEDIA_SERVER.LIBRARIES".to_string())}
                    label={Some(translate.t(LABEL_LIBRARIES))} value={libraries} on_change={Some(on_libraries)} />
            </Card>
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
