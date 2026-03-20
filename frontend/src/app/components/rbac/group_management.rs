use crate::{
    app::components::{
        menu_item::MenuItem, popup_menu::PopupMenu, AppIcon, Card, Panel, Table, TableDefinition, TextButton,
        ToggleSwitch,
    },
    config_field_custom, edit_field_text, generate_form_reducer,
    hooks::use_service_context,
    html_if,
    i18n::use_translation,
    model::DialogResult,
    services::{CreateGroupRequest, DialogService},
};
use shared::model::RbacGroupDto;
use std::{collections::HashSet, rc::Rc};
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

const PERMISSION_DOMAINS: &[&str] = &["config", "source", "user", "playlist", "library", "system", "epg"];

const GROUP_HEADERS: [&str; 3] = ["LABEL.EMPTY", "LABEL.NAME", "LABEL.RBAC_PERMISSIONS"];
const GROUP_DISPLAY_PANEL: &str = "display";
const GROUP_EDIT_PANEL: &str = "edit";

#[derive(Debug, Clone, Eq, PartialEq)]
enum GroupAction {
    Edit,
    Delete,
}

impl std::fmt::Display for GroupAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Edit => write!(f, "edit"),
            Self::Delete => write!(f, "delete"),
        }
    }
}

#[derive(Clone, PartialEq)]
enum FormMode {
    Hidden,
    Add,
    Edit(String),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct GroupFormDto {
    name: String,
    permissions: Vec<String>,
}

generate_form_reducer!(
    state: GroupFormState { form: GroupFormDto },
    action_name: GroupFormAction,
    fields {
        Name => name: String,
        Permissions => permissions: Vec<String>,
    }
);

fn summarize_permissions(perms: &[String], no_perms_label: &str) -> String {
    if perms.is_empty() {
        return no_perms_label.to_string();
    }
    let perm_set: HashSet<&str> = perms.iter().map(String::as_str).collect();
    let mut parts: Vec<String> = Vec::new();
    for domain in PERMISSION_DOMAINS {
        let read_key = format!("{domain}.read");
        let write_key = format!("{domain}.write");
        let has_read = perm_set.contains(read_key.as_str());
        let has_write = perm_set.contains(write_key.as_str());
        match (has_read, has_write) {
            (true, true) => parts.push(format!("{domain}:rw")),
            (true, false) => parts.push(format!("{domain}:r")),
            (false, true) => parts.push(format!("{domain}:w")),
            _ => {}
        }
    }
    if parts.is_empty() {
        no_perms_label.to_string()
    } else {
        parts.join(", ")
    }
}

fn collect_write_without_read_warnings(perms: &[String]) -> Vec<String> {
    let perm_set: HashSet<&str> = perms.iter().map(String::as_str).collect();
    PERMISSION_DOMAINS
        .iter()
        .filter(|domain| {
            let write_key = format!("{domain}.write");
            let read_key = format!("{domain}.read");
            perm_set.contains(write_key.as_str()) && !perm_set.contains(read_key.as_str())
        })
        .map(|domain| (*domain).to_string())
        .collect()
}

fn active_panel(mode: &FormMode) -> &'static str {
    match mode {
        FormMode::Hidden => GROUP_DISPLAY_PANEL,
        FormMode::Add | FormMode::Edit(_) => GROUP_EDIT_PANEL,
    }
}

#[derive(Properties, PartialEq, Clone)]
pub struct GroupManagementProps {
    pub groups: Option<Vec<RbacGroupDto>>,
    pub on_groups_changed: Callback<()>,
}

#[component]
pub fn GroupManagement(props: &GroupManagementProps) -> Html {
    let services = use_service_context();
    let translate = use_translation();
    let dialog = use_context::<DialogService>().expect("Dialog service not found");

    let form_mode = use_state(|| FormMode::Hidden);
    let form_state: UseReducerHandle<GroupFormState> =
        use_reducer(|| GroupFormState { form: GroupFormDto::default(), modified: false });

    let popup_anchor_ref = use_state(|| None::<web_sys::Element>);
    let popup_is_open = use_state(|| false);
    let selected_group = use_state(|| None::<Rc<RbacGroupDto>>);

    let on_groups_changed = props.on_groups_changed.clone();

    let reset_form = {
        let form_mode = form_mode.clone();
        let form_state = form_state.clone();
        move || {
            form_mode.set(FormMode::Hidden);
            form_state.dispatch(GroupFormAction::SetAll(GroupFormDto::default()));
        }
    };

    let on_add_click = {
        let form_mode = form_mode.clone();
        let form_state = form_state.clone();
        Callback::from(move |_: String| {
            form_mode.set(FormMode::Add);
            form_state.dispatch(GroupFormAction::SetAll(GroupFormDto::default()));
        })
    };

    let on_edit_group = {
        let form_mode = form_mode.clone();
        let form_state = form_state.clone();
        let groups = props.groups.clone();
        move |name: String| {
            if let Some(ref group_list) = groups {
                if let Some(group) = group_list.iter().find(|g| g.name == name) {
                    form_mode.set(FormMode::Edit(name));
                    form_state.dispatch(GroupFormAction::SetAll(GroupFormDto {
                        name: group.name.clone(),
                        permissions: group.permissions.clone(),
                    }));
                }
            }
        }
    };

    let on_cancel = {
        let reset_form = reset_form.clone();
        Callback::from(move |_: String| reset_form())
    };

    let on_permission_toggle = {
        let form_state = form_state.clone();
        Callback::from(move |(perm, value): (String, bool)| {
            let mut current = form_state.form.permissions.clone();
            if value {
                if !current.contains(&perm) {
                    current.push(perm);
                }
            } else {
                current.retain(|p| p != &perm);
            }
            form_state.dispatch(GroupFormAction::Permissions(current));
        })
    };

    let on_save = {
        let services = services.clone();
        let translate = translate.clone();
        let form_mode = form_mode.clone();
        let form_state = form_state.clone();
        let reset_form = reset_form.clone();
        let on_groups_changed = on_groups_changed.clone();
        Callback::from(move |_: String| {
            let name = form_state.form.name.clone();
            let permissions = form_state.form.permissions.clone();
            let write_without_read_warnings = collect_write_without_read_warnings(&permissions);
            let mode = (*form_mode).clone();
            let services = services.clone();
            let translate = translate.clone();
            let reset_form = reset_form.clone();
            let on_groups_changed = on_groups_changed.clone();

            if name.trim().is_empty() {
                services.toastr.error(translate.t("MESSAGES.RBAC.GROUP_NAME_REQUIRED"));
                return;
            }
            if !write_without_read_warnings.is_empty() {
                services.toastr.error(format!(
                    "{}: {}",
                    write_without_read_warnings.join(", "),
                    translate.t("MESSAGES.RBAC.WRITE_WITHOUT_READ")
                ));
                return;
            }

            spawn_local(async move {
                let req = CreateGroupRequest { name: name.clone(), permissions };
                let result = match &mode {
                    FormMode::Add => services.rbac.create_group(req).await,
                    FormMode::Edit(original_name) => services.rbac.update_group(original_name, req).await,
                    FormMode::Hidden => return,
                };

                match result {
                    Ok(()) => {
                        let msg = match &mode {
                            FormMode::Add => translate.t("MESSAGES.RBAC.GROUP_CREATED"),
                            _ => translate.t("MESSAGES.RBAC.GROUP_UPDATED"),
                        };
                        services.toastr.success(msg);
                        reset_form();
                        on_groups_changed.emit(());
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
        let set_selected = selected_group.clone();
        let set_anchor = popup_anchor_ref.clone();
        let set_open = popup_is_open.clone();
        Callback::from(move |(dto, event): (Rc<RbacGroupDto>, MouseEvent)| {
            if let Some(target) = event.target_dyn_into::<web_sys::Element>() {
                set_selected.set(Some(dto));
                set_anchor.set(Some(target));
                set_open.set(true);
            }
        })
    };

    let handle_menu_click = {
        let popup_is_open = popup_is_open.clone();
        let selected_group = selected_group.clone();
        let on_edit_group = on_edit_group.clone();
        let dialog = dialog.clone();
        let translate = translate.clone();
        let services = services.clone();
        let on_groups_changed = on_groups_changed.clone();
        Callback::from(move |(name, e): (String, MouseEvent)| {
            e.prevent_default();
            e.stop_propagation();
            match name.as_str() {
                "edit" => {
                    if let Some(dto) = &*selected_group {
                        on_edit_group(dto.name.clone());
                    }
                }
                "delete" => {
                    let dialog = dialog.clone();
                    let translate = translate.clone();
                    let services = services.clone();
                    let selected_group = selected_group.clone();
                    let on_groups_changed = on_groups_changed.clone();
                    spawn_local(async move {
                        let result = dialog.confirm(&translate.t("MESSAGES.CONFIRM_DELETE")).await;
                        if result == DialogResult::Ok {
                            if let Some(dto) = &*selected_group {
                                match services.rbac.delete_group(&dto.name).await {
                                    Ok(()) => {
                                        services.toastr.success(translate.t("MESSAGES.RBAC.GROUP_DELETED"));
                                        on_groups_changed.emit(());
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
                if col < GROUP_HEADERS.len() {
                    { translate.t(GROUP_HEADERS[col]) }
                }
            }
        })
    };

    let no_perms_label = translate.t("LABEL.RBAC_NO_PERMISSIONS");
    let render_data_cell = {
        let popup_onclick = handle_popup_onclick.clone();
        let no_perms_label = no_perms_label.clone();
        Callback::<(usize, usize, Rc<RbacGroupDto>), Html>::from(
            move |(_row, col, dto): (usize, usize, Rc<RbacGroupDto>)| match col {
                0 => {
                    if dto.builtin {
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
                1 => {
                    html! {
                        <>
                            { &dto.name }
                            { if dto.builtin {
                                html! { <span class="tp__chip tp__chip--info">{" built-in"}</span> }
                            } else {
                                html! {}
                            }}
                        </>
                    }
                }
                2 => html! { summarize_permissions(&dto.permissions, &no_perms_label) },
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
        let num_cols = GROUP_HEADERS.len();
        let groups_val = props.groups.clone();
        use_memo(groups_val, move |group_list| {
            let items =
                group_list.as_ref().map(|list| Rc::new(list.iter().map(|g| Rc::new(g.clone())).collect::<Vec<_>>()));
            TableDefinition::<RbacGroupDto> {
                items,
                num_cols,
                is_sortable,
                on_sort,
                render_header_cell: render_header,
                render_data_cell: render_data,
            }
        })
    };

    // Memoize write-without-read warnings
    let perms_for_warnings = form_state.form.permissions.clone();
    let write_without_read_warnings = use_memo(perms_for_warnings, |perms| collect_write_without_read_warnings(perms));

    let is_builtin_selected = selected_group.as_ref().is_some_and(|g| g.builtin);
    let active_panel = active_panel(&form_mode);
    let is_edit = matches!(*form_mode, FormMode::Edit(_));

    html! {
        <Card>
            <Panel value={GROUP_DISPLAY_PANEL.to_string()} active={active_panel.to_string()}>
                <div class="tp__config-view__header">
                    <h2>{ translate.t("LABEL.RBAC_GROUP_MANAGEMENT") }</h2>
                    <TextButton class="primary" name="add_group"
                        icon="Add"
                        title={translate.t("LABEL.RBAC_ADD_GROUP")}
                        onclick={on_add_click.clone()} />
                </div>

                <Table::<RbacGroupDto> definition={table_definition} />
            </Panel>

            <Panel value={GROUP_EDIT_PANEL.to_string()} active={active_panel.to_string()}>
                <div class="tp__form-page">
                    <h3>{
                        if is_edit {
                            translate.t("LABEL.RBAC_EDIT_GROUP")
                        } else {
                            translate.t("LABEL.RBAC_ADD_GROUP")
                        }
                    }</h3>

                    <div class="tp__group-management-view tp__config-view-page">
                        <div class="tp__group-management-view__header tp__config-view-page__header">
                            { if is_edit {
                                config_field_custom!(translate.t("LABEL.NAME"), form_state.form.name.clone())
                            } else {
                                edit_field_text!(form_state, translate.t("LABEL.NAME"), name, GroupFormAction::Name)
                            }}
                        </div>

                       <label class="tp__form-field__label">{ translate.t("LABEL.RBAC_PERMISSIONS") }</label>
                        <Card>
                            <table class="tp__table__table">
                                <thead>
                                    <tr>
                                        <th>{ translate.t("LABEL.RBAC_DOMAIN") }</th>
                                        <th>{ translate.t("LABEL.RBAC_READ") }</th>
                                        <th>{ translate.t("LABEL.RBAC_WRITE") }</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    { for PERMISSION_DOMAINS.iter().map(|domain| {
                                        let read_perm = format!("{domain}.read");
                                        let write_perm = format!("{domain}.write");
                                        let read_checked = form_state.form.permissions.contains(&read_perm);
                                        let write_checked = form_state.form.permissions.contains(&write_perm);
                                        let is_epg_write =  *domain == "epg";

                                        let on_read = {
                                            let toggle = on_permission_toggle.clone();
                                            let perm = read_perm.clone();
                                            Callback::from(move |value: bool| toggle.emit((perm.clone(), value)))
                                        };
                                        let on_write = {
                                            let toggle = on_permission_toggle.clone();
                                            let perm = write_perm.clone();
                                            Callback::from(move |value: bool| toggle.emit((perm.clone(), value)))
                                        };

                                        html! {
                                            <tr key={*domain}>
                                                <td>{ domain }</td>
                                                <td class="tp__table__cell--center">
                                                    <ToggleSwitch
                                                        value={read_checked}
                                                        on_change={on_read}
                                                    />
                                                </td>
                                                <td class="tp__table__cell--center">
                                                 { html_if!(!is_epg_write, {
                                                    <ToggleSwitch
                                                        value={write_checked}
                                                        on_change={on_write}
                                                    />
                                                  })}
                                                </td>
                                            </tr>
                                        }
                                    })}
                                </tbody>
                            </table>
                        </Card>

                        { if !write_without_read_warnings.is_empty() {
                            html! {
                                <div class="tp__config-view-page__info">
                                    { for write_without_read_warnings.iter().map(|domain| {
                                        html! {
                                            <div key={domain.clone()}>
                                                { format!("{}: {}", domain, translate.t("MESSAGES.RBAC.WRITE_WITHOUT_READ")) }
                                            </div>
                                        }
                                    })}
                                </div>
                            }
                        } else {
                            html! {}
                        }}
                    </div>

                    <div class="tp__form-page__toolbar">
                        <TextButton class="secondary" name="cancel"
                            icon="Cancel"
                            title={translate.t("LABEL.CANCEL")}
                            onclick={on_cancel.clone()} />
                        <TextButton class="primary" name="save"
                            icon="Save"
                            title={translate.t("LABEL.SAVE")}
                            onclick={on_save.clone()} />
                        </div>
                </div>
            </Panel>
            <PopupMenu is_open={*popup_is_open} anchor_ref={(*popup_anchor_ref).clone()} on_close={handle_popup_close}>
                { html_if!(!is_builtin_selected, {
                    <>
                        <MenuItem icon="Edit" name={GroupAction::Edit.to_string()} label={translate.t("LABEL.EDIT")} onclick={&handle_menu_click} />
                        <hr/>
                        <MenuItem icon="Delete" name={GroupAction::Delete.to_string()} label={translate.t("LABEL.DELETE")} onclick={&handle_menu_click} class="tp__delete_action" />
                    </>
                })}
            </PopupMenu>
        </Card>
    }
}
