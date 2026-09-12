use super::*;
use crate::model::{build_input_update_card_models, build_input_update_run_views, PlaylistUpdateCardStatuses};
use shared::model::{
    ConfigInputDto, InputType, LibraryScanResult, LibraryStatus, PersistedPlaylistUpdateClusterSnapshot,
    PlaylistUpdateStatusDto, SourcesConfigDto,
};
use yew::virtual_dom::{VNode, VTag};

fn tags(node: &VNode) -> Vec<&VTag> {
    match node {
        VNode::VTag(tag) => std::iter::once(tag.as_ref()).chain(tag.children().into_iter().flat_map(tags)).collect(),
        VNode::VList(list) => list.iter().flat_map(tags).collect(),
        _ => Vec::new(),
    }
}

fn has_attribute(tag: &VTag, key: &str, value: &str) -> bool {
    tag.attributes.iter().any(|(name, contents)| name == key && contents == value)
}

fn visible_text(node: &VNode, expanded: bool) -> String {
    match node {
        VNode::VText(text) => text.text.to_string(),
        VNode::VList(list) => list.iter().map(|node| visible_text(node, expanded)).collect::<Vec<_>>().join(" "),
        VNode::VTag(tag) if tag.tag() == "details" && !expanded => {
            assert!(!tag.attributes.iter().any(|(key, _)| key == "open"), "card starts collapsed");
            let summary = tags(tag.children().unwrap()).into_iter().find(|tag| tag.tag() == "summary").unwrap();
            visible_text(summary.children().unwrap(), expanded)
        }
        VNode::VTag(tag) => tag.children().map_or_else(String::new, |node| visible_text(node, expanded)),
        _ => String::new(),
    }
}

fn card_view(input_type: InputType) -> (InputUpdateCardModel, InputUpdateRunView) {
    let sources = SourcesConfigDto {
        inputs: vec![ConfigInputDto { id: 17, enabled: true, input_type, ..ConfigInputDto::default() }],
        ..SourcesConfigDto::default()
    };
    let persisted = PlaylistUpdateStatusDto::default();
    let cards = build_input_update_card_models(&sources, &persisted);
    let view =
        build_input_update_run_views(&sources, &cards, &PlaylistUpdateCardStatuses::default(), &persisted).remove(0);
    (cards.into_iter().next().unwrap(), view)
}

fn render_content(card: &InputUpdateCardModel, view: &InputUpdateRunView) -> Html {
    // This is the complete content renderer called by InputUpdateCard, not library_details.
    // Unchanged interaction-owned nodes occupy their real Last-update/Target positions.
    input_content(
        view,
        card.capabilities,
        html! { <div class="tp__playlist-update-view__input-card-footer">{"Last update"}</div> },
        html! { <section class="tp__playlist-update-view__input-targets">{"Affected targets"}</section> },
        str::to_string,
    )
}

fn assert_rendered_cluster_badges(node: &VNode, modifier: &str, symbol: char, label: &str) {
    let badges: Vec<_> = tags(node)
        .into_iter()
        .filter(|tag| {
            tag.attributes.iter().any(|(key, value)| {
                key == "class"
                    && value.split_whitespace().any(|class| class == "tp__playlist-update-view__pipeline-status")
            })
        })
        .collect();
    assert_eq!(badges.len(), 6, "three compact chips and three expanded detail badges");
    assert_eq!(badges.iter().filter(|badge| has_attribute(badge, "role", "listitem")).count(), 3);
    for badge in badges {
        assert!(badge
            .attributes
            .iter()
            .any(|(key, value)| key == "class" && value.split_whitespace().any(|class| class == modifier)));
        assert!(visible_text(badge.children().unwrap(), true).contains(symbol));
        if has_attribute(badge, "role", "listitem") {
            assert!(has_attribute(badge, "title", label));
        } else {
            assert!(visible_text(badge.children().unwrap(), true).contains(label));
        }
    }
    assert_eq!(visible_text(node, false).matches(symbol).count(), 3, "details remain collapsed");
    assert_eq!(visible_text(node, true).matches(symbol).count(), 6);
}

#[test]
fn input_update_card_persisted_failed_badges_match_live_errors_after_reload() {
    for input_type in [InputType::Xtream, InputType::Stalker, InputType::M3u] {
        for snapshot in [
            None,
            Some(PersistedPlaylistUpdateClusterSnapshot {
                policy: Some(InputRefreshPolicy::FORCE),
                source: Some(InputDataSourceView::Provider),
                quality_guard_threshold: Some(95),
                ..PersistedPlaylistUpdateClusterSnapshot::default()
            }),
            Some(PersistedPlaylistUpdateClusterSnapshot {
                technical_state: Some(PersistedPlaylistUpdateTechnicalState::Failed),
                ..PersistedPlaylistUpdateClusterSnapshot::default()
            }),
        ] {
            let sources = SourcesConfigDto {
                inputs: vec![ConfigInputDto { id: 17, enabled: true, input_type, ..ConfigInputDto::default() }],
                ..SourcesConfigDto::default()
            };
            let clusters = [XtreamCluster::Live, XtreamCluster::Series, XtreamCluster::Video].map(|cluster| {
                serde_json::json!({"cluster": cluster, "status": "failed", "timestamp": 123, "last_update": snapshot})
            });
            let persisted: PlaylistUpdateStatusDto = serde_json::from_value(serde_json::json!({
                "inputs": [{"input_id": 17, "clusters": clusters}]
            }))
            .unwrap();
            let cards = build_input_update_card_models(&sources, &persisted);
            let view =
                build_input_update_run_views(&sources, &cards, &PlaylistUpdateCardStatuses::default(), &persisted)
                    .remove(0);
            assert!(view.run_id.is_none());
            assert!(
                view.cluster_results.iter().all(|cluster| {
                    cluster.technical_state == snapshot.and_then(|saved| saved.technical_state)
                        && cluster.outcome.is_none()
                        && cluster.quality.is_none()
                        && cluster.active_count.is_none()
                }),
                "display must not invent a technical cause, Quality decision or active population"
            );
            assert_rendered_cluster_badges(
                &render_content(&cards[0], &view),
                "tp__playlist-update-view__pipeline-status--error",
                '✕',
                "LABEL.UPDATE_STATUS_FAILED",
            );
        }
    }
}

#[test]
fn input_update_card_persisted_failed_quality_rejection_keeps_warning_badges() {
    let sources = SourcesConfigDto {
        inputs: vec![ConfigInputDto {
            id: 17,
            enabled: true,
            input_type: InputType::Xtream,
            ..ConfigInputDto::default()
        }],
        ..SourcesConfigDto::default()
    };
    let clusters = [XtreamCluster::Live, XtreamCluster::Series, XtreamCluster::Video].map(|cluster| {
        serde_json::json!({"cluster": cluster, "status": "failed", "timestamp": 123, "last_update": {
            "source": "provider", "technical_state": "succeeded",
            "quality": {"threshold": 95, "baseline_count": 100, "candidate_count": 1, "achieved_quality": 1, "decision": "rejected"}
        }})
    });
    let persisted: PlaylistUpdateStatusDto = serde_json::from_value(serde_json::json!({
        "inputs": [{"input_id": 17, "clusters": clusters}]
    }))
    .unwrap();
    let cards = build_input_update_card_models(&sources, &persisted);
    let view =
        build_input_update_run_views(&sources, &cards, &PlaylistUpdateCardStatuses::default(), &persisted).remove(0);
    assert!(view.cluster_results.iter().all(|cluster| cluster.outcome == Some(InputClusterOutcomeView::Rejected)));
    assert_rendered_cluster_badges(
        &render_content(&cards[0], &view),
        "tp__playlist-update-view__pipeline-status--rejected",
        '⚠',
        "MESSAGES.PLAYLIST_UPDATE.CLUSTER_REJECTED",
    );
}

#[test]
fn pipeline_transparency_input_update_card_shows_failed_live_reload_and_followup_priority() {
    use crate::model::{PlaylistUpdateAcceptedScope, PlaylistUpdateCardStatusAction};
    use shared::model::{
        PlaylistUpdateInputTelemetry, PlaylistUpdateProgressEvent, PlaylistUpdateRunStateEvent, PlaylistUpdateState,
    };
    use std::rc::Rc;

    let sources = SourcesConfigDto {
        inputs: vec![ConfigInputDto {
            id: 17,
            enabled: true,
            input_type: InputType::Xtream,
            ..ConfigInputDto::default()
        }],
        ..SourcesConfigDto::default()
    };
    let persisted: PlaylistUpdateStatusDto = serde_json::from_value(serde_json::json!({
        "inputs": [{"input_id": 17, "clusters": [{"cluster": XtreamCluster::Series, "status": "failed", "timestamp": 123,
            "last_update": {"source": "provider", "technical_state": "failed"}}]}]
    }))
    .unwrap();
    let cards = build_input_update_card_models(&sources, &persisted);
    let connection = WebSocketConnectionContext::new(1, true);
    let project = |state: &PlaylistUpdateCardStatuses| {
        build_input_update_run_views(&sources, &cards, state, &persisted).remove(0)
    };
    let assert_failed = |view: &InputUpdateRunView| {
        let node = render_content(&cards[0], view);
        let badge = tags(&node)
            .into_iter()
            .find(|tag| {
                has_attribute(tag, "role", "listitem")
                    && has_attribute(
                        tag,
                        "aria-label",
                        "MESSAGES.PLAYLIST_UPDATE.CONTENT_SHOWS: LABEL.UPDATE_STATUS_FAILED",
                    )
            })
            .unwrap();
        assert!(badge.attributes.iter().any(|(key, value)| key == "class" && value.contains("pipeline-status--error")));
        assert!(visible_text(badge.children().unwrap(), false).contains('✕'));
        let details = tags(&node)
            .into_iter()
            .find(|tag| has_attribute(tag, "aria-labelledby", "input-update-card-17-series-details"))
            .unwrap();
        assert!(visible_text(details.children().unwrap(), true).contains("LABEL.UPDATE_STATUS_FAILED"));
    };
    assert_failed(&project(&PlaylistUpdateCardStatuses::default()));
    for decision in [serde_json::Value::Null, serde_json::json!("accepted")] {
        let telemetry: PlaylistUpdateInputTelemetry = serde_json::from_value(serde_json::json!({
            "refresh_policy": InputRefreshPolicy::FORCE,
            "clusters": [
                {"cluster": XtreamCluster::Live, "requested": true, "source": "provider", "decision": "accepted"},
                {"cluster": XtreamCluster::Series, "requested": true, "source": "provider", "decision": decision, "technical_state": "failed"},
                {"cluster": XtreamCluster::Video, "requested": true, "source": "provider", "decision": "accepted"}
            ]
        })).unwrap();
        let running = Rc::new(PlaylistUpdateCardStatuses::for_connection(connection)).reduce(
            PlaylistUpdateCardStatusAction::Progress {
                connection_context: connection,
                progress: PlaylistUpdateProgressEvent::input_completed(
                    "R1".into(),
                    1.into(),
                    17,
                    PlaylistUpdateState::Failure,
                    "provider",
                    "completed",
                )
                .with_input_telemetry(telemetry),
            },
        );
        let queued = running.reduce(PlaylistUpdateCardStatusAction::Accepted(PlaylistUpdateAcceptedScope {
            connection_context: connection,
            run_id: "R2".into(),
            input_ids: vec![17],
            refresh_policy: InputRefreshPolicy::REFRESH,
        }));
        let current = project(&queued);
        assert_eq!(current.run_id.as_ref().map(AsRef::as_ref), Some("R1"));
        assert_failed(&current);
        for sibling in current.cluster_results.iter().filter(|cluster| cluster.cluster != XtreamCluster::Series) {
            assert_eq!(cluster_summary_status(sibling), ClusterSummaryStatus::Accepted);
        }
        let next = queued.reduce(PlaylistUpdateCardStatusAction::Completed {
            connection_context: connection,
            completed: PlaylistUpdateRunStateEvent::correlated("R1".into(), 1.into(), PlaylistUpdateState::Failure),
        });
        let next = project(&next);
        assert_eq!(next.run_id.as_ref().map(AsRef::as_ref), Some("R2"));
        assert_eq!(next.overall_state, InputUpdateCardStatus::Queued);
        assert!(next
            .cluster_results
            .iter()
            .all(|cluster| cluster.technical_state.is_none() && cluster.persisted_status.is_none()));
    }
}

#[test]
fn update_overview_badges_cluster_labels() {
    for locale in [
        include_str!("../../../../public/assets/i18n/en.json"),
        include_str!("../../../../public/assets/i18n/ar.json"),
        include_str!("../../../../public/assets/i18n/ru.json"),
    ] {
        let translations: serde_json::Value = serde_json::from_str(locale).unwrap();
        for (cluster, expected) in
            [(XtreamCluster::Live, "Live"), (XtreamCluster::Series, "Shows"), (XtreamCluster::Video, "Movies")]
        {
            let key = cluster_label_key(cluster).replace('.', "/");
            assert_eq!(translations.pointer(&format!("/{key}")).and_then(serde_json::Value::as_str), Some(expected));
        }
    }
}

fn content_badge_labels(card: &InputUpdateCardModel, view: &InputUpdateRunView) -> Vec<String> {
    let translations: serde_json::Value =
        serde_json::from_str(include_str!("../../../../public/assets/i18n/en.json")).unwrap();
    let rendered = input_content(view, card.capabilities, Html::default(), Html::default(), |key| {
        translations
            .pointer(&format!("/{}", key.replace('.', "/")))
            .and_then(serde_json::Value::as_str)
            .unwrap_or(key)
            .to_owned()
    });
    tags(&rendered)
        .into_iter()
        .filter(|tag| has_attribute(tag, "role", "listitem"))
        .map(|tag| {
            let label = tags(tag.children().unwrap()).into_iter().find(|tag| tag.tag() == "span").unwrap();
            visible_text(label.children().unwrap(), false)
        })
        .collect()
}

#[test]
fn update_overview_badges_content_order_is_live_shows_movies() {
    for input_type in [InputType::Xtream, InputType::Stalker, InputType::M3u, InputType::Staged] {
        let (card, mut view) = card_view(input_type);
        view.cluster_results = card_view(InputType::Xtream).1.cluster_results;
        assert_eq!(content_badge_labels(&card, &view), ["Live", "Shows", "Movies"], "{input_type}");
        view.cluster_results.reverse();
        assert_eq!(
            content_badge_labels(&card, &view),
            ["Live", "Shows", "Movies"],
            "incoming telemetry order is not presentation order"
        );
        view.cluster_results.retain(|cluster| cluster.cluster != XtreamCluster::Live);
        assert_eq!(content_badge_labels(&card, &view), ["Shows", "Movies"], "do not invent missing clusters");
    }
}

#[test]
fn update_overview_badges_library_content_order_is_shows_movies() {
    let (card, view) = card_view(InputType::Library);
    let view = view.with_library_catalog(Some(&LibraryStatus {
        enabled: true,
        series: 2,
        movies: 3,
        episodes: 8,
        total_items: 5,
        path: None,
    }));
    assert_eq!(content_badge_labels(&card, &view), ["Shows", "Movies"]);
}

#[test]
fn update_overview_badges_no_mixed_cluster_vocabulary() {
    let translations: serde_json::Value =
        serde_json::from_str(include_str!("../../../../public/assets/i18n/en.json")).unwrap();
    for input_type in [InputType::Xtream, InputType::Stalker, InputType::M3u, InputType::Library] {
        let (card, mut view) = card_view(input_type);
        if input_type == InputType::M3u {
            view.cluster_results = card_view(InputType::Xtream).1.cluster_results;
        }
        if input_type == InputType::Library {
            view = view.with_library_catalog(Some(&LibraryStatus {
                enabled: true,
                movies: 2,
                series: 1,
                episodes: 3,
                total_items: 3,
                path: None,
            }));
        }
        let node = input_content(&view, card.capabilities, Html::default(), Html::default(), |key| {
            translations
                .pointer(&format!("/{}", key.replace('.', "/")))
                .and_then(serde_json::Value::as_str)
                .unwrap_or(key)
                .to_owned()
        });
        let closed = visible_text(&node, false);
        assert!(closed.contains("Shows") && closed.contains("Movies"), "{input_type}: {closed}");
        assert_eq!(closed.contains("Live"), input_type != InputType::Library);
        let expanded = visible_text(&node, true);
        assert!(!expanded.contains("Series") && !expanded.contains("VOD") && !closed.contains("Video"), "{expanded}");
    }
}

#[test]
fn update_overview_badges_no_generic_accepted_for_cluster_content() {
    let (card, mut view) = card_view(InputType::M3u);
    view.cluster_results = card_view(InputType::Xtream).1.cluster_results;
    view.input_state = InputUpdateCardStatus::Success;
    for cluster in &mut view.cluster_results {
        cluster.outcome = Some(InputClusterOutcomeView::Accepted);
        cluster.requested = Some(true);
    }
    let node = render_content(&card, &view);
    let summary = tags(&node)
        .into_iter()
        .find(|tag| has_attribute(tag, "class", "tp__playlist-update-view__pipeline-summary"))
        .unwrap();
    let text = visible_text(summary.children().unwrap(), true);
    assert!(!text.contains("CLUSTER_ACCEPTED") && !text.contains(view.input_state.label_key()));
    for cluster in &view.cluster_results {
        assert!(text.contains(cluster_label_key(cluster.cluster)));
    }
    assert_eq!(
        tags(summary.children().unwrap()).iter().filter(|tag| has_attribute(tag, "role", "listitem")).count(),
        3
    );
}

#[test]
fn update_overview_badges_catalog_content_never_invents_live_or_cluster_success() {
    for input_type in [InputType::Library, InputType::Plex, InputType::Emby, InputType::Jellyfin] {
        let (card, view) = card_view(input_type);
        let view = view.with_library_catalog(Some(&LibraryStatus {
            enabled: true,
            series: 1,
            episodes: 12,
            total_items: 1,
            ..LibraryStatus::default()
        }));
        assert_eq!(
            view.catalog_content_clusters(),
            if input_type == InputType::Library { vec![XtreamCluster::Series] } else { vec![] }
        );
        let node = render_content(&card, &view);
        let closed = visible_text(&node, false);
        assert!(
            !closed.contains("CONTENT_LIVE")
                && !closed.contains("CONTENT_MOVIES")
                && !closed.contains("CLUSTER_ACCEPTED")
        );
        assert_eq!(closed.contains("CONTENT_SHOWS"), input_type == InputType::Library);
    }
}

#[test]
fn pipeline_transparency_input_update_card_library_content_is_compact_and_details_are_library_only() {
    let (card, mut view) = card_view(InputType::Library);
    view.input_state = InputUpdateCardStatus::Failed;
    view.cache_source = Some(InputDataSourceView::Provider); // Actual internal acquisition remains intact.
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
    let rendered = render_content(&card, &view);
    let all_tags = tags(&rendered);
    let summary =
        all_tags.iter().find(|tag| has_attribute(tag, "class", "tp__playlist-update-view__pipeline-summary")).unwrap();
    let target_position =
        all_tags.iter().position(|tag| has_attribute(tag, "class", "tp__playlist-update-view__input-targets")).unwrap();
    let details_position = all_tags.iter().position(|tag| tag.tag() == "details").unwrap();
    assert!(target_position < details_position);
    let closed = visible_text(&rendered, false);
    assert!(
        closed.contains("CONTENT_SHOWS") && closed.contains("CONTENT_MOVIES"),
        "catalog content remains compact: {closed}"
    );
    assert!(!closed.contains(view.input_state.label_key()), "aggregate input outcome is not a content type");
    assert!(closed.contains("Affected targets") && closed.contains("UPDATE_DETAILS") && closed.contains("Last update"));
    assert!(!closed.contains("LIBRARY_"), "Library counters must not leak out of the closed disclosure: {closed}");
    let chips = tags(summary.children().unwrap());
    assert_eq!(chips.iter().filter(|tag| has_attribute(tag, "role", "list")).count(), 1);
    assert_eq!(chips.iter().filter(|tag| has_attribute(tag, "role", "listitem")).count(), 2);
    assert!(!chips.iter().any(|tag| matches!(tag.tag(), "section" | "dl")), "no detail sections inside the chip list");
    let details = visible_text(all_tags[details_position].children().unwrap(), true);
    for label in [
        "CATALOG",
        "LAST_RESCAN",
        "EPISODES",
        "TOTAL_ITEMS",
        "FILES_SCANNED",
        "GROUPS_SCANNED",
        "ADDED",
        "UPDATED",
        "REMOVED",
        "ERRORS",
    ] {
        assert!(details.contains(&format!("LIBRARY_{label}")), "{label} missing from real disclosure: {details}");
    }
    assert!(details.contains("CONTENT_MOVIES") && details.contains("CONTENT_SHOWS"));
    for forbidden in ["LABEL.SOURCE", "LABEL.CACHE", "LABEL.PROVIDER", "CACHE_NOT_USED", "QUALITY_"] {
        assert!(!details.contains(forbidden), "Library rendered generic details: {details}");
    }
    assert!(details.contains("19") && details.contains("11") && details.contains('4'));
    view.library_scan_result = None;
    let reload = visible_text(&render_content(&card, &view), true);
    assert_eq!(reload.matches('—').count(), 6, "catalog survives reload; scan metrics remain unknown");
    view.library_catalog = None;
    assert_eq!(visible_text(&render_content(&card, &view), true).matches('—').count(), 11);
}

#[test]
fn pipeline_transparency_input_update_card_provider_content_keeps_source_cache_and_cluster_quality_details() {
    for input_type in [InputType::Xtream, InputType::Stalker, InputType::M3u, InputType::Plex] {
        for source in [InputDataSourceView::Provider, InputDataSourceView::Cache] {
            let (card, mut view) = card_view(input_type);
            view.cache_source = Some(source);
            view.refresh_policy = Some(InputRefreshPolicy::NORMAL);
            for cluster in &mut view.cluster_results {
                cluster.requested = Some(true);
                cluster.source = Some(source);
                cluster.threshold = Some(95);
                cluster.quality = Some(99);
                cluster.outcome = Some(InputClusterOutcomeView::Accepted);
            }
            let rendered = render_content(&card, &view);
            let closed = visible_text(&rendered, false);
            assert!(!closed.contains("LABEL.SOURCE") && !closed.contains("QUALITY_THRESHOLD"));
            let expanded = visible_text(&rendered, true);
            assert!(!expanded.contains("LIBRARY_"));
            assert!(expanded.contains("LABEL.SOURCE") && expanded.contains("LABEL.CACHE"), "{input_type}: {expanded}");
            assert!(expanded.contains(if source == InputDataSourceView::Provider {
                "CACHE_NOT_USED"
            } else {
                "CACHE_USED"
            }));
            assert_eq!(expanded.contains("QUALITY_THRESHOLD"), !view.cluster_results.is_empty());
            assert_eq!(expanded.contains("QUALITY_ACHIEVED"), !view.cluster_results.is_empty());
        }
    }
}

#[test]
fn input_update_card_general_status_keeps_colors_in_details_without_becoming_a_content_badge() {
    for input_type in [InputType::M3u, InputType::Library, InputType::Plex] {
        for (state, modifier, symbol) in [
            (InputUpdateCardStatus::Ready, "neutral", "–"),
            (InputUpdateCardStatus::Queued, "queued", "●"),
            (InputUpdateCardStatus::Updating, "updating", "●"),
            (InputUpdateCardStatus::Success, "accepted", "✓"),
            (InputUpdateCardStatus::Partial, "rejected", "⚠"),
            (InputUpdateCardStatus::Failed, "error", "✕"),
        ] {
            let (card, mut view) = card_view(input_type);
            view.input_state = state;
            let rendered = render_content(&card, &view);
            let expected_class = format!(
                "tp__playlist-update-view__pipeline-status tp__playlist-update-view__pipeline-status--{modifier}"
            );
            let all_tags = tags(&rendered);
            let badges = all_tags.iter().filter(|tag| has_attribute(tag, "class", &expected_class)).collect::<Vec<_>>();
            assert_eq!(badges.len(), usize::from(input_type != InputType::Library), "{input_type}: {state:?}");
            assert!(!badges.iter().any(|badge| has_attribute(badge, "role", "listitem")));
            for badge in badges {
                let text = visible_text(badge.children().unwrap(), true);
                assert!(text.contains(state.label_key()) && text.contains(symbol), "{text}");
            }
            assert!(!all_tags.iter().any(|tag| tag
                .attributes
                .iter()
                .any(|(key, value)| { key == "class" && value.contains("tp__task-status") })));
        }
    }
}

#[test]
fn input_update_card_header_colors_reload_result_without_claiming_target_success() {
    let (_, mut view) = card_view(InputType::M3u);
    view.input_state = InputUpdateCardStatus::Success;
    let rendered = card_status_badge(&view, str::to_string);
    let text = visible_text(&rendered, true);
    assert!(text.contains("UPDATE_STATUS_SUCCESS") && !text.contains("LABEL.INPUT"));
    let all_tags = tags(&rendered);
    let badge = all_tags[0];
    assert!(has_attribute(badge, "role", "status"));
    assert!(has_attribute(badge, "aria-live", "polite"));
    assert!(has_attribute(badge, "aria-atomic", "true"));
    assert!(has_attribute(
        badge,
        "class",
        "tp__playlist-update-view__pipeline-status tp__playlist-update-view__pipeline-status--accepted"
    ));
    view.run_id = Some("current".into());
    view.overall_state = InputUpdateCardStatus::Updating;
    let active = visible_text(&card_status_badge(&view, str::to_string), true);
    assert!(active.contains("UPDATE_STATUS_UPDATING"));
    assert!(!active.contains("UPDATE_STATUS_SUCCESS") && !active.contains("LABEL.INPUT:"));
    view.overall_state = InputUpdateCardStatus::Failed;
    assert!(visible_text(&card_status_badge(&view, str::to_string), true).contains("UPDATE_STATUS_FAILED"));
}

#[test]
fn input_update_card_header_badges_share_geometry_and_have_no_input_prefix() {
    for input_type in [InputType::Xtream, InputType::M3u, InputType::Library, InputType::Plex] {
        for status in [
            InputUpdateCardStatus::Ready,
            InputUpdateCardStatus::Queued,
            InputUpdateCardStatus::Updating,
            InputUpdateCardStatus::Success,
            InputUpdateCardStatus::Partial,
            InputUpdateCardStatus::Failed,
        ] {
            let (_, mut view) = card_view(input_type);
            view.run_id = Some("active".into());
            view.overall_state = status;
            let rendered = card_status_badge(&view, str::to_string);
            let badge = tags(&rendered)[0];
            assert!(has_attribute(
                badge,
                "class",
                &format!("tp__playlist-update-view__pipeline-status {}", status.modifier())
            ));
            assert!(!badge.attributes.iter().any(|(name, _)| name == "style"));
            assert_eq!(
                visible_text(&rendered, true),
                format!("{} {}", status.label_key(), input_status_symbol(status))
            );
        }
    }
}
