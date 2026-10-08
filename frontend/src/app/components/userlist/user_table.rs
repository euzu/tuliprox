use crate::{
    app::{
        components::{
            convert_bool_to_chip_style, menu_item::MenuItem, popup_menu::PopupMenu, AppIcon, CellValue, Chip,
            FilterView, HideContent, MaxConnections, PagedTable, ProxyTypeView, RevealContent, TableDefinition,
            UserStatus, UserlistContext, UserlistPage, PAGE_SIZES, TP_PAGE_SIZE_KEY,
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
    foundation::get_filter,
    model::{permission::Permission, SortOrder},
    utils::{unix_ts_to_str, Substring},
};
use std::{cmp::Ordering, collections::HashSet, rc::Rc, str::FromStr};
use yew::{platform::spawn_local, prelude::*};

crate::app::components::define_table_columns! {
    enum UsersColumn {
        Actions => ("actions", "TABLE_COLUMNS.ACTIONS") {
            can_hide: false,
            content: false,
        },
        Enabled => ("enabled", "LABEL.ENABLED") { sortable: true },
        Status => ("status", "LABEL.STATUS") { sortable: true },
        Playlist => ("playlist", "LABEL.PLAYLIST") { sortable: true },
        Username => ("username", "LABEL.USERNAME") {
            can_hide: false,
            sortable: true,
        },
        Password => ("password", "LABEL.PASSWORD"),
        Token => ("token", "LABEL.TOKEN"),
        Proxy => ("proxy", "LABEL.PROXY") { sortable: true },
        Server => ("server", "LABEL.SERVER") { sortable: true },
        MaxConnections => ("max_connections", "LABEL.MAX_CON") { sortable: true },
        SoftConnections => ("soft_connections", "LABEL.SOFT_CON") { sortable: true },
        Priority => ("priority", "LABEL.PRIORITY") { sortable: true },
        SoftPriority => ("soft_priority", "LABEL.SOFT_PRIORITY") { sortable: true },
        UiEnabled => ("ui_enabled", "LABEL.UI_ENABLED"),
        EpgTimeshift => ("epg_timeshift", "LABEL.EPG_TIMESHIFT"),
        EpgRequestTimeshift => ("epg_request_timeshift", "LABEL.EPG_REQUEST_TIMESHIFT"),
        CreatedAt => ("created_at", "LABEL.CREATED_AT") { sortable: true },
        ExpDate => ("exp_date", "LABEL.EXP_DATE") { sortable: true },
        Comment => ("comment", "LABEL.COMMENT"),
        Filter => ("filter", "LABEL.FILTER"),
    }
}

impl UsersColumn {
    const fn index(self) -> usize { self as usize }
}

fn get_cell_value(user: &TargetUser, col: usize) -> CellValue<'_> {
    match UsersColumn::from_index(col) {
        Some(UsersColumn::Enabled) => CellValue::Bool(user.credentials.is_active()),
        Some(UsersColumn::Status) => {
            user.credentials.status.as_ref().map_or(CellValue::Empty, |s| CellValue::Status(*s))
        }
        Some(UsersColumn::Playlist) => CellValue::Text(user.target.as_str()),
        Some(UsersColumn::Username) => CellValue::Text(user.credentials.username.as_str()),
        Some(UsersColumn::Proxy) => CellValue::Proxy(user.credentials.proxy),
        Some(UsersColumn::Server) => user.credentials.server.as_ref().map_or(CellValue::Empty, |s| CellValue::Text(s)),
        Some(UsersColumn::MaxConnections) => CellValue::U32(user.credentials.max_connections),
        Some(UsersColumn::SoftConnections) => CellValue::U16(user.credentials.soft_connections),
        Some(UsersColumn::Priority) => CellValue::I8(user.credentials.priority),
        Some(UsersColumn::SoftPriority) => CellValue::I8(user.credentials.soft_priority),
        Some(UsersColumn::CreatedAt) => {
            user.credentials.created_at.as_ref().map_or(CellValue::Empty, |d| CellValue::Date(*d))
        }
        Some(UsersColumn::ExpDate) => {
            user.credentials.exp_date.as_ref().map_or(CellValue::Empty, |d| CellValue::Date(*d))
        }
        _ => CellValue::Empty,
    }
}

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
    let columns = use_memo((), |()| UsersColumn::columns());
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

    let render_header_cell = {
        let translate = translate.clone();
        Callback::from(
            move |index| html! { {UsersColumn::from_index(index).map_or_else(String::new, |column| translate.t(column.header_label()))} },
        )
    };

    let templates = use_memo(config_ctx.clone(), |ctx| {
        ctx.config.as_ref().and_then(|config| {
            config
                .templates
                .as_ref()
                .map(|definition| definition.templates.clone())
                .or_else(|| config.sources.templates.clone())
        })
    });

    let render_data_cell = {
        let templates = templates.clone();
        let translator = translate.clone();
        let popup_onclick = handle_popup_onclick.clone();
        let target_names = target_names.clone();
        Callback::<(usize, usize, Rc<TargetUser>), Html>::from(
            move |(row, col, dto): (usize, usize, Rc<TargetUser>)| {
                let user_active = dto.credentials.is_active();
                match UsersColumn::from_index(col) {
                    Some(UsersColumn::Actions) => {
                        let popup_onclick = popup_onclick.clone();
                        html! {
                            <button class="tp__icon-button"
                                onclick={Callback::from(move |event: MouseEvent| popup_onclick.emit((dto.clone(), event)))}
                                data-row={row.to_string()}>
                                <AppIcon name="Popup"></AppIcon>
                            </button>
                        }
                    }
                    Some(UsersColumn::Enabled) => html! { <Chip class={ convert_bool_to_chip_style(user_active ) }
                                  label={if user_active {translator.t("LABEL.ENABLED")} else { translator.t("LABEL.DISABLED")} }
                                   /> },
                    Some(UsersColumn::Status) => html! { <UserStatus status={ dto.credentials.status } /> },
                    Some(UsersColumn::Playlist) => html! { <span class={if target_names.contains(dto.target.as_str()) {""} else {"tp__user-table__invalid-target"} }>{dto.target.as_str()}</span> },
                    Some(UsersColumn::Username) => html! { dto.credentials.username.as_str() },
                    Some(UsersColumn::Password) => html! { <HideContent content={dto.credentials.password.clone()}></HideContent> },
                    Some(UsersColumn::Token) => html! { dto.credentials.token.as_ref().map_or_else(|| html!{}, |token| html! { <HideContent content={token.clone()}></HideContent>}) },
                    Some(UsersColumn::Proxy) => html! {<ProxyTypeView value={dto.credentials.proxy} /> },
                    Some(UsersColumn::Server) => dto.credentials.server.as_ref().map_or_else(|| html! {}, |s| html! { s }),
                    Some(UsersColumn::MaxConnections) => html! { <MaxConnections value={dto.credentials.max_connections} /> },
                    Some(UsersColumn::SoftConnections) => html! { <span class="tp__table__number-cell">{ dto.credentials.soft_connections }</span> },
                    Some(UsersColumn::Priority) => html! { <span class="tp__table__number-cell">{ dto.credentials.priority }</span> },
                    Some(UsersColumn::SoftPriority) => html! { <span class="tp__table__number-cell">{ dto.credentials.soft_priority }</span> },
                    Some(UsersColumn::UiEnabled) => html! { <Chip class={ convert_bool_to_chip_style(dto.credentials.ui_enabled ) }
                                   label={if dto.credentials.ui_enabled {translator.t("LABEL.ENABLED")} else { translator.t("LABEL.DISABLED")} }
                                    />  },
                    Some(UsersColumn::EpgTimeshift) => dto.credentials.epg_timeshift.as_ref().map_or_else(|| html! {}, |s| html! { s }),
                    Some(UsersColumn::EpgRequestTimeshift) => dto.credentials.epg_request_timeshift.as_ref().map_or_else(|| html! {}, |s| html! { s }),
                    Some(UsersColumn::CreatedAt) => dto.credentials.created_at.as_ref().and_then(|ts| unix_ts_to_str(*ts)).map_or_else(|| html! { <AppIcon name="Unlimited" /> }, |s| html! { { s } }),
                    Some(UsersColumn::ExpDate) => dto.credentials.exp_date.as_ref().and_then(|ts| unix_ts_to_str(*ts)).map_or_else(|| html! { <AppIcon name="Unlimited" /> }, |s| html! { <span class="tp__table__nowrap">{ s }</span> }),
                    Some(UsersColumn::Comment) => dto.credentials.comment.as_ref()
                        .map_or_else(|| html! {},
                                     |comment| html! { <RevealContent preview={Some(html! {comment.substring(0, 50)})}>{comment}</RevealContent> }),
                    Some(UsersColumn::Filter) => dto.credentials.filter.as_ref().map_or_else(|| html! {}, |filter| {
                        let content = match get_filter(filter, templates.as_deref()) {
                            Ok(parsed) => html! { <FilterView pretty={true} filter={Some(parsed)} /> },
                            Err(_) => html! { <pre class="tp__filter__code">{filter}</pre> },
                        };
                        html! { <RevealContent>{content}</RevealContent> }
                    }),
                    _ => html! {""},
                }
            },
        )
    };

    let on_sort = {
        let users = props.users.clone();
        let user_list = user_list.clone();
        Callback::<Option<(usize, SortOrder)>, ()>::from(move |args| {
            if let Some((col, order)) = args {
                let Some(column) = UsersColumn::from_index(col) else {
                    return;
                };
                let col = column.index();
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
    let total_pages = if total_items == 0 {
        1
    } else {
        u32::try_from(total_items.div_ceil(u64::from((*page_size).max(1)))).unwrap_or(u32::MAX)
    };
    let current_page = (*page).clamp(1, total_pages);

    let table_definition = {
        let columns = columns.clone();
        // first register for config update
        let render_header_cell_cb = render_header_cell.clone();
        let render_data_cell_cb = render_data_cell.clone();
        let on_sort = on_sort.clone();

        let page_size_value = *page_size;
        // Dereference the UseStateHandle to pass the actual value as dependency.
        // Yew 0.22 compares UseStateHandle by identity, not value, so use_memo
        // would never detect value changes if we passed the handle directly.
        use_memo(
            ((*user_list).clone(), current_page, page_size_value, (*templates).clone()),
            move |(targets, current_page, page_size, _templates)| {
                let items = if targets.as_ref().is_none_or(|l| l.is_empty()) {
                    None
                } else {
                    targets.as_ref().map(|list| {
                        let start = usize::try_from(u64::from(current_page.saturating_sub(1)) * u64::from(*page_size))
                            .unwrap_or(usize::MAX);
                        let page_items =
                            list.iter().skip(start).take(*page_size as usize).cloned().collect::<Vec<Rc<TargetUser>>>();
                        Rc::new(page_items)
                    })
                };
                TableDefinition::<TargetUser> {
                    table_id: "users".into(),
                    columns: columns.clone(),
                    row_key: Callback::from(|(_, user): (usize, Rc<TargetUser>)| {
                        format!("{}:{}:{}", user.target.len(), user.target, user.credentials.username).into()
                    }),
                    items,

                    on_sort,
                    render_header_cell: render_header_cell_cb,
                    render_data_cell: render_data_cell_cb,
                }
            },
        )
    };

    let handle_page_change = {
        let page = page.clone();
        Callback::from(move |new_page: u32| page.set(new_page.max(1)))
    };

    let handle_page_size_change = {
        let page = page.clone();
        let page_size = page_size.clone();
        Callback::from(move |new_size: u16| {
            if !PAGE_SIZES.contains(&new_size) {
                return;
            }
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

#[cfg(test)]
mod column_tests {
    use super::UsersColumn;
    #[test]
    fn users_column_indices_and_ids_are_stable() {
        for &column in UsersColumn::ALL {
            assert_eq!(UsersColumn::from_index(column.index()), Some(column));
        }
        assert_eq!(UsersColumn::Filter.index(), 19);
        assert_eq!(UsersColumn::Filter.id(), "filter");
        assert_eq!(UsersColumn::from_index(20), None);
    }
}

#[cfg(test)]
#[path = "user_table.test.rs"]
mod tests;
