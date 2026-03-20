use crate::{
    app::components::{
        config::HasFormData, menu_item::MenuItem, popup_menu::PopupMenu, AppIcon, Card, DropDownOption,
        DropDownSelection, Panel, Select, Table, TableDefinition, TextButton,
    },
    config_field_child, config_field_custom, edit_field_text, generate_form_reducer,
    hooks::use_service_context,
    html_if,
    i18n::use_translation,
    model::DialogResult,
    services::{CreateUserRequest, DialogService, UpdateUserRequest},
};
use shared::model::{permission::Permission, RbacGroupDto, WebUiUserDto};
use std::rc::Rc;
use wasm_bindgen_futures::spawn_local;
use yew::{prelude::*, suspense::use_future};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct UserFormDto {
    username: String,
    password: String,
    confirm_password: String,
    groups: Vec<String>,
}

generate_form_reducer!(
    state: UserFormState { form: UserFormDto },
    action_name: UserFormAction,
    fields {
        Username => username: String,
        Password => password: String,
        ConfirmPassword => confirm_password: String,
        Groups => groups: Vec<String>,
    }
);

#[derive(Clone, PartialEq)]
enum FormMode {
    Hidden,
    Add,
    Edit(String),
}

const USER_HEADERS: [&str; 3] = ["LABEL.EMPTY", "LABEL.USERNAME", "LABEL.GROUPS"];
const USER_DISPLAY_PANEL: &str = "display";
const USER_EDIT_PANEL: &str = "edit";

#[derive(Debug, Clone, Eq, PartialEq)]
enum UserAction {
    Edit,
    Delete,
}

impl std::fmt::Display for UserAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Edit => write!(f, "edit"),
            Self::Delete => write!(f, "delete"),
        }
    }
}

fn active_panel(mode: &FormMode) -> &'static str {
    match mode {
        FormMode::Hidden => USER_DISPLAY_PANEL,
        FormMode::Add | FormMode::Edit(_) => USER_EDIT_PANEL,
    }
}

#[derive(Properties, PartialEq, Clone)]
pub struct UserManagementProps {
    pub groups: Option<Vec<RbacGroupDto>>,
}

#[component]
pub fn UserManagement(props: &UserManagementProps) -> Html {
    let services = use_service_context();
    let translate = use_translation();
    let dialog = use_context::<DialogService>().expect("Dialog service not found");
    let can_read_users = services.auth.has_permission(Permission::UserRead);
    let can_write_users = services.auth.has_permission(Permission::UserWrite);

    let users = use_state(|| None::<Vec<WebUiUserDto>>);
    let form_mode = use_state(|| FormMode::Hidden);
    let form_state: UseReducerHandle<UserFormState> =
        use_reducer(|| UserFormState { form: UserFormDto::default(), modified: false });

    let popup_anchor_ref = use_state(|| None::<web_sys::Element>);
    let popup_is_open = use_state(|| false);
    let selected_user = use_state(|| None::<Rc<WebUiUserDto>>);

    // Fetch users on mount
    {
        let services = services.clone();
        let users = users.clone();
        let _ = use_future(move || async move {
            if can_read_users {
                if let Ok(Some(result)) = services.rbac.list_users().await {
                    users.set(Some(result));
                }
            }
        });
    }

    let refetch_users = {
        let services = services.clone();
        let users = users.clone();
        move || {
            if !can_read_users {
                return;
            }
            let services = services.clone();
            let users = users.clone();
            spawn_local(async move {
                if let Ok(Some(result)) = services.rbac.list_users().await {
                    users.set(Some(result));
                }
            });
        }
    };

    let reset_form = {
        let form_mode = form_mode.clone();
        let form_state = form_state.clone();
        move || {
            form_mode.set(FormMode::Hidden);
            form_state.dispatch(UserFormAction::SetAll(UserFormDto::default()));
        }
    };

    let on_add_click = {
        let form_mode = form_mode.clone();
        let form_state = form_state.clone();
        Callback::from(move |_: String| {
            if !can_write_users {
                return;
            }
            form_mode.set(FormMode::Add);
            form_state.dispatch(UserFormAction::SetAll(UserFormDto::default()));
        })
    };

    let on_edit_user = {
        let form_mode = form_mode.clone();
        let form_state = form_state.clone();
        let users = users.clone();
        move |username: String| {
            if !can_write_users {
                return;
            }
            if let Some(ref user_list) = *users {
                if let Some(user) = user_list.iter().find(|u| u.username == username) {
                    form_mode.set(FormMode::Edit(username));
                    form_state.dispatch(UserFormAction::SetAll(UserFormDto {
                        username: user.username.clone(),
                        password: String::new(),
                        confirm_password: String::new(),
                        groups: user.groups.clone(),
                    }));
                }
            }
        }
    };

    let on_cancel = {
        let reset_form = reset_form.clone();
        Callback::from(move |_: String| reset_form())
    };

    let on_group_select = {
        let form_state = form_state.clone();
        Callback::from(move |(_name, selection): (String, DropDownSelection)| {
            let selected = match selection {
                DropDownSelection::Empty => vec![],
                DropDownSelection::Single(id) => vec![id],
                DropDownSelection::Multi(ids) => ids,
            };
            form_state.dispatch(UserFormAction::Groups(selected));
        })
    };

    let on_save = {
        let services = services.clone();
        let translate = translate.clone();
        let form_mode = form_mode.clone();
        let form_state = form_state.clone();
        let reset_form = reset_form.clone();
        let refetch_users = refetch_users.clone();
        Callback::from(move |_: String| {
            let username = form_state.form.username.clone();
            let password = form_state.form.password.clone();
            let confirm = form_state.form.confirm_password.clone();
            let groups = form_state.form.groups.clone();
            let mode = (*form_mode).clone();
            let services = services.clone();
            let translate = translate.clone();
            let reset_form = reset_form.clone();
            let refetch_users = refetch_users.clone();

            if !can_write_users {
                services.toastr.error(translate.t("UNAUTHORIZED"));
                return;
            }
            if username.trim().is_empty() {
                services.toastr.error(translate.t("MESSAGES.RBAC.USERNAME_REQUIRED"));
                return;
            }

            match &mode {
                FormMode::Add => {
                    if password.len() < 8 {
                        services.toastr.error(translate.t("MESSAGES.RBAC.PASSWORD_MIN_LENGTH"));
                        return;
                    }
                    if password != confirm {
                        services.toastr.error(translate.t("MESSAGES.RBAC.PASSWORDS_DONT_MATCH"));
                        return;
                    }
                }
                FormMode::Edit(_) => {
                    if !password.is_empty() {
                        if password.len() < 8 {
                            services.toastr.error(translate.t("MESSAGES.RBAC.PASSWORD_MIN_LENGTH"));
                            return;
                        }
                        if password != confirm {
                            services.toastr.error(translate.t("MESSAGES.RBAC.PASSWORDS_DONT_MATCH"));
                            return;
                        }
                    }
                }
                FormMode::Hidden => return,
            }

            spawn_local(async move {
                let result = match &mode {
                    FormMode::Add => services.rbac.create_user(CreateUserRequest { username, password, groups }).await,
                    FormMode::Edit(uname) => {
                        let pwd = if password.is_empty() { None } else { Some(password) };
                        services.rbac.update_user(uname, UpdateUserRequest { password: pwd, groups }).await
                    }
                    FormMode::Hidden => return,
                };

                match result {
                    Ok(()) => {
                        let msg = match &mode {
                            FormMode::Add => translate.t("MESSAGES.RBAC.USER_CREATED"),
                            _ => translate.t("MESSAGES.RBAC.USER_UPDATED"),
                        };
                        services.toastr.success(msg);
                        reset_form();
                        refetch_users();
                    }
                    Err(err) => {
                        services.toastr.error(format!("{err}"));
                    }
                }
            });
        })
    };

    // Popup menu handlers
    let handle_popup_close = {
        let popup_is_open = popup_is_open.clone();
        Callback::from(move |()| popup_is_open.set(false))
    };

    let handle_popup_onclick = {
        let set_selected = selected_user.clone();
        let set_anchor = popup_anchor_ref.clone();
        let set_open = popup_is_open.clone();
        Callback::from(move |(dto, event): (Rc<WebUiUserDto>, MouseEvent)| {
            if let Some(target) = event.target_dyn_into::<web_sys::Element>() {
                set_selected.set(Some(dto));
                set_anchor.set(Some(target));
                set_open.set(true);
            }
        })
    };

    let current_username = services.auth.get_username();

    let handle_menu_click = {
        let popup_is_open = popup_is_open.clone();
        let selected_user = selected_user.clone();
        let on_edit_user = on_edit_user.clone();
        let dialog = dialog.clone();
        let translate = translate.clone();
        let services = services.clone();
        let refetch_users = refetch_users.clone();
        Callback::from(move |(name, e): (String, MouseEvent)| {
            e.prevent_default();
            e.stop_propagation();
            if !can_write_users {
                popup_is_open.set(false);
                return;
            }
            match name.as_str() {
                "edit" => {
                    if let Some(dto) = &*selected_user {
                        on_edit_user(dto.username.clone());
                    }
                }
                "delete" => {
                    let dialog = dialog.clone();
                    let translate = translate.clone();
                    let services = services.clone();
                    let selected_user = selected_user.clone();
                    let refetch_users = refetch_users.clone();
                    spawn_local(async move {
                        let result = dialog.confirm(&translate.t("MESSAGES.CONFIRM_DELETE")).await;
                        if result == DialogResult::Ok {
                            if let Some(dto) = &*selected_user {
                                match services.rbac.delete_user(&dto.username).await {
                                    Ok(()) => {
                                        services.toastr.success(translate.t("MESSAGES.RBAC.USER_DELETED"));
                                        refetch_users();
                                    }
                                    Err(err) => {
                                        services.toastr.error(format!("{err}"));
                                    }
                                }
                            }
                        }
                    });
                }
                _ => {}
            }
            popup_is_open.set(false);
        })
    };

    // Table definition
    let render_header_cell = {
        let translate = translate.clone();
        Callback::<usize, Html>::from(move |col| {
            html! {
                if col < USER_HEADERS.len() {
                    { translate.t(USER_HEADERS[col]) }
                }
            }
        })
    };

    let render_data_cell = {
        let popup_onclick = handle_popup_onclick.clone();
        Callback::<(usize, usize, Rc<WebUiUserDto>), Html>::from(
            move |(_row, col, dto): (usize, usize, Rc<WebUiUserDto>)| match col {
                0 => {
                    if !can_write_users {
                        return html! {};
                    }
                    let popup_onclick = popup_onclick.clone();
                    html! {
                        <button class="tp__icon-button"
                            onclick={Callback::from(move |event: MouseEvent| popup_onclick.emit((dto.clone(), event)))}>
                            <AppIcon name="Popup" />
                        </button>
                    }
                }
                1 => html! { &dto.username },
                2 => html! { dto.groups.join(", ") },
                _ => html! {},
            },
        )
    };

    let is_sortable = Callback::<usize, bool>::from(|_col: usize| false);
    let on_sort = Callback::<Option<(usize, shared::model::SortOrder)>, ()>::from(|_| {});

    let table_definition = {
        let render_header = render_header_cell.clone();
        let render_data = render_data_cell.clone();
        let is_sortable = is_sortable.clone();
        let on_sort = on_sort.clone();
        let num_cols = USER_HEADERS.len();
        use_memo((*users).clone(), move |user_list| {
            let items =
                user_list.as_ref().map(|list| Rc::new(list.iter().map(|u| Rc::new(u.clone())).collect::<Vec<_>>()));
            TableDefinition::<WebUiUserDto> {
                items,
                num_cols,
                is_sortable,
                on_sort,
                render_header_cell: render_header,
                render_data_cell: render_data,
            }
        })
    };

    // Memoize group options for Select multi-select
    let selected_groups = form_state.form.groups.clone();
    let groups_for_memo = props.groups.clone();
    let group_options: Rc<Vec<DropDownOption>> = use_memo((groups_for_memo, selected_groups), |(groups, selected)| {
        groups
            .as_ref()
            .map(|g| {
                g.iter()
                    .map(|grp| DropDownOption::new(&grp.name, html! { &grp.name }, selected.contains(&grp.name)))
                    .collect()
            })
            .unwrap_or_default()
    });

    let user_is_admin = form_state.form.groups.iter().any(|g| g == "admin");
    let is_self_selected = selected_user.as_ref().is_some_and(|u| u.username == current_username);
    let active_panel = active_panel(&form_mode);
    let is_edit = matches!(*form_mode, FormMode::Edit(_));

    html! {
        <Card>
            <Panel value={USER_DISPLAY_PANEL.to_string()} active={active_panel.to_string()}>
                <div class="tp__config-view__header">
                    <h2>{ translate.t("LABEL.RBAC_USERS") }</h2>
                    { html_if!(can_write_users, {
                        <TextButton class="primary" name="add_user"
                            icon="Add"
                            title={translate.t("LABEL.RBAC_ADD_USER")}
                            onclick={on_add_click.clone()} />
                    })}
                </div>

                <Table::<WebUiUserDto> definition={table_definition} />
            </Panel>

            <Panel value={USER_EDIT_PANEL.to_string()} active={active_panel.to_string()}>
                <div class="tp__form-page">
                    <h3>{
                        if is_edit {
                            translate.t("LABEL.RBAC_EDIT_USER")
                        } else {
                            translate.t("LABEL.RBAC_ADD_USER")
                        }
                    }</h3>

                    <div class="tp__form-page__body">
                        { if is_edit {
                            config_field_custom!(translate.t("LABEL.USERNAME"), form_state.data().username.clone())
                        } else {
                            edit_field_text!(form_state, translate.t("LABEL.USERNAME"), username, UserFormAction::Username)
                        }}
                        { edit_field_text!(form_state, translate.t("LABEL.PASSWORD"), password, UserFormAction::Password, true) }
                        { edit_field_text!(form_state, translate.t("LABEL.REPEAT_PASSWORD"), confirm_password, UserFormAction::ConfirmPassword, true) }

                        { config_field_child!(translate.t("LABEL.GROUPS"), {
                            html! {
                                <>
                                    <Select
                                        name="user_groups"
                                        options={group_options}
                                        multi_select={true}
                                        on_select={on_group_select.clone()}
                                    />
                                    { html_if!(user_is_admin, {
                                        <div class="tp__config-view-page__info">
                                            { translate.t("MESSAGES.RBAC.ADMIN_HINT") }
                                        </div>
                                    })}
                                </>
                            }
                        })}
                    </div>

                    <div class="tp__form-page__toolbar">
                        <TextButton class="secondary" name="cancel"
                            icon="Cancel"
                            title={translate.t("LABEL.CANCEL")}
                            onclick={on_cancel.clone()} />
                        { html_if!(can_write_users, {
                            <TextButton class="primary" name="save"
                                icon="Save"
                                title={translate.t("LABEL.SAVE")}
                                onclick={on_save.clone()} />
                        })}
                    </div>
                </div>
            </Panel>
            <PopupMenu is_open={*popup_is_open} anchor_ref={(*popup_anchor_ref).clone()} on_close={handle_popup_close}>
                { html_if!(can_write_users, {
                    <>
                        <MenuItem icon="Edit" name={UserAction::Edit.to_string()} label={translate.t("LABEL.EDIT")} onclick={&handle_menu_click} />
                        { html_if!(!is_self_selected, {
                            <>
                                <hr/>
                                <MenuItem icon="Delete" name={UserAction::Delete.to_string()} label={translate.t("LABEL.DELETE")} onclick={&handle_menu_click} class="tp__delete_action" />
                            </>
                        })}
                    </>
                })}
            </PopupMenu>
        </Card>
    }
}
