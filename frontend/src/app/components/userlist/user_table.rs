use crate::{
    app::{
        components::{
            convert_bool_to_chip_style, make_translated_header_callback, menu_item::MenuItem, popup_menu::PopupMenu,
            AppIcon, CellValue, Chip, HideContent, MaxConnections, PagedTable, ProxyTypeView, RevealContent,
            TableDefinition, UserStatus, UserlistContext, UserlistPage, PAGE_SIZES, TP_PAGE_SIZE_KEY,
        },
        context::{target_users_to_api_proxy_users, TargetUser},
        ConfigContext, TargetUserList,
    },
    hooks::{use_clipboard_copy, use_service_context},
    html_if,
    i18n::use_translation,
    model::DialogResult,
    services::DialogService,
    utils::{get_local_storage_item, set_local_storage_item},
};
use shared::{
    defaults::default_page_size,
    model::{permission::Permission, SortOrder},
    utils::{unix_ts_to_str, Substring},
};
use std::{cmp::Ordering, collections::HashSet, rc::Rc, str::FromStr};
use yew::{platform::spawn_local, prelude::*};

const HEADERS: [&str; 19] = [
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
    "LABEL.SOFT_CON",
    "LABEL.PRIORITY",
    "LABEL.SOFT_PRIORITY",
    "LABEL.UI_ENABLED",
    "LABEL.EPG_TIMESHIFT",
    "LABEL.EPG_REQUEST_TIMESHIFT",
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
        8 => user.credentials.server.as_ref().map_or(CellValue::Empty, |s| CellValue::Text(s)),
        9 => CellValue::U32(user.credentials.max_connections),
        10 => CellValue::U16(user.credentials.soft_connections),
        11 => CellValue::I8(user.credentials.priority),
        12 => CellValue::I8(user.credentials.soft_priority),
        16 => user.credentials.created_at.as_ref().map_or(CellValue::Empty, |d| CellValue::Date(*d)),
        17 => user.credentials.exp_date.as_ref().map_or(CellValue::Empty, |d| CellValue::Date(*d)),
        _ => CellValue::Empty,
    }
}

fn is_col_sortable(col: usize) -> bool { matches!(col, 1 | 2 | 3 | 4 | 7 | 8 | 9 | 10 | 11 | 12 | 16 | 17) }

#[derive(Debug, Clone, Eq, PartialEq, strum_macros::Display, strum_macros::EnumString)]
#[strum(serialize_all = "snake_case")]
enum TableAction {
    Edit,
    Refresh,
    Delete,
    CopyCredentials,
}

#[derive(Properties, PartialEq, Clone)]
pub struct UserTableProps {
    pub users: TargetUserList,
}

#[component]
pub fn UserTable(props: &UserTableProps) -> Html {
    let translate = use_translation();
    let copy_to_clipboard = use_clipboard_copy();
    let service_ctx = use_service_context();
    let config_ctx = use_context::<ConfigContext>().expect("Config context not found");
    let dialog = use_context::<DialogService>().expect("Dialog service not found");
    let userlist_context = use_context::<UserlistContext>().expect("Userlist context not found");
    let can_write_users = service_ctx.auth.has_permission(Permission::UserWrite);
    let popup_anchor_ref = use_state(|| None::<web_sys::Element>);
    let popup_is_open = use_state(|| false);
    let selected_dto = use_state(|| None::<Rc<TargetUser>>);
    let user_list = use_state(|| props.users.clone());
    let page = use_state(|| 1u32);
    let page_size = use_state(|| {
        get_local_storage_item(TP_PAGE_SIZE_KEY)
            .and_then(|v| v.parse::<u16>().ok())
            .filter(|size| PAGE_SIZES.contains(size))
            .unwrap_or_else(default_page_size)
    });
    let target_names = use_memo(config_ctx.clone(), |cfg| {
        cfg.config
            .as_ref()
            .map(|c| {
                c.sources
                    .sources
                    .iter()
                    .flat_map(|s| s.targets.iter())
                    .map(|t| t.name.clone())
                    .collect::<HashSet<String>>()
            })
            .unwrap_or_default()
    });

    {
        let user_list = user_list.clone();
        let users = props.users.clone();
        let page = page.clone();
        use_effect_with(users, move |users| {
            user_list.set(users.clone());
            page.set(1);
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

    let render_header_cell = make_translated_header_callback(translate.clone(), &HEADERS);

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
                    5 => html! { <HideContent content={dto.credentials.password.clone()}></HideContent> },
                    6 => html! { dto.credentials.token.as_ref().map_or_else(|| html!{}, |token| html! { <HideContent content={token.clone()}></HideContent>}) },
                    7 => html! {<ProxyTypeView value={dto.credentials.proxy} /> },
                    8 => dto.credentials.server.as_ref().map_or_else(|| html! {}, |s| html! { s }),
                    9 => html! { <MaxConnections value={dto.credentials.max_connections} /> },
                    10 => html! { <span class="tp__table__number-cell">{ dto.credentials.soft_connections }</span> },
                    11 => html! { <span class="tp__table__number-cell">{ dto.credentials.priority }</span> },
                    12 => html! { <span class="tp__table__number-cell">{ dto.credentials.soft_priority }</span> },
                    13 => html! { <Chip class={ convert_bool_to_chip_style(dto.credentials.ui_enabled ) }
                                   label={if dto.credentials.ui_enabled {translator.t("LABEL.ENABLED")} else { translator.t("LABEL.DISABLED")} }
                                    />  },
                    14 => dto.credentials.epg_timeshift.as_ref().map_or_else(|| html! {}, |s| html! { s }),
                    15 => dto.credentials.epg_request_timeshift.as_ref().map_or_else(|| html! {}, |s| html! { s }),
                    16 => dto.credentials.created_at.as_ref().and_then(|ts| unix_ts_to_str(*ts)).map_or_else(|| html! { <AppIcon name="Unlimited" /> }, |s| html! { { s } }),
                    17 => dto.credentials.exp_date.as_ref().and_then(|ts| unix_ts_to_str(*ts)).map_or_else(|| html! { <AppIcon name="Unlimited" /> }, |s| html! { <span class="tp__table__nowrap">{ s }</span> }),
                    18 => dto.credentials.comment.as_ref()
                        .map_or_else(|| html! {},
                                     |comment| html! { <RevealContent preview={Some(html! {comment.substring(0, 50)})}>{comment}</RevealContent> }),
                    _ => html! {""},
                }
            },
        )
    };

    let is_sortable = Callback::<usize, bool>::from(is_col_sortable);

    let on_sort = {
        let users = props.users.clone();
        let user_list = user_list.clone();
        Callback::<Option<(usize, SortOrder)>, ()>::from(move |args| {
            if let Some((col, order)) = args {
                if let Some(new_user_list) = users.as_ref() {
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

    let total_items = user_list.as_ref().map_or(0, |l| l.len()) as u64;
    let total_pages = if total_items == 0 { 1 } else { total_items.div_ceil(u64::from(*page_size)) as u32 };
    let current_page = (*page).min(total_pages);

    let table_definition = {
        // first register for config update
        let render_header_cell_cb = render_header_cell.clone();
        let render_data_cell_cb = render_data_cell.clone();
        let on_sort = on_sort.clone();
        let is_sortable = is_sortable.clone();
        let num_cols = HEADERS.len();
        let page_size_value = *page_size;
        // Dereference the UseStateHandle to pass the actual value as dependency.
        // Yew 0.22 compares UseStateHandle by identity, not value, so use_memo
        // would never detect value changes if we passed the handle directly.
        use_memo(((*user_list).clone(), current_page, page_size_value), move |(targets, current_page, page_size)| {
            let items = if targets.as_ref().is_none_or(|l| l.is_empty()) {
                None
            } else {
                targets.as_ref().map(|list| {
                    let start = ((current_page - 1) as usize) * (*page_size as usize);
                    let page_items =
                        list.iter().skip(start).take(*page_size as usize).cloned().collect::<Vec<Rc<TargetUser>>>();
                    Rc::new(page_items)
                })
            };
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

    let handle_page_change = {
        let page = page.clone();
        Callback::from(move |new_page: u32| page.set(new_page.max(1)))
    };

    let handle_page_size_change = {
        let page = page.clone();
        let page_size = page_size.clone();
        Callback::from(move |new_size: u16| {
            set_local_storage_item(TP_PAGE_SIZE_KEY, &new_size.to_string());
            page_size.set(new_size);
            page.set(1);
        })
    };

    let handle_menu_click = {
        let popup_is_open_state = popup_is_open.clone();
        let confirm = dialog.clone();
        let translate = translate.clone();
        let services = service_ctx.clone();
        let selected_dto = selected_dto.clone();
        let ul_context = userlist_context.clone();
        let copy_to_clipboard = copy_to_clipboard.clone();
        Callback::from(move |(name, e): (String, MouseEvent)| {
            e.prevent_default();
            e.stop_propagation();
            if let Ok(action) = TableAction::from_str(&name) {
                match action {
                    TableAction::Edit => {
                        if can_write_users {
                            if let Some(dto) = &*selected_dto {
                                ul_context.selected_user.set(Some(Rc::clone(dto)));
                                ul_context.active_page.set(UserlistPage::Edit);
                            }
                        }
                    }
                    TableAction::Refresh => {}
                    TableAction::Delete => {
                        if !can_write_users {
                            popup_is_open_state.set(false);
                            return;
                        }
                        let confirm = confirm.clone();
                        let translator = translate.clone();
                        let services = services.clone();
                        let userlist = ul_context.clone();
                        let selected_user = selected_dto.clone();
                        spawn_local(async move {
                            let result = confirm.confirm(&translator.t("MESSAGES.CONFIRM_DELETE")).await;
                            if result == DialogResult::Ok {
                                if let Some(dto) = &*selected_user {
                                    let remove_selected_user = || {
                                        if let Some(user_list) = userlist.users.as_ref() {
                                            let new_list: Vec<Rc<TargetUser>> = user_list
                                                .iter()
                                                .filter(|target_user| {
                                                    !(target_user.target.eq(&dto.target)
                                                        && target_user
                                                            .credentials
                                                            .username
                                                            .eq(&dto.credentials.username))
                                                })
                                                .map(Rc::clone)
                                                .collect();
                                            let new_list_rc = Rc::new(new_list);
                                            userlist.users.set(Some(new_list_rc.clone()));
                                            userlist.filtered_users.set(None);
                                            if let Some(on_users_change) = userlist.on_users_change.as_ref() {
                                                on_users_change
                                                    .emit(target_users_to_api_proxy_users(&Some(new_list_rc)));
                                            }
                                            services.toastr.success(translator.t("MESSAGES.USER_DELETED"));
                                        }
                                    };

                                    if userlist.local_mode {
                                        remove_selected_user();
                                        return;
                                    }

                                    match services
                                        .user
                                        .delete_user(dto.target.clone(), dto.credentials.username.clone())
                                        .await
                                    {
                                        Ok(()) => remove_selected_user(),
                                        Err(err) => services.toastr.error(err.to_string()),
                                    }
                                }
                            }
                        });
                    }
                    TableAction::CopyCredentials => {
                        if let Some(dto) = &*selected_dto {
                            let text = format!(
                                "username: {} password: {} token: {}",
                                dto.credentials.username,
                                dto.credentials.password,
                                dto.credentials.token.as_ref().map_or_else(String::new, std::clone::Clone::clone)
                            );
                            copy_to_clipboard.emit(text);
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
               <PagedTable::<TargetUser> definition={table_definition.clone()}
                    page={current_page}
                    page_size={*page_size}
                    total_items={total_items}
                    total_pages={total_pages}
                    has_prev={current_page > 1}
                    has_next={current_page < total_pages}
                    on_page_change={handle_page_change}
                    on_page_size_change={handle_page_size_change} />
                <PopupMenu is_open={*popup_is_open} anchor_ref={(*popup_anchor_ref).clone()} on_close={handle_popup_close}>
                    { html_if!(can_write_users, {
                        <MenuItem icon="Edit" name={TableAction::Edit.to_string()} label={translate.t("LABEL.EDIT")} onclick={&handle_menu_click}></MenuItem>
                    })}
                    <MenuItem icon="Clipboard" name={TableAction::CopyCredentials.to_string()} label={translate.t("LABEL.COPY_CREDENTIALS")} onclick={&handle_menu_click}></MenuItem>
                    { html_if!(can_write_users, {
                        <>
                            <hr/>
                            <MenuItem icon="Delete" name={TableAction::Delete.to_string()} label={translate.t("LABEL.DELETE")} onclick={&handle_menu_click} class="tp__delete_action"></MenuItem>
                        </>
                    })}
                </PopupMenu>
            </>
             }
          }
        </div>
    }
}
