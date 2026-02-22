use crate::{
    app::{
        components::{
            config::{
                config_page::{ConfigForm, LABEL_API_CONFIG},
                config_view_context::ConfigViewContext,
            },
            input::Input,
            menu_item::MenuItem,
            popup_menu::PopupMenu,
            AppIcon, Card, CustomDialog, NoContent, Table, TableDefinition, TextButton,
        },
        context::ConfigContext,
    },
    config_field, config_field_bool, config_field_empty, edit_field_bool, edit_field_number_u16, edit_field_text,
    generate_form_reducer, html_if,
};
use shared::{
    concat_string,
    error::TuliproxError,
    info_err_res,
    model::{ApiProxyConfigDto, ApiProxyServerInfoDto, ConfigApiDto, SortOrder},
};
use std::{fmt::Display, rc::Rc, str::FromStr};
use yew::prelude::*;
use yew_i18n::use_translation;

const LABEL_NAME: &str = "LABEL.NAME";
const LABEL_PROTOCOL: &str = "LABEL.PROTOCOL";
const LABEL_HOST: &str = "LABEL.HOST";
const LABEL_PORT: &str = "LABEL.PORT";
const LABEL_TIMEZONE: &str = "LABEL.TIMEZONE";
const LABEL_MESSAGE: &str = "LABEL.MESSAGE";
const LABEL_PATH: &str = "LABEL.PATH";
const LABEL_WEB_ROOT: &str = "LABEL.WEB_ROOT";
const LABEL_API_PROXY_CONFIG: &str = "LABEL.API_PROXY_CONFIG";
const LABEL_USE_USER_DB: &str = "LABEL.USE_USER_DB";
const LABEL_SERVER: &str = "LABEL.SERVER";
const LABEL_ADD_SERVER: &str = "LABEL.ADD_SERVER";

const SERVER_HEADERS: [&str; 8] = ["EMPTY", "NAME", "PROTOCOL", "HOST", "PORT", "TIMEZONE", "MESSAGE", "PATH"];

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ServerTableAction {
    Delete,
    Edit,
}

impl Display for ServerTableAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Delete => "Delete",
                Self::Edit => "Edit",
            }
        )
    }
}

impl FromStr for ServerTableAction {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        if s.eq("Delete") {
            Ok(Self::Delete)
        } else if s.eq("Edit") {
            Ok(Self::Edit)
        } else {
            info_err_res!("Unknown Server Action: {}", s)
        }
    }
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
    }
);

#[function_component]
pub fn ApiConfigView() -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("Config context not found");
    let config_view_ctx = use_context::<ConfigViewContext>().expect("ConfigViewContext not found");

    let popup_anchor_ref = use_state(|| None::<web_sys::Element>);
    let popup_is_open = use_state(|| false);
    let selected_server_index = use_state(|| None::<usize>);

    let server_dialog_mode = use_state(|| None::<ServerDialogMode>);
    let server_dialog_form = use_state(ApiProxyServerInfoDto::default);
    let server_dialog_error = use_state(|| None::<String>);

    let form_state_api_config: UseReducerHandle<ApiConfigFormState> =
        use_reducer(|| ApiConfigFormState { form: ConfigApiDto::default(), modified: false });

    let form_state_api_proxy_config: UseReducerHandle<ApiProxyConfigFormState> =
        use_reducer(|| ApiProxyConfigFormState { form: ApiProxyConfigDto::default(), modified: false });

    {
        let on_form_change = config_view_ctx.on_form_change.clone();
        let deps = (form_state_api_config.clone(), form_state_api_config.modified);
        use_effect_with(deps, move |(state, modified)| {
            on_form_change.emit(ConfigForm::Api(*modified, state.form.clone()));
            || ()
        });
    }

    {
        let on_form_change = config_view_ctx.on_form_change.clone();
        let deps = (form_state_api_proxy_config.clone(), form_state_api_proxy_config.modified);
        use_effect_with(deps, move |(state, modified)| {
            on_form_change.emit(ConfigForm::ApiProxy(*modified, state.form.clone()));
            || ()
        });
    }

    {
        let form_state_api_config = form_state_api_config.clone();
        let api_config = config_ctx.config.as_ref().map(|c| c.config.api.clone());
        let deps = (api_config, *config_view_ctx.edit_mode);
        use_effect_with(deps, move |(cfg, _mode)| {
            if let Some(api) = cfg {
                form_state_api_config.dispatch(ApiConfigFormAction::SetAll(api.clone()));
            } else {
                form_state_api_config.dispatch(ApiConfigFormAction::SetAll(ConfigApiDto::default()));
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
        let deps = config_view_ctx.edit_mode.clone();
        use_effect_with(deps, move |mode| {
            *mode_ref.borrow_mut() = **mode;
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
                server_dialog_error.set(Some(format!("Non-unique server info name found {}", server.name)));
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
                    if col < SERVER_HEADERS.len() {
                        translator.t(&concat_string!("LABEL.", SERVER_HEADERS[col]))
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
            move |(row, col, dto): (usize, usize, Rc<ApiProxyServerInfoDto>)| match SERVER_HEADERS[col] {
                "EMPTY" => {
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
                "NAME" => html! {&dto.name},
                "PROTOCOL" => html! {&dto.protocol},
                "HOST" => html! {&dto.host},
                "PORT" => html! {dto.port.as_ref().map_or_else(String::new, ToString::to_string)},
                "TIMEZONE" => html! {&dto.timezone},
                "MESSAGE" => html! {&dto.message},
                "PATH" => html! {dto.path.as_ref().map_or_else(String::new, ToString::to_string)},
                _ => html! {""},
            },
        )
    };

    let table_definition = {
        let is_sortable = Callback::<usize, bool>::from(move |_col| false);
        let on_sort = Callback::<Option<(usize, SortOrder)>, ()>::from(move |_args| {});
        let render_header_cell = render_header_cell.clone();
        let render_data_cell = render_data_cell.clone();
        let num_cols = SERVER_HEADERS.len();
        let servers = form_state_api_proxy_config.form.server.clone();
        use_memo(servers, move |servers| TableDefinition::<ApiProxyServerInfoDto> {
            items: if servers.is_empty() {
                None
            } else {
                Some(Rc::new(servers.iter().map(|server| Rc::new(server.clone())).collect()))
            },
            num_cols,
            is_sortable,
            on_sort,
            render_header_cell: render_header_cell.clone(),
            render_data_cell: render_data_cell.clone(),
        })
    };

    let render_view_mode_api_config = || {
        if let Some(config) = &config_ctx.config {
            html! {
                <Card class="tp__api-config-card">
                    { config_field!(config.config.api, translate.t(LABEL_HOST), host) }
                    { config_field!(config.config.api, translate.t(LABEL_PORT), port) }
                    { config_field!(config.config.api, translate.t(LABEL_WEB_ROOT), web_root) }
                </Card>
            }
        } else {
            html! {
                <Card class="tp__api-config-card">
                    { config_field_empty!(translate.t(LABEL_HOST)) }
                    { config_field_empty!(translate.t(LABEL_PORT)) }
                    { config_field_empty!(translate.t(LABEL_WEB_ROOT)) }
                </Card>
            }
        }
    };

    let render_edit_mode_api_config = || {
        html! {
            <Card class="tp__api-config-card">
                { edit_field_text!(form_state_api_config, translate.t(LABEL_HOST), host, ApiConfigFormAction::Host) }
                { edit_field_number_u16!(form_state_api_config, translate.t(LABEL_PORT), port, ApiConfigFormAction::Port) }
                { edit_field_text!(form_state_api_config, translate.t(LABEL_WEB_ROOT), web_root, ApiConfigFormAction::WebRoot) }
            </Card>
        }
    };

    let render_edit_mode_api_proxy_config = || {
        html! {
            <Card class="tp__api-config-card">
                <div class="tp__config-view-page__title tp__api-config-view__section-title">{translate.t(LABEL_API_PROXY_CONFIG)}</div>
                <div class="tp__api-config-section">
                    { edit_field_bool!(form_state_api_proxy_config, translate.t(LABEL_USE_USER_DB), use_user_db, ApiProxyConfigFormAction::UseUserDb) }
                </div>

                <div class="tp__api-config-section-header tp__list-list__header">
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

    let render_view_mode_api_proxy_config = || {
        html! {
            <Card class="tp__api-config-card">
                <div class="tp__config-view-page__title tp__api-config-view__section-title">{translate.t(LABEL_API_PROXY_CONFIG)}</div>
                <div class="tp__api-config-section">
                    { config_field_bool!(form_state_api_proxy_config.form, translate.t(LABEL_USE_USER_DB), use_user_db) }
                </div>
                <div class="tp__api-config-view__section-title">{translate.t(LABEL_SERVER)}</div>
                <div class="tp__api-config-view__proxy-server tp__api-config-view__proxy-server__view">
                    {
                        if form_state_api_proxy_config.form.server.is_empty() {
                            html! { <NoContent /> }
                        } else {
                            html! { <Table::<ApiProxyServerInfoDto> definition={table_definition.clone()} /> }
                        }
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
                close_on_backdrop_click={true}
                on_close={Some(handle_server_dialog_close.clone())}
            >
                <h2>{title}</h2>
                <div class="tp__api-server-dialog__body">
                    <div class="tp__api-server-dialog__grid">
                        <Input
                            name="api_proxy_server_name"
                            label={Some(translate.t(LABEL_NAME))}
                            value={server_dialog_form.name.clone()}
                            on_change={Some(handle_server_name_change.clone())}
                        />
                        <Input
                            name="api_proxy_server_protocol"
                            label={Some(translate.t(LABEL_PROTOCOL))}
                            value={server_dialog_form.protocol.clone()}
                            on_change={Some(handle_server_protocol_change.clone())}
                        />
                        <Input
                            name="api_proxy_server_host"
                            label={Some(translate.t(LABEL_HOST))}
                            value={server_dialog_form.host.clone()}
                            on_change={Some(handle_server_host_change.clone())}
                        />
                        <Input
                            name="api_proxy_server_port"
                            label={Some(translate.t(LABEL_PORT))}
                            value={server_dialog_form.port.clone().unwrap_or_default()}
                            on_change={Some(handle_server_port_change.clone())}
                        />
                        <Input
                            name="api_proxy_server_timezone"
                            label={Some(translate.t(LABEL_TIMEZONE))}
                            value={server_dialog_form.timezone.clone()}
                            on_change={Some(handle_server_timezone_change.clone())}
                        />
                        <Input
                            name="api_proxy_server_path"
                            label={Some(translate.t(LABEL_PATH))}
                            value={server_dialog_form.path.clone().unwrap_or_default()}
                            on_change={Some(handle_server_path_change.clone())}
                        />
                        <div class="tp__api-server-dialog__message">
                            <Input
                                name="api_proxy_server_message"
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
            <div class="tp__config-view-page__title">{translate.t(LABEL_API_CONFIG)}</div>
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
                            </>
                        }
                    }
                }
            </div>
            {server_dialog_html}
        </div>
    }
}
