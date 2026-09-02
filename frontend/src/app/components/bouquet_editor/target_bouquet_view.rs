use crate::{
    app::components::{
        bouquet_editor::{BouquetEditor, BouquetOrigins, BouquetSelection},
        LoadingIndicator, RadioButtonGroup, TextButton,
    },
    hooks::use_service_context,
    i18n::use_translation,
    services::TargetBouquetService,
};
use shared::model::{
    permission::Permission, PlaylistClusterBouquetDto, PlaylistClusterCategoriesDto, TargetBouquetDto,
    TargetBouquetMode, TargetBouquetStreamEventDto, TargetBouquetTargetDto, XtreamCluster,
};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
};
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

type ClusterGroups = HashMap<XtreamCluster, HashSet<String>>;
type ClusterOrigins = HashMap<XtreamCluster, HashMap<String, HashSet<String>>>;

fn finish_catalog(groups: ClusterGroups) -> PlaylistClusterCategoriesDto {
    let finish = |cluster| {
        groups.get(&cluster).map(|values| {
            let mut values: Vec<String> = values.iter().cloned().collect();
            values.sort_unstable();
            values
        })
    };
    PlaylistClusterCategoriesDto {
        live: finish(XtreamCluster::Live).filter(|values| !values.is_empty()),
        vod: finish(XtreamCluster::Video).filter(|values| !values.is_empty()),
        series: finish(XtreamCluster::Series).filter(|values| !values.is_empty()),
    }
}

fn finish_origins(origins: ClusterOrigins) -> BouquetOrigins {
    origins
        .into_iter()
        .map(|(cluster, groups)| {
            let groups = groups
                .into_iter()
                .map(|(name, inputs)| {
                    let mut inputs: Vec<String> = inputs.into_iter().collect();
                    inputs.sort_unstable();
                    (name, inputs)
                })
                .collect();
            (cluster, groups)
        })
        .collect()
}

fn unavailable_groups(
    bouquet: Option<&PlaylistClusterBouquetDto>,
    categories: &PlaylistClusterCategoriesDto,
) -> Vec<(XtreamCluster, String)> {
    let mut unavailable = Vec::new();
    let mut collect = |cluster, selected: Option<&Vec<String>>, available: Option<&Vec<String>>| {
        let available: HashSet<&str> = available.into_iter().flatten().map(String::as_str).collect();
        unavailable.extend(
            selected
                .into_iter()
                .flatten()
                .filter(|name| !available.contains(name.as_str()))
                .cloned()
                .map(|name| (cluster, name)),
        );
    };
    if let Some(bouquet) = bouquet {
        collect(XtreamCluster::Live, bouquet.live.as_ref(), categories.live.as_ref());
        collect(XtreamCluster::Video, bouquet.vod.as_ref(), categories.vod.as_ref());
        collect(XtreamCluster::Series, bouquet.series.as_ref(), categories.series.as_ref());
    }
    unavailable.sort_unstable_by(|left, right| (left.0 as u8).cmp(&(right.0 as u8)).then_with(|| left.1.cmp(&right.1)));
    unavailable
}

#[component]
pub fn TargetBouquetView() -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let can_write = services.auth.has_permission(Permission::PlaylistWrite);

    let is_loading_targets = use_state(|| true);
    let is_loading_groups = use_state(|| false);
    let targets = use_state(Vec::<TargetBouquetTargetDto>::new);
    let selected_target = use_state(|| Option::<u16>::None);
    let categories = use_state(PlaylistClusterCategoriesDto::default);
    let origins = use_state(BouquetOrigins::default);
    let warnings = use_state(Vec::<String>::new);
    let active_bouquet = use_state(|| Option::<TargetBouquetDto>::None);
    let bouquet_mode = use_state(TargetBouquetMode::default);
    let pending_selection = use_mut_ref(BouquetSelection::default);
    let selection_has_changed = use_state(|| false);
    let has_unsaved_changes = use_state(|| false);
    let reload_nonce = use_state(|| 0_u64);
    let request_generation = use_mut_ref(|| 0_u64);

    {
        let is_loading_targets = is_loading_targets.clone();
        let targets = targets.clone();
        let selected_target = selected_target.clone();
        let warnings = warnings.clone();
        let services = services.clone();
        let reload = *reload_nonce;
        use_effect_with(reload, move |_| {
            is_loading_targets.set(true);
            spawn_local(async move {
                match TargetBouquetService::list_targets().await {
                    Ok(loaded) => {
                        let selected_still_exists = (*selected_target)
                            .is_some_and(|selected| loaded.iter().any(|target| target.id == selected));
                        if !selected_still_exists {
                            selected_target.set(loaded.first().map(|target| target.id));
                        }
                        targets.set(loaded);
                    }
                    Err(err) => {
                        targets.set(Vec::new());
                        warnings.set(vec![format!("Failed to load target bouquets: {err}")]);
                        services.toastr.error(format!("Failed to load target bouquets: {err}"));
                    }
                }
                is_loading_targets.set(false);
            });
        });
    }

    {
        let target_id = *selected_target;
        let reload = *reload_nonce;
        let is_loading_groups = is_loading_groups.clone();
        let categories = categories.clone();
        let origins = origins.clone();
        let warnings = warnings.clone();
        let active_bouquet = active_bouquet.clone();
        let bouquet_mode = bouquet_mode.clone();
        let pending_selection = pending_selection.clone();
        let selection_has_changed = selection_has_changed.clone();
        let has_unsaved_changes = has_unsaved_changes.clone();
        let request_generation = request_generation.clone();
        let services = services.clone();

        use_effect_with((target_id, reload), move |(target_id, _)| {
            let generation = {
                let mut current = request_generation.borrow_mut();
                *current = current.wrapping_add(1);
                *current
            };
            let abort_controller = web_sys::AbortController::new().ok();
            let abort_on_cleanup = abort_controller.clone();
            let cleanup = move || {
                if let Some(controller) = abort_on_cleanup {
                    controller.abort();
                }
            };
            let Some(target_id) = *target_id else {
                categories.set(PlaylistClusterCategoriesDto::default());
                origins.set(BouquetOrigins::default());
                active_bouquet.set(None);
                bouquet_mode.set(TargetBouquetMode::default());
                is_loading_groups.set(false);
                return cleanup;
            };
            is_loading_groups.set(true);
            categories.set(PlaylistClusterCategoriesDto::default());
            origins.set(BouquetOrigins::default());
            warnings.set(Vec::new());
            active_bouquet.set(None);
            bouquet_mode.set(TargetBouquetMode::default());
            has_unsaved_changes.set(false);

            spawn_local(async move {
                let mut loaded_groups = ClusterGroups::new();
                let mut loaded_origins = ClusterOrigins::new();
                let mut loaded_warnings = Vec::new();
                let mut selection = None;

                let abort_signal = abort_controller.map(|controller| controller.signal());
                let result =
                    TargetBouquetService::fetch_target_bouquet_stream(target_id, abort_signal, |event| match event {
                        TargetBouquetStreamEventDto::Selection { bouquet } => selection = bouquet,
                        TargetBouquetStreamEventDto::InputChunk { input, cluster, groups, .. } => {
                            for group in groups {
                                loaded_groups.entry(cluster).or_default().insert(group.clone());
                                loaded_origins
                                    .entry(cluster)
                                    .or_default()
                                    .entry(group)
                                    .or_default()
                                    .insert(input.clone());
                            }
                        }
                        TargetBouquetStreamEventDto::InputWarning { message, .. } => loaded_warnings.push(message),
                        _ => {}
                    })
                    .await;

                if *request_generation.borrow() != generation {
                    return;
                }
                match result {
                    Ok(()) => {
                        categories.set(finish_catalog(loaded_groups));
                        origins.set(finish_origins(loaded_origins));
                        warnings.set(loaded_warnings);
                        bouquet_mode
                            .set(selection.as_ref().map_or_else(TargetBouquetMode::default, |value| value.mode));
                        active_bouquet.set(selection);
                        *pending_selection.borrow_mut() = BouquetSelection::default();
                        selection_has_changed.set(false);
                    }
                    Err(err) => {
                        let message = format!("Failed to load target bouquet: {err}");
                        warnings.set(vec![message.clone()]);
                        services.toastr.error(message);
                    }
                }
                is_loading_groups.set(false);
            });

            cleanup
        });
    }

    let on_selection_change = {
        let pending_selection = pending_selection.clone();
        let selection_has_changed = selection_has_changed.clone();
        let has_unsaved_changes = has_unsaved_changes.clone();
        Callback::from(move |sel: Rc<RefCell<BouquetSelection>>| {
            *pending_selection.borrow_mut() = sel.borrow().clone();
            selection_has_changed.set(true);
            has_unsaved_changes.set(true);
        })
    };

    let on_selection_initialize = {
        let pending_selection = pending_selection.clone();
        Callback::from(move |selection: Rc<RefCell<BouquetSelection>>| {
            *pending_selection.borrow_mut() = selection.borrow().clone();
        })
    };

    let on_mode_change = {
        let bouquet_mode = bouquet_mode.clone();
        let active_bouquet = active_bouquet.clone();
        let pending_selection = pending_selection.clone();
        let selection_has_changed = selection_has_changed.clone();
        let categories = categories.clone();
        let has_unsaved_changes = has_unsaved_changes.clone();
        Callback::from(move |selections: Rc<Vec<String>>| {
            let mode = match selections.first().map(String::as_str) {
                Some("blacklist") => TargetBouquetMode::Blacklist,
                _ => TargetBouquetMode::Whitelist,
            };
            if mode != *bouquet_mode {
                if *selection_has_changed {
                    let groups = pending_selection.borrow().to_target_dto(&categories, mode);
                    active_bouquet.set(Some(TargetBouquetDto::new(mode, groups)));
                    selection_has_changed.set(false);
                }
                bouquet_mode.set(mode);
                has_unsaved_changes.set(true);
            }
        })
    };

    let handle_save = {
        let selected_target = selected_target.clone();
        let pending_selection = pending_selection.clone();
        let selection_has_changed = selection_has_changed.clone();
        let categories = categories.clone();
        let active_bouquet = active_bouquet.clone();
        let bouquet_mode = bouquet_mode.clone();
        let reload_nonce = reload_nonce.clone();
        let services = services.clone();
        let translate = translate.clone();

        Callback::from(move |_: String| {
            if let Some(target_id) = *selected_target {
                let groups = if *selection_has_changed {
                    pending_selection.borrow().to_target_dto(&categories, *bouquet_mode)
                } else {
                    active_bouquet
                        .as_ref()
                        .map_or_else(PlaylistClusterBouquetDto::default, |value| value.groups.clone())
                };
                let bouquet = TargetBouquetDto::new(*bouquet_mode, groups);
                let services_state = services.clone();
                let translate_state = translate.clone();
                let reload_nonce_state = reload_nonce.clone();

                spawn_local(async move {
                    match TargetBouquetService::save_target_bouquet(target_id, &bouquet).await {
                        Ok(()) => {
                            services_state.toastr.success(translate_state.t("MESSAGES.SAVE.BOUQUET.SUCCESS"));
                            reload_nonce_state.set(reload_nonce_state.wrapping_add(1));
                        }
                        Err(err) => {
                            services_state.toastr.error(format!("Failed to save target bouquet: {err}"));
                        }
                    }
                });
            }
        })
    };

    let handle_reset = {
        let selected_target = selected_target.clone();
        let reload_nonce = reload_nonce.clone();
        let services = services.clone();
        let translate = translate.clone();

        Callback::from(move |_: String| {
            if let Some(target_id) = *selected_target {
                let services_state = services.clone();
                let translate_state = translate.clone();
                let reload_nonce_state = reload_nonce.clone();

                spawn_local(async move {
                    match TargetBouquetService::delete_target_bouquet(target_id).await {
                        Ok(()) => {
                            services_state.toastr.success(translate_state.t("LABEL.RESET"));
                            reload_nonce_state.set(reload_nonce_state.wrapping_add(1));
                        }
                        Err(err) => {
                            services_state.toastr.error(format!("Failed to reset bouquet: {err}"));
                        }
                    }
                });
            }
        })
    };

    let handle_refresh = {
        let reload_nonce = reload_nonce.clone();
        Callback::from(move |_: String| reload_nonce.set(reload_nonce.wrapping_add(1)))
    };

    let current_target_dto = targets.iter().find(|target| Some(target.id) == *selected_target).cloned();
    let target_names: Vec<String> = targets.iter().map(|target| target.id.to_string()).collect();
    let target_labels: Vec<String> =
        targets.iter().map(|t| if t.restricted { format!("{} ★", t.name) } else { t.name.clone() }).collect();

    let selected_target_vec = Rc::new(selected_target.map(|id| vec![id.to_string()]).unwrap_or_default());
    let unavailable = unavailable_groups(active_bouquet.as_ref().map(|value| &value.groups), &categories);
    let selected_mode = Rc::new(vec![match *bouquet_mode {
        TargetBouquetMode::Whitelist => "whitelist".to_string(),
        TargetBouquetMode::Blacklist => "blacklist".to_string(),
    }]);

    html! {
        <div class="tp__target-bouquet-view">
            <div class="tp__target-bouquet-view__header">
                <div class="tp__target-bouquet-view__title">
                    <h2>{ translate.t("LABEL.TARGET_BOUQUETS") }</h2>
                </div>
                <div class="tp__target-bouquet-view__actions">
                    <TextButton
                        name="Refresh"
                        hint={translate.t("LABEL.REFRESH")}
                        title={translate.t("LABEL.REFRESH")}
                        icon="Refresh"
                        onclick={handle_refresh}
                    />
                    <TextButton
                        name="Reset"
                        title={translate.t("LABEL.RESET")}
                        icon="Trash"
                        disabled={!can_write || selected_target.is_none()}
                        onclick={handle_reset}
                    />
                    <TextButton
                        name="Save"
                        title={translate.t("LABEL.SAVE")}
                        icon="Save"
                        class="secondary"
                        disabled={!can_write || !*has_unsaved_changes}
                        onclick={handle_save}
                    />
                </div>
            </div>

            { if warnings.is_empty() {
                html! {}
            } else {
                html! {
                    <div class="tp__target-bouquet-view__warnings">
                        { for warnings.iter().map(|w| html! {
                            <div class="tp__target-bouquet-view__warning-item">
                                <span class="tp__target-bouquet-view__warning-icon">{ "⚠️" }</span>
                                <span>{ w }</span>
                            </div>
                        }) }
                    </div>
                }
            } }

            <div class="tp__target-bouquet-view__targets-bar">
                <RadioButtonGroup
                    multi_select={false}
                    none_allowed={false}
                    on_select={Callback::from({
                        let selected_target = selected_target.clone();
                        move |selections: Rc<Vec<String>>| {
                            if let Some(target_id) = selections.first().and_then(|value| value.parse::<u16>().ok()) {
                                selected_target.set(Some(target_id));
                            }
                        }
                    })}
                    options={Rc::new(target_names)}
                    labels={Rc::new(target_labels)}
                    selected={selected_target_vec}
                />
            </div>

            { if *is_loading_targets || *is_loading_groups {
                html! { <LoadingIndicator loading={true} /> }
            } else if let Some(target) = current_target_dto {
                html! {
                    <div class="tp__target-bouquet-view__editor-container">
                        <div class="tp__target-bouquet-view__target-meta">
                            <span class="tp__target-bouquet-view__meta-label">{ "Inputs: " }</span>
                            <span class="tp__target-bouquet-view__meta-value">{ target.inputs.join(", ") }</span>
                        </div>
                        <p class="tp__target-bouquet-view__notice">
                            { "Changes are applied by the next playlist update." }
                        </p>
                        <div class="tp__target-bouquet-view__mode">
                            <strong>{ "Selection mode" }</strong>
                            <RadioButtonGroup
                                multi_select={false}
                                none_allowed={false}
                                on_select={on_mode_change}
                                options={Rc::new(vec!["whitelist".to_string(), "blacklist".to_string()])}
                                labels={Rc::new(vec!["Whitelist (include selected)".to_string(), "Blacklist (exclude selected)".to_string()])}
                                selected={selected_mode}
                            />
                        </div>
                        { if unavailable.is_empty() { html! {} } else { html! {
                            <div class="tp__target-bouquet-view__unavailable">
                                <strong>{ "Configured groups currently unavailable:" }</strong>
                                { for unavailable.iter().map(|(cluster, name)| html! {
                                    <span>{ format!("{cluster}: {name}") }</span>
                                }) }
                            </div>
                        } } }
                        <BouquetEditor
                            categories={Some((*categories).clone())}
                            bouquet={active_bouquet.as_ref().map(|value| value.groups.clone())}
                            select_all_when_unrestricted={*bouquet_mode == TargetBouquetMode::Whitelist}
                            origins={Some(Rc::new((*origins).clone()))}
                            on_change={on_selection_change}
                            on_initialize={on_selection_initialize}
                        />
                    </div>
                }
            } else {
                html! {
                    <div class="tp__target-bouquet-view__no-targets">
                        <p>{ "No targets configured." }</p>
                    </div>
                }
            } }
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_selection_is_preserved_for_display() {
        let bouquet = PlaylistClusterBouquetDto {
            live: Some(vec!["News".to_string(), "Temporary".to_string()]),
            vod: None,
            series: None,
        };
        let categories = PlaylistClusterCategoriesDto { live: Some(vec!["News".to_string()]), vod: None, series: None };

        assert_eq!(unavailable_groups(Some(&bouquet), &categories), vec![(XtreamCluster::Live, "Temporary".into())]);
    }
}
