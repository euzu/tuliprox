use crate::{
    app::components::{
        playlist::stats::{
            epg_coverage_panel::EpgCoveragePanel, input_output_panel::InputOutputPanel, kpi_row::KpiRow,
            stats_meta_line::StatsMetaLine, targets_table::TargetsTable,
        },
        Breadcrumbs, DropDownOption, DropDownSelection, LoadingIndicator, NoContent, Select, TextButton,
    },
    hooks::use_service_context,
    html_if,
    i18n::use_translation,
    model::{derive_effective_target, EventMessage, UiTargetAnalyticsEntry},
};
use gloo_timers::callback::Interval;
use log::error;
use shared::{
    model::{ConfigType, Permission, TargetType},
    utils::{current_time_secs, HEADER_PLAYLIST_SCOPE_REVISION},
};
use std::rc::Rc;
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

#[component]
pub fn PlaylistStatsView() -> Html {
    let translate = use_translation();
    let services = use_service_context();

    let selected_target_name = use_state(|| None::<String>);
    let analytics = use_state(|| (Rc::<Vec<Rc<UiTargetAnalyticsEntry>>>::default(), None::<String>));
    let request_generation = use_mut_ref(|| 0u64);
    let (targets, scope_revision) = &*analytics;
    let is_loading = use_state(|| true);
    let is_updating = use_state(|| false);
    let now = use_state(current_time_secs);

    let can_write_playlist = services.auth.has_permission(Permission::PlaylistWrite);

    // Data reloader: updates targets and now timestamp without resetting user's selection
    let reload_data = {
        let (analytics, is_loading, services, now) =
            (analytics.clone(), is_loading.clone(), services.clone(), now.clone());
        Callback::from(move |()| {
            let (analytics, is_loading, services, now) =
                (analytics.clone(), is_loading.clone(), services.clone(), now.clone());
            let request_generation = request_generation.clone();
            let generation = {
                let mut current = request_generation.borrow_mut();
                *current += 1;
                *current
            };
            spawn_local(async move {
                let result = services.playlist.get_target_analytics().await;
                if generation != *request_generation.borrow() {
                    return;
                }
                now.set(current_time_secs());
                match result {
                    Ok(resp) => {
                        if let Some(body) = resp.body {
                            let items =
                                body.targets.into_iter().map(UiTargetAnalyticsEntry::from).map(Rc::new).collect();
                            analytics.set((Rc::new(items), resp.headers.get(HEADER_PLAYLIST_SCOPE_REVISION).cloned()));
                        }
                    }
                    Err(err) => error!("Failed to fetch target analytics: {err}"),
                }
                is_loading.set(false);
            });
        })
    };

    // Initial load & per-minute interval ticker for relative times
    {
        let (reload_data, now) = (reload_data.clone(), now.clone());
        use_effect_with((), move |()| {
            reload_data.emit(());
            let interval = Interval::new(60_000, move || now.set(current_time_secs()));
            move || drop(interval)
        });
    }

    // WebSocket event listener
    {
        let reload_data = reload_data.clone();
        let services = services.clone();
        use_effect_with((), move |()| {
            let services_cleanup = services.clone();
            let sub_id = services.event.subscribe(move |msg| match msg {
                EventMessage::PlaylistUpdate(_)
                | EventMessage::WebSocketStatus(true)
                | EventMessage::ConfigChange(ConfigType::Sources) => {
                    reload_data.emit(());
                }
                _ => {}
            });
            move || services_cleanup.event.unsubscribe(sub_id)
        });
    }

    // Derive effective target at render time
    let effective_target = derive_effective_target(selected_target_name.as_deref(), targets);
    let effective_target_id = effective_target.map(|t| t.target_id);

    // "Update now" handler
    let handle_update_now = {
        let (is_updating, services, translate, reload_data) =
            (is_updating.clone(), services.clone(), translate.clone(), reload_data.clone());
        let scope_revision = scope_revision.clone();
        Callback::from(move |_| {
            let (Some(target_id), Some(revision)) = (effective_target_id, scope_revision.clone()) else { return };
            is_updating.set(true);
            let (is_updating, services, translate, reload_data) =
                (is_updating.clone(), services.clone(), translate.clone(), reload_data.clone());
            spawn_local(async move {
                match services.playlist.update_target_ids(&[target_id], Some(&revision)).await {
                    Ok(_) => {
                        services.toastr.success(translate.t("MESSAGES.PLAYLIST_UPDATE.SUCCESS"));
                        reload_data.emit(());
                    }
                    Err(err) => {
                        reload_data.emit(());
                        error!("Failed to trigger playlist update: {err}");
                        services.toastr.error(translate.t("MESSAGES.PLAYLIST_UPDATE.FAIL"));
                    }
                }
                is_updating.set(false);
            });
        })
    };

    // CSV export handler
    let handle_export_csv = {
        let (services, translate) = (services.clone(), translate.clone());
        let target_name = effective_target.map_or_else(|| "export".to_string(), |t| t.name.clone());
        let scope_revision = scope_revision.clone();
        let reload_data = reload_data.clone();
        Callback::from(move |()| {
            let (Some(target_id), Some(revision)) = (effective_target_id, scope_revision.clone()) else { return };
            let reload_data = reload_data.clone();
            let (services, translate, target_name) = (services.clone(), translate.clone(), target_name.clone());
            spawn_local(async move {
                if let Err(err) = services.playlist.download_unmapped_csv(target_id, &target_name, &revision).await {
                    reload_data.emit(());
                    error!("Failed to download unmapped CSV: {err}");
                    services.toastr.error(translate.t("LABEL.PLAYLIST_STATS_EXPORT_FAILED"));
                }
            });
        })
    };

    let on_table_select_target = {
        let targets = targets.clone();
        Callback::from(move |id: u16| {
            if let Some(target) = targets.iter().find(|target| target.target_id == id) {
                selected_target_name.set(Some(target.name.clone()));
            }
        })
    };
    let on_target_select = {
        let select = on_table_select_target.clone();
        Callback::from(move |(_, selection): (String, DropDownSelection)| {
            if let DropDownSelection::Single(id) = selection {
                if let Ok(id) = id.parse() {
                    select.emit(id);
                }
            }
        })
    };

    let breadcrumbs = use_state(|| Rc::new(vec![translate.t("LABEL.PLAYLISTS"), translate.t("LABEL.PLAYLIST_STATS")]));

    if *is_loading {
        return html! {
            <div class="tp__playlist-stats"><Breadcrumbs items={&*breadcrumbs} /><LoadingIndicator loading={true} /></div>
        };
    }

    if targets.is_empty() {
        return html! {
            <div class="tp__playlist-stats"><Breadcrumbs items={&*breadcrumbs} /><NoContent text={translate.t("LABEL.PLAYLIST_STATS_NO_TARGETS")} /></div>
        };
    }

    let select_options = Rc::new(
        targets
            .iter()
            .map(|t| {
                DropDownOption::new(
                    &t.target_id.to_string(),
                    html! { t.name.clone() },
                    Some(t.target_id) == effective_target_id,
                )
            })
            .collect::<Vec<_>>(),
    );

    html! {
        <div class="tp__playlist-stats">
            <Breadcrumbs items={&*breadcrumbs} />
            <div class="tp__playlist-stats__header">
                <div class="tp__playlist-stats__header-left">
                    <h1>{translate.t("LABEL.PLAYLIST_STATS_TITLE")}</h1>
                    <p>{translate.t("LABEL.PLAYLIST_STATS_SUBTITLE")}</p>
                </div>
                <div class="tp__playlist-stats__header-tools">
                    <Select
                        name="playlist_stats_target_select"
                        class="tp__target-select"
                        options={select_options}
                        on_select={on_target_select}
                    />
                    { html_if!(can_write_playlist, {
                        <TextButton
                            name="playlist_stats_update_now"
                            icon="Refresh"
                            title={translate.t("LABEL.PLAYLIST_STATS_UPDATE_NOW")}
                            disabled={*is_updating || effective_target_id.is_none() || scope_revision.is_none()}
                            onclick={handle_update_now}
                        />
                    })}
                </div>
            </div>
            <div class="tp__playlist-stats__body">
            { if let Some(entry) = effective_target {
                html! {
                    <>
                        <StatsMetaLine
                            outputs={entry.outputs.clone()}
                            analytics={entry.analytics.clone()}
                            next_scheduled={entry.next_scheduled}
                            now={*now}
                        />
                        { if let Some(ref a) = entry.analytics {
                            let has_xtream = entry.outputs.contains(&TargetType::Xtream);
                            html! {
                                <>
                                    <KpiRow analytics={a.clone()} has_xtream={has_xtream} />
                                    <div class="tp__playlist-stats__panels">
                                        <InputOutputPanel analytics={a.clone()} />
                                        <EpgCoveragePanel
                                            analytics={Rc::clone(a)}
                                            now={*now}
                                            on_export_csv={handle_export_csv}
                                        />
                                    </div>
                                </>
                            }
                        } else {
                            html! {
                                <NoContent
                                    text={translate.t("LABEL.PLAYLIST_STATS_STATUS_NO_DATA")}
                                    hint={translate.t("LABEL.PLAYLIST_STATS_NO_DATA_HINT")}
                                />
                            }
                        }}
                    </>
                }
            } else {
                Html::default()
            }}
            <TargetsTable
                targets={targets.clone()}
                selected_target_id={effective_target_id}
                on_select={on_table_select_target}
                now={*now}
            />
            </div>
        </div>
    }
}
