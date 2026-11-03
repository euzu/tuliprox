//! Test-build-only controlled host for the actual details renderer and product CSS.
use super::*;
use crate::model::{build_input_update_card_models, build_input_update_run_views, PlaylistUpdateCardStatuses};
use shared::model::{
    ConfigInputDto, InputType, LibraryScanResult, LibraryStatus, PlaylistUpdateStatusDto, ProcessingOrder,
    SourcesConfigDto,
};
use std::sync::Arc;
use wasm_bindgen::prelude::wasm_bindgen;

fn fixture(name: &str) -> (InputUpdateRunView, InputUpdateCapabilities) {
    let source_type = if name.starts_with("library") { InputType::Library } else { InputType::Xtream };
    let sources = SourcesConfigDto {
        inputs: vec![ConfigInputDto {
            id: 1,
            name: Arc::from("dev0"),
            enabled: true,
            input_type: source_type,
            ..Default::default()
        }],
        ..Default::default()
    };
    let persisted = PlaylistUpdateStatusDto::default();
    let cards = build_input_update_card_models(&sources, &persisted);
    let mut view =
        build_input_update_run_views(&sources, &cards, &PlaylistUpdateCardStatuses::default(), &persisted).remove(0);
    for cluster in &mut view.cluster_results {
        cluster.requested = Some(true);
        cluster.threshold = Some(95);
        cluster.source = Some(InputDataSourceView::Cache);
        if cluster.cluster == XtreamCluster::Live {
            cluster.threshold = Some(100);
            cluster.baseline_count = Some(9705);
            cluster.candidate_count = Some(9673);
            cluster.active_count = Some(9705);
            cluster.quality = Some(99);
            cluster.outcome = Some(InputClusterOutcomeView::Rejected);
            cluster.source = Some(InputDataSourceView::Provider);
            cluster.technical_state = Some(PersistedPlaylistUpdateTechnicalState::Succeeded);
        }
    }
    let orders = [
        ProcessingOrder::Frm,
        ProcessingOrder::Fmr,
        ProcessingOrder::Mrf,
        ProcessingOrder::Rfm,
        ProcessingOrder::Rmf,
        ProcessingOrder::Mfr,
    ];
    view.target_results = orders
        .into_iter()
        .enumerate()
        .map(|(i, order)| TargetRunView {
            target_id: 10 + u16::try_from(i).unwrap(),
            target_name: match i {
                0 => "dev00".into(),
                1 => "dev02".into(),
                2 => "X".repeat(80),
                _ => "duplicate".into(),
            },
            input_ids: vec![1],
            pipeline: Some(TargetPipelineView {
                processing_order: order,
                filter_summary: RuleStageSummary { configured_count: if i == 2 { 9 } else { usize::from(i == 0) } },
                rename_summary: RuleStageSummary {
                    configured_count: if i == 0 {
                        5
                    } else if i == 1 {
                        2
                    } else {
                        0
                    },
                },
                mapping_summary: RuleStageSummary {
                    configured_count: if i == 0 {
                        33
                    } else if i == 1 {
                        7
                    } else {
                        2
                    },
                },
            }),
        })
        .collect();
    if name == "targets" {
        view.target_results.push(TargetRunView {
            target_id: 99,
            target_name: "Unknown pipeline".into(),
            input_ids: vec![1],
            pipeline: None,
        });
    } else {
        view.target_results.truncate(1);
    }
    if name.starts_with("library") {
        view.cluster_results.clear();
        view.library_catalog =
            Some(LibraryStatus { enabled: true, movies: 2, series: 3, episodes: 19, total_items: 5, path: None });
        view.library_scan_result = Some(LibraryScanResult {
            files_scanned: 11,
            groups_scanned: 7,
            files_added: 3,
            files_updated: 2,
            files_removed: 1,
            errors: 4,
        });
        if name == "library-partial" {
            view.library_scan_result = None;
        }
        if name == "library-unknown" {
            view.library_scan_result = None;
            view.library_catalog = None;
        }
    }
    if name == "clusterless" {
        view.source_type = InputType::Plex;
        view.cluster_results.clear();
    }
    if name == "context" {
        view.refresh_policy = Some(InputRefreshPolicy::REFRESH);
        view.run_id = Some("controlled-run".into());
        view.localized_progress_details = vec![PlaylistUpdateProgressDetail::InputCompletedPartially];
    }
    (view, cards[0].capabilities)
}

#[derive(Properties, PartialEq)]
struct FixtureProps {
    view: Rc<InputUpdateRunView>,
    capabilities: InputUpdateCapabilities,
    locale: String,
}

#[component]
fn ActualDetailsFixture(props: &FixtureProps) -> Html {
    let messages: serde_json::Value = serde_json::from_str(match props.locale.as_str() {
        "ar" => include_str!("../../../../../public/assets/i18n/ar.json"),
        "es" => include_str!("../../../../../public/assets/i18n/es.json"),
        "ru" => include_str!("../../../../../public/assets/i18n/ru.json"),
        _ => include_str!("../../../../../public/assets/i18n/en.json"),
    })
    .unwrap();
    let translate = |key: &str| {
        key.split('.')
            .try_fold(&messages, |value, key| value.get(key))
            .and_then(serde_json::Value::as_str)
            .unwrap_or(key)
            .to_string()
    };
    html! {<div class="tp__playlist-update-view"><div class="tp__playlist-update-view__cards"><div class="tp__playlist-update-view__cards-grid">
        <Card class={classes!("tp__playlist-update-view__input-card")}><article><div class="tp__playlist-update-view__input-card-body">
            {input_content(&props.view,props.capabilities,Html::default(),Html::default(),Html::default(),translate)}
        </div></article></Card>
    </div></div></div>}
}

#[wasm_bindgen]
pub fn ap4_mount(scene: &str, locale: &str) {
    wasm_logger::init(wasm_logger::Config::default());
    std::panic::set_hook(Box::new(|info| log::error!("{info}")));
    let (view, capabilities) = fixture(scene);
    let root = web_sys::window().unwrap().document().unwrap().get_element_by_id("fixture-root").unwrap();
    yew::Renderer::<ActualDetailsFixture>::with_root_and_props(
        root,
        FixtureProps { view: Rc::new(view), capabilities, locale: locale.to_string() },
    )
    .render();
}
