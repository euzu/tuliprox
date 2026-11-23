use crate::{
    app::components::{
        build_recording_options, build_tri_state_options,
        input::Input,
        menu_item::MenuItem,
        parse_optional_input, parse_plan_recording_level, parse_positive_byte_size, parse_tri_state_selection,
        popup_menu::PopupMenu,
        select::Select,
        selection_first_owned,
        userlist::{ProxyTypeInput, ProxyTypeView},
        AppIcon, Breadcrumbs, Card, CustomDialog, DropDownSelection, FieldWrapper, NoContent, Table, TableDefinition,
        TextButton, ToggleSwitch, MSG_BYTE_SIZE_INVALID,
    },
    config_field_child,
    hooks::use_service_context,
    i18n::use_translation,
};
use shared::model::{
    ClusterFlags, PlansConfigDto, PlayerFeaturesDto, PlayerRecordingLevel, ProxyType, SortOrder, UserPlanDto,
    UserPlanTrialDto,
};
use std::{rc::Rc, str::FromStr};
use web_sys::MouseEvent;
use yew::{platform::spawn_local, prelude::*};

crate::app::components::define_table_columns! {
    enum PlanColumn {
        Actions => ("actions", "TABLE_COLUMNS.ACTIONS") {
            can_hide: false,
            content: false,
            header_label: Some("LABEL.EMPTY"),
        },
        Name => ("name", "LABEL.NAME") { can_hide: false },
        Cluster => ("cluster", "LABEL.CLUSTER"),
        Proxy => ("proxy", "LABEL.PROXY"),
        MaxConnections => ("max_connections", "LABEL.MAX_CONNECTIONS"),
        SoftConnections => ("soft_connections", "LABEL.SOFT_CONNECTIONS"),
        Filter => ("filter", "LABEL.FILTER"),
        Trial => ("trial", "LABEL.TRIAL"),
        Comment => ("comment", "LABEL.COMMENT"),
    }
}

const MSG_NON_UNIQUE_PLAN_NAME: &str = "MESSAGES.SAVE.API_PROXY_CONFIG.NON_UNIQUE_PLAN_NAME";

#[derive(Clone, Copy, PartialEq)]
enum PlanDialogMode {
    Add,
    Edit(usize),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, strum_macros::Display, strum_macros::EnumString)]
enum PlanTableAction {
    Delete,
    Edit,
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
                player_features: None,
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

fn update_plan_features<F>(dialog_form: &UseStateHandle<UserPlanDto>, updater: F)
where
    F: FnOnce(&mut PlayerFeaturesDto),
{
    let mut form = (**dialog_form).clone();
    let mut features = form.player_features.unwrap_or_default();
    updater(&mut features);
    form.player_features = if features.is_empty() { None } else { Some(features) };
    dialog_form.set(form);
}

fn make_feature_toggle_handler(
    dialog_form: &UseStateHandle<UserPlanDto>,
    setter: fn(&mut PlayerFeaturesDto, Option<bool>),
) -> Callback<bool> {
    let dialog_form = dialog_form.clone();
    Callback::from(move |checked: bool| {
        update_plan_features(&dialog_form, move |f| {
            setter(f, if checked { Some(true) } else { None });
        });
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
    let columns = use_memo((), |()| PlanColumn::columns());
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

    let handle_api_enabled_select = {
        let dialog_form = dialog_form.clone();
        Callback::from(move |(_name, selections): (String, DropDownSelection)| {
            let val = parse_tri_state_selection(&selections);
            update_plan_features(&dialog_form, move |f| {
                f.api_enabled = val;
            });
        })
    };

    let handle_source_switch_change = make_feature_toggle_handler(&dialog_form, |f, val| f.source_switch = val);

    let handle_joinable_streams_change = make_feature_toggle_handler(&dialog_form, |f, val| f.joinable_streams = val);

    let handle_free_shared_joins_change = make_feature_toggle_handler(&dialog_form, |f, val| f.free_shared_joins = val);

    let handle_plan_recording_select = {
        let dialog_form = dialog_form.clone();
        Callback::from(move |(_, selections): (String, DropDownSelection)| {
            let selected_id = selection_first_owned(selections);
            let level = parse_plan_recording_level(selected_id.as_deref());
            update_plan_features(&dialog_form, move |f| {
                f.recording = level;
            });
        })
    };

    let handle_recording_quota_change = {
        let dialog_form = dialog_form.clone();
        Callback::from(move |value: String| {
            let quota = (!value.trim().is_empty()).then(|| shared::model::ByteSize::new(value.trim()));
            update_plan_features(&dialog_form, move |f| {
                f.recording_quota = quota;
            });
        })
    };

    let handle_max_concurrent_recordings_change = {
        let dialog_form = dialog_form.clone();
        Callback::from(move |value: String| {
            if let Ok(concurrent) = parse_optional_input::<u32>(&value) {
                update_plan_features(&dialog_form, move |f| {
                    f.max_concurrent_recordings = concurrent;
                });
            }
        })
    };

    let handle_max_recording_rules_change = {
        let dialog_form = dialog_form.clone();
        Callback::from(move |value: String| {
            if let Ok(rules) = parse_optional_input::<u32>(&value) {
                update_plan_features(&dialog_form, move |f| {
                    f.max_recording_rules = rules;
                });
            }
        })
    };

    let handle_watch_progress_change = make_feature_toggle_handler(&dialog_form, |f, val| f.watch_progress = val);

    let handle_event_stream_change = make_feature_toggle_handler(&dialog_form, |f, val| f.event_stream = val);

    let handle_max_free_shared_joins_change = {
        let dialog_form = dialog_form.clone();
        Callback::from(move |value: String| {
            if let Ok(joins) = parse_optional_input::<u32>(&value) {
                update_plan_features(&dialog_form, move |f| {
                    f.max_free_shared_joins = joins;
                });
            }
        })
    };

    let current_recording =
        dialog_form.player_features.as_ref().and_then(|f| f.recording).unwrap_or(PlayerRecordingLevel::None);
    let plan_recording_options = use_memo((current_recording, translate.clone()), |(current, translate)| {
        build_recording_options(Some(*current), false, |k| translate.t(k))
    });
    let plan_api_enabled_options = use_memo(
        (dialog_form.player_features.as_ref().and_then(|f| f.api_enabled), translate.clone()),
        |(current, translate)| {
            build_tri_state_options(
                *current,
                &translate.t("LABEL.FOLLOW_USER_UI"),
                &translate.t("LABEL.ENABLED"),
                &translate.t("LABEL.DISABLED"),
            )
        },
    );

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
            if let Some(features) = &plan.player_features {
                if features.recording_quota.as_ref().is_some_and(|q| parse_positive_byte_size(q.as_str()).is_err()) {
                    dialog_error.set(Some(translate.t(MSG_BYTE_SIZE_INVALID)));
                    return;
                }
                if features.max_concurrent_recordings == Some(0)
                    || features.max_recording_rules == Some(0)
                    || features.max_free_shared_joins == Some(0)
                {
                    dialog_error.set(Some(translate.t("MESSAGES.VALIDATION.PLAYER_FEATURES_NUMBER_GREATER_THAN_ZERO")));
                    return;
                }
            }
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
        Callback::<usize, Html>::from(move |col: usize| match PlanColumn::from_index(col) {
            Some(PlanColumn::Actions) | None => html! {},
            Some(column) => html! { {translate.t(column.header_label())} },
        })
    };

    let render_data_cell = {
        let popup_onclick = handle_popup_onclick.clone();
        Callback::<(usize, usize, Rc<UserPlanDto>), Html>::from(
            move |(row, col, dto): (usize, usize, Rc<UserPlanDto>)| match PlanColumn::from_index(col) {
                Some(PlanColumn::Actions) => {
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
                Some(PlanColumn::Name) => html! {&dto.name},
                Some(PlanColumn::Cluster) => html! {cluster_flags_label(dto.output_clusters)},
                Some(PlanColumn::Proxy) => {
                    dto.proxy.as_ref().map_or_else(|| html! {}, |proxy| html! {<ProxyTypeView value={*proxy} />})
                }
                Some(PlanColumn::MaxConnections) => html! {dto.max_connections.to_string()},
                Some(PlanColumn::SoftConnections) => html! {dto.soft_connections.to_string()},
                Some(PlanColumn::Filter) => html! {dto.filter.clone().unwrap_or_default()},
                Some(PlanColumn::Trial) => html! {dto.trial.as_ref().map_or_else(String::new, |t| t.duration.clone())},
                Some(PlanColumn::Comment) => html! {dto.comment.clone().unwrap_or_default()},
                _ => html! {""},
            },
        )
    };

    let table_definition = {
        let columns = columns.clone();

        let on_sort = Callback::<Option<(usize, SortOrder)>, ()>::from(move |_args| {});

        let items = (*plans).clone();
        use_memo(items, move |items| TableDefinition::<UserPlanDto> {
            table_id: "config.user_plans".into(),
            columns: columns.clone(),
            row_key: Callback::from(|(_, item): (usize, Rc<UserPlanDto>)| item.name.clone().into()),
            items: if items.is_empty() {
                None
            } else {
                Some(Rc::new(items.iter().map(|plan| Rc::new(plan.clone())).collect()))
            },

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
                        <div class="tp__api-server-dialog__section">
                            <div class="tp__api-server-dialog__section-title">{translate.t("LABEL.PLAYER_FEATURES")}</div>
                            <div class="tp__api-server-dialog__section-hint">{translate.t("LABEL.PLAYER_FEATURES_HINT")}</div>
                        </div>
                        { config_field_child!(translate.t("LABEL.PLAYER_API_ENABLED"), "USER_PLAN.PLAYER_API_ENABLED", {
                            html! {
                                <Select
                                    name="plan_player_api_enabled"
                                    multi_select={false}
                                    options={plan_api_enabled_options.clone()}
                                    on_select={handle_api_enabled_select.clone()}
                                />
                            }
                        }) }
                        { config_field_child!(translate.t("LABEL.SOURCE_SWITCH"), "USER_PLAN.SOURCE_SWITCH", {
                            html! {
                                <ToggleSwitch
                                    value={dialog_form.player_features.as_ref().and_then(|f| f.source_switch).unwrap_or(false)}
                                    on_change={handle_source_switch_change.clone()}
                                />
                            }
                        }) }
                        { config_field_child!(translate.t("LABEL.JOINABLE_STREAMS"), "USER_PLAN.JOINABLE_STREAMS", {
                            html! {
                                <ToggleSwitch
                                    value={dialog_form.player_features.as_ref().and_then(|f| f.joinable_streams).unwrap_or(false)}
                                    on_change={handle_joinable_streams_change.clone()}
                                />
                            }
                        }) }
                        { config_field_child!(translate.t("LABEL.RECORDING"), "USER_PLAN.RECORDING", {
                            html! {
                                <Select
                                    name="plan_recording"
                                    multi_select={false}
                                    options={plan_recording_options.clone()}
                                    on_select={handle_plan_recording_select.clone()}
                                />
                            }
                        }) }
                        <Input
                            name="plan_recording_quota"
                            field_id={Some("USER_PLAN.RECORDING_QUOTA".to_string())}
                            label={Some(translate.t("LABEL.PLAYER_RECORDING_QUOTA"))}
                            value={dialog_form.player_features.as_ref().and_then(|f| f.recording_quota.as_ref()).map(|v| v.as_str().to_string()).unwrap_or_default()}
                            on_change={Some(handle_recording_quota_change.clone())}
                        />
                        <Input
                            name="plan_max_concurrent_recordings"
                            field_id={Some("USER_PLAN.MAX_CONCURRENT_RECORDINGS".to_string())}
                            label={Some(translate.t("LABEL.MAX_CONCURRENT_RECORDINGS"))}
                            value={dialog_form.player_features.as_ref().and_then(|f| f.max_concurrent_recordings).map(|v| v.to_string()).unwrap_or_default()}
                            on_change={Some(handle_max_concurrent_recordings_change.clone())}
                        />
                        <Input
                            name="plan_max_recording_rules"
                            field_id={Some("USER_PLAN.MAX_RECORDING_RULES".to_string())}
                            label={Some(translate.t("LABEL.MAX_RECORDING_RULES"))}
                            value={dialog_form.player_features.as_ref().and_then(|f| f.max_recording_rules).map(|v| v.to_string()).unwrap_or_default()}
                            on_change={Some(handle_max_recording_rules_change.clone())}
                        />
                        { config_field_child!(translate.t("LABEL.FREE_SHARED_JOINS"), "USER_PLAN.FREE_SHARED_JOINS", {
                            html! {
                                <ToggleSwitch
                                    value={dialog_form.player_features.as_ref().and_then(|f| f.free_shared_joins).unwrap_or(false)}
                                    on_change={handle_free_shared_joins_change.clone()}
                                />
                            }
                        }) }
                        <Input
                            name="plan_max_free_shared_joins"
                            field_id={Some("USER_PLAN.MAX_FREE_SHARED_JOINS".to_string())}
                            label={Some(translate.t("LABEL.MAX_FREE_SHARED_JOINS"))}
                            value={dialog_form.player_features.as_ref().and_then(|f| f.max_free_shared_joins).map(|v| v.to_string()).unwrap_or_default()}
                            on_change={Some(handle_max_free_shared_joins_change.clone())}
                        />
                        { config_field_child!(translate.t("LABEL.WATCH_PROGRESS"), "USER_PLAN.WATCH_PROGRESS", {
                            html! {
                                <ToggleSwitch
                                    value={dialog_form.player_features.as_ref().and_then(|f| f.watch_progress).unwrap_or(false)}
                                    on_change={handle_watch_progress_change.clone()}
                                />
                            }
                        }) }
                        { config_field_child!(translate.t("LABEL.EVENT_STREAM"), "USER_PLAN.EVENT_STREAM", {
                            html! {
                                <ToggleSwitch
                                    value={dialog_form.player_features.as_ref().and_then(|f| f.event_stream).unwrap_or(false)}
                                    on_change={handle_event_stream_change.clone()}
                                />
                            }
                        }) }
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
                        <div class="tp__toolbar">
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_player_features_validation_rejects_zero() {
        let mut plan = UserPlanDto {
            name: "plan_test".to_string(),
            player_features: Some(PlayerFeaturesDto {
                recording_quota: Some(shared::model::ByteSize::new("0")),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(plan.prepare().is_err());

        plan.player_features = Some(PlayerFeaturesDto { max_concurrent_recordings: Some(0), ..Default::default() });
        assert!(plan.prepare().is_err());

        plan.player_features = Some(PlayerFeaturesDto { max_recording_rules: Some(0), ..Default::default() });
        assert!(plan.prepare().is_err());

        plan.player_features = Some(PlayerFeaturesDto { max_free_shared_joins: Some(0), ..Default::default() });
        assert!(plan.prepare().is_err());

        plan.player_features = Some(PlayerFeaturesDto {
            recording_quota: Some(shared::model::ByteSize::new("1024")),
            max_concurrent_recordings: Some(1),
            max_free_shared_joins: Some(1),
            ..Default::default()
        });
        assert!(plan.prepare().is_ok());
    }

    #[test]
    fn plan_player_features_empty_cleared_on_prepare() {
        let mut plan = UserPlanDto {
            name: "plan_empty_feat".to_string(),
            player_features: Some(PlayerFeaturesDto::default()),
            ..Default::default()
        };
        assert!(plan.prepare().is_ok());
        assert_eq!(plan.player_features, None);
    }

    #[test]
    fn plan_player_features_api_enabled_retained() {
        let mut plan = UserPlanDto {
            name: "plan_api_feat".to_string(),
            player_features: Some(PlayerFeaturesDto { api_enabled: Some(true), ..Default::default() }),
            ..Default::default()
        };
        assert!(plan.prepare().is_ok());
        assert_eq!(plan.player_features.unwrap().api_enabled, Some(true));
    }

    #[test]
    fn plan_player_features_api_disabled_retained() {
        let mut plan = UserPlanDto {
            name: "plan_api_disabled".to_string(),
            player_features: Some(PlayerFeaturesDto { api_enabled: Some(false), ..Default::default() }),
            ..Default::default()
        };
        assert!(plan.prepare().is_ok());
        assert_eq!(plan.player_features.unwrap().api_enabled, Some(false));
    }
}
