use std::cmp::Ordering;
use crate::app::components::menu_item::MenuItem;
use crate::app::components::popup_menu::PopupMenu;
use crate::app::components::{convert_bool_to_chip_style, AppIcon, CellValue, Chip, HideContent,
                             MaxConnections, ProxyTypeView, RevealContent, Table, TableDefinition,
                             UserStatus, UserlistContext, UserlistPage};
use crate::app::context::TargetUser;
use crate::model::DialogResult;
use crate::services::DialogService;
use shared::error::{info_err_res, TuliproxError};
use shared::model::{SortOrder};
use shared::utils::{unix_ts_to_str, Substring};
use std::fmt::Display;
use std::rc::Rc;
use std::str::FromStr;
use std::collections::HashSet;
use yew::platform::spawn_local;
use yew::prelude::*;
use yew_hooks::use_clipboard;
use yew_i18n::use_translation;
use crate::app::{ConfigContext, TargetUserList};
use crate::hooks::use_service_context;

const HEADERS: [&str; 15] = [
    "LABEL.EMPTY",
    "LABEL.ENABLED",
    "LABEL.STATUS",
    "LABEL.PLAYLIST",
    "LABEL.USERNAME",
    "LABEL.PASSWORD",
    "LABEL.TOKEN",
    "LABEL.PROXY",
    "LABEL.SERVER",
    "LABEL.MAX_CON",
    "LABEL.UI_ENABLED",
    "LABEL.EPG_TIMESHIFT",
    "LABEL.CREATED_AT",
    "LABEL.EXP_DATE",
    "LABEL.COMMENT",
];

fn get_cell_value(user: &TargetUser, col: usize) -> CellValue<'_> {
    match col {
        1 => CellValue::Bool(user.credentials.is_active()),
        2 => user.credentials.status.as_ref().map_or(CellValue::Empty, |s| CellValue::Status(*s)),
        3 => CellValue::Text(user.target.as_str()),
        4 => CellValue::Text(user.credentials.username.as_str()),
        7 => CellValue::Proxy(user.credentials.proxy),
        8 => user.credentials.server.as_ref().map_or(CellValue::Empty, |s|CellValue::Text(s)),
        12 => user.credentials.created_at.as_ref().map_or(CellValue::Empty, |d| CellValue::Date(*d)),
        13 => user.credentials.exp_date.as_ref().map_or(CellValue::Empty, |d| CellValue::Date(*d)),
        _ => CellValue::Empty,
    }
}

fn is_col_sortable(col: usize) -> bool {
    matches!(col, 1 | 2 | 3 | 4 | 7 | 8 | 12  | 13)
}


#[derive(Debug, Clone, Eq, PartialEq)]
enum TableAction {
    Edit,
    Refresh,
    Delete,
    CopyCredentials,
}

impl Display for TableAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", match self {
            Self::Edit => "edit",
            Self::Refresh => "refresh",
            Self::Delete => "delete",
            Self::CopyCredentials => "copy_credentials",
        })
    }
}

impl FromStr for TableAction {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        if s.eq("edit") {
            Ok(Self::Edit)
        } else if s.eq("refresh") {
            Ok(Self::Refresh)
        } else if s.eq("delete") {
            Ok(Self::Delete)
        } else if s.eq("copy_credentials") {
            Ok(Self::CopyCredentials)
        } else {
            info_err_res!("Unknown TableAction: {}", s)
        }
    }
}

#[derive(Properties, PartialEq, Clone)]
pub struct UserTableProps {
    pub users: TargetUserList,
}

#[function_component]
pub fn UserTable(props: &UserTableProps) -> Html {
    let translate = use_translation();
    let clipboard = use_clipboard();
    let service_ctx = use_service_context();
    let config_ctx = use_context::<ConfigContext>().expect("Config context not found");
    let dialog = use_context::<DialogService>().expect("Dialog service not found");
    let userlist_context = use_context::<UserlistContext>().expect("Userlist context not found");
    let popup_anchor_ref = use_state(|| None::<web_sys::Element>);
    let popup_is_open = use_state(|| false);
    let selected_dto = use_state(|| None::<Rc<TargetUser>>);
    let user_list = use_state(|| props.users.clone());
    let target_names = use_memo(config_ctx.clone(), |cfg|
        cfg.config.as_ref().map(|c| c.sources.sources.iter().flat_map(|s| s.targets.iter())
            .map(|t| t.name.clone())
            .collect::<HashSet<String>>()
            )
            .unwrap_or_default()
    );


    {
        let user_list = user_list.clone();
        let users = props.users.clone();
        use_effect_with(users, move |users| {
            user_list.set(users.clone());
            || ()
        });
    }

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
        Callback::from(move |(dto, event): (Rc<TargetUser>, MouseEvent)| {
            if let Some(target) = event.target_dyn_into::<web_sys::Element>() {
                set_selected_dto.set(Some(dto.clone()));
                set_anchor_ref.set(Some(target));
                set_is_open.set(true);
            }
        })
    };

    let render_header_cell = {
        let translator = translate.clone();
        Callback::<usize, Html>::from(move |col| {
            html! {
                {
                    if col < HEADERS.len() {
                       translator.t(HEADERS[col])
                    } else {
                      String::new()
                    }
               }
            }
        })
    };

    let render_data_cell = {
        let translator = translate.clone();
        let popup_onclick = handle_popup_onclick.clone();
        let target_names = target_names.clone();
        Callback::<(usize, usize, Rc<TargetUser>), Html>::from(
            move |(row, col, dto): (usize, usize, Rc<TargetUser>)| {
                let user_active = dto.credentials.is_active();
                match col {
                    0 => {
                        let popup_onclick = popup_onclick.clone();
                        html! {
                            <button class="tp__icon-button"
                                onclick={Callback::from(move |event: MouseEvent| popup_onclick.emit((dto.clone(), event)))}
                                data-row={row.to_string()}>
                                <AppIcon name="Popup"></AppIcon>
                            </button>
                        }
                    }
                    1 => html! { <Chip class={ convert_bool_to_chip_style(user_active ) }
                                  label={if user_active {translator.t("LABEL.ENABLED")} else { translator.t("LABEL.DISABLED")} }
                                   /> },
                    2 => html! { <UserStatus status={ dto.credentials.status } /> },
                    3 => html! { <span class={if target_names.contains(dto.target.as_str()) {""} else {"tp__user-table__invalid-target"} }>{dto.target.as_str()}</span> },
                    4 => html! { dto.credentials.username.as_str() },
                    5 => html! { <HideContent content={dto.credentials.password.to_string()}></HideContent> },
                    6 => html! { dto.credentials.token.as_ref().map_or_else(|| html!{}, |token| html! { <HideContent content={token.to_string()}></HideContent>}) },
                    7 => html! {<ProxyTypeView value={dto.credentials.proxy} /> },
                    8 => dto.credentials.server.as_ref().map_or_else(|| html! {}, |s| html! { s }),
                    9 => html! { <MaxConnections value={dto.credentials.max_connections} /> },
                    10 => html! { <Chip class={ convert_bool_to_chip_style(dto.credentials.ui_enabled ) }
                                   label={if dto.credentials.ui_enabled {translator.t("LABEL.ENABLED")} else { translator.t("LABEL.DISABLED")} }
                                    />  },
                    11 => dto.credentials.epg_timeshift.as_ref().map_or_else(|| html! {}, |s| html! { s }),
                    12 => dto.credentials.created_at.as_ref().and_then(|ts| unix_ts_to_str(*ts))
                        .map(|s| html! { { s } }).unwrap_or_else(|| html! { <AppIcon name="Unlimited" /> }),
                    13 => dto.credentials.exp_date.as_ref().and_then(|ts| unix_ts_to_str(*ts))
                        .map(|s| html! { { s } }).unwrap_or_else(|| html! { <AppIcon name="Unlimited" /> }),
                    14 => dto.credentials.comment.as_ref()
                        .map_or_else(|| html! {},
                                     |comment| html! { <RevealContent preview={Some(html! {comment.substring(0, 50)})}>{comment}</RevealContent> }),
                    _ => html! {""},
                }
            })
    };


    let is_sortable = Callback::<usize, bool>::from(is_col_sortable);

    let on_sort = {
        let users = props.users.clone();
        let user_list = user_list.clone();
        Callback::<Option<(usize, SortOrder)>, ()>::from(move |args| {
            if let Some((col, order)) = args {
                if let Some(new_user_list)= users.as_ref() {
                    let mut new_user_list = new_user_list.as_ref().clone();
                    new_user_list.sort_by(|a, b| {
                        let a_value = get_cell_value(a, col);
                        let b_value = get_cell_value(b, col);
                        match order {
                            SortOrder::Asc => a_value.cmp(&b_value),
                            SortOrder::Desc => b_value.cmp(&a_value),
                            SortOrder::None => Ordering::Equal,
                        }
                    });
                    user_list.set(Some(Rc::new(new_user_list)));
                }
            } else {
                user_list.set(users.clone());
            }
        })
    };

    let table_definition = {
        // first register for config update
        let render_header_cell_cb = render_header_cell.clone();
        let render_data_cell_cb = render_data_cell.clone();
        let on_sort = on_sort.clone();
        let is_sortable = is_sortable.clone();
        let num_cols = HEADERS.len();
        let user_list_clone = user_list.clone();
        use_memo(user_list_clone.clone(), move |targets| {
            let items = if (*targets).as_ref().is_none_or(|l| l.is_empty()) {None} else {(**targets).clone()};
            TableDefinition::<TargetUser> {
                items,
                num_cols,
                is_sortable,
                on_sort,
                render_header_cell: render_header_cell_cb,
                render_data_cell: render_data_cell_cb,
            }
        })
    };


    let handle_menu_click = {
        let popup_is_open_state = popup_is_open.clone();
        let confirm = dialog.clone();
        let translate = translate.clone();
        let services = service_ctx.clone();
        let selected_dto = selected_dto.clone();
        let ul_context = userlist_context.clone();
        let clipboard = clipboard.clone();
        let dialog = dialog.clone();
        Callback::from(move |(name, e): (String, MouseEvent)| {
            e.prevent_default();
            e.stop_propagation();
            if let Ok(action) = TableAction::from_str(&name) {
                match action {
                    TableAction::Edit => {
                        if let Some(dto) = &*selected_dto {
                            ul_context.selected_user.set(Some(Rc::clone(dto)));
                            ul_context.active_page.set(UserlistPage::Edit);
                        }
                    }
                    TableAction::Refresh => {}
                    TableAction::Delete => {
                        let confirm = confirm.clone();
                        let translator = translate.clone();
                        let services = services.clone();
                        let userlist = ul_context.clone();
                        let selected_user = selected_dto.clone();
                        spawn_local(async move {
                            let result = confirm.confirm(&translator.t("MESSAGES.CONFIRM_DELETE")).await;
                            if result == DialogResult::Ok {
                                if let Some(dto) = &*selected_user {
                                    match services.user.delete_user(dto.target.clone(), dto.credentials.username.clone()).await {
                                        Ok(()) => {
                                            if let Some(user_list) = userlist.users.as_ref() {
                                                let new_list: Vec<Rc<TargetUser>> = user_list.iter().filter(|target_user|
                                                    !(target_user.target.eq(&dto.target) && target_user.credentials.username.eq(&dto.credentials.username))
                                                ).map(Rc::clone).collect();
                                                userlist.users.set(Some(Rc::new(new_list)));
                                                services.toastr.success(translator.t("MESSAGES.USER_DELETED"));
                                            }
                                        },
                                        Err(err) => {
                                           services.toastr.error(err.to_string());
                                        }
                                    }
                                }
                            }
                        });
                    }
                    TableAction::CopyCredentials => {
                        if let Some(dto) = &*selected_dto {
                            let text = format!("username: {} password: {} token: {}",
                                               dto.credentials.username, dto.credentials.password,
                                               dto.credentials.token.as_ref().map_or_else(String::new, |t| t.to_string()));

                            if *clipboard.is_supported {
                                clipboard.write_text(text);
                            } else {
                                let dlg = dialog.clone();
                                spawn_local(async move {
                                    let _result = dlg.content(html! {<input value={text} readonly={true} class="tp__copy-input"/>}, None, false).await;
                                });
                            }
                        }
                    }
                }
            }
            popup_is_open_state.set(false);
        })
    };

    html! {
        <div class="tp__user-table">
          {
            html! {
              <>
               <Table::<TargetUser> definition={table_definition.clone()} />
                <PopupMenu is_open={*popup_is_open} anchor_ref={(*popup_anchor_ref).clone()} on_close={handle_popup_close}>
                    <MenuItem icon="Edit" name={TableAction::Edit.to_string()} label={translate.t("LABEL.EDIT")} onclick={&handle_menu_click}></MenuItem>
                    <MenuItem icon="Clipboard" name={TableAction::CopyCredentials.to_string()} label={translate.t("LABEL.COPY_CREDENTIALS")} onclick={&handle_menu_click}></MenuItem>
                    <hr/>
                    <MenuItem icon="Delete" name={TableAction::Delete.to_string()} label={translate.t("LABEL.DELETE")} onclick={&handle_menu_click} class="tp__delete_action"></MenuItem>
                </PopupMenu>
            </>
             }
          }
        </div>
    }
}
