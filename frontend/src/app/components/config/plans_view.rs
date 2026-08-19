use crate::{
    app::components::{
        input::Input,
        menu_item::MenuItem,
        popup_menu::PopupMenu,
        userlist::{ProxyTypeInput, ProxyTypeView},
        AppIcon, Breadcrumbs, Card, CustomDialog, FieldWrapper, NoContent, Table, TableDefinition, TextButton,
    },
    hooks::use_service_context,
    i18n::use_translation,
};
use shared::{
    concat_string,
    error::TuliproxError,
    model::{ClusterFlags, PlansConfigDto, ProxyType, SortOrder, UserPlanDto, UserPlanTrialDto},
};
use std::{fmt::Display, rc::Rc, str::FromStr};
use web_sys::MouseEvent;
use yew::{platform::spawn_local, prelude::*};

const PLAN_HEADERS: [&str; 9] =
    ["EMPTY", "NAME", "CLUSTER", "PROXY", "MAX_CONNECTIONS", "SOFT_CONNECTIONS", "FILTER", "TRIAL", "COMMENT"];
const MSG_NON_UNIQUE_PLAN_NAME: &str = "MESSAGES.SAVE.API_PROXY_CONFIG.NON_UNIQUE_PLAN_NAME";

#[derive(Clone, Copy, PartialEq)]
enum PlanDialogMode {
    Add,
    Edit(usize),
}

enum PlanTableAction {
    Delete,
    Edit,
}
impl Display for PlanTableAction {
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
impl FromStr for PlanTableAction {
    type Err = TuliproxError;
    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        match s {
            "Delete" => Ok(Self::Delete),
            "Edit" => Ok(Self::Edit),
            _ => Err(TuliproxError::Config(format!("Unknown Plan Action: {s}"))),
        }
    }
}

fn build_default_plan(existing_plans: &[UserPlanDto]) -> UserPlanDto {
    let mut index = existing_plans.len() + 1;
    loop {
        let name = format!("plan_{index}");
        if !existing_plans.iter().any(|plan| plan.name == name) {
            return UserPlanDto {
                name,
                output_clusters: None,
                proxy: None,
                max_connections: 1,
                soft_connections: 0,
                filter: None,
                trial: None,
                comment: None,
            };
        }
        index += 1;
    }
}

fn plan_name_exists(plans: &[UserPlanDto], plan_name: &str, ignore_index: Option<usize>) -> bool {
    plans
        .iter()
        .enumerate()
        .any(|(idx, plan)| ignore_index.is_none_or(|ignore_idx| idx != ignore_idx) && plan.name == plan_name)
}

fn cluster_flags_label(flags: Option<ClusterFlags>) -> String {
    flags.map_or_else(String::new, |f| {
        let mut parts = Vec::new();
        if f.contains(ClusterFlags::Live) {
            parts.push("L");
        }
        if f.contains(ClusterFlags::Vod) {
            parts.push("V");
        }
        if f.contains(ClusterFlags::Series) {
            parts.push("S");
        }
        parts.join(" ")
    })
}

fn make_field_handler<F>(dialog_form: &UseStateHandle<UserPlanDto>, updater: F) -> Callback<String>
where
    F: Fn(&mut UserPlanDto, String) + 'static,
{
    let dialog_form = dialog_form.clone();
    Callback::from(move |value: String| {
        let mut form = (*dialog_form).clone();
        updater(&mut form, value);
        dialog_form.set(form);
    })
}

#[component]
pub fn PlansView() -> Html {
    let translate = use_translation();
    let services = use_service_context();

    let plans = use_state(Vec::<UserPlanDto>::new);
    let dialog_mode = use_state(|| None::<PlanDialogMode>);
    let dialog_form = use_state(UserPlanDto::default);
    let dialog_error = use_state(|| None::<String>);
    let popup_is_open = use_state(|| false);
    let popup_anchor_ref = use_state(|| None::<web_sys::Element>);
    let selected_index = use_state(|| None::<usize>);
    let breadcrumbs = use_state(|| Rc::new(vec![translate.t("LABEL.PLANS")]));

    // Load plans from plans.yml on mount.
    {
        let plans = plans.clone();
        let services = services.clone();
        use_effect_with((), move |()| {
            spawn_local(async move {
                if let Some(cfg) = services.config.get_plans_config().await {
                    plans.set(cfg.plans.clone());
                }
            });
            || ()
        });
    }

    // Persist the full plan list to plans.yml immediately after any mutation.
    let persist_plans = {
        let services = services.clone();
        let translate = translate.clone();
        Callback::from(move |list: Vec<UserPlanDto>| {
            let services = services.clone();
            let translate = translate.clone();
            let plans_dto = PlansConfigDto { plans: list };
            spawn_local(async move {
                match services.config.save_plans_config(plans_dto).await {
                    Ok(()) => services.toastr.success(translate.t("MESSAGES.SAVE.API_PROXY_CONFIG.SUCCESS")),
                    Err(_) => services.toastr.error(translate.t("MESSAGES.SAVE.API_PROXY_CONFIG.FAIL")),
                }
            });
        })
    };

    let handle_popup_onclick = {
        let selected_index = selected_index.clone();
        let popup_anchor_ref = popup_anchor_ref.clone();
        let popup_is_open = popup_is_open.clone();
        Callback::from(move |(row, event): (usize, MouseEvent)| {
            if let Some(target) = event.target_dyn_into::<web_sys::Element>() {
                selected_index.set(Some(row));
                popup_anchor_ref.set(Some(target));
                popup_is_open.set(true);
            }
        })
    };

    let handle_popup_close = {
        let popup_is_open = popup_is_open.clone();
        Callback::from(move |()| popup_is_open.set(false))
    };

    let handle_menu_click = {
        let popup_is_open = popup_is_open.clone();
        let selected_index = selected_index.clone();
        let plans = plans.clone();
        let dialog_mode = dialog_mode.clone();
        let dialog_form = dialog_form.clone();
        let dialog_error = dialog_error.clone();
        let persist_plans = persist_plans.clone();
        Callback::from(move |(name, _): (String, MouseEvent)| {
            if let (Ok(action), Some(index)) = (PlanTableAction::from_str(&name), *selected_index) {
                match action {
                    PlanTableAction::Delete => {
                        let mut list = (*plans).clone();
                        if index < list.len() {
                            list.remove(index);
                            plans.set(list.clone());
                            persist_plans.emit(list);
                        }
                    }
                    PlanTableAction::Edit => {
                        if let Some(plan) = plans.get(index) {
                            dialog_form.set(plan.clone());
                            dialog_error.set(None);
                            dialog_mode.set(Some(PlanDialogMode::Edit(index)));
                        }
                    }
                }
            }
            popup_is_open.set(false);
        })
    };

    let handle_add_plan = {
        let plans = plans.clone();
        let dialog_mode = dialog_mode.clone();
        let dialog_form = dialog_form.clone();
        let dialog_error = dialog_error.clone();
        Callback::from(move |_| {
            dialog_form.set(build_default_plan(&plans));
            dialog_error.set(None);
            dialog_mode.set(Some(PlanDialogMode::Add));
        })
    };

    let handle_dialog_close = {
        let dialog_mode = dialog_mode.clone();
        let dialog_error = dialog_error.clone();
        Callback::from(move |()| {
            dialog_error.set(None);
            dialog_mode.set(None);
        })
    };
    let handle_dialog_cancel = {
        let handle_dialog_close = handle_dialog_close.clone();
        Callback::from(move |_| handle_dialog_close.emit(()))
    };

    let handle_name_change = make_field_handler(&dialog_form, |form, value| form.name = value);
    let handle_max_connections_change = make_field_handler(&dialog_form, |form, value| {
        form.max_connections = value.trim().parse::<u32>().unwrap_or(0);
    });
    let handle_soft_connections_change = make_field_handler(&dialog_form, |form, value| {
        form.soft_connections = value.trim().parse::<u16>().unwrap_or(0);
    });
    let handle_filter_change = make_field_handler(&dialog_form, |form, value| {
        form.filter = if value.trim().is_empty() { None } else { Some(value) };
    });
    let handle_trial_change = make_field_handler(&dialog_form, |form, value| {
        form.trial =
            if value.trim().is_empty() { None } else { Some(UserPlanTrialDto { duration: value.trim().to_string() }) };
    });
    let handle_comment_change = make_field_handler(&dialog_form, |form, value| {
        form.comment = if value.trim().is_empty() { None } else { Some(value) };
    });
    let handle_proxy_change = {
        let dialog_form = dialog_form.clone();
        Callback::from(move |proxy: ProxyType| {
            let mut form = (*dialog_form).clone();
            form.proxy = Some(proxy);
            dialog_form.set(form);
        })
    };

    let handle_dialog_save = {
        let dialog_mode = dialog_mode.clone();
        let dialog_form = dialog_form.clone();
        let dialog_error = dialog_error.clone();
        let plans = plans.clone();
        let translate = translate.clone();
        let persist_plans = persist_plans.clone();
        Callback::from(move |_| {
            let Some(mode) = *dialog_mode else { return };
            let mut plan = (*dialog_form).clone();
            if let Err(err) = plan.prepare() {
                dialog_error.set(Some(err.to_string()));
                return;
            }
            let ignore_index = match mode {
                PlanDialogMode::Add => None,
                PlanDialogMode::Edit(index) => Some(index),
            };
            if plan_name_exists(&plans, &plan.name, ignore_index) {
                let message = translate.t(MSG_NON_UNIQUE_PLAN_NAME).replace("{name}", &plan.name);
                dialog_error.set(Some(message));
                return;
            }
            let mut list = (*plans).clone();
            match mode {
                PlanDialogMode::Add => list.push(plan),
                PlanDialogMode::Edit(index) => {
                    if let Some(existing) = list.get_mut(index) {
                        *existing = plan;
                    }
                }
            }
            plans.set(list.clone());
            persist_plans.emit(list);
            dialog_error.set(None);
            dialog_mode.set(None);
        })
    };

    let render_header_cell = {
        let translate = translate.clone();
        Callback::<usize, Html>::from(move |col: usize| {
            if col == 0 || col >= PLAN_HEADERS.len() {
                html! {}
            } else {
                html! { {translate.t(&concat_string!("LABEL.", PLAN_HEADERS[col]))} }
            }
        })
    };

    let render_data_cell = {
        let popup_onclick = handle_popup_onclick.clone();
        Callback::<(usize, usize, Rc<UserPlanDto>), Html>::from(
            move |(row, col, dto): (usize, usize, Rc<UserPlanDto>)| match PLAN_HEADERS[col] {
                "EMPTY" => {
                    let popup_onclick = popup_onclick.clone();
                    html! {
                        <button
                            class="tp__icon-button"
                            onclick={Callback::from(move |event: MouseEvent| popup_onclick.emit((row, event)))}
                            data-row={row.to_string()}
                        >
                            <AppIcon name="Popup"/>
                        </button>
                    }
                }
                "NAME" => html! {&dto.name},
                "CLUSTER" => html! {cluster_flags_label(dto.output_clusters)},
                "PROXY" => {
                    dto.proxy.as_ref().map_or_else(|| html! {}, |proxy| html! {<ProxyTypeView value={*proxy} />})
                }
                "MAX_CONNECTIONS" => html! {dto.max_connections.to_string()},
                "SOFT_CONNECTIONS" => html! {dto.soft_connections.to_string()},
                "FILTER" => html! {dto.filter.clone().unwrap_or_default()},
                "TRIAL" => html! {dto.trial.as_ref().map_or_else(String::new, |t| t.duration.clone())},
                "COMMENT" => html! {dto.comment.clone().unwrap_or_default()},
                _ => html! {""},
            },
        )
    };

    let table_definition = {
        let is_sortable = Callback::<usize, bool>::from(move |_col| false);
        let on_sort = Callback::<Option<(usize, SortOrder)>, ()>::from(move |_args| {});
        let num_cols = PLAN_HEADERS.len();
        let items = (*plans).clone();
        use_memo(items, move |items| TableDefinition::<UserPlanDto> {
            items: if items.is_empty() {
                None
            } else {
                Some(Rc::new(items.iter().map(|plan| Rc::new(plan.clone())).collect()))
            },
            num_cols,
            is_sortable,
            on_sort,
            render_header_cell: render_header_cell.clone(),
            render_data_cell: render_data_cell.clone(),
        })
    };

    let dialog_html = if let Some(mode) = *dialog_mode {
        let title = match mode {
            PlanDialogMode::Add => translate.t("LABEL.ADD_PLAN"),
            PlanDialogMode::Edit(_) => format!("{} {}", translate.t("LABEL.EDIT"), translate.t("LABEL.PLAN")),
        };
        html! {
            <CustomDialog
                open={true}
                class={Some("tp__api-server-dialog".to_string())}
                modal={true}
                close_on_backdrop_click={false}
                on_close={Some(handle_dialog_close.clone())}
            >
                <h2>{title}</h2>
                <div class="tp__api-server-dialog__body">
                    <div class="tp__api-server-dialog__grid">
                        <Input
                            name="plan_name"
                            field_id={Some("USER_PLAN.NAME".to_string())}
                            label={Some(translate.t("LABEL.NAME"))}
                            value={dialog_form.name.clone()}
                            on_change={Some(handle_name_change.clone())}
                        />
                        <FieldWrapper
                            label={Some(translate.t("LABEL.PROXY"))}
                            field_id={"USER_PLAN.PROXY"}
                        >
                            <ProxyTypeInput
                                value={dialog_form.proxy.unwrap_or_default()}
                                on_change={handle_proxy_change.clone()}
                            />
                        </FieldWrapper>
                        <Input
                            name="plan_max_connections"
                            field_id={Some("USER_PLAN.MAX_CONNECTIONS".to_string())}
                            label={Some(translate.t("LABEL.MAX_CONNECTIONS"))}
                            value={dialog_form.max_connections.to_string()}
                            on_change={Some(handle_max_connections_change.clone())}
                        />
                        <Input
                            name="plan_soft_connections"
                            field_id={Some("USER_PLAN.SOFT_CONNECTIONS".to_string())}
                            label={Some(translate.t("LABEL.SOFT_CONNECTIONS"))}
                            value={dialog_form.soft_connections.to_string()}
                            on_change={Some(handle_soft_connections_change.clone())}
                        />
                        <Input
                            name="plan_trial"
                            field_id={Some("USER_PLAN.TRIAL".to_string())}
                            label={Some(translate.t("LABEL.TRIAL"))}
                            value={dialog_form.trial.as_ref().map(|t| t.duration.clone()).unwrap_or_default()}
                            on_change={Some(handle_trial_change.clone())}
                        />
                        <Input
                            name="plan_comment"
                            field_id={Some("USER_PLAN.COMMENT".to_string())}
                            label={Some(translate.t("LABEL.COMMENT"))}
                            value={dialog_form.comment.clone().unwrap_or_default()}
                            on_change={Some(handle_comment_change.clone())}
                        />
                        <div class="tp__api-server-dialog__message">
                            <Input
                                name="plan_filter"
                                field_id={Some("USER_PLAN.FILTER".to_string())}
                                label={Some(translate.t("LABEL.FILTER"))}
                                value={dialog_form.filter.clone().unwrap_or_default()}
                                on_change={Some(handle_filter_change.clone())}
                            />
                        </div>
                    </div>
                    {
                        if let Some(error) = (*dialog_error).as_ref() {
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
                        name="cancel_plan_dialog"
                        icon="Cancel"
                        title={translate.t("LABEL.CANCEL")}
                        onclick={handle_dialog_cancel.clone()}
                    />
                    <TextButton
                        class="primary"
                        name="save_plan_dialog"
                        icon="Save"
                        title={translate.t("LABEL.SAVE")}
                        onclick={handle_dialog_save.clone()}
                    />
                </div>
            </CustomDialog>
        }
    } else {
        html! {}
    };

    html! {
        <div class="tp__plans-view tp__list-view">
            <Breadcrumbs items={&*breadcrumbs} />
            <div class="tp__list-view__body">
                <Card class="tp__api-config-card">
                    <div class="tp__api-config-view__section-header tp__list-list__header">
                        <div class="tp__api-config-view__section-title">{translate.t("LABEL.PLANS")}</div>
                        <div class="tp__dialog__toolbar">
                            <TextButton class="primary" name="add_plan" icon="Add" title={translate.t("LABEL.ADD_PLAN")} onclick={handle_add_plan.clone()} />
                        </div>
                    </div>
                    <div class="tp__api-config-view__proxy-server tp__api-config-view__proxy-server__edit">
                        {
                            if plans.is_empty() {
                                html! {
                                    <NoContent
                                        text={translate.t("MESSAGES.EMPTY_STATE.API_PROXY_PLANS_TITLE")}
                                        hint={translate.t("MESSAGES.EMPTY_STATE.API_PROXY_PLANS_HINT")}
                                    />
                                }
                            } else {
                                html! { <Table::<UserPlanDto> definition={table_definition.clone()} /> }
                            }
                        }
                        <PopupMenu is_open={*popup_is_open} anchor_ref={(*popup_anchor_ref).clone()} on_close={handle_popup_close.clone()}>
                            <MenuItem
                                icon="Delete"
                                name={PlanTableAction::Delete.to_string()}
                                label={translate.t("LABEL.DELETE")}
                                onclick={handle_menu_click.clone()}
                                class="tp__delete_action"
                            />
                            <MenuItem
                                icon="Edit"
                                name={PlanTableAction::Edit.to_string()}
                                label={translate.t("LABEL.EDIT")}
                                onclick={handle_menu_click.clone()}
                            />
                        </PopupMenu>
                    </div>
                </Card>
            </div>
            {dialog_html}
        </div>
    }
}
