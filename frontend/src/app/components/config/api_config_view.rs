use crate::{
    app::{
        components::{
            config::{
                config_page::{ConfigForm, ConfigPage},
                config_view_context::ConfigViewContext,
                use_emit_config_form,
            },
            input::Input,
            menu_item::MenuItem,
            number_input::NumberInput,
            popup_menu::PopupMenu,
            AppIcon, Card, CustomDialog, IconButton, NoContent, Table, TableDefinition, TextButton,
        },
        context::ConfigContext,
    },
    config_field, config_field_bool, config_field_custom, config_field_empty, edit_field_bool, edit_field_number_u16,
    edit_field_text, generate_form_reducer, html_if,
    i18n::use_translation,
    use_default_form_reducer,
};
use shared::model::{
    ApiProxyConfigDto, ApiProxyServerInfoDto, ConfigApiDto, CredentialFieldGenDto, CredentialGenerationDto,
    CredentialSegmentDto, SortOrder, CREDENTIAL_LENGTH_MAX, CREDENTIAL_LENGTH_MIN, CREDENTIAL_MIXED_CHARS,
    CREDENTIAL_UPPER_CHARS,
};
use std::{rc::Rc, str::FromStr};
use yew::prelude::*;

crate::app::components::define_table_columns! {
    enum ServerColumn {
        Actions => ("actions", "TABLE_COLUMNS.ACTIONS") {
            can_hide: false,
            content: false,
            header_label: Some("LABEL.EMPTY"),
        },
        Name => ("name", "LABEL.NAME") { can_hide: false },
        Protocol => ("protocol", "LABEL.PROTOCOL"),
        Host => ("host", "LABEL.HOST"),
        Port => ("port", "LABEL.PORT"),
        Timezone => ("timezone", "LABEL.TIMEZONE"),
        Message => ("message", "LABEL.MESSAGE"),
        Path => ("path", "LABEL.PATH"),
    }
}

const LABEL_NAME: &str = "LABEL.NAME";
const LABEL_PROTOCOL: &str = "LABEL.PROTOCOL";
const LABEL_HOST: &str = "LABEL.HOST";
const LABEL_PORT: &str = "LABEL.PORT";
const LABEL_TIMEZONE: &str = "LABEL.TIMEZONE";
const LABEL_MESSAGE: &str = "LABEL.MESSAGE";
const LABEL_PATH: &str = "LABEL.PATH";
const LABEL_WEB_ROOT: &str = "LABEL.WEB_ROOT";
const LABEL_API_CONFIG: &str = "LABEL.API_CONFIG";
const LABEL_API_PROXY_CONFIG: &str = "LABEL.API_PROXY_CONFIG";
const LABEL_USE_USER_DB: &str = "LABEL.USE_USER_DB";
const LABEL_AUTH_ERROR_STATUS: &str = "LABEL.AUTH_ERROR_STATUS";
const LABEL_CREDENTIAL_GENERATION: &str = "LABEL.CREDENTIAL_GENERATION";
const LABEL_CREDENTIAL_LENGTH: &str = "LABEL.CREDENTIAL_LENGTH";
const LABEL_CREDENTIAL_CHARS: &str = "LABEL.CREDENTIAL_CHARS";
const LABEL_CREDENTIAL_EXCLUDE: &str = "LABEL.CREDENTIAL_EXCLUDE";
const LABEL_CREDENTIAL_ADD_SEGMENT: &str = "LABEL.CREDENTIAL_ADD_SEGMENT";
const LABEL_CREDENTIAL_REMOVE_SEGMENT: &str = "LABEL.CREDENTIAL_REMOVE_SEGMENT";
const LABEL_CREDENTIAL_SEGMENT: &str = "LABEL.CREDENTIAL_SEGMENT";
const LABEL_CREDENTIAL_DEFAULT: &str = "LABEL.CREDENTIAL_DEFAULT";
const LABEL_TOTAL: &str = "LABEL.TOTAL";
const LABEL_USERNAME: &str = "LABEL.USERNAME";
const LABEL_PASSWORD: &str = "LABEL.PASSWORD";
const LABEL_TOKEN: &str = "LABEL.TOKEN";
const LABEL_SERVER: &str = "LABEL.SERVER";
const LABEL_ADD_SERVER: &str = "LABEL.ADD_SERVER";
const MSG_NON_UNIQUE_SERVER_NAME: &str = "MESSAGES.SAVE.API_PROXY_CONFIG.NON_UNIQUE_SERVER_NAME";

#[derive(Debug, Clone, Copy, Eq, PartialEq, strum_macros::Display, strum_macros::EnumString)]
enum ServerTableAction {
    Delete,
    Edit,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ServerDialogMode {
    Add,
    Edit(usize),
}

fn build_default_server(existing_servers: &[ApiProxyServerInfoDto]) -> ApiProxyServerInfoDto {
    let mut index = existing_servers.len() + 1;
    loop {
        let name = format!("Server {index}");
        if !existing_servers.iter().any(|server| server.name == name) {
            return ApiProxyServerInfoDto {
                name,
                protocol: "http".to_string(),
                host: String::new(),
                port: None,
                timezone: "UTC".to_string(),
                message: "Welcome to Tuliprox".to_string(),
                path: None,
            };
        }
        index += 1;
    }
}

fn server_name_exists(servers: &[ApiProxyServerInfoDto], server_name: &str, ignore_index: Option<usize>) -> bool {
    servers.iter().enumerate().any(|(idx, server)| {
        if ignore_index.is_some_and(|ignore_idx| idx == ignore_idx) {
            false
        } else {
            server.name == server_name
        }
    })
}

fn make_field_handler<F>(server_dialog_form: &UseStateHandle<ApiProxyServerInfoDto>, updater: F) -> Callback<String>
where
    F: Fn(&mut ApiProxyServerInfoDto, String) + 'static,
{
    let server_dialog_form = server_dialog_form.clone();
    Callback::from(move |value: String| {
        let mut form = (*server_dialog_form).clone();
        updater(&mut form, value);
        server_dialog_form.set(form);
    })
}

generate_form_reducer!(
    state: ApiConfigFormState { form: ConfigApiDto },
    action_name: ApiConfigFormAction,
    fields {
        Host => host: String,
        Port => port: u16,
        WebRoot => web_root: String,
    }
);

generate_form_reducer!(
    state: ApiProxyConfigFormState { form: ApiProxyConfigDto },
    action_name: ApiProxyConfigFormAction,
    fields {
        Server => server: Vec<ApiProxyServerInfoDto>,
        UseUserDb => use_user_db: bool,
        AuthErrorStatus => auth_error_status: u16,
        CredentialGeneration => credential_generation: CredentialGenerationDto,
    }
);

type CredentialFieldSelector = fn(&mut CredentialGenerationDto) -> &mut CredentialFieldGenDto;

struct CredentialEditFieldItem {
    name: &'static str,
    label: &'static str,
    default_chars: &'static str,
    value: CredentialFieldGenDto,
    error: Option<String>,
    select: CredentialFieldSelector,
}

struct CredentialViewFieldItem<'a> {
    label: &'static str,
    default_chars: &'static str,
    value: &'a CredentialFieldGenDto,
}

#[derive(Properties, Clone, PartialEq)]
struct CredentialGenEditProps {
    title: String,
    name: &'static str,
    default_chars: &'static str,
    value: CredentialFieldGenDto,
    error: Option<String>,
    on_change: Callback<CredentialFieldGenDto>,
}

#[component]
fn CredentialGenEdit(props: &CredentialGenEditProps) -> Html {
    let translate = use_translation();
    let segments = props.value.to_segments();
    let count = segments.len();
    let name = props.name;
    let default_chars = props.default_chars;

    let total_length: u32 = segments.iter().map(|s| u32::from(s.length)).sum();

    let rows = segments.iter().enumerate().map(|(idx, segment)| {
        let on_length = {
            let (segments, on_change) = (segments.clone(), props.on_change.clone());
            Callback::from(move |length: Option<i64>| {
                let mut next = segments.clone();
                if let Some(seg) = next.get_mut(idx) {
                    seg.length = length.and_then(|l| u16::try_from(l).ok()).unwrap_or(0);
                    on_change.emit(CredentialFieldGenDto::from_segments(next));
                }
            })
        };
        let on_chars = {
            let (segments, on_change) = (segments.clone(), props.on_change.clone());
            Callback::from(move |chars: String| {
                let mut next = segments.clone();
                if let Some(seg) = next.get_mut(idx) {
                    seg.chars = chars;
                    on_change.emit(CredentialFieldGenDto::from_segments(next));
                }
            })
        };
        let on_exclude = {
            let (segments, on_change) = (segments.clone(), props.on_change.clone());
            Callback::from(move |exclude: String| {
                let mut next = segments.clone();
                if let Some(seg) = next.get_mut(idx) {
                    seg.exclude = exclude;
                    on_change.emit(CredentialFieldGenDto::from_segments(next));
                }
            })
        };
        let on_remove = {
            let (segments, on_change) = (segments.clone(), props.on_change.clone());
            Callback::from(move |(_name, _event): (String, MouseEvent)| {
                let mut next = segments.clone();
                if next.len() > 1 && idx < next.len() {
                    next.remove(idx);
                    on_change.emit(CredentialFieldGenDto::from_segments(next));
                }
            })
        };

        let placeholder_chars = format!("{} ({})", default_chars, translate.t(LABEL_CREDENTIAL_DEFAULT));

        html! {
            <div class="tp__api-config-view__credential-gen__segment" key={idx}>
                <div class="tp__api-config-view__credential-gen__segment-header">
                    <span class="tp__api-config-view__credential-gen__segment-title">
                        {format!("{} {}", translate.t(LABEL_CREDENTIAL_SEGMENT), idx + 1)}
                    </span>
                    { html_if!(count > 1, {
                        <IconButton
                            icon="Delete"
                            name={format!("{name}_{idx}_remove")}
                            hint={translate.t(LABEL_CREDENTIAL_REMOVE_SEGMENT)}
                            onclick={on_remove}
                        />
                    }) }
                </div>
                <div class="tp__api-config-view__credential-gen__segment-fields">
                    <div class="tp__form-field tp__form-field__number">
                        <NumberInput
                            label={translate.t(LABEL_CREDENTIAL_LENGTH)}
                            name={format!("{name}_{idx}_length")}
                            hint_key={Some("API_PROXY_CONFIG.CREDENTIAL_LENGTH".to_string())}
                            value={i64::from(segment.length)}
                            min_i64={i64::from(CREDENTIAL_LENGTH_MIN)}
                            max_i64={i64::from(CREDENTIAL_LENGTH_MAX)}
                            on_change={on_length}
                        />
                    </div>
                    <div class="tp__form-field tp__form-field__text">
                        <Input
                            label={translate.t(LABEL_CREDENTIAL_CHARS)}
                            name={format!("{name}_{idx}_chars")}
                            placeholder={placeholder_chars}
                            autocomplete={false}
                            hint_key={Some("API_PROXY_CONFIG.CREDENTIAL_CHARS".to_string())}
                            value={segment.chars.clone()}
                            on_change={Some(on_chars)}
                        />
                    </div>
                    <div class="tp__form-field tp__form-field__text">
                        <Input
                            label={translate.t(LABEL_CREDENTIAL_EXCLUDE)}
                            name={format!("{name}_{idx}_exclude")}
                            placeholder={"1lI0O".to_string()}
                            autocomplete={false}
                            hint_key={Some("API_PROXY_CONFIG.CREDENTIAL_EXCLUDE".to_string())}
                            value={segment.exclude.clone()}
                            on_change={Some(on_exclude)}
                        />
                    </div>
                </div>
            </div>
        }
    });

    let on_add = {
        let (segments, on_change) = (segments.clone(), props.on_change.clone());
        Callback::from(move |_: String| {
            let mut next = segments.clone();
            next.push(CredentialSegmentDto::default());
            on_change.emit(CredentialFieldGenDto::from_segments(next));
        })
    };

    html! {
        <div class="tp__api-config-view__credential-gen">
            <div class="tp__api-config-view__credential-gen__header">
                <div class="tp__api-config-view__section-title">{props.title.clone()}</div>
                <span class="tp__api-config-view__credential-gen__badge">
                    {format!("{}: {total_length}", translate.t(LABEL_TOTAL))}
                </span>
            </div>
            <div class="tp__api-config-view__credential-gen__segments">
                { for rows }
            </div>
            { props.error.as_ref().map_or_else(Html::default, |error| html! {
                <div class="tp__api-config-view__credential-gen-error" role="alert">{error.clone()}</div>
            }) }
            <div class="tp__api-config-view__credential-gen__actions">
                <TextButton
                    class="secondary"
                    name={format!("{name}_add")}
                    icon="Add"
                    title={translate.t(LABEL_CREDENTIAL_ADD_SEGMENT)}
                    onclick={on_add}
                />
            </div>
        </div>
    }
}

#[component]
pub fn ApiConfigView() -> Html {
    let translate = use_translation();
    let columns = use_memo((), |()| ServerColumn::columns());
    let config_ctx = use_context::<ConfigContext>().expect("Config context not found");
    let config_view_ctx = use_context::<ConfigViewContext>().expect("ConfigViewContext not found");

    let popup_anchor_ref = use_state(|| None::<web_sys::Element>);
    let popup_is_open = use_state(|| false);
    let selected_server_index = use_state(|| None::<usize>);

    let server_dialog_mode = use_state(|| None::<ServerDialogMode>);
    let server_dialog_form = use_state(ApiProxyServerInfoDto::default);
    let server_dialog_error = use_state(|| None::<String>);

    let form_state_api_config: UseReducerHandle<ApiConfigFormState> =
        use_default_form_reducer!(ApiConfigFormState { form: ConfigApiDto::default() });

    let form_state_api_proxy_config: UseReducerHandle<ApiProxyConfigFormState> =
        use_default_form_reducer!(ApiProxyConfigFormState { form: ApiProxyConfigDto::default() });

    {
        use_emit_config_form(&form_state_api_config, config_view_ctx.on_form_change.clone(), ConfigForm::Api);
    }

    {
        use_emit_config_form(
            &form_state_api_proxy_config,
            config_view_ctx.on_form_change.clone(),
            ConfigForm::ApiProxy,
        );
    }

    {
        let form_state_api_config = form_state_api_config.clone();
        let api_config = config_ctx.config.as_ref().map(|c| c.config.api.clone());
        let deps = (api_config, *config_view_ctx.edit_mode);
        use_effect_with(deps, move |(cfg, _mode)| {
            if let Some(api) = cfg {
                let mut next_api = api.clone();
                next_api.prepare();
                form_state_api_config.dispatch(ApiConfigFormAction::SetAll(next_api));
            } else {
                let mut defaults = ConfigApiDto::default();
                defaults.prepare();
                form_state_api_config.dispatch(ApiConfigFormAction::SetAll(defaults));
            }
            || ()
        });
    }

    {
        let form_state_api_proxy_config = form_state_api_proxy_config.clone();
        let api_proxy_config = config_ctx.api_proxy.clone();
        let deps = (api_proxy_config, *config_view_ctx.edit_mode);
        use_effect_with(deps, move |(cfg, _mode)| {
            if let Some(api) = cfg {
                form_state_api_proxy_config.dispatch(ApiProxyConfigFormAction::SetAll(api.as_ref().clone()));
            } else {
                form_state_api_proxy_config.dispatch(ApiProxyConfigFormAction::SetAll(ApiProxyConfigDto::default()));
            }
            || ()
        });
    }

    let edit_mode_ref = use_mut_ref(|| false);
    {
        let mode_ref = edit_mode_ref.clone();
        let deps = *config_view_ctx.edit_mode;
        use_effect_with(deps, move |mode| {
            *mode_ref.borrow_mut() = *mode;
        });
    }

    let handle_popup_close = {
        let popup_is_open = popup_is_open.clone();
        Callback::from(move |()| popup_is_open.set(false))
    };

    let handle_popup_onclick = {
        let popup_anchor_ref = popup_anchor_ref.clone();
        let popup_is_open = popup_is_open.clone();
        let selected_server_index = selected_server_index.clone();
        Callback::from(move |(row, event): (usize, MouseEvent)| {
            if let Some(target) = event.target_dyn_into::<web_sys::Element>() {
                selected_server_index.set(Some(row));
                popup_anchor_ref.set(Some(target));
                popup_is_open.set(true);
            }
        })
    };

    let handle_menu_click = {
        let popup_is_open = popup_is_open.clone();
        let selected_server_index = selected_server_index.clone();
        let form_state_api_proxy_config = form_state_api_proxy_config.clone();
        let server_dialog_mode = server_dialog_mode.clone();
        let server_dialog_form = server_dialog_form.clone();
        let server_dialog_error = server_dialog_error.clone();
        Callback::from(move |(name, _): (String, MouseEvent)| {
            if let Ok(action) = ServerTableAction::from_str(&name) {
                if let Some(index) = *selected_server_index {
                    match action {
                        ServerTableAction::Delete => {
                            let mut servers = form_state_api_proxy_config.form.server.clone();
                            if index < servers.len() {
                                servers.remove(index);
                                form_state_api_proxy_config.dispatch(ApiProxyConfigFormAction::Server(servers));
                            }
                        }
                        ServerTableAction::Edit => {
                            if let Some(server) = form_state_api_proxy_config.form.server.get(index) {
                                server_dialog_form.set(server.clone());
                                server_dialog_error.set(None);
                                server_dialog_mode.set(Some(ServerDialogMode::Edit(index)));
                            }
                        }
                    }
                }
            }
            popup_is_open.set(false);
        })
    };

    let handle_add_server = {
        let form_state_api_proxy_config = form_state_api_proxy_config.clone();
        let server_dialog_mode = server_dialog_mode.clone();
        let server_dialog_form = server_dialog_form.clone();
        let server_dialog_error = server_dialog_error.clone();
        Callback::from(move |_| {
            server_dialog_form.set(build_default_server(&form_state_api_proxy_config.form.server));
            server_dialog_error.set(None);
            server_dialog_mode.set(Some(ServerDialogMode::Add));
        })
    };

    let handle_server_dialog_close = {
        let server_dialog_mode = server_dialog_mode.clone();
        let server_dialog_error = server_dialog_error.clone();
        Callback::from(move |()| {
            server_dialog_error.set(None);
            server_dialog_mode.set(None);
        })
    };
    let handle_server_dialog_cancel = {
        let handle_server_dialog_close = handle_server_dialog_close.clone();
        Callback::from(move |_| handle_server_dialog_close.emit(()))
    };

    let handle_server_name_change = make_field_handler(&server_dialog_form, |form, value| form.name = value);
    let handle_server_protocol_change = make_field_handler(&server_dialog_form, |form, value| form.protocol = value);
    let handle_server_host_change = make_field_handler(&server_dialog_form, |form, value| form.host = value);
    let handle_server_port_change = make_field_handler(&server_dialog_form, |form, value| {
        form.port = if value.trim().is_empty() { None } else { Some(value) };
    });
    let handle_server_timezone_change = make_field_handler(&server_dialog_form, |form, value| form.timezone = value);
    let handle_server_message_change = make_field_handler(&server_dialog_form, |form, value| form.message = value);
    let handle_server_path_change = make_field_handler(&server_dialog_form, |form, value| {
        form.path = if value.trim().is_empty() { None } else { Some(value) };
    });

    let handle_server_dialog_save = {
        let server_dialog_mode = server_dialog_mode.clone();
        let server_dialog_form = server_dialog_form.clone();
        let server_dialog_error = server_dialog_error.clone();
        let form_state_api_proxy_config = form_state_api_proxy_config.clone();
        let translate = translate.clone();
        Callback::from(move |_| {
            let Some(dialog_mode) = *server_dialog_mode else {
                return;
            };

            let mut server = (*server_dialog_form).clone();
            if let Err(err) = server.prepare() {
                server_dialog_error.set(Some(err.to_string()));
                return;
            }

            let ignore_index = match dialog_mode {
                ServerDialogMode::Add => None,
                ServerDialogMode::Edit(index) => Some(index),
            };

            if server_name_exists(&form_state_api_proxy_config.form.server, &server.name, ignore_index) {
                let message = translate.t(MSG_NON_UNIQUE_SERVER_NAME).replace("{name}", &server.name);
                server_dialog_error.set(Some(message));
                return;
            }

            let mut servers = form_state_api_proxy_config.form.server.clone();
            match dialog_mode {
                ServerDialogMode::Add => servers.push(server),
                ServerDialogMode::Edit(index) => {
                    if let Some(server_to_update) = servers.get_mut(index) {
                        *server_to_update = server;
                    }
                }
            }

            form_state_api_proxy_config.dispatch(ApiProxyConfigFormAction::Server(servers));
            server_dialog_error.set(None);
            server_dialog_mode.set(None);
        })
    };

    let render_header_cell = {
        let translator = translate.clone();
        Callback::<usize, Html>::from(move |col| {
            html! {
                {
                    if let Some(column) = ServerColumn::from_index(col) {
                        translator.t(column.header_label())
                    } else {
                        String::new()
                    }
                }
            }
        })
    };

    let render_data_cell = {
        let popup_onclick = handle_popup_onclick.clone();
        let edit_mode_ref = edit_mode_ref.clone();
        Callback::<(usize, usize, Rc<ApiProxyServerInfoDto>), Html>::from(
            move |(row, col, dto): (usize, usize, Rc<ApiProxyServerInfoDto>)| match ServerColumn::from_index(col) {
                Some(ServerColumn::Actions) => {
                    let popup_onclick = popup_onclick.clone();
                    let edit_mode_ref = edit_mode_ref.clone();
                    html! {
                        <button
                            class="tp__icon-button"
                            onclick={Callback::from(move |event: MouseEvent| {
                                if *edit_mode_ref.borrow() {
                                    popup_onclick.emit((row, event));
                                }
                            })}
                            data-row={row.to_string()}
                        >
                            <AppIcon name="Popup"/>
                        </button>
                    }
                }
                Some(ServerColumn::Name) => html! {&dto.name},
                Some(ServerColumn::Protocol) => html! {&dto.protocol},
                Some(ServerColumn::Host) => html! {&dto.host},
                Some(ServerColumn::Port) => html! {dto.port.as_ref().map_or_else(String::new, ToString::to_string)},
                Some(ServerColumn::Timezone) => html! {&dto.timezone},
                Some(ServerColumn::Message) => html! {&dto.message},
                Some(ServerColumn::Path) => html! {dto.path.as_ref().map_or_else(String::new, ToString::to_string)},
                _ => html! {""},
            },
        )
    };

    let table_definition = {
        let columns = columns.clone();

        let on_sort = Callback::<Option<(usize, SortOrder)>, ()>::from(move |_args| {});
        let render_header_cell = render_header_cell.clone();
        let render_data_cell = render_data_cell.clone();

        let servers = form_state_api_proxy_config.form.server.clone();
        let translation_marker = translate.t(LABEL_SERVER);
        use_memo((servers, translation_marker), move |(servers, _translation_marker)| TableDefinition::<
            ApiProxyServerInfoDto,
        > {
            table_id: "config.api_servers".into(),
            columns: columns.clone(),
            row_key: Callback::from(|(_, item): (usize, Rc<ApiProxyServerInfoDto>)| item.name.clone().into()),
            items: if servers.is_empty() {
                None
            } else {
                Some(Rc::new(servers.iter().map(|server| Rc::new(server.clone())).collect()))
            },

            on_sort,
            render_header_cell: render_header_cell.clone(),
            render_data_cell: render_data_cell.clone(),
        })
    };

    let render_view_mode_api_config = || {
        if let Some(config) = &config_ctx.config {
            let mut api_config = config.config.api.clone();
            api_config.prepare();
            html! {
                <Card class="tp__api-config-card">
                    <div class="tp__api-config-view__section-title">{translate.t(LABEL_API_CONFIG)}</div>
                    { config_field!(api_config, translate.t(LABEL_HOST), host) }
                    { config_field!(api_config, translate.t(LABEL_PORT), port) }
                    { config_field!(api_config, translate.t(LABEL_WEB_ROOT), web_root) }
                </Card>
            }
        } else {
            html! {
                <Card class="tp__api-config-card">
                    <div class="tp__api-config-view__section-title">{translate.t(LABEL_API_CONFIG)}</div>
                    { config_field_empty!(translate.t(LABEL_HOST), "CONFIG_API.HOST") }
                    { config_field_empty!(translate.t(LABEL_PORT), "CONFIG_API.PORT") }
                    { config_field_empty!(translate.t(LABEL_WEB_ROOT), "CONFIG_API.WEB_ROOT") }
                </Card>
            }
        }
    };

    let render_edit_mode_api_config = || {
        html! {
            <Card class="tp__api-config-card">
                <div class="tp__api-config-view__section-title">{translate.t(LABEL_API_CONFIG)}</div>
                { edit_field_text!(form_state_api_config, translate.t(LABEL_HOST), host, ApiConfigFormAction::Host) }
                { edit_field_number_u16!(form_state_api_config, translate.t(LABEL_PORT), port, ApiConfigFormAction::Port) }
                { edit_field_text!(form_state_api_config, translate.t(LABEL_WEB_ROOT), web_root, ApiConfigFormAction::WebRoot) }
            </Card>
        }
    };

    let render_edit_mode_api_proxy_config = || {
        html! {
            <Card class="tp__api-config-card">
                <div class="tp__api-config-view__section-title">{translate.t(LABEL_API_PROXY_CONFIG)}</div>
                <div class="tp__api-config-view__section">
                    { edit_field_bool!(form_state_api_proxy_config, translate.t(LABEL_USE_USER_DB), use_user_db, ApiProxyConfigFormAction::UseUserDb) }
                    { edit_field_number_u16!(form_state_api_proxy_config, translate.t(LABEL_AUTH_ERROR_STATUS), auth_error_status, ApiProxyConfigFormAction::AuthErrorStatus) }
                </div>

                <div class="tp__api-config-view__section-header tp__list-list__header">
                    <div class="tp__api-config-view__section-title">{translate.t(LABEL_SERVER)}</div>
                    <TextButton class="primary" name="add_server" icon="Add" title={translate.t(LABEL_ADD_SERVER)} onclick={handle_add_server.clone()} />
                </div>
                <div class="tp__api-config-view__proxy-server tp__api-config-view__proxy-server__edit">
                    <Table::<ApiProxyServerInfoDto> definition={table_definition.clone()} />
                    <PopupMenu is_open={*popup_is_open} anchor_ref={(*popup_anchor_ref).clone()} on_close={handle_popup_close.clone()}>
                        <MenuItem
                            icon="Delete"
                            name={ServerTableAction::Delete.to_string()}
                            label={translate.t("LABEL.DELETE")}
                            onclick={handle_menu_click.clone()}
                            class="tp__delete_action"
                        />
                        <MenuItem
                            icon="Edit"
                            name={ServerTableAction::Edit.to_string()}
                            label={translate.t("LABEL.EDIT")}
                            onclick={handle_menu_click.clone()}
                        />
                    </PopupMenu>
                </div>
            </Card>
        }
    };

    let render_edit_mode_credential_generation = || {
        let generation = form_state_api_proxy_config.form.credential_generation.clone();
        let [username_error, password_error, token_error] = generation.field_errors();
        let fields = [
            CredentialEditFieldItem {
                name: "username",
                label: LABEL_USERNAME,
                default_chars: CREDENTIAL_UPPER_CHARS,
                value: generation.username.clone(),
                error: username_error,
                select: |g| &mut g.username,
            },
            CredentialEditFieldItem {
                name: "password",
                label: LABEL_PASSWORD,
                default_chars: CREDENTIAL_UPPER_CHARS,
                value: generation.password.clone(),
                error: password_error,
                select: |g| &mut g.password,
            },
            CredentialEditFieldItem {
                name: "token",
                label: LABEL_TOKEN,
                default_chars: CREDENTIAL_MIXED_CHARS,
                value: generation.token.clone(),
                error: token_error,
                select: |g| &mut g.token,
            },
        ];

        html! {
            <Card class="tp__api-config-card">
                <div class="tp__api-config-view__section-title">
                    {translate.t(LABEL_CREDENTIAL_GENERATION)}
                </div>
                <div class="tp__api-config-view__credential-gen-list">
                    {
                        fields.into_iter().map(|item| {
                            let (state, generation) = (form_state_api_proxy_config.clone(), generation.clone());
                            let select = item.select;
                            let on_change = Callback::from(move |next: CredentialFieldGenDto| {
                                let mut generation = generation.clone();
                                *select(&mut generation) = next;
                                state.dispatch(ApiProxyConfigFormAction::CredentialGeneration(generation));
                            });
                            html! {
                                <CredentialGenEdit
                                    key={item.name}
                                    name={item.name}
                                    title={translate.t(item.label)}
                                    default_chars={item.default_chars}
                                    value={item.value}
                                    error={item.error}
                                    {on_change}
                                />
                            }
                        }).collect::<Html>()
                    }
                </div>
            </Card>
        }
    };

    let render_view_mode_api_proxy_config = || {
        html! {
            <Card class="tp__api-config-card">
                <div class="tp__api-config-view__section-title">{translate.t(LABEL_API_PROXY_CONFIG)}</div>
                <div class="tp__api-config-view__section">
                    { config_field_bool!(form_state_api_proxy_config.form, translate.t(LABEL_USE_USER_DB), use_user_db) }
                    { config_field!(form_state_api_proxy_config.form, translate.t(LABEL_AUTH_ERROR_STATUS), auth_error_status) }
                </div>
                <div class="tp__api-config-view__section-title">{translate.t(LABEL_SERVER)}</div>
                <div class="tp__api-config-view__proxy-server tp__api-config-view__proxy-server__view">
                    {
                        if form_state_api_proxy_config.form.server.is_empty() {
                            html! {
                                <NoContent
                                    text={translate.t("MESSAGES.EMPTY_STATE.API_PROXY_SERVER_TITLE")}
                                    hint={translate.t("MESSAGES.EMPTY_STATE.API_PROXY_SERVER_HINT")}
                                />
                            }
                        } else {
                            html! { <Table::<ApiProxyServerInfoDto> definition={table_definition.clone()} /> }
                        }
                    }
                </div>
            </Card>
        }
    };

    let render_view_mode_credential_generation = || {
        let generation = &form_state_api_proxy_config.form.credential_generation;
        let fields = [
            CredentialViewFieldItem {
                label: LABEL_USERNAME,
                default_chars: CREDENTIAL_UPPER_CHARS,
                value: &generation.username,
            },
            CredentialViewFieldItem {
                label: LABEL_PASSWORD,
                default_chars: CREDENTIAL_UPPER_CHARS,
                value: &generation.password,
            },
            CredentialViewFieldItem {
                label: LABEL_TOKEN,
                default_chars: CREDENTIAL_MIXED_CHARS,
                value: &generation.token,
            },
        ];

        html! {
            <Card class="tp__api-config-card">
                <div class="tp__api-config-view__section-title">
                    {translate.t(LABEL_CREDENTIAL_GENERATION)}
                </div>
                <div class="tp__api-config-view__credential-gen-list">
                    {
                        fields.into_iter().map(|item| {
                            let segments = item.value.to_segments();
                            let total_length: u32 = segments.iter().map(|s| u32::from(s.length)).sum();
                            let default_chars = item.default_chars;
                            html! {
                                <div class="tp__api-config-view__credential-gen" key={item.label}>
                                    <div class="tp__api-config-view__credential-gen__header">
                                        <div class="tp__api-config-view__section-title">{translate.t(item.label)}</div>
                                        <span class="tp__api-config-view__credential-gen__badge">
                                            {format!("{}: {total_length}", translate.t(LABEL_TOTAL))}
                                        </span>
                                    </div>
                                    <div class="tp__api-config-view__credential-gen__segments">
                                        { for segments.iter().enumerate().map(|(idx, segment)| {
                                            let chars_display = if segment.chars.is_empty() {
                                                format!("{} ({})", default_chars, translate.t(LABEL_CREDENTIAL_DEFAULT))
                                            } else {
                                                segment.chars.clone()
                                            };
                                            let exclude_display = if segment.exclude.is_empty() {
                                                "-".to_string()
                                            } else {
                                                segment.exclude.clone()
                                            };
                                            html! {
                                                <div class="tp__api-config-view__credential-gen__segment" key={idx}>
                                                    <div class="tp__api-config-view__credential-gen__segment-header">
                                                        <span class="tp__api-config-view__credential-gen__segment-title">
                                                            {format!("{} {}", translate.t(LABEL_CREDENTIAL_SEGMENT), idx + 1)}
                                                        </span>
                                                    </div>
                                                    <div class="tp__api-config-view__credential-gen__segment-fields">
                                                        { config_field!(segment, translate.t(LABEL_CREDENTIAL_LENGTH), length, Some("API_PROXY_CONFIG.CREDENTIAL_LENGTH".to_string())) }
                                                        { config_field_custom!(translate.t(LABEL_CREDENTIAL_CHARS), chars_display, Some("API_PROXY_CONFIG.CREDENTIAL_CHARS".to_string())) }
                                                        { config_field_custom!(translate.t(LABEL_CREDENTIAL_EXCLUDE), exclude_display, Some("API_PROXY_CONFIG.CREDENTIAL_EXCLUDE".to_string())) }
                                                    </div>
                                                </div>
                                            }
                                        }) }
                                    </div>
                                </div>
                            }
                        }).collect::<Html>()
                    }
                </div>
            </Card>
        }
    };

    let server_dialog_html = if let Some(mode) = *server_dialog_mode {
        let title = match mode {
            ServerDialogMode::Add => translate.t(LABEL_ADD_SERVER),
            ServerDialogMode::Edit(_) => format!("{} {}", translate.t("LABEL.EDIT"), translate.t(LABEL_SERVER)),
        };

        html! {
            <CustomDialog
                open={true}
                class={Some("tp__api-server-dialog".to_string())}
                modal={true}
                close_on_backdrop_click={false}
                on_close={Some(handle_server_dialog_close.clone())}
            >
                <h2>{title}</h2>
                <div class="tp__api-server-dialog__body">
                    <div class="tp__api-server-dialog__grid">
                        <Input
                            name="api_proxy_server_name"
                            field_id={Some("API_PROXY_SERVER_INFO.NAME".to_string())}
                            label={Some(translate.t(LABEL_NAME))}
                            value={server_dialog_form.name.clone()}
                            on_change={Some(handle_server_name_change.clone())}
                        />
                        <Input
                            name="api_proxy_server_protocol"
                            field_id={Some("API_PROXY_SERVER_INFO.PROTOCOL".to_string())}
                            label={Some(translate.t(LABEL_PROTOCOL))}
                            value={server_dialog_form.protocol.clone()}
                            on_change={Some(handle_server_protocol_change.clone())}
                        />
                        <Input
                            name="api_proxy_server_host"
                            field_id={Some("API_PROXY_SERVER_INFO.HOST".to_string())}
                            label={Some(translate.t(LABEL_HOST))}
                            value={server_dialog_form.host.clone()}
                            on_change={Some(handle_server_host_change.clone())}
                        />
                        <Input
                            name="api_proxy_server_port"
                            field_id={Some("API_PROXY_SERVER_INFO.PORT".to_string())}
                            label={Some(translate.t(LABEL_PORT))}
                            value={server_dialog_form.port.clone().unwrap_or_default()}
                            on_change={Some(handle_server_port_change.clone())}
                        />
                        <Input
                            name="api_proxy_server_timezone"
                            field_id={Some("API_PROXY_SERVER_INFO.TIMEZONE".to_string())}
                            label={Some(translate.t(LABEL_TIMEZONE))}
                            value={server_dialog_form.timezone.clone()}
                            on_change={Some(handle_server_timezone_change.clone())}
                        />
                        <Input
                            name="api_proxy_server_path"
                            field_id={Some("API_PROXY_SERVER_INFO.PATH".to_string())}
                            label={Some(translate.t(LABEL_PATH))}
                            value={server_dialog_form.path.clone().unwrap_or_default()}
                            on_change={Some(handle_server_path_change.clone())}
                        />
                        <div class="tp__api-server-dialog__message">
                            <Input
                                name="api_proxy_server_message"
                                field_id={Some("API_PROXY_SERVER_INFO.MESSAGE".to_string())}
                                label={Some(translate.t(LABEL_MESSAGE))}
                                value={server_dialog_form.message.clone()}
                                on_change={Some(handle_server_message_change.clone())}
                            />
                        </div>
                    </div>
                    {
                        if let Some(error) = (*server_dialog_error).as_ref() {
                            html! {
                                <div class="tp__webui-config-view__info tp__config-view-page__info">
                                    <span class="error">{error.clone()}</span>
                                </div>
                            }
                        } else {
                            html! {}
                        }
                    }
                </div>
                <div class="tp__dialog__toolbar">
                    <TextButton
                        class="secondary"
                        name="cancel_server_dialog"
                        icon="Cancel"
                        title={translate.t("LABEL.CANCEL")}
                        onclick={handle_server_dialog_cancel.clone()}
                    />
                    <TextButton
                        class="primary"
                        name="save_server_dialog"
                        icon="Save"
                        title={translate.t("LABEL.SAVE")}
                        onclick={handle_server_dialog_save.clone()}
                    />
                </div>
            </CustomDialog>
        }
    } else {
        html! {}
    };

    html! {
        <div class="tp__api-config-view tp__config-view-page">
            <div class="tp__config-view-page__title">{translate.t(ConfigPage::Api.title_key())}</div>
            {
                html_if!(*config_view_ctx.edit_mode && config_view_ctx.show_restart_notice, {
                    <div class="tp__webui-config-view__info tp__config-view-page__info">
                        <AppIcon name="Warn"/>
                        <span class="info">{translate.t("INFO.RESTART_TO_APPLY_CHANGES")}</span>
                    </div>
                })
            }
            <div class="tp__api-config-view__body tp__config-view-page__body">
                {
                    if *config_view_ctx.edit_mode {
                        html! {
                            <>
                                <div class="tp__api-config-view__section tp__config-view-page__body">
                                    { render_edit_mode_api_config() }
                                </div>
                                <div class="tp__api-config-view__section tp__config-view-page__body">
                                    { render_edit_mode_api_proxy_config() }
                                </div>
                                <div class="tp__api-config-view__section tp__config-view-page__body">
                                    { render_edit_mode_credential_generation() }
                                </div>
                            </>
                        }
                    } else {
                        html! {
                            <>
                                <div class="tp__api-config-view__section">
                                    { render_view_mode_api_config() }
                                </div>
                                <div class="tp__api-config-view__section">
                                    { render_view_mode_api_proxy_config() }
                                </div>
                                <div class="tp__api-config-view__section">
                                    { render_view_mode_credential_generation() }
                                </div>
                            </>
                        }
                    }
                }
            </div>
            {server_dialog_html}
        </div>
    }
}
