use std::fmt::Display;
use crate::app::components::config::config_page::{ConfigForm, LABEL_API_CONFIG};
use crate::app::components::config::config_view_context::ConfigViewContext;
use crate::app::components::{AppIcon, Card, NoContent, Table, TableDefinition, TextButton};
use crate::app::context::ConfigContext;
use crate::{config_field, config_field_bool, config_field_empty, edit_field_bool, edit_field_number_u16,
            edit_field_text, generate_form_reducer, html_if};
use shared::model::{ApiProxyConfigDto, ApiProxyServerInfoDto, ConfigApiDto, SortOrder};
use std::rc::Rc;
use std::str::FromStr;
use yew::prelude::*;
use yew_i18n::use_translation;
use shared::{concat_string, info_err_res};
use shared::error::TuliproxError;
use crate::app::components::menu_item::MenuItem;
use crate::app::components::popup_menu::PopupMenu;

const LABEL_HOST: &str = "LABEL.HOST";
const LABEL_PORT: &str = "LABEL.PORT";
const LABEL_WEB_ROOT: &str = "LABEL.WEB_ROOT";
const LABEL_API_PROXY_CONFIG: &str = "LABEL.API_PROXY_CONFIG";
const LABEL_USE_USER_DB: &str = "LABEL.USE_USER_DB";
// const LABEL_NAME: &str = "LABEL.NAME";
// const LABEL_PROTOCOL: &str = "LABEL.PROTOCOL";
// const LABEL_TIMEZONE: &str = "LABEL.TIMEZONE";
// const LABEL_MESSAGE: &str = "LABEL.MESSAGE";
// const LABEL_PATH: &str = "LABEL.PATH";
const LABEL_SERVER: &str = "LABEL.SERVER";
const LABEL_ADD_SERVER: &str = "LABEL.ADD_SERVER";

const SERVER_HEADERS: [&str; 8] = [
    "EMPTY",
    "NAME",
    "PROTOCOL",
    "HOST",
    "PORT",
    "TIMEZONE",
    "MESSAGE",
    "PATH",
];


#[derive(Debug, Clone, Eq, PartialEq)]
enum ServerTableAction {
    Delete,
    Edit,
}

impl Display for ServerTableAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", match self {
            Self::Delete => "Delete",
            Self::Edit => "Edit",
        })
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

// Generate form reducer for edit mode
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
    let selected_dto = use_state(|| None::<Rc<ApiProxyServerInfoDto>>);

    let form_state_api_config: UseReducerHandle<ApiConfigFormState> = use_reducer(|| {
        ApiConfigFormState { form: ConfigApiDto::default(), modified: false }
    });

    let form_state_api_proxy_config: UseReducerHandle<ApiProxyConfigFormState> = use_reducer(|| {
        ApiProxyConfigFormState { form: ApiProxyConfigDto::default(), modified: false }
    });

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
        let api_config = config_ctx
            .config
            .as_ref()
            .map(|c| c.config.api.clone());

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

    let edit_mode_ref = use_mut_ref(|| false);
    {
        let mode_ref = edit_mode_ref.clone();
        let deps = config_view_ctx.edit_mode.clone();
        use_effect_with(deps, move |mode| {
            *mode_ref.borrow_mut() = **mode;
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


    let handle_popup_close = {
        let set_is_open = popup_is_open.clone();
        Callback::from(move |()| {
            set_is_open.set(false);
        })
    };

    let handle_popup_onclick = {
        let set_selected_dto = selected_dto.clone();
        let set_anchor_ref = popup_anchor_ref.clone();
        let set_is_open = popup_is_open.clone();
        Callback::from(move |(dto, event): (Rc<ApiProxyServerInfoDto>, MouseEvent)| {
            if let Some(server) = event.target_dyn_into::<web_sys::Element>() {
                set_selected_dto.set(Some(dto.clone()));
                set_anchor_ref.set(Some(server));
                set_is_open.set(true);
            }
        })
    };

    let handle_menu_click = {
        let popup_is_open_state = popup_is_open.clone();
        let selected_dto = selected_dto.clone();
        Callback::from(move |(name, _): (String, _)| {
            if let Ok(action) = ServerTableAction::from_str(&name) {
                match action {
                    ServerTableAction::Delete => {
                        if let Some(_dto) = (*selected_dto).as_ref() {
                            // TODO
                        }
                    }
                    ServerTableAction::Edit => {
                        if let Some(_dto) = &*selected_dto {
                            // TODO
                        }
                    }
                }
            }
            popup_is_open_state.set(false);
        })
    };

    let render_data_cell = {
        let popup_onclick = handle_popup_onclick.clone();
        let edit_mode_ref = edit_mode_ref.clone();
        Callback::<(usize, usize, Rc<ApiProxyServerInfoDto>), Html>::from(
            move |(row, col, dto): (usize, usize, Rc<ApiProxyServerInfoDto>)| {
                match SERVER_HEADERS[col] {
                    "EMPTY" => {
                      let popup_onclick = popup_onclick.clone();
                      let edit_mode_ref = edit_mode_ref.clone();
                      html! {
                        <button class="tp__icon-button"
                            onclick={Callback::from(move |event: MouseEvent| if *edit_mode_ref.borrow() {
                              popup_onclick.emit((dto.clone(), event))})}
                            data-row={row.to_string()}>
                            <AppIcon name="Popup"></AppIcon>
                        </button>
                      }
                    }
                    "NAME" => html! {&dto.name},
                    "PROTOCOL" => html! {&dto.protocol},
                    "HOST" => html! {&dto.host},
                    "PORT" => html! {&dto.port.as_ref().map_or_else(String::new, ToString::to_string)},
                    "TIMEZONE" => html! {&dto.timezone},
                    "MESSAGE" => html! {&dto.message},
                    "PATH" => html! {&dto.path.as_ref().map_or_else(String::new, ToString::to_string)},
                    _ => html! {""},
                }
            })
    };

    let table_definition = {
        let is_sortable = Callback::<usize, bool>::from(move |_col| false);
        let on_sort = Callback::<Option<(usize, SortOrder)>, ()>::from(move |_args| {});
        // first register for config update
        let render_header_cell_cb = render_header_cell.clone();
        let render_data_cell_cb = render_data_cell.clone();
        let num_cols = SERVER_HEADERS.len();
        use_memo(config_ctx.api_proxy.clone(), move |config| {
            config.as_ref().map(|api_proxy|
                Rc::new(TableDefinition::<ApiProxyServerInfoDto> {
                    items: if api_proxy.server.is_empty() { None } else { Some(Rc::new(api_proxy.server.iter().map(|s| Rc::new(s.clone())).collect())) },
                    num_cols,
                    is_sortable,
                    on_sort,
                    render_header_cell: render_header_cell_cb,
                    render_data_cell: render_data_cell_cb,
                }))
        })
    };

    // let render_server = |idx: usize, server: ApiProxyServerInfoDto, edit_mode: bool| -> Html {
    //     let form_state_api_proxy_config = form_state_api_proxy_config.clone();
    //     let translate = translate.clone();
    //
    //     let on_change = {
    //         let form_state_api_proxy_config = form_state_api_proxy_config.clone();
    //         Callback::from(move |new_server: ApiProxyServerInfoDto| {
    //             let mut servers = form_state_api_proxy_config.form.server.clone();
    //             if let Some(s) = servers.get_mut(idx) {
    //                 *s = new_server;
    //                 form_state_api_proxy_config.dispatch(ApiProxyConfigFormAction::Server(servers));
    //             }
    //         })
    //     };
    //
    //     let on_remove = {
    //         let form_state_api_proxy_config = form_state_api_proxy_config.clone();
    //         Callback::from(move |_| {
    //             let mut servers = form_state_api_proxy_config.form.server.clone();
    //             servers.remove(idx);
    //             form_state_api_proxy_config.dispatch(ApiProxyConfigFormAction::Server(servers));
    //         })
    //     };
    //
    //     html! {
    //         <Card class="tp__api-proxy-server-card">
    //             <div class="tp__api-proxy-server-card__header">
    //                 <h3>{ &server.name }</h3>
    //                 {html_if!(edit_mode, {
    //                     <TextButton class="danger" name="delete_server" icon="Trash" title={translate.t("LABEL.DELETE")} onclick={on_remove} />
    //                 })}
    //             </div>
    //             <div class="tp__api-proxy-server-card__body">
    //                 {if edit_mode {
    //                     let s = server.clone();
    //                     let oc = on_change.clone();
    //                     html! {
    //                         <>
    //                             <div class="tp__form-field tp__form-field__text">
    //                                 <Input label={translate.t(LABEL_NAME)} value={s.name.clone()}
    //                                     on_change={
    //                                         let s = s.clone(); let oc = oc.clone();
    //                                         Callback::from(move |v| { let mut s = s.clone(); s.name = v; oc.emit(s); })
    //                                     } />
    //                             </div>
    //                             <div class="tp__form-field tp__form-field__text">
    //                                 <Input label={translate.t(LABEL_PROTOCOL)} value={s.protocol.clone()}
    //                                     on_change={
    //                                         let s = s.clone(); let oc = oc.clone();
    //                                         Callback::from(move |v| { let mut s = s.clone(); s.protocol = v; oc.emit(s); })
    //                                     } />
    //                             </div>
    //                             <div class="tp__form-field tp__form-field__text">
    //                                 <crate::app::components::input::Input label={translate.t(LABEL_HOST)} value={s.host.clone()}
    //                                     on_change={
    //                                         let s = s.clone(); let oc = oc.clone();
    //                                         Callback::from(move |v| { let mut s = s.clone(); s.host = v; oc.emit(s); })
    //                                     } />
    //                             </div>
    //                             <div class="tp__form-field tp__form-field__text">
    //                                 <Input label={translate.t(LABEL_PORT)} value={s.port.clone().unwrap_or_default()}
    //                                     on_change={
    //                                         let s = s.clone(); let oc = oc.clone();
    //                                         Callback::from(move |v: String| { let mut s = s.clone(); s.port = if v.is_empty() { None } else { Some(v) }; oc.emit(s); })
    //                                     } />
    //                             </div>
    //                             <div class="tp__form-field tp__form-field__text">
    //                                 <Input label={translate.t(LABEL_TIMEZONE)} value={s.timezone.clone()}
    //                                     on_change={
    //                                         let s = s.clone(); let oc = oc.clone();
    //                                         Callback::from(move |v: String| { let mut s = s.clone(); s.timezone = v; oc.emit(s); })
    //                                     } />
    //                             </div>
    //                             <div class="tp__form-field tp__form-field__text">
    //                                 <crate::app::components::input::Input label={translate.t(LABEL_MESSAGE)} value={s.message.clone()}
    //                                     on_change={
    //                                         let s = s.clone(); let oc = oc.clone();
    //                                         Callback::from(move |v| { let mut s = s.clone(); s.message = v; oc.emit(s); })
    //                                     } />
    //                             </div>
    //                             <div class="tp__form-field tp__form-field__text">
    //                                 <crate::app::components::input::Input label={translate.t(LABEL_PATH)} value={s.path.clone().unwrap_or_default()}
    //                                     on_change={
    //                                         let s = s.clone(); let oc = oc.clone();
    //                                         Callback::from(move |v: String| { let mut s = s.clone(); s.path = if v.is_empty() { None } else { Some(v) }; oc.emit(s); })
    //                                     } />
    //                             </div>
    //                         </>
    //                     }
    //                 } else {
    //                     html! {
    //                         <>
    //                             { config_field!(server, translate.t(LABEL_NAME), name) }
    //                             { config_field!(server, translate.t(LABEL_PROTOCOL), protocol) }
    //                             { config_field!(server, translate.t(LABEL_HOST), host) }
    //                             { config_field_custom!(translate.t(LABEL_PORT), server.port.clone().unwrap_or_default()) }
    //                             { config_field!(server, translate.t(LABEL_TIMEZONE), timezone) }
    //                             { config_field!(server, translate.t(LABEL_MESSAGE), message) }
    //                             { config_field_custom!(translate.t(LABEL_PATH), server.path.clone().unwrap_or_default()) }
    //                         </>
    //                     }
    //                 }}
    //             </div>
    //         </Card>
    //     }
    // };

    let handle_add_server = {
        let form_state_api_proxy_config = form_state_api_proxy_config.clone();
        Callback::from(move |_| {
            let mut servers = form_state_api_proxy_config.form.server.clone();
            servers.push(ApiProxyServerInfoDto {
                name: "New Server".to_string(),
                protocol: "http".to_string(),
                host: "".to_string(),
                port: None,
                timezone: "UTC".to_string(),
                message: "Welcome to Tuliprox".to_string(),
                path: None,
            });
            form_state_api_proxy_config.dispatch(ApiProxyConfigFormAction::Server(servers));
        })
    };

    let render_edit_mode_api_proxy_config = || {
        html! {
            <Card class="tp__api-config-card">
                <div class="tp__webui-config-view__info tp__config-view-page__info">
                    <AppIcon name="Warn"/> <span class="info">{"This part is `Work in progress`. Feature is not implemented"}</span>
                </div>

                <div class="tp__config-view-page__title tp__api-config-view__section-title">{translate.t(LABEL_API_PROXY_CONFIG)}</div>
                <div class="tp__api-config-section">
                    { edit_field_bool!(form_state_api_proxy_config, translate.t(LABEL_USE_USER_DB), use_user_db, ApiProxyConfigFormAction::UseUserDb) }
                </div>

                <div class="tp__api-config-section-header tp__list-list__header">
                    <div class="tp__api-config-view__section-title">{translate.t(LABEL_SERVER)}</div>
                    <TextButton class="primary" name="add_server" icon="Add" title={translate.t(LABEL_ADD_SERVER)} onclick={handle_add_server} />
                </div>
                <div class="tp__api-config-view__proxy-server tp__api-config-view__proxy-server__edit">
                {
                    if let Some(definition) = table_definition.as_ref() {
                            html! {
                                <>
                                <Table::<ApiProxyServerInfoDto> definition={definition.clone()} />
                                <PopupMenu is_open={*popup_is_open} anchor_ref={(*popup_anchor_ref).clone()} on_close={handle_popup_close}>
                                    <MenuItem icon="Delete" name={ServerTableAction::Delete.to_string()} label={translate.t("LABEL.DELETE")} onclick={&handle_menu_click} class="tp__delete_action"></MenuItem>
                                    <MenuItem icon="Edit" name={ServerTableAction::Edit.to_string()} label={translate.t("LABEL.EDIT")} onclick={&handle_menu_click}></MenuItem>
                                </PopupMenu>
                                </>
                            }
                        } else {
                            html!{}
                        }
                 }
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
                    {if form_state_api_proxy_config.form.server.is_empty() {
                        html! { <NoContent /> }
                     } else if let Some(definition) = table_definition.as_ref() {
                        html! {
                            <Table::<ApiProxyServerInfoDto> definition={definition.clone()} />
                        }
                     } else {
                        html!{}
                     }
                    }
                </div>
            </Card>
        }
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

    html! {
        <div class="tp__api-config-view tp__config-view-page">
           <div class="tp__config-view-page__title">{translate.t(LABEL_API_CONFIG)}</div>
            {
             html_if!(*config_view_ctx.edit_mode, {
                  <div class="tp__webui-config-view__info tp__config-view-page__info">
                    <AppIcon name="Warn"/> <span class="info">{translate.t("INFO.RESTART_TO_APPLY_CHANGES")}</span>
                  </div>
            })}
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
        </div>
    }
}
