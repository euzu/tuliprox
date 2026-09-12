use super::processing::{processing_stages, PlaylistProcessingStage};
use crate::{
    app::components::{
        popup_menu::{PopupMenuPlacement, PopupMenuWidth},
        Card, DropDownOption, DropDownSelection, PlaylistProcessing, Select, TextButton, TitledCard,
    },
    error::Error,
    hooks::use_service_context,
    i18n::use_translation,
    model::{
        last_update_view_model, DialogResult, InputClusterOutcomeView, InputClusterRunView, InputDataSourceView,
        InputUpdateCapabilities, InputUpdateCapabilitiesExt, InputUpdateCardModel, InputUpdateCardState,
        InputUpdateCardStatus, InputUpdateRunView, LastUpdateRelative, PlaylistUpdateAcceptedScope, RuleStageSummary,
        TargetPipelineView, TargetRunView,
    },
    services::{DialogService, WebSocketConnectionContext},
    utils::content_cluster_presentation,
};
#[cfg(not(target_arch = "wasm32"))]
use chrono::{Local, TimeZone};
use shared::model::{
    InputRefreshPolicy, OperationRunAccepted, PersistedPlaylistUpdateClusterState,
    PersistedPlaylistUpdateTechnicalState, PlaylistUpdateRunId, XtreamCluster,
};
use std::rc::Rc;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::JsValue;
use yew::{platform::spawn_local, prelude::*};

const POLICY_NORMAL: &str = "normal";
const POLICY_REFRESH: &str = "refresh";
const POLICY_FORCE: &str = "force";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TargetSelectionState {
    Empty,
    Mixed,
    All,
}

impl TargetSelectionState {
    const fn aria_pressed(self) -> &'static str {
        match self {
            Self::Empty => "false",
            Self::Mixed => "mixed",
            Self::All => "true",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PlaylistUpdateActionOutcome {
    Accepted(Option<PlaylistUpdateRunId>),
    Conflict,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfirmationSubmissionDecision {
    Continue,
    Cancel,
    IgnoreStale,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InputUpdateCardInteractionState {
    controls: InputUpdateCardState,
    submitting_for: Option<WebSocketConnectionContext>,
}

impl InputUpdateCardInteractionState {
    fn from_model(model: &InputUpdateCardModel) -> Self {
        Self {
            controls: InputUpdateCardState {
                selected_target_ids: model.targets.iter().map(|target| target.id).collect(),
                policy: InputRefreshPolicy::NORMAL,
            },
            submitting_for: None,
        }
    }

    const fn is_submitting(&self) -> bool { self.submitting_for.is_some() }

    fn is_submitting_for(&self, connection_context: WebSocketConnectionContext) -> bool {
        self.submitting_for == Some(connection_context)
    }

    // Normalize the current card selection independently of the cloned request_state.
    // Without provider policies, NORMAL is inert; capabilities still gate every action.
    fn policy_for_capabilities(&self, capabilities: InputUpdateCapabilities) -> InputRefreshPolicy {
        if capabilities.supports(self.controls.policy) {
            self.controls.policy
        } else {
            capabilities.policies().first().copied().unwrap_or(InputRefreshPolicy::NORMAL)
        }
    }
}

enum InputUpdateCardAction {
    ToggleTarget(u16),
    ToggleAll(Vec<u16>),
    SelectPolicy(InputRefreshPolicy),
    SyncPolicy(InputUpdateCapabilities),
    BeginSubmission(WebSocketConnectionContext),
    CancelSubmission(WebSocketConnectionContext),
    ConnectionChanged,
    FinishSubmission { connection_context: WebSocketConnectionContext, outcome: PlaylistUpdateActionOutcome },
}

fn target_selection_state(selected_target_ids: &[u16], target_ids: &[u16]) -> TargetSelectionState {
    if target_ids.is_empty() {
        return TargetSelectionState::Empty;
    }
    let selected_count = target_ids.iter().filter(|target_id| selected_target_ids.contains(target_id)).count();
    match selected_count {
        0 => TargetSelectionState::Empty,
        count if count == target_ids.len() => TargetSelectionState::All,
        _ => TargetSelectionState::Mixed,
    }
}

fn reduce_input_update_card_state(
    mut state: InputUpdateCardInteractionState,
    action: InputUpdateCardAction,
) -> InputUpdateCardInteractionState {
    match action {
        InputUpdateCardAction::ToggleTarget(target_id) if !state.is_submitting() => {
            if let Some(index) = state.controls.selected_target_ids.iter().position(|selected| *selected == target_id) {
                state.controls.selected_target_ids.remove(index);
            } else {
                state.controls.selected_target_ids.push(target_id);
            }
        }
        InputUpdateCardAction::ToggleAll(target_ids) if !state.is_submitting() => {
            state.controls.selected_target_ids = if target_selection_state(
                &state.controls.selected_target_ids,
                &target_ids,
            ) == TargetSelectionState::All
            {
                Vec::new()
            } else {
                target_ids
            };
        }
        InputUpdateCardAction::SelectPolicy(policy) if !state.is_submitting() => state.controls.policy = policy,
        InputUpdateCardAction::SyncPolicy(capabilities) => {
            state.controls.policy = state.policy_for_capabilities(capabilities);
        }
        InputUpdateCardAction::BeginSubmission(connection_context) if !state.is_submitting() => {
            state.submitting_for = Some(connection_context);
        }
        InputUpdateCardAction::CancelSubmission(connection_context) if state.is_submitting_for(connection_context) => {
            state.submitting_for = None;
        }
        InputUpdateCardAction::ConnectionChanged => state.submitting_for = None,
        InputUpdateCardAction::FinishSubmission { connection_context, outcome }
            if state.is_submitting_for(connection_context) =>
        {
            state.submitting_for = None;
            if matches!(outcome, PlaylistUpdateActionOutcome::Accepted(_)) {
                state.controls.policy = InputRefreshPolicy::NORMAL;
            }
        }
        InputUpdateCardAction::ToggleTarget(_)
        | InputUpdateCardAction::ToggleAll(_)
        | InputUpdateCardAction::SelectPolicy(_)
        | InputUpdateCardAction::BeginSubmission(_)
        | InputUpdateCardAction::CancelSubmission(_)
        | InputUpdateCardAction::FinishSubmission { .. } => {}
    }
    state
}

impl Reducible for InputUpdateCardInteractionState {
    type Action = InputUpdateCardAction;

    fn reduce(self: Rc<Self>, action: Self::Action) -> Rc<Self> {
        Rc::new(reduce_input_update_card_state((*self).clone(), action))
    }
}

fn policy_from_selection(selection: &DropDownSelection) -> Option<InputRefreshPolicy> {
    let DropDownSelection::Single(policy) = selection else {
        return None;
    };
    match policy.as_str() {
        POLICY_NORMAL => Some(InputRefreshPolicy::NORMAL),
        POLICY_REFRESH => Some(InputRefreshPolicy::REFRESH),
        POLICY_FORCE => Some(InputRefreshPolicy::FORCE),
        _ => None,
    }
}

const fn policy_id(policy: InputRefreshPolicy) -> &'static str {
    if policy.bypasses_quality() {
        POLICY_FORCE
    } else if policy.bypasses_cache() {
        POLICY_REFRESH
    } else {
        POLICY_NORMAL
    }
}

const fn policy_label_key(policy: InputRefreshPolicy) -> &'static str {
    if policy.bypasses_quality() {
        "LABEL.FORCE_UPDATE"
    } else if policy.bypasses_cache() {
        "LABEL.REFRESH"
    } else {
        "LABEL.UPDATE"
    }
}

const fn start_button_label_key(policy: InputRefreshPolicy) -> &'static str {
    if policy.bypasses_quality() {
        "LABEL.FORCE_UPDATE_START"
    } else if policy.bypasses_cache() {
        "LABEL.REFRESH_START"
    } else {
        "LABEL.UPDATE_START"
    }
}

const fn requires_confirmation(policy: InputRefreshPolicy) -> bool { policy.bypasses_quality() }

fn confirmation_allows_submission(policy: InputRefreshPolicy, result: Option<DialogResult>) -> bool {
    !requires_confirmation(policy) || result == Some(DialogResult::Ok)
}

fn confirmation_submission_decision(
    request_connection_context: WebSocketConnectionContext,
    current_connection_context: WebSocketConnectionContext,
    policy: InputRefreshPolicy,
    result: Option<DialogResult>,
) -> ConfirmationSubmissionDecision {
    if !request_connection_context.is_same_live_connection(current_connection_context) {
        ConfirmationSubmissionDecision::IgnoreStale
    } else if confirmation_allows_submission(policy, result) {
        ConfirmationSubmissionDecision::Continue
    } else {
        ConfirmationSubmissionDecision::Cancel
    }
}

fn classify_update_outcome(result: Result<OperationRunAccepted, Error>) -> PlaylistUpdateActionOutcome {
    match result {
        Ok(accepted) => PlaylistUpdateActionOutcome::Accepted(accepted.run_id),
        Err(Error::Conflict(_)) => PlaylistUpdateActionOutcome::Conflict,
        Err(_) => PlaylistUpdateActionOutcome::Failed,
    }
}

fn current_submission_outcome(
    request_connection_context: WebSocketConnectionContext,
    current_connection_context: WebSocketConnectionContext,
    outcome: PlaylistUpdateActionOutcome,
) -> Option<PlaylistUpdateActionOutcome> {
    request_connection_context.is_same_live_connection(current_connection_context).then_some(outcome)
}

fn selected_current_target_ids(model: &InputUpdateCardModel, selected_target_ids: &[u16]) -> Vec<u16> {
    model.targets.iter().map(|target| target.id).filter(|target_id| selected_target_ids.contains(target_id)).collect()
}

fn can_start_update(state: &InputUpdateCardInteractionState, model: &InputUpdateCardModel, can_submit: bool) -> bool {
    can_submit
        && model.capabilities.supports_action(model.capabilities.selected_action(state.controls.policy))
        && (model.capabilities != InputUpdateCapabilities::Rescan
            || state.controls.policy == InputRefreshPolicy::NORMAL)
        && !state.is_submitting()
        && !selected_current_target_ids(model, &state.controls.selected_target_ids).is_empty()
}

fn action_message(template: String, input_name: &str, action: &str) -> String {
    template.replace("{input}", input_name).replace("{action}", action)
}

fn count_message(template: String, count: impl std::fmt::Display) -> String {
    template.replace("{count}", &count.to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClusterSummaryStatus {
    Accepted,
    Rejected,
    Cached,
    TechnicalError,
    PersistedOk,
    PersistedFailed,
    NotRequested,
    Unavailable,
}

impl ClusterSummaryStatus {
    const fn label_key(self) -> &'static str {
        match self {
            Self::Accepted => "MESSAGES.PLAYLIST_UPDATE.CLUSTER_ACCEPTED",
            Self::Rejected => "MESSAGES.PLAYLIST_UPDATE.CLUSTER_REJECTED",
            Self::Cached => "MESSAGES.PLAYLIST_UPDATE.CLUSTER_CACHED",
            Self::TechnicalError | Self::PersistedFailed => "LABEL.UPDATE_STATUS_FAILED",
            Self::PersistedOk => "LABEL.UPDATE_STATUS_SUCCESS",
            Self::NotRequested => "MESSAGES.PLAYLIST_UPDATE.CLUSTER_NOT_REQUESTED",
            Self::Unavailable => "MESSAGES.PLAYLIST_UPDATE.DATA_UNAVAILABLE",
        }
    }

    const fn symbol(self) -> &'static str {
        match self {
            Self::Accepted => "✓",
            Self::Rejected => "⚠",
            Self::Cached => "●",
            Self::TechnicalError | Self::PersistedFailed => "✕",
            Self::PersistedOk => "✓",
            Self::NotRequested | Self::Unavailable => "–",
        }
    }

    const fn modifier(self) -> &'static str {
        match self {
            Self::Accepted => "tp__playlist-update-view__pipeline-status--accepted",
            Self::Rejected => "tp__playlist-update-view__pipeline-status--rejected",
            Self::Cached => "tp__playlist-update-view__pipeline-status--cached",
            Self::TechnicalError | Self::PersistedFailed => "tp__playlist-update-view__pipeline-status--error",
            Self::PersistedOk => "tp__playlist-update-view__pipeline-status--persisted-ok",
            Self::NotRequested | Self::Unavailable => "tp__playlist-update-view__pipeline-status--neutral",
        }
    }
}

fn cluster_summary_status(cluster: &InputClusterRunView) -> ClusterSummaryStatus {
    if !cluster.requested.unwrap_or(cluster.enabled_by_configuration) {
        return ClusterSummaryStatus::NotRequested;
    }
    if cluster.technical_state == Some(PersistedPlaylistUpdateTechnicalState::Failed) {
        return ClusterSummaryStatus::TechnicalError;
    }
    if cluster.outcome == Some(InputClusterOutcomeView::Rejected) {
        return ClusterSummaryStatus::Rejected;
    }
    if cluster.persisted_status.is_some_and(|persisted| {
        persisted.last_update.is_some()
            && persisted.status == PersistedPlaylistUpdateClusterState::Failed
            && persisted.last_update.is_some_and(|snapshot| snapshot.technical_state.is_none())
    }) {
        return ClusterSummaryStatus::PersistedFailed;
    }
    match (cluster.outcome, cluster.source) {
        (Some(InputClusterOutcomeView::TechnicalError), _) => ClusterSummaryStatus::TechnicalError,
        (Some(InputClusterOutcomeView::Rejected), _) => ClusterSummaryStatus::Rejected,
        (_, Some(InputDataSourceView::Cache)) => ClusterSummaryStatus::Cached,
        (Some(InputClusterOutcomeView::Accepted), _) => ClusterSummaryStatus::Accepted,
        (None, Some(InputDataSourceView::Provider))
            if cluster.technical_state == Some(PersistedPlaylistUpdateTechnicalState::Succeeded) =>
        {
            ClusterSummaryStatus::Accepted
        }
        (None, Some(InputDataSourceView::Provider)) => ClusterSummaryStatus::Unavailable,
        (None, None) => match cluster.persisted_status.map(|persisted| persisted.status) {
            Some(PersistedPlaylistUpdateClusterState::Ok) => ClusterSummaryStatus::PersistedOk,
            Some(PersistedPlaylistUpdateClusterState::Failed) => ClusterSummaryStatus::PersistedFailed,
            None => ClusterSummaryStatus::Unavailable,
        },
    }
}

const fn cluster_label_key(cluster: XtreamCluster) -> &'static str { content_cluster_presentation(cluster).label_key }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QualityPresentation {
    Bypassed,
    Disabled,
    Unavailable,
    ConfiguredThreshold { threshold: u8, identical_population_required: bool },
    Evaluated { threshold: u8, quality: u8, identical_population_required: bool },
}

fn quality_presentation(
    cluster: &InputClusterRunView,
    policy: Option<InputRefreshPolicy>,
) -> Option<QualityPresentation> {
    if !cluster.requested.unwrap_or(cluster.enabled_by_configuration) {
        return None;
    }
    if cluster.persisted_status.is_some_and(|persisted| persisted.last_update.is_none()) {
        return Some(cluster.threshold.map_or(QualityPresentation::Disabled, |threshold| {
            QualityPresentation::ConfiguredThreshold { threshold, identical_population_required: threshold == 100 }
        }));
    }
    let Some(threshold) = cluster.threshold else {
        return Some(QualityPresentation::Disabled);
    };
    if let Some(quality) = cluster.quality {
        return Some(QualityPresentation::Evaluated {
            threshold,
            quality,
            identical_population_required: threshold == 100,
        });
    }
    if cluster
        .persisted_status
        .and_then(|persisted| persisted.last_update)
        .is_some_and(|snapshot| snapshot.quality.is_some())
    {
        return Some(QualityPresentation::ConfiguredThreshold {
            threshold,
            identical_population_required: threshold == 100,
        });
    }
    if policy.is_some_and(InputRefreshPolicy::bypasses_quality) {
        return Some(QualityPresentation::Bypassed);
    }
    Some(QualityPresentation::Unavailable)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CachePresentation {
    Used,
    Provider,
    BypassedRefresh,
    BypassedForce,
}

impl CachePresentation {
    const fn label_key(self) -> &'static str {
        match self {
            Self::Used => "MESSAGES.PLAYLIST_UPDATE.CACHE_USED",
            Self::Provider => "MESSAGES.PLAYLIST_UPDATE.CACHE_NOT_USED",
            Self::BypassedRefresh => "MESSAGES.PLAYLIST_UPDATE.CACHE_BYPASSED_REFRESH",
            Self::BypassedForce => "MESSAGES.PLAYLIST_UPDATE.CACHE_BYPASSED_FORCE",
        }
    }
}

fn cache_presentation(
    policy: Option<InputRefreshPolicy>,
    source: Option<InputDataSourceView>,
) -> Option<CachePresentation> {
    Some(match source? {
        InputDataSourceView::Cache => CachePresentation::Used,
        InputDataSourceView::Provider if policy.is_some_and(InputRefreshPolicy::bypasses_quality) => {
            CachePresentation::BypassedForce
        }
        InputDataSourceView::Provider if policy.is_some_and(InputRefreshPolicy::bypasses_cache) => {
            CachePresentation::BypassedRefresh
        }
        InputDataSourceView::Provider => CachePresentation::Provider,
    })
}

const fn source_label_key(source: InputDataSourceView) -> &'static str {
    match source {
        InputDataSourceView::Cache => "LABEL.CACHE",
        InputDataSourceView::Provider => "LABEL.PROVIDER",
    }
}

const fn input_status_symbol(status: InputUpdateCardStatus) -> &'static str {
    match status {
        InputUpdateCardStatus::Ready => "–",
        InputUpdateCardStatus::Queued | InputUpdateCardStatus::Updating => "●",
        InputUpdateCardStatus::Success => "✓",
        InputUpdateCardStatus::Partial => "⚠",
        InputUpdateCardStatus::Failed => "✕",
    }
}

fn card_status_badge(view: &InputUpdateRunView, translate: impl Fn(&str) -> String) -> Html {
    // A persisted input result does not imply a completed target publication.
    // Only a correlated run supplies the current action status.
    let status = if view.run_id.is_none() && view.input_state != InputUpdateCardStatus::Ready {
        view.input_state
    } else {
        view.overall_state
    };
    html! {
        <span
            class={classes!("tp__playlist-update-view__pipeline-status", status.modifier())}
            role="status"
            aria-live="polite"
            aria-atomic="true"
        >
            {translate(status.label_key())}
            <span aria-hidden="true">{input_status_symbol(status)}</span>
        </span>
    }
}

fn detail_row(label: String, value: String) -> Html {
    html! {
        <div class="tp__playlist-update-view__detail-row">
            <dt>{label}</dt>
            <dd>{value}</dd>
        </div>
    }
}

fn quality_details(
    cluster: &InputClusterRunView,
    policy: Option<InputRefreshPolicy>,
    translate: &impl Fn(&str) -> String,
) -> Html {
    match quality_presentation(cluster, policy) {
        None => Html::default(),
        Some(QualityPresentation::Bypassed) => detail_row(
            translate("MESSAGES.PLAYLIST_UPDATE.QUALITY_GUARD"),
            translate("MESSAGES.PLAYLIST_UPDATE.QUALITY_BYPASSED"),
        ),
        Some(QualityPresentation::Disabled) => {
            detail_row(translate("MESSAGES.PLAYLIST_UPDATE.QUALITY_GUARD"), translate("LABEL.DISABLED"))
        }
        Some(QualityPresentation::Unavailable) => detail_row(
            translate("MESSAGES.PLAYLIST_UPDATE.QUALITY_GUARD"),
            translate("MESSAGES.PLAYLIST_UPDATE.DATA_UNAVAILABLE"),
        ),
        Some(QualityPresentation::ConfiguredThreshold { threshold, identical_population_required }) => {
            quality_threshold_details(threshold, identical_population_required, translate)
        }
        Some(QualityPresentation::Evaluated { threshold, quality, identical_population_required }) => html! {
            <>
                {quality_threshold_details(threshold, identical_population_required, translate)}
                {detail_row(
                    translate("MESSAGES.PLAYLIST_UPDATE.QUALITY_ACHIEVED"),
                    format!("{quality}%"),
                )}
            </>
        },
    }
}

fn quality_threshold_details(
    threshold: u8,
    identical_population_required: bool,
    translate: &impl Fn(&str) -> String,
) -> Html {
    html! {
        <>
            {detail_row(
                translate("MESSAGES.PLAYLIST_UPDATE.QUALITY_THRESHOLD"),
                format!("{threshold}%"),
            )}
            if identical_population_required {
                {detail_row(
                    translate("MESSAGES.PLAYLIST_UPDATE.REQUIRED"),
                    translate("MESSAGES.PLAYLIST_UPDATE.IDENTICAL_POPULATION"),
                )}
            }
        </>
    }
}

fn cache_details(
    source: Option<InputDataSourceView>,
    policy: Option<InputRefreshPolicy>,
    translate: &impl Fn(&str) -> String,
) -> Html {
    html! {
        <>
            if let Some(source) = source {
                {detail_row(translate("LABEL.SOURCE"), translate(source_label_key(source)))}
            }
            if let Some(cache) = cache_presentation(policy, source) {
                {detail_row(translate("LABEL.CACHE"), translate(cache.label_key()))}
            }
        </>
    }
}

fn cluster_details(
    input_id: u16,
    cluster: &InputClusterRunView,
    policy: Option<InputRefreshPolicy>,
    translate: &impl Fn(&str) -> String,
) -> Html {
    let status = cluster_summary_status(cluster);
    let heading_id = format!("input-update-card-{input_id}-{}-details", cluster.cluster.as_stream_type());
    let outcome = cluster.outcome.map(|outcome| match outcome {
        InputClusterOutcomeView::Accepted => ClusterSummaryStatus::Accepted,
        InputClusterOutcomeView::Rejected => ClusterSummaryStatus::Rejected,
        InputClusterOutcomeView::TechnicalError => ClusterSummaryStatus::TechnicalError,
    });

    html! {
        <section class="tp__playlist-update-view__cluster-detail" aria-labelledby={heading_id.clone()}>
            <header>
                <h4 id={heading_id}>{translate(cluster_label_key(cluster.cluster))}</h4>
                <span class={classes!("tp__playlist-update-view__pipeline-status", status.modifier())}>
                    <span aria-hidden="true">{status.symbol()}</span>
                    {translate(status.label_key())}
                </span>
            </header>
            <dl>
                if cluster.persisted_status.is_some_and(|persisted| persisted.last_update.is_some()) {
                    if let Some(policy) = policy {
                        {detail_row(
                            translate("MESSAGES.PLAYLIST_UPDATE.MODE"),
                            translate(policy_label_key(policy)),
                        )}
                    }
                }
                if let Some(count) = cluster.baseline_count {
                    {detail_row(translate("MESSAGES.PLAYLIST_UPDATE.BASELINE"), count.to_string())}
                }
                if let Some(count) = cluster.candidate_count {
                    {detail_row(translate("MESSAGES.PLAYLIST_UPDATE.CANDIDATE"), count.to_string())}
                }
                if let Some(count) = cluster.active_count {
                    {detail_row(translate("MESSAGES.PLAYLIST_UPDATE.ACTIVE_POPULATION"), count.to_string())}
                }
                {quality_details(cluster, policy, translate)}
                if let Some(outcome) = outcome {
                    {detail_row(
                        translate(if outcome == ClusterSummaryStatus::TechnicalError {
                            "LABEL.STATUS"
                        } else {
                            "MESSAGES.PLAYLIST_UPDATE.DECISION"
                        }),
                        translate(outcome.label_key()),
                    )}
                }
                if cluster.technical_state == Some(PersistedPlaylistUpdateTechnicalState::Failed)
                    && outcome != Some(ClusterSummaryStatus::TechnicalError) {
                    {detail_row(
                        translate("LABEL.STATUS"),
                        translate(ClusterSummaryStatus::TechnicalError.label_key()),
                    )}
                }
                {cache_details(cluster.source, policy, translate)}
            </dl>
        </section>
    }
}

const fn target_stage_label_key(stage: PlaylistProcessingStage) -> &'static str {
    match stage {
        PlaylistProcessingStage::Filter => "LABEL.FILTER",
        PlaylistProcessingStage::Rename => "LABEL.RENAME",
        PlaylistProcessingStage::Mapping => "LABEL.MAPPING",
    }
}

const fn target_stage_summary(stage: PlaylistProcessingStage, pipeline: TargetPipelineView) -> RuleStageSummary {
    match stage {
        PlaylistProcessingStage::Filter => pipeline.filter_summary,
        PlaylistProcessingStage::Rename => pipeline.rename_summary,
        PlaylistProcessingStage::Mapping => pipeline.mapping_summary,
    }
}

fn target_pipeline_details(input_id: u16, target: &TargetRunView, translate: &impl Fn(&str) -> String) -> Html {
    let heading_id = format!("input-update-card-{input_id}-target-{}-pipeline", target.target_id);

    html! {
        <article
            key={target.target_id}
            class="tp__playlist-update-view__target-detail"
            aria-labelledby={heading_id.clone()}
        >
            <header>
                <h5 id={heading_id}>{target.target_name.as_str()}</h5>
            </header>
            if let Some(pipeline) = target.pipeline {
                <div class="tp__playlist-update-view__target-processing-order">
                    <span>{translate("LABEL.PROCESSING_ORDER")}</span>
                    <PlaylistProcessing order={pipeline.processing_order} />
                </div>
                <dl>
                    {for processing_stages(pipeline.processing_order).into_iter().map(|stage| {
                        let summary = target_stage_summary(stage, pipeline);
                        detail_row(
                            translate(target_stage_label_key(stage)),
                            count_message(
                                translate("MESSAGES.PLAYLIST_UPDATE.CONFIGURED_COUNT"),
                                summary.configured_count,
                            ),
                        )
                    })}
                </dl>
            } else {
                <p class="tp__playlist-update-view__target-pipeline-unavailable">
                    {translate("MESSAGES.PLAYLIST_UPDATE.DATA_UNAVAILABLE")}
                </p>
            }
        </article>
    }
}

fn policy_options(
    capabilities: InputUpdateCapabilities,
    selected_policy: InputRefreshPolicy,
    label: impl Fn(&str) -> String,
) -> Rc<Vec<DropDownOption>> {
    if capabilities == InputUpdateCapabilities::Rescan {
        return Rc::new(vec![DropDownOption::new("rescan", Html::from(label("LABEL.RESCAN")), true)]);
    }
    Rc::new(
        capabilities
            .policies()
            .iter()
            .copied()
            .map(|policy| {
                DropDownOption::new(
                    policy_id(policy),
                    Html::from(label(policy_label_key(policy))),
                    policy == selected_policy,
                )
            })
            .collect(),
    )
}

#[cfg(target_arch = "wasm32")]
fn format_local_timestamp(timestamp: u64) -> String {
    let date = js_sys::Date::new(&JsValue::from_f64(timestamp as f64 * 1_000.0));
    date.to_locale_string("default", &JsValue::UNDEFINED).into()
}

#[cfg(not(target_arch = "wasm32"))]
fn format_local_timestamp(timestamp: u64) -> String {
    i64::try_from(timestamp)
        .ok()
        .and_then(|seconds| Local.timestamp_opt(seconds, 0).single())
        .map_or_else(|| timestamp.to_string(), |date| date.format("%Y-%m-%d %H:%M:%S").to_string())
}

// Render the complete content hierarchy independently of interaction hooks.
// Target controls and Last update keep their existing owners and callbacks.
fn input_content(
    view: &InputUpdateRunView,
    capabilities: InputUpdateCapabilities,
    last_update: Html,
    targets: Html,
    translate: impl Fn(&str) -> String,
) -> Html {
    let pipeline_summary_heading_id = format!("input-update-card-{}-pipeline-summary", view.input_id);
    let target_pipeline_heading_id = format!("input-update-card-{}-target-pipelines", view.input_id);
    let mut catalog_clusters = view.catalog_content_clusters();
    catalog_clusters.sort_by_key(|cluster| content_cluster_presentation(*cluster).rank);
    let mut cluster_results: Vec<_> = view.cluster_results.iter().collect();
    cluster_results.sort_by_key(|cluster| content_cluster_presentation(cluster.cluster).rank);
    html! {
        <div class="tp__playlist-update-view__input-content">
            {last_update}
            <section
                class="tp__playlist-update-view__pipeline-summary"
                aria-labelledby={pipeline_summary_heading_id.clone()}
            >
                <h3 id={pipeline_summary_heading_id}>
                    {translate(if view.cluster_results.is_empty() && catalog_clusters.is_empty() { "LABEL.INPUT" } else { "LABEL.CLUSTER" })}
                </h3>
                <div class="tp__playlist-update-view__cluster-chips" role="list">
                    if view.cluster_results.is_empty() && !catalog_clusters.is_empty() {
                        {for catalog_clusters.iter().map(|cluster| html! {
                            <span class={classes!("tp__playlist-update-view__pipeline-status", InputUpdateCardStatus::Ready.modifier())} role="listitem"
                                aria-label={translate(cluster_label_key(*cluster))}>
                                <span>{translate(cluster_label_key(*cluster))}</span>
                            </span>
                        })}
                    } else if view.cluster_results.is_empty() {
                        <span role="listitem">{"—"}</span>
                    } else {
                        {for cluster_results.iter().map(|cluster| {
                            let cluster_status = cluster_summary_status(cluster);
                            let cluster_label = translate(cluster_label_key(cluster.cluster));
                            let state_label = translate(cluster_status.label_key());
                            html! {
                                <span
                                    class={classes!(
                                        "tp__playlist-update-view__pipeline-status",
                                        cluster_status.modifier(),
                                    )}
                                    role="listitem"
                                    aria-label={format!("{cluster_label}: {state_label}")}
                                    title={state_label}
                                >
                                    <span>{cluster_label}</span>
                                    <span aria-hidden="true">{cluster_status.symbol()}</span>
                                </span>
                            }
                        })}
                    }
                </div>
            </section>
            {targets}
            <details class="tp__playlist-update-view__update-details">
                <summary>{translate("MESSAGES.PLAYLIST_UPDATE.UPDATE_DETAILS")}</summary>
                <div class="tp__playlist-update-view__update-details-body">
                    if let Some(policy) = view.refresh_policy.filter(|_| capabilities != InputUpdateCapabilities::Rescan) {
                        <dl class="tp__playlist-update-view__run-context">
                            {detail_row(
                                translate("MESSAGES.PLAYLIST_UPDATE.MODE"),
                                translate(policy_label_key(policy)),
                            )}
                        </dl>
                    }
                    if view.source_type == shared::model::InputType::Library {
                        {library_update_details::library_details(view, &translate)}
                    } else if view.cluster_results.is_empty() {
                        <section class="tp__playlist-update-view__cluster-detail">
                            <header>
                                <h4>{translate("LABEL.INPUT")}</h4>
                                <span class={classes!("tp__playlist-update-view__pipeline-status", view.input_state.modifier())}>
                                    {translate(view.input_state.label_key())}
                                    <span aria-hidden="true">{input_status_symbol(view.input_state)}</span>
                                </span>
                            </header>
                            <dl>
                                {cache_details(view.cache_source, view.refresh_policy, &translate)}
                            </dl>
                        </section>
                    } else {
                        {for cluster_results.iter().map(|cluster| {
                            cluster_details(
                                view.input_id,
                                cluster,
                                cluster.refresh_policy.or(view.refresh_policy),
                                &translate,
                            )
                        })}
                    }
                    if !view.progress_details.is_empty() {
                        <section class="tp__playlist-update-view__progress-details">
                            <h4>{translate("MESSAGES.PLAYLIST_UPDATE.RUN_DETAILS")}</h4>
                            <ul>
                                {for view.progress_details.iter().map(|detail| html! { <li>{detail}</li> })}
                            </ul>
                        </section>
                    }
                    if !view.target_results.is_empty() {
                        <section
                            class="tp__playlist-update-view__target-details"
                            aria-labelledby={target_pipeline_heading_id.clone()}
                        >
                            <h4 id={target_pipeline_heading_id.clone()}>{translate("LABEL.TARGETS")}</h4>
                            <div class="tp__playlist-update-view__target-details-list">
                                {for view.target_results.iter().map(|target| {
                                    target_pipeline_details(view.input_id, target, &translate)
                                })}
                            </div>
                        </section>
                    }
                </div>
            </details>
        </div>
    }
}

#[derive(Properties, Clone, PartialEq)]
pub struct InputUpdateCardProps {
    pub model: Rc<InputUpdateCardModel>,
    pub view: Rc<InputUpdateRunView>,
    pub now: u64,
    pub connection_context: WebSocketConnectionContext,
    #[prop_or_default]
    pub on_request_accepted: Callback<PlaylistUpdateAcceptedScope>,
    #[prop_or(true)]
    pub can_submit: bool,
}

#[component]
pub fn InputUpdateCard(props: &InputUpdateCardProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let dialog = use_context::<DialogService>().expect("Dialog service not found");
    let model = Rc::clone(&props.model);
    let view = Rc::clone(&props.view);
    let state = use_reducer(|| InputUpdateCardInteractionState::from_model(&model));
    let target_ids = view.target_results.iter().map(|target| target.target_id).collect::<Vec<_>>();
    let all_selection = target_selection_state(&state.controls.selected_target_ids, &target_ids);

    {
        let state = state.clone();
        use_effect_with(props.connection_context, move |_| {
            state.dispatch(InputUpdateCardAction::ConnectionChanged);
            || ()
        });
    }

    {
        let state = state.clone();
        use_effect_with(
            (model.capabilities, state.controls.policy, state.is_submitting()),
            move |(capabilities, _, _)| {
                if state.controls.policy != state.policy_for_capabilities(*capabilities) {
                    state.dispatch(InputUpdateCardAction::SyncPolicy(*capabilities));
                }
                || ()
            },
        );
    }

    // Render consistently while the effect synchronizes the retained reducer state.
    let effective_policy = state.policy_for_capabilities(model.capabilities);
    let policy_options = policy_options(model.capabilities, effective_policy, |key| translate.t(key));

    let on_policy_select = {
        let state = state.clone();
        let capabilities = model.capabilities;
        Callback::from(move |(_name, selection): (String, DropDownSelection)| {
            if let Some(policy) = policy_from_selection(&selection).filter(|policy| capabilities.supports(*policy)) {
                state.dispatch(InputUpdateCardAction::SelectPolicy(policy));
            }
        })
    };

    let on_toggle_all = {
        let state = state.clone();
        let target_ids = target_ids.clone();
        Callback::from(move |_| state.dispatch(InputUpdateCardAction::ToggleAll(target_ids.clone())))
    };

    let on_start = {
        let dialog = dialog.clone();
        let model = Rc::clone(&model);
        let services = services.clone();
        let state = state.clone();
        let translate = translate.clone();
        let can_submit = props.can_submit;
        let connection_context = props.connection_context;
        let on_request_accepted = props.on_request_accepted.clone();
        Callback::from(move |_: String| {
            if !can_start_update(&state, &model, can_submit)
                || !services.websocket.is_current_connection(connection_context)
            {
                return;
            }
            let request_state = state.controls.clone();
            let target_ids = selected_current_target_ids(&model, &request_state.selected_target_ids);
            if target_ids.is_empty() {
                return;
            }
            state.dispatch(InputUpdateCardAction::BeginSubmission(connection_context));
            let dialog = dialog.clone();
            let model = Rc::clone(&model);
            let services = services.clone();
            let state = state.clone();
            let translate = translate.clone();
            let on_request_accepted = on_request_accepted.clone();
            spawn_local(async move {
                let confirmation = if requires_confirmation(request_state.policy) {
                    Some(dialog.confirm(&translate.t("MESSAGES.PLAYLIST_UPDATE.FORCE_CONFIRM")).await)
                } else {
                    None
                };
                match confirmation_submission_decision(
                    connection_context,
                    services.websocket.connection_context(),
                    request_state.policy,
                    confirmation,
                ) {
                    ConfirmationSubmissionDecision::Continue => {}
                    ConfirmationSubmissionDecision::Cancel => {
                        state.dispatch(InputUpdateCardAction::CancelSubmission(connection_context));
                        return;
                    }
                    ConfirmationSubmissionDecision::IgnoreStale => return,
                }

                let result = services
                    .playlist
                    .update_input(&target_ids, model.input_id, model.capabilities.selected_action(request_state.policy))
                    .await;
                let Some(outcome) = current_submission_outcome(
                    connection_context,
                    services.websocket.connection_context(),
                    classify_update_outcome(result),
                ) else {
                    return;
                };
                state
                    .dispatch(InputUpdateCardAction::FinishSubmission { connection_context, outcome: outcome.clone() });
                if let PlaylistUpdateActionOutcome::Accepted(Some(run_id)) = &outcome {
                    on_request_accepted.emit(PlaylistUpdateAcceptedScope {
                        connection_context,
                        run_id: run_id.clone(),
                        input_ids: vec![model.input_id],
                        refresh_policy: request_state.policy,
                    });
                }

                let action = translate.t(if model.capabilities == InputUpdateCapabilities::Rescan {
                    "LABEL.RESCAN"
                } else {
                    policy_label_key(request_state.policy)
                });
                let message_key = match outcome {
                    PlaylistUpdateActionOutcome::Accepted(_) => "MESSAGES.PLAYLIST_UPDATE.INPUT_ACCEPTED",
                    PlaylistUpdateActionOutcome::Conflict => "MESSAGES.PLAYLIST_UPDATE.INPUT_BUSY",
                    PlaylistUpdateActionOutcome::Failed => "MESSAGES.PLAYLIST_UPDATE.INPUT_FAILED",
                };
                let message = action_message(translate.t(message_key), &model.input_name, &action);
                match outcome {
                    PlaylistUpdateActionOutcome::Accepted(_) => services.toastr.success(message),
                    PlaylistUpdateActionOutcome::Conflict => services.toastr.warning(message),
                    PlaylistUpdateActionOutcome::Failed => services.toastr.error(message),
                }
            });
        })
    };

    let heading_id = format!("input-update-card-{}-title", view.input_id);
    let target_group_heading_id = format!("input-update-card-{}-targets", view.input_id);
    let policy_description_id = format!("input-update-card-{}-policy-description", view.input_id);
    let start_label = translate.t(if model.capabilities == InputUpdateCapabilities::Rescan {
        "LABEL.START_RESCAN"
    } else {
        start_button_label_key(effective_policy)
    });
    let action_label = translate.t("LABEL.ACTION");
    let select_label =
        action_message(translate.t("MESSAGES.PLAYLIST_UPDATE.POLICY_LABEL"), &view.input_name, &action_label);
    let start_aria_label =
        action_message(translate.t("MESSAGES.PLAYLIST_UPDATE.START_LABEL"), &view.input_name, &start_label);
    let policy_description = translate.t(model.capabilities.description_key(effective_policy));
    let last_update = last_update_view_model(view.last_update_at, props.now);
    let last_update_text = match last_update.relative {
        LastUpdateRelative::Never => translate.t("LABEL.LAST_UPDATE_NEVER"),
        LastUpdateRelative::JustNow => translate.t("LABEL.LAST_UPDATE_JUST_NOW"),
        LastUpdateRelative::Minutes(1) => translate.t("LABEL.LAST_UPDATE_MINUTE_AGO"),
        LastUpdateRelative::Minutes(count) => count_message(translate.t("LABEL.LAST_UPDATE_MINUTES_AGO"), count),
        LastUpdateRelative::Hours(1) => translate.t("LABEL.LAST_UPDATE_HOUR_AGO"),
        LastUpdateRelative::Hours(count) => count_message(translate.t("LABEL.LAST_UPDATE_HOURS_AGO"), count),
        LastUpdateRelative::LocalDateTime(timestamp) => format_local_timestamp(timestamp),
    };
    let last_update_title = last_update.title_timestamp.map(format_local_timestamp).unwrap_or_default();

    html! {
        <Card class={classes!("tp__playlist-update-view__input-card")}>
            <article aria-labelledby={heading_id.clone()}>
                <header class="tp__playlist-update-view__input-card-header">
                    <div class="tp__playlist-update-view__input-identity">
                        <h2 id={heading_id}>{view.input_name.as_ref()}</h2>
                        <span class="tp__playlist-update-view__input-type">{view.source_type.to_string()}</span>
                    </div>
                    {card_status_badge(&view, |key| translate.t(key))}
                </header>
                <div class="tp__playlist-update-view__input-card-body">
                    {input_content(
                        &view,
                        model.capabilities,
                        html! {
                        <div class="tp__playlist-update-view__input-card-footer">
                            <span>{translate.t("LABEL.LAST_UPDATE")}{":"}</span>
                            <span class="tp__playlist-update-view__last-update" title={last_update_title}>
                                {last_update_text}
                            </span>
                        </div>
                        },
                        html! {
                        <section class="tp__playlist-update-view__input-targets">
                            <h3 id={target_group_heading_id.clone()}>{translate.t("LABEL.AFFECTED_TARGETS")}</h3>
                            if view.target_results.is_empty() {
                                <p class="tp__playlist-update-view__empty-targets">
                                    {translate.t("MESSAGES.PLAYLIST_UPDATE.NO_TARGETS_ASSIGNED")}
                                </p>
                            } else {
                                <div
                                    class="tp__playlist-update-view__target-toggles"
                                    role="group"
                                    aria-labelledby={target_group_heading_id}
                                >
                                    <TextButton
                                        name={format!("input_update_{}_targets_all", view.input_id)}
                                        class={classes!(
                                            "tp__playlist-update-view__target-toggle",
                                            (all_selection == TargetSelectionState::All).then_some("active"),
                                            (all_selection == TargetSelectionState::Mixed).then_some("mixed"),
                                        ).to_string()}
                                        title={translate.t("LABEL.ALL")}
                                        aria_label={Some(translate.t("LABEL.ALL"))}
                                        aria_pressed={Some(all_selection.aria_pressed().to_string())}
                                        disabled={state.is_submitting() || !model.capabilities.rebuilds_targets()}
                                        onclick={on_toggle_all}
                                    />
                                    {for view.target_results.iter().map(|target| {
                                        let selected = state.controls.selected_target_ids.contains(&target.target_id);
                                        let selection = if selected { TargetSelectionState::All } else { TargetSelectionState::Empty };
                                        let state = state.clone();
                                        let target_id = target.target_id;
                                        html! {
                                            <TextButton
                                                key={target.target_id}
                                                name={format!("input_update_{}_target_{target_id}", view.input_id)}
                                                class={classes!(
                                                    "tp__playlist-update-view__target-toggle",
                                                    selected.then_some("active"),
                                                ).to_string()}
                                                title={target.target_name.clone()}
                                                aria_label={Some(target.target_name.clone())}
                                                aria_pressed={Some(selection.aria_pressed().to_string())}
                                                disabled={state.is_submitting() || !model.capabilities.rebuilds_targets()}
                                                onclick={move |_| state.dispatch(InputUpdateCardAction::ToggleTarget(target_id))}
                                            />
                                        }
                                    })}
                                </div>
                            }
                        </section>
                        },
                        |key| translate.t(key),
                    )}
                    <section class="tp__playlist-update-view__input-actions">
                        <div class="tp__playlist-update-view__policy-control">
                            <span class="tp__playlist-update-view__action-label">{action_label}</span>
                            if model.capabilities.rebuilds_targets() {
                                <Select
                                    name={select_label}
                                    icon={Some("ChevronDown".to_string())}
                                    class="tp__playlist-update-view__policy-select"
                                    popup_width={PopupMenuWidth::MatchAnchor}
                                    popup_placement={PopupMenuPlacement::BottomEnd}
                                    options={policy_options}
                                    on_select={on_policy_select}
                                    aria_describedby={policy_description_id.clone()}
                                />
                            } else {
                                <TextButton
                                    name={format!("input_update_{}_unavailable", model.input_id)}
                                    title={translate.t(if model.capabilities == InputUpdateCapabilities::BulkOnly { "LABEL.BULK_ONLY" } else { "LABEL.DISABLED" })}
                                    aria_label={Some(select_label)}
                                    disabled={true}
                                    onclick={Callback::noop()}
                                />
                            }
                        </div>
                        <div class="tp__playlist-update-view__policy-notes">
                            <TitledCard title={translate.t("LABEL.NOTES")}>
                                <p id={policy_description_id} class="tp__playlist-update-view__policy-description" aria-live="polite">
                                    {policy_description}
                                </p>
                            </TitledCard>
                        </div>
                        if model.capabilities.rebuilds_targets() {
                            <TextButton
                                name={format!("input_update_{}", model.input_id)}
                                icon="Refresh"
                                title={start_label.clone()}
                                aria_label={Some(start_aria_label)}
                                disabled={!props.connection_context.is_connected()
                                    || !can_start_update(&state, &model, props.can_submit)}
                                onclick={on_start}
                            />
                        }
                    </section>
                </div>
            </article>
        </Card>
    }
}

#[cfg(test)]
#[path = "input_update_card_integration_tests.rs"]
mod integration_tests;

#[path = "library_update_details.rs"]
mod library_update_details;

#[cfg(test)]
#[path = "input_update_card_render_tests.rs"]
mod render_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{InputUpdateCardTarget, PersistedClusterStatusView};
    use shared::model::{
        InputType, InputUpdateAction, PersistedPlaylistUpdateClusterSnapshot, PersistedPlaylistUpdateQualityDecision,
    };
    use std::sync::Arc;

    fn card(input_id: u16, target_ids: &[u16]) -> InputUpdateCardModel {
        let targets =
            target_ids.iter().map(|target_id| (*target_id, format!("target-{target_id}"))).collect::<Vec<_>>();
        card_with_targets(input_id, &targets)
    }

    fn card_with_targets(input_id: u16, targets: &[(u16, String)]) -> InputUpdateCardModel {
        InputUpdateCardModel {
            input_id,
            input_name: Arc::from(format!("input-{input_id}")),
            input_type: InputType::Xtream,
            capabilities: InputUpdateCapabilities::for_input(&shared::model::ConfigInputDto {
                input_type: InputType::Xtream,
                ..shared::model::ConfigInputDto::default()
            }),
            targets: targets
                .iter()
                .map(|(target_id, name)| InputUpdateCardTarget { id: *target_id, name: name.clone() })
                .collect(),
            last_update_at: None,
        }
    }

    fn reduce(
        state: InputUpdateCardInteractionState,
        action: InputUpdateCardAction,
    ) -> InputUpdateCardInteractionState {
        reduce_input_update_card_state(state, action)
    }

    const fn connection(socket_epoch: u64) -> WebSocketConnectionContext {
        WebSocketConnectionContext::new(socket_epoch, true)
    }

    fn cluster_view(cluster: XtreamCluster) -> InputClusterRunView {
        InputClusterRunView {
            cluster,
            enabled_by_configuration: true,
            requested: None,
            refresh_policy: None,
            threshold: None,
            baseline_count: None,
            candidate_count: None,
            active_count: None,
            quality: None,
            source: None,
            outcome: None,
            technical_state: None,
            persisted_status: None,
        }
    }

    #[test]
    fn input_update_card_cluster_summary_distinguishes_typed_outcomes_and_source() {
        let mut cluster = cluster_view(XtreamCluster::Live);
        assert_eq!(cluster_summary_status(&cluster), ClusterSummaryStatus::Unavailable);

        cluster.outcome = Some(InputClusterOutcomeView::Accepted);
        assert_eq!(cluster_summary_status(&cluster), ClusterSummaryStatus::Accepted);
        cluster.source = Some(InputDataSourceView::Cache);
        assert_eq!(cluster_summary_status(&cluster), ClusterSummaryStatus::Cached);
        cluster.outcome = Some(InputClusterOutcomeView::Rejected);
        assert_eq!(cluster_summary_status(&cluster), ClusterSummaryStatus::Rejected);
        cluster.outcome = Some(InputClusterOutcomeView::TechnicalError);
        assert_eq!(cluster_summary_status(&cluster), ClusterSummaryStatus::TechnicalError);

        cluster.enabled_by_configuration = false;
        assert_eq!(cluster_summary_status(&cluster), ClusterSummaryStatus::NotRequested);
    }

    #[test]
    fn input_update_card_persisted_cluster_status_is_general_and_threshold_only() {
        let mut failed = cluster_view(XtreamCluster::Video);
        failed.threshold = Some(90);
        failed.persisted_status = Some(PersistedClusterStatusView {
            status: PersistedPlaylistUpdateClusterState::Failed,
            timestamp: 202,
            last_update: None,
        });

        let failed_summary = cluster_summary_status(&failed);
        assert_eq!(failed_summary, ClusterSummaryStatus::PersistedFailed);
        assert_eq!(failed_summary.label_key(), "LABEL.UPDATE_STATUS_FAILED");
        assert_ne!(failed_summary, ClusterSummaryStatus::Rejected);
        assert_ne!(failed_summary, ClusterSummaryStatus::TechnicalError);
        assert_eq!(
            quality_presentation(&failed, None),
            Some(QualityPresentation::ConfiguredThreshold { threshold: 90, identical_population_required: false })
        );
        assert_eq!((failed.quality, failed.source, failed.outcome), (None, None, None));

        failed.persisted_status = Some(PersistedClusterStatusView {
            status: PersistedPlaylistUpdateClusterState::Ok,
            timestamp: 303,
            last_update: None,
        });
        let ok_summary = cluster_summary_status(&failed);
        assert_eq!(ok_summary, ClusterSummaryStatus::PersistedOk);
        assert_eq!(ok_summary.label_key(), "LABEL.UPDATE_STATUS_SUCCESS");
    }

    #[test]
    fn persisted_cluster_status_snapshot_renders_facts_without_collapsing_technical_failure_into_quality() {
        let snapshot = PersistedPlaylistUpdateClusterSnapshot {
            policy: Some(InputRefreshPolicy::REFRESH),
            source: Some(InputDataSourceView::Provider),
            quality_guard_threshold: None,
            quality: Some(shared::model::PersistedPlaylistUpdateQualitySnapshot {
                threshold: 90,
                baseline_count: Some(1_000),
                candidate_count: Some(950),
                achieved_quality: Some(95),
                decision: PersistedPlaylistUpdateQualityDecision::Accepted,
            }),
            active_count: None,
            technical_state: Some(PersistedPlaylistUpdateTechnicalState::Failed),
        };
        let mut cluster = cluster_view(XtreamCluster::Live);
        cluster.refresh_policy = snapshot.policy;
        cluster.threshold = snapshot.quality.map(|quality| quality.threshold);
        cluster.baseline_count = snapshot.quality.and_then(|quality| quality.baseline_count);
        cluster.candidate_count = snapshot.quality.and_then(|quality| quality.candidate_count);
        cluster.quality = snapshot.quality.and_then(|quality| quality.achieved_quality);
        cluster.source = snapshot.source;
        cluster.outcome = Some(InputClusterOutcomeView::Accepted);
        cluster.technical_state = snapshot.technical_state;
        cluster.persisted_status = Some(PersistedClusterStatusView {
            status: PersistedPlaylistUpdateClusterState::Failed,
            timestamp: 303,
            last_update: Some(snapshot),
        });

        assert_eq!(cluster_summary_status(&cluster), ClusterSummaryStatus::TechnicalError);
        assert_eq!(cluster.outcome, Some(InputClusterOutcomeView::Accepted));
        assert_eq!(cluster.active_count, None);
        assert_eq!(
            quality_presentation(&cluster, cluster.refresh_policy),
            Some(QualityPresentation::Evaluated { threshold: 90, quality: 95, identical_population_required: false })
        );
        assert_eq!(
            cache_presentation(cluster.refresh_policy, cluster.source),
            Some(CachePresentation::BypassedRefresh)
        );

        let mut bootstrap_snapshot = snapshot;
        bootstrap_snapshot.policy = None;
        bootstrap_snapshot.quality = Some(shared::model::PersistedPlaylistUpdateQualitySnapshot {
            threshold: 90,
            baseline_count: None,
            candidate_count: Some(42),
            achieved_quality: None,
            decision: PersistedPlaylistUpdateQualityDecision::Accepted,
        });
        bootstrap_snapshot.active_count = Some(42);
        bootstrap_snapshot.technical_state = Some(PersistedPlaylistUpdateTechnicalState::Succeeded);
        let mut bootstrap = cluster;
        bootstrap.refresh_policy = None;
        bootstrap.baseline_count = None;
        bootstrap.candidate_count = Some(42);
        bootstrap.active_count = Some(42);
        bootstrap.quality = None;
        bootstrap.technical_state = bootstrap_snapshot.technical_state;
        bootstrap.persisted_status = Some(PersistedClusterStatusView {
            status: PersistedPlaylistUpdateClusterState::Ok,
            timestamp: 404,
            last_update: Some(bootstrap_snapshot),
        });
        assert_eq!(
            quality_presentation(&bootstrap, bootstrap.refresh_policy),
            Some(QualityPresentation::ConfiguredThreshold { threshold: 90, identical_population_required: false }),
            "bootstrap keeps the evaluated threshold without inventing achieved Quality"
        );
    }

    #[test]
    fn persisted_cluster_status_snapshot_keeps_force_bypass_and_disabled_guard_distinct() {
        let mut forced = cluster_view(XtreamCluster::Video);
        forced.refresh_policy = Some(InputRefreshPolicy::FORCE);
        forced.threshold = Some(95);
        forced.source = Some(InputDataSourceView::Provider);
        forced.persisted_status = Some(PersistedClusterStatusView {
            status: PersistedPlaylistUpdateClusterState::Ok,
            timestamp: 404,
            last_update: Some(PersistedPlaylistUpdateClusterSnapshot {
                policy: forced.refresh_policy,
                source: forced.source,
                quality_guard_threshold: Some(95),
                quality: None,
                active_count: Some(73),
                technical_state: Some(PersistedPlaylistUpdateTechnicalState::Succeeded),
            }),
        });
        assert_eq!(quality_presentation(&forced, forced.refresh_policy), Some(QualityPresentation::Bypassed));

        forced.threshold = None;
        forced.persisted_status = Some(PersistedClusterStatusView {
            status: PersistedPlaylistUpdateClusterState::Ok,
            timestamp: 505,
            last_update: Some(PersistedPlaylistUpdateClusterSnapshot {
                policy: forced.refresh_policy,
                source: forced.source,
                quality_guard_threshold: Some(0),
                quality: None,
                active_count: Some(73),
                technical_state: Some(PersistedPlaylistUpdateTechnicalState::Succeeded),
            }),
        });
        assert_eq!(quality_presentation(&forced, forced.refresh_policy), Some(QualityPresentation::Disabled));

        forced.source = Some(InputDataSourceView::Cache);
        assert_eq!(
            cache_presentation(forced.refresh_policy, forced.source),
            Some(CachePresentation::Used),
            "the actual cache source must remain stronger than Force policy"
        );
    }

    #[test]
    fn pipeline_transparency_model_runtime_request_state_controls_cluster_presentation() {
        let mut cluster = cluster_view(XtreamCluster::Live);
        cluster.outcome = Some(InputClusterOutcomeView::Accepted);
        cluster.requested = Some(false);

        assert_eq!(cluster_summary_status(&cluster), ClusterSummaryStatus::NotRequested);
        assert_eq!(quality_presentation(&cluster, Some(InputRefreshPolicy::NORMAL)), None);

        cluster.enabled_by_configuration = false;
        cluster.requested = Some(true);
        assert_eq!(cluster_summary_status(&cluster), ClusterSummaryStatus::Accepted);
    }

    #[test]
    fn input_update_card_quality_disables_guard_for_normal_and_force_without_threshold() {
        let mut cluster = cluster_view(XtreamCluster::Video);
        cluster.quality = Some(37);
        assert_eq!(
            quality_presentation(&cluster, Some(InputRefreshPolicy::NORMAL)),
            Some(QualityPresentation::Disabled)
        );
        assert_eq!(
            quality_presentation(&cluster, Some(InputRefreshPolicy::FORCE)),
            Some(QualityPresentation::Disabled)
        );
    }

    #[test]
    fn input_update_card_quality_bypasses_only_an_active_guard() {
        let mut cluster = cluster_view(XtreamCluster::Video);
        cluster.threshold = Some(90);
        assert_eq!(
            quality_presentation(&cluster, Some(InputRefreshPolicy::FORCE)),
            Some(QualityPresentation::Bypassed)
        );
        assert_eq!(
            quality_presentation(&cluster, Some(InputRefreshPolicy::NORMAL)),
            Some(QualityPresentation::Unavailable)
        );
    }

    #[test]
    fn input_update_card_quality_keeps_threshold_100_identical_population_semantics() {
        let mut cluster = cluster_view(XtreamCluster::Video);
        cluster.threshold = Some(100);
        cluster.quality = Some(100);
        assert_eq!(
            quality_presentation(&cluster, Some(InputRefreshPolicy::NORMAL)),
            Some(QualityPresentation::Evaluated { threshold: 100, quality: 100, identical_population_required: true })
        );
    }

    #[test]
    fn playlist_update_view_pipeline_transparency_target_uses_real_order_and_configured_counts() {
        use PlaylistProcessingStage::{Filter, Mapping, Rename};

        let pipeline = TargetPipelineView {
            processing_order: shared::model::ProcessingOrder::Mfr,
            filter_summary: RuleStageSummary { configured_count: 2 },
            rename_summary: RuleStageSummary { configured_count: 0 },
            mapping_summary: RuleStageSummary { configured_count: 3 },
        };
        let ordered = processing_stages(pipeline.processing_order)
            .map(|stage| (stage, target_stage_summary(stage, pipeline).configured_count));

        assert_eq!(ordered, [(Mapping, 3), (Filter, 2), (Rename, 0)]);

        let source = include_str!("input_update_card.rs").split("#[cfg(test)]").next().expect("card component");
        let disclosure = source.split_once("<details class=\"tp__playlist-update-view__update-details\">").unwrap().1;
        let target_details = disclosure.split_once("if !view.target_results.is_empty()").unwrap().1;
        let target_renderer =
            source.split_once("fn target_pipeline_details").unwrap().1.split_once("fn policy_options").unwrap().0;
        assert!(target_details.contains("target_pipeline_details(view.input_id, target, &translate)"));
        assert!(target_renderer.contains("key={target.target_id}"));
        assert!(target_renderer.contains("target-{}-pipeline\", target.target_id"));
        assert!(target_renderer.contains("<PlaylistProcessing order={pipeline.processing_order} />"));
        assert!(target_renderer.contains("processing_stages(pipeline.processing_order)"));
        assert!(target_renderer.contains("MESSAGES.PLAYLIST_UPDATE.CONFIGURED_COUNT"));
        assert!(target_renderer.contains("summary.configured_count"));
        assert!(target_renderer.contains("if let Some(pipeline) = target.pipeline"));
        assert!(target_renderer.contains("MESSAGES.PLAYLIST_UPDATE.DATA_UNAVAILABLE"));
        for unsupported_runtime_claim in [
            "matched_count",
            "changed_count",
            "removed_count",
            "output_count",
            "overall_state",
            "cluster_summary_status",
        ] {
            assert!(!target_renderer.contains(unsupported_runtime_claim));
        }
    }

    #[test]
    fn input_update_card_cache_provider_refresh_and_force_are_explicit() {
        assert_eq!(
            cache_presentation(Some(InputRefreshPolicy::NORMAL), Some(InputDataSourceView::Cache)),
            Some(CachePresentation::Used)
        );
        assert_eq!(
            cache_presentation(Some(InputRefreshPolicy::NORMAL), Some(InputDataSourceView::Provider)),
            Some(CachePresentation::Provider)
        );
        assert_eq!(
            cache_presentation(Some(InputRefreshPolicy::REFRESH), Some(InputDataSourceView::Provider)),
            Some(CachePresentation::BypassedRefresh)
        );
        assert_eq!(
            cache_presentation(Some(InputRefreshPolicy::FORCE), Some(InputDataSourceView::Provider)),
            Some(CachePresentation::BypassedForce)
        );
        assert_eq!(
            cache_presentation(Some(InputRefreshPolicy::REFRESH), Some(InputDataSourceView::Cache)),
            Some(CachePresentation::Used)
        );
        assert_eq!(
            cache_presentation(Some(InputRefreshPolicy::FORCE), Some(InputDataSourceView::Cache)),
            Some(CachePresentation::Used)
        );
        assert_eq!(cache_presentation(Some(InputRefreshPolicy::REFRESH), None), None);
        assert_eq!(cache_presentation(Some(InputRefreshPolicy::FORCE), None), None);
    }

    #[test]
    fn input_update_card_m3u_partial_and_error_render_without_synthetic_clusters() {
        let source = include_str!("input_update_card.rs").split("#[cfg(test)]").next().expect("card component");
        let summary = source.split_once("class=\"tp__playlist-update-view__pipeline-summary\"").unwrap().1;
        assert!(summary.contains("if view.cluster_results.is_empty()"));
        assert!(summary.contains("\"LABEL.INPUT\""));
        assert!(source.contains("<details class=\"tp__playlist-update-view__update-details\">"));
        assert!(source.contains("cluster.refresh_policy.or(view.refresh_policy)"));
        assert_eq!(input_status_symbol(InputUpdateCardStatus::Partial), "⚠");
        assert_eq!(input_status_symbol(InputUpdateCardStatus::Failed), "✕");
        assert_ne!(ClusterSummaryStatus::Rejected, ClusterSummaryStatus::TechnicalError);
    }

    fn reconfigured_card(input_type: InputType) -> InputUpdateCardModel {
        let mut model = card(7, &[1, 2]);
        model.input_type = input_type;
        model.capabilities = InputUpdateCapabilities::for_input(&shared::model::ConfigInputDto {
            input_type,
            ..shared::model::ConfigInputDto::default()
        });
        model
    }

    #[test]
    fn input_update_card_capability_change_normalizes_retained_force_and_visible_controls() {
        for input_type in [InputType::Xtream, InputType::Stalker] {
            let old_model = reconfigured_card(input_type);
            let model = reconfigured_card(InputType::Plex);
            assert_eq!(model.input_id, old_model.input_id);
            let mut state = InputUpdateCardInteractionState::from_model(&old_model);
            state.controls.policy = InputRefreshPolicy::FORCE;
            state.controls.selected_target_ids = vec![2];

            // Even the render before synchronization must not show an unsupported Plex Force action.
            let visible_policy = state.policy_for_capabilities(model.capabilities);
            assert_eq!(visible_policy, InputRefreshPolicy::NORMAL);
            let options = policy_options(model.capabilities, visible_policy, str::to_owned);
            assert_eq!(
                options.iter().map(|option| (option.id.as_str(), option.selected)).collect::<Vec<_>>(),
                [("normal", true), ("refresh", false)]
            );
            assert_eq!(start_button_label_key(visible_policy), "LABEL.UPDATE_START");
            assert_eq!(
                model.capabilities.description_key(visible_policy),
                "MESSAGES.PLAYLIST_UPDATE.CACHE_UPDATE_DESCRIPTION"
            );
            assert!(!can_start_update(&state, &model, true));

            let synchronized = reduce(state, InputUpdateCardAction::SyncPolicy(model.capabilities));
            assert_eq!(synchronized.controls.policy, InputRefreshPolicy::NORMAL);
            assert_eq!(synchronized.controls.selected_target_ids, [2]);
            assert!(can_start_update(&synchronized, &model, true));
            assert_eq!(
                reduce(synchronized.clone(), InputUpdateCardAction::SyncPolicy(model.capabilities)),
                synchronized
            );
        }
    }

    #[test]
    fn input_update_card_capability_sync_keeps_supported_policies_and_uses_capability_order() {
        let mut state = InputUpdateCardInteractionState::from_model(&reconfigured_card(InputType::Xtream));
        state.controls.selected_target_ids = vec![2];
        for (input_type, policy) in [
            (InputType::M3u, InputRefreshPolicy::NORMAL),
            (InputType::M3u, InputRefreshPolicy::REFRESH),
            (InputType::M3u, InputRefreshPolicy::FORCE),
            (InputType::Stalker, InputRefreshPolicy::FORCE),
        ] {
            state.controls.policy = policy;
            let capabilities = reconfigured_card(input_type).capabilities;
            assert_eq!(state.policy_for_capabilities(capabilities), policy);
            assert_eq!(reduce(state.clone(), InputUpdateCardAction::SyncPolicy(capabilities)), state);
            let submitting = reduce(state.clone(), InputUpdateCardAction::BeginSubmission(connection(1)));
            assert_eq!(submitting.policy_for_capabilities(capabilities), policy);
            assert_eq!(reduce(submitting.clone(), InputUpdateCardAction::SyncPolicy(capabilities)), submitting);
        }
        // The fallback comes from the ordered capability list, not a hardcoded NORMAL/typematrix.
        let capabilities = InputUpdateCapabilities::Manual { policies: &[InputRefreshPolicy::REFRESH] };
        let synchronized = reduce(state, InputUpdateCardAction::SyncPolicy(capabilities));
        assert_eq!(synchronized.controls.policy, InputRefreshPolicy::REFRESH);
        assert_eq!(synchronized.controls.selected_target_ids, [2]);
    }

    #[test]
    fn input_update_card_capability_sync_neutralizes_policy_without_provider_actions() {
        for capabilities in [InputUpdateCapabilities::Rescan, InputUpdateCapabilities::Disabled] {
            let mut model = reconfigured_card(InputType::Xtream);
            model.capabilities = capabilities;
            for policy in [InputRefreshPolicy::FORCE, InputRefreshPolicy::REFRESH] {
                for submitting_for in [None, Some(connection(1))] {
                    let mut state = InputUpdateCardInteractionState::from_model(&model);
                    state.controls.policy = policy;
                    state.controls.selected_target_ids = vec![2];
                    state.submitting_for = submitting_for;
                    let request_snapshot = state.controls.clone();
                    assert_eq!(state.policy_for_capabilities(capabilities), InputRefreshPolicy::NORMAL);
                    let synchronized = reduce(state.clone(), InputUpdateCardAction::SyncPolicy(capabilities));
                    state.controls.policy = InputRefreshPolicy::NORMAL;
                    assert_eq!(synchronized, state);
                    assert!(!capabilities.supports(synchronized.controls.policy));
                    let expected_options =
                        if capabilities == InputUpdateCapabilities::Rescan { vec!["rescan"] } else { Vec::new() };
                    assert_eq!(
                        policy_options(capabilities, synchronized.controls.policy, str::to_owned)
                            .iter()
                            .map(|option| option.id.as_str())
                            .collect::<Vec<_>>(),
                        expected_options
                    );
                    assert_eq!(
                        can_start_update(&synchronized, &model, true),
                        capabilities == InputUpdateCapabilities::Rescan && submitting_for.is_none()
                    );
                    assert_eq!(request_snapshot.policy, policy);
                    assert_eq!(request_snapshot.selected_target_ids, [2]);
                }
            }
        }
    }

    #[test]
    fn input_update_card_capability_sync_during_submission_preserves_request_snapshot() {
        let old_model = reconfigured_card(InputType::Xtream);
        let model = reconfigured_card(InputType::Plex);
        assert_eq!(old_model.input_id, model.input_id);
        let mut state = InputUpdateCardInteractionState::from_model(&old_model);
        state.controls.policy = InputRefreshPolicy::FORCE;
        state.controls.selected_target_ids = vec![2];
        let request_snapshot = state.controls.clone();
        let submitting = reduce(state, InputUpdateCardAction::BeginSubmission(connection(1)));
        let visible_policy = submitting.policy_for_capabilities(model.capabilities);
        assert_eq!(visible_policy, InputRefreshPolicy::NORMAL);
        let options = policy_options(model.capabilities, visible_policy, str::to_owned);
        assert_eq!(
            options.iter().map(|option| (option.id.as_str(), option.selected)).collect::<Vec<_>>(),
            [("normal", true), ("refresh", false)]
        );
        assert_eq!(start_button_label_key(visible_policy), "LABEL.UPDATE_START");
        assert_eq!(
            model.capabilities.description_key(visible_policy),
            "MESSAGES.PLAYLIST_UPDATE.CACHE_UPDATE_DESCRIPTION"
        );
        let synchronized = reduce(submitting.clone(), InputUpdateCardAction::SyncPolicy(model.capabilities));
        let mut expected = submitting;
        expected.controls.policy = InputRefreshPolicy::NORMAL;
        assert_eq!(synchronized, expected);
        assert_eq!(synchronized.policy_for_capabilities(model.capabilities), InputRefreshPolicy::NORMAL);
        assert!(!can_start_update(&synchronized, &model, true));
        assert_eq!(request_snapshot.policy, InputRefreshPolicy::FORCE);
        assert_eq!(selected_current_target_ids(&old_model, &request_snapshot.selected_target_ids), [2]);

        for outcome in [
            PlaylistUpdateActionOutcome::Accepted(Some("accepted".into())),
            PlaylistUpdateActionOutcome::Conflict,
            PlaylistUpdateActionOutcome::Failed,
        ] {
            let finished = reduce(
                synchronized.clone(),
                InputUpdateCardAction::FinishSubmission { connection_context: connection(1), outcome },
            );
            assert!(!finished.is_submitting());
            assert_eq!(finished.policy_for_capabilities(model.capabilities), InputRefreshPolicy::NORMAL);
            let finished = reduce(finished, InputUpdateCardAction::SyncPolicy(model.capabilities));
            assert_eq!(finished.controls.policy, InputRefreshPolicy::NORMAL);
            assert_eq!(finished.controls.selected_target_ids, request_snapshot.selected_target_ids);
            assert_eq!(request_snapshot.policy, InputRefreshPolicy::FORCE);
        }
    }

    #[test]
    fn input_update_card_capability_sync_keeps_submission_ownership_for_stale_callbacks() {
        let capabilities = reconfigured_card(InputType::Plex).capabilities;
        for policy in [InputRefreshPolicy::FORCE, InputRefreshPolicy::REFRESH] {
            let mut state = InputUpdateCardInteractionState::from_model(&reconfigured_card(InputType::Xtream));
            state.controls.policy = policy;
            state.controls.selected_target_ids = vec![2];
            let old_snapshot = state.controls.clone();
            let previous = reduce(state, InputUpdateCardAction::BeginSubmission(connection(1)));
            let reconnected = reduce(previous, InputUpdateCardAction::ConnectionChanged);
            let request_snapshot = reconnected.controls.clone();
            let submitting = reduce(reconnected, InputUpdateCardAction::BeginSubmission(connection(2)));
            let synchronized = reduce(submitting, InputUpdateCardAction::SyncPolicy(capabilities));
            let expected_policy = if policy == InputRefreshPolicy::FORCE { InputRefreshPolicy::NORMAL } else { policy };
            assert_eq!(synchronized.controls.policy, expected_policy);
            assert_eq!(synchronized.submitting_for, Some(connection(2)));

            for confirmation in [DialogResult::Cancel, DialogResult::Ok] {
                assert_eq!(
                    confirmation_submission_decision(
                        connection(1),
                        connection(2),
                        old_snapshot.policy,
                        Some(confirmation)
                    ),
                    ConfirmationSubmissionDecision::IgnoreStale
                );
            }
            let cancelled = reduce(synchronized.clone(), InputUpdateCardAction::CancelSubmission(connection(1)));
            assert_eq!(cancelled, synchronized);
            for outcome in [
                PlaylistUpdateActionOutcome::Accepted(Some("old-run".into())),
                PlaylistUpdateActionOutcome::Conflict,
                PlaylistUpdateActionOutcome::Failed,
            ] {
                assert_eq!(current_submission_outcome(connection(1), connection(2), outcome.clone()), None);
                assert_eq!(
                    reduce(
                        synchronized.clone(),
                        InputUpdateCardAction::FinishSubmission { connection_context: connection(1), outcome }
                    ),
                    synchronized
                );
            }
            assert_eq!(request_snapshot.policy, policy);
            assert_eq!(request_snapshot.selected_target_ids, [2]);
            assert_eq!(old_snapshot, request_snapshot);
            for action in
                [InputUpdateCardAction::CancelSubmission(connection(2)), InputUpdateCardAction::ConnectionChanged]
            {
                let idle = reduce(synchronized.clone(), action);
                assert!(!idle.is_submitting());
                assert_eq!(idle.controls, synchronized.controls);
                assert_eq!(reduce(idle.clone(), InputUpdateCardAction::SyncPolicy(capabilities)), idle);
            }
        }
    }

    #[test]
    fn input_update_card_capability_sync_is_wired_to_model_policy_and_submission_changes() {
        let source = include_str!("input_update_card.rs").split("#[cfg(test)]").next().expect("card component");
        assert!(source.contains("(model.capabilities, state.controls.policy, state.is_submitting())"));
        assert!(source.contains("state.dispatch(InputUpdateCardAction::SyncPolicy(*capabilities))"));
        assert!(source.contains("let effective_policy = state.policy_for_capabilities(model.capabilities)"));
        for binding in [
            "policy_options(model.capabilities, effective_policy,",
            "start_button_label_key(effective_policy)",
            "description_key(effective_policy)",
        ] {
            assert!(source.contains(binding), "missing effective policy binding: {binding}");
        }
    }

    #[test]
    fn playlist_update_view_initializes_all_targets_selected_by_stable_id() {
        let state = InputUpdateCardInteractionState::from_model(&card(7, &[30, 10, 20]));

        assert_eq!(state.controls.selected_target_ids, vec![30, 10, 20]);
        assert_eq!(state.controls.policy, InputRefreshPolicy::NORMAL);
        assert!(!state.is_submitting());
    }

    #[test]
    fn playlist_update_view_all_selection_covers_full_mixed_empty_and_toggle_rules() {
        let targets = vec![1, 2, 3];
        let initial = InputUpdateCardInteractionState::from_model(&card(7, &targets));
        assert_eq!(target_selection_state(&initial.controls.selected_target_ids, &targets), TargetSelectionState::All);

        let empty = reduce(initial, InputUpdateCardAction::ToggleAll(targets.clone()));
        assert_eq!(target_selection_state(&empty.controls.selected_target_ids, &targets), TargetSelectionState::Empty);

        let all = reduce(empty, InputUpdateCardAction::ToggleAll(targets.clone()));
        let mixed = reduce(all, InputUpdateCardAction::ToggleTarget(2));
        assert_eq!(target_selection_state(&mixed.controls.selected_target_ids, &targets), TargetSelectionState::Mixed);

        let individually_restored = reduce(mixed.clone(), InputUpdateCardAction::ToggleTarget(2));
        assert_eq!(
            target_selection_state(&individually_restored.controls.selected_target_ids, &targets),
            TargetSelectionState::All
        );
        let cleared = reduce(individually_restored, InputUpdateCardAction::ToggleAll(targets.clone()));
        assert!(cleared.controls.selected_target_ids.is_empty());
        assert_eq!(
            target_selection_state(&cleared.controls.selected_target_ids, &targets),
            TargetSelectionState::Empty
        );

        let restored = reduce(mixed, InputUpdateCardAction::ToggleAll(targets.clone()));
        assert_eq!(restored.controls.selected_target_ids, targets);
    }

    #[test]
    fn playlist_update_view_target_group_exposes_the_mixed_toggle_state() {
        assert_eq!(TargetSelectionState::Empty.aria_pressed(), "false");
        assert_eq!(TargetSelectionState::Mixed.aria_pressed(), "mixed");
        assert_eq!(TargetSelectionState::All.aria_pressed(), "true");
    }

    #[test]
    fn playlist_update_view_target_button_labels_stay_plain_and_preserve_accessible_selection_states() {
        let source = include_str!("input_update_card.rs").split("#[cfg(test)]").next().expect("card component");
        assert!(source.contains("title={translate.t(\"LABEL.ALL\")}"));
        assert!(source.contains("title={target.target_name.clone()}"));
        for (selection, expected_pressed) in [
            (TargetSelectionState::Empty, "false"),
            (TargetSelectionState::Mixed, "mixed"),
            (TargetSelectionState::All, "true"),
        ] {
            assert_eq!(selection.aria_pressed(), expected_pressed);
        }
    }

    #[test]
    fn playlist_update_view_card_states_are_independent_by_stable_input_id() {
        let first = InputUpdateCardInteractionState::from_model(&card(7, &[1, 2]));
        let second = InputUpdateCardInteractionState::from_model(&card(8, &[3, 4]));

        let first = reduce(first, InputUpdateCardAction::ToggleTarget(1));

        assert_eq!(first.controls.selected_target_ids, vec![2]);
        assert_eq!(second.controls.selected_target_ids, vec![3, 4]);
    }

    #[test]
    fn playlist_update_view_start_is_disabled_without_selected_targets() {
        let targetless = card(7, &[]);
        let state = InputUpdateCardInteractionState::from_model(&targetless);
        let populated = card(7, &[1]);

        assert!(!can_start_update(&state, &targetless, true));
        assert!(!can_start_update(&InputUpdateCardInteractionState::from_model(&populated), &populated, false));
    }

    #[test]
    fn playlist_update_view_maps_select_values_directly_to_existing_policies() {
        let cases = [
            (POLICY_NORMAL, InputRefreshPolicy::NORMAL),
            (POLICY_REFRESH, InputRefreshPolicy::REFRESH),
            (POLICY_FORCE, InputRefreshPolicy::FORCE),
        ];
        for (id, expected) in cases {
            assert_eq!(policy_from_selection(&DropDownSelection::Single(id.to_string())), Some(expected));
            assert_eq!(policy_id(expected), id);
        }
        assert_eq!(policy_from_selection(&DropDownSelection::Empty), None);
    }

    #[test]
    fn playlist_update_view_policy_options_only_render_mode_names_and_keep_selected_policy() {
        let cases = [
            (InputRefreshPolicy::NORMAL, POLICY_NORMAL, "Update"),
            (InputRefreshPolicy::REFRESH, POLICY_REFRESH, "Refresh"),
            (InputRefreshPolicy::FORCE, POLICY_FORCE, "Force Update"),
        ];
        for (selected, _, _) in cases {
            let options = policy_options(card(7, &[1]).capabilities, selected, |key| {
                match key {
                    "LABEL.UPDATE" => "Update",
                    "LABEL.REFRESH" => "Refresh",
                    "LABEL.FORCE_UPDATE" => "Force Update",
                    _ => panic!("only mode-name translations belong in the dropdown"),
                }
                .to_string()
            });

            assert_eq!(options.len(), cases.len());
            for (option, (policy, id, label)) in options.iter().zip(cases) {
                assert_eq!(option.id, id);
                assert_eq!(option.label, Html::from(label));
                assert_eq!(option.selected, policy == selected);
            }
        }
    }

    #[test]
    fn input_update_capabilities_control_options_and_start_without_a_component_type_filter() {
        let cases: &[(InputType, &[&str], bool)] = &[
            (InputType::Xtream, &[POLICY_NORMAL, POLICY_REFRESH, POLICY_FORCE], true),
            (InputType::Stalker, &[POLICY_NORMAL, POLICY_REFRESH, POLICY_FORCE], true),
            (InputType::M3u, &[POLICY_NORMAL, POLICY_REFRESH, POLICY_FORCE], true),
            (InputType::M3uBatch, &[], false),
            (InputType::XtreamBatch, &[], false),
            (InputType::StalkerBatch, &[], false),
            (InputType::Staged, &[], false),
            (InputType::Library, &["rescan"], true),
            (InputType::Plex, &[POLICY_NORMAL, POLICY_REFRESH], true),
            (InputType::Jellyfin, &[], false),
            (InputType::Emby, &[], false),
        ];
        for &(input_type, expected, starts_playlist_update) in cases {
            for enabled in [true, false] {
                let mut model = card(7, &[3, 9]);
                // Leave the display type as Xtream: controls must consume only the supplied capabilities.
                model.capabilities = InputUpdateCapabilities::for_input(&shared::model::ConfigInputDto {
                    input_type,
                    enabled,
                    ..shared::model::ConfigInputDto::default()
                });
                let expected = if enabled { expected } else { &[] };
                for policy in [InputRefreshPolicy::NORMAL, InputRefreshPolicy::REFRESH, InputRefreshPolicy::FORCE] {
                    let options = policy_options(model.capabilities, policy, str::to_owned);
                    assert_eq!(options.iter().map(|option| option.id.as_str()).collect::<Vec<_>>(), expected);
                    let state = reduce(
                        InputUpdateCardInteractionState::from_model(&model),
                        InputUpdateCardAction::SelectPolicy(policy),
                    );
                    assert_eq!(
                        can_start_update(&state, &model, true),
                        enabled
                            && starts_playlist_update
                            && ((expected == ["rescan"] && policy == InputRefreshPolicy::NORMAL)
                                || expected.contains(&policy_id(policy)))
                    );
                    assert!(!can_start_update(&state, &model, false));
                    let empty_selection = reduce(state, InputUpdateCardAction::ToggleAll(vec![3, 9]));
                    assert!(!can_start_update(&empty_selection, &model, true));
                }
            }
        }
    }

    #[test]
    fn input_update_card_library_rescan_uses_common_submission_and_requires_targets() {
        let mut model = card(7, &[3, 9]);
        model.capabilities = InputUpdateCapabilities::Rescan;
        let state = InputUpdateCardInteractionState::from_model(&model);
        assert!(can_start_update(&state, &model, true));
        assert!(!can_start_update(&state, &model, false));
        assert_eq!(model.capabilities.selected_action(state.controls.policy), InputUpdateAction::Rescan);
        assert_eq!(policy_options(model.capabilities, state.controls.policy, str::to_owned)[0].id, "rescan");
        let submitting = reduce(state.clone(), InputUpdateCardAction::BeginSubmission(connection(1)));
        assert!(!can_start_update(&submitting, &model, true));
        let empty = reduce(state, InputUpdateCardAction::ToggleAll(vec![3, 9]));
        assert!(!can_start_update(&empty, &model, true));
    }

    #[test]
    fn input_update_card_m3u_refresh_keeps_conflict_and_accepted_request_semantics() {
        let mut model = card(7, &[3, 9]);
        model.capabilities = InputUpdateCapabilities::for_input(&shared::model::ConfigInputDto {
            input_type: InputType::M3u,
            ..shared::model::ConfigInputDto::default()
        });
        let state = reduce(
            InputUpdateCardInteractionState::from_model(&model),
            InputUpdateCardAction::SelectPolicy(InputRefreshPolicy::REFRESH),
        );
        assert!(can_start_update(&state, &model, true));
        assert!(!requires_confirmation(state.controls.policy));
        let submitting = reduce(state.clone(), InputUpdateCardAction::BeginSubmission(connection(1)));
        assert!(!can_start_update(&submitting, &model, true));
        let conflict = reduce(
            submitting.clone(),
            InputUpdateCardAction::FinishSubmission {
                connection_context: connection(1),
                outcome: classify_update_outcome(Err(Error::Conflict("busy".to_owned()))),
            },
        );
        assert_eq!(conflict, state);
        let accepted = reduce(
            submitting,
            InputUpdateCardAction::FinishSubmission {
                connection_context: connection(1),
                outcome: classify_update_outcome(Ok(OperationRunAccepted::playlist_update("m3u-run".into()))),
            },
        );
        assert_eq!(accepted.controls.policy, InputRefreshPolicy::NORMAL);
        assert_eq!(accepted.controls.selected_target_ids, vec![3, 9]);
        assert!(!accepted.is_submitting());
    }

    #[test]
    fn playlist_update_view_policy_notes_reuse_existing_translations_in_every_locale() -> Result<(), serde_json::Error>
    {
        for source in [
            include_str!("../../../../public/assets/i18n/en.json"),
            include_str!("../../../../public/assets/i18n/ru.json"),
            include_str!("../../../../public/assets/i18n/ar.json"),
        ] {
            let locale: serde_json::Value = serde_json::from_str(source)?;
            assert!(locale
                .pointer("/LABEL/NOTES")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|label| !label.is_empty()));
            for policy in [InputRefreshPolicy::NORMAL, InputRefreshPolicy::REFRESH, InputRefreshPolicy::FORCE] {
                let pointer = format!("/{}", card(7, &[1]).capabilities.description_key(policy).replace('.', "/"));
                assert!(locale
                    .pointer(&pointer)
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|description| !description.is_empty()));
            }
        }
        Ok(())
    }

    #[test]
    fn playlist_update_view_button_text_follows_selected_policy() {
        assert_eq!(start_button_label_key(InputRefreshPolicy::NORMAL), "LABEL.UPDATE_START");
        assert_eq!(start_button_label_key(InputRefreshPolicy::REFRESH), "LABEL.REFRESH_START");
        assert_eq!(start_button_label_key(InputRefreshPolicy::FORCE), "LABEL.FORCE_UPDATE_START");
    }

    #[test]
    fn playlist_update_view_policy_descriptions_follow_cache_and_quality_semantics() -> Result<(), serde_json::Error> {
        let cases = [
            (InputRefreshPolicy::NORMAL, "MESSAGES.PLAYLIST_UPDATE.POLICY_UPDATE_DESCRIPTION", false, false),
            (InputRefreshPolicy::REFRESH, "MESSAGES.PLAYLIST_UPDATE.POLICY_REFRESH_DESCRIPTION", true, false),
            (InputRefreshPolicy::FORCE, "MESSAGES.PLAYLIST_UPDATE.POLICY_FORCE_DESCRIPTION", true, true),
        ];

        for (policy, description_key, bypasses_cache, bypasses_quality) in cases {
            assert_eq!(card(7, &[1]).capabilities.description_key(policy), description_key);
            assert_eq!(policy.bypasses_cache(), bypasses_cache);
            assert_eq!(policy.bypasses_quality(), bypasses_quality);
        }

        let english: serde_json::Value = serde_json::from_str(include_str!("../../../../public/assets/i18n/en.json"))?;
        assert_eq!(
            english.pointer("/MESSAGES/PLAYLIST_UPDATE/POLICY_UPDATE_DESCRIPTION").and_then(|value| value.as_str()),
            Some("Rebuild the selected targets with all required input data. For this input, reuse a valid cache and enforce the configured update quality.")
        );
        assert_eq!(
            english.pointer("/MESSAGES/PLAYLIST_UPDATE/POLICY_REFRESH_DESCRIPTION").and_then(|value| value.as_str()),
            Some("Rebuild the selected targets with all required input data. Reload this input without its cache and enforce the configured update quality.")
        );
        assert_eq!(
            english.pointer("/MESSAGES/PLAYLIST_UPDATE/POLICY_FORCE_DESCRIPTION").and_then(|value| value.as_str()),
            Some("Rebuild the selected targets with all required input data. For this input only, bypass cache and the configured update quality for this run.")
        );
        Ok(())
    }

    #[test]
    fn playlist_update_view_accessible_control_labels_include_action_and_input_name() {
        assert_eq!(
            action_message("{action} for {input}".to_string(), "cdn-dev", "Start force update"),
            "Start force update for cdn-dev"
        );
        assert_eq!(action_message("{action} for {input}".to_string(), "cdn-dev", "Action"), "Action for cdn-dev");
    }

    #[test]
    fn playlist_update_view_only_force_requires_confirmation_and_cancel_sends_nothing() {
        assert!(!requires_confirmation(InputRefreshPolicy::NORMAL));
        assert!(!requires_confirmation(InputRefreshPolicy::REFRESH));
        assert!(requires_confirmation(InputRefreshPolicy::FORCE));
        assert!(!confirmation_allows_submission(InputRefreshPolicy::FORCE, Some(DialogResult::Cancel)));
        assert!(confirmation_allows_submission(InputRefreshPolicy::FORCE, Some(DialogResult::Ok)));
        assert!(confirmation_allows_submission(InputRefreshPolicy::NORMAL, None));
    }

    #[test]
    fn playlist_update_view_accepted_force_request_resets_policy_and_keeps_targets() {
        let mut state = InputUpdateCardInteractionState::from_model(&card(7, &[1, 2]));
        state.controls.policy = InputRefreshPolicy::FORCE;
        state.submitting_for = Some(connection(1));

        let updated = reduce(
            state,
            InputUpdateCardAction::FinishSubmission {
                connection_context: connection(1),
                outcome: classify_update_outcome(Ok(OperationRunAccepted::playlist_update("accepted-run".into()))),
            },
        );

        assert_eq!(updated.controls.policy, InputRefreshPolicy::NORMAL);
        assert_eq!(updated.controls.selected_target_ids, vec![1, 2]);
        assert!(!updated.is_submitting());
    }

    #[test]
    fn playlist_update_view_conflict_preserves_policy_targets_and_not_started_state() {
        let mut state = InputUpdateCardInteractionState::from_model(&card(7, &[1, 2]));
        state.controls.policy = InputRefreshPolicy::FORCE;
        state.submitting_for = Some(connection(1));

        let updated = reduce(
            state,
            InputUpdateCardAction::FinishSubmission {
                connection_context: connection(1),
                outcome: classify_update_outcome(Err(Error::Conflict("busy".to_string()))),
            },
        );

        assert_eq!(updated.controls.policy, InputRefreshPolicy::FORCE);
        assert_eq!(updated.controls.selected_target_ids, vec![1, 2]);
        assert!(!updated.is_submitting());
    }

    #[test]
    fn playlist_update_action_technical_failure_preserves_policy_and_targets() {
        let mut state = InputUpdateCardInteractionState::from_model(&card(7, &[1, 2]));
        state.controls.policy = InputRefreshPolicy::REFRESH;
        state.submitting_for = Some(connection(1));

        let updated = reduce(
            state,
            InputUpdateCardAction::FinishSubmission {
                connection_context: connection(1),
                outcome: classify_update_outcome(Err(Error::RequestError)),
            },
        );

        assert_eq!(updated.controls.policy, InputRefreshPolicy::REFRESH);
        assert_eq!(updated.controls.selected_target_ids, vec![1, 2]);
        assert!(!updated.is_submitting());
    }

    #[test]
    fn playlist_update_status_connection_change_cancels_submission_and_preserves_card_controls() {
        let mut state = InputUpdateCardInteractionState::from_model(&card(7, &[1, 2]));
        state.controls.policy = InputRefreshPolicy::FORCE;
        state.controls.selected_target_ids = vec![2];
        state.submitting_for = Some(connection(1));

        let updated = reduce(state, InputUpdateCardAction::ConnectionChanged);

        assert!(!updated.is_submitting());
        assert_eq!(updated.controls.policy, InputRefreshPolicy::FORCE);
        assert_eq!(updated.controls.selected_target_ids, vec![2]);
    }

    #[test]
    fn input_update_card_stale_http_callback_cannot_finish_new_connection_submission() {
        let connection_a = connection(1);
        let connection_b = connection(2);
        let mut state = InputUpdateCardInteractionState::from_model(&card(7, &[1, 2]));
        state.controls.policy = InputRefreshPolicy::FORCE;
        state.controls.selected_target_ids = vec![2];
        let state = reduce(state, InputUpdateCardAction::BeginSubmission(connection_a));
        let state = reduce(state, InputUpdateCardAction::ConnectionChanged);
        let state = reduce(state, InputUpdateCardAction::BeginSubmission(connection_b));

        assert_eq!(
            current_submission_outcome(
                connection_a,
                connection_b,
                PlaylistUpdateActionOutcome::Accepted(Some("stale-a".into())),
            ),
            None
        );
        let state = reduce(
            state,
            InputUpdateCardAction::FinishSubmission {
                connection_context: connection_a,
                outcome: PlaylistUpdateActionOutcome::Accepted(Some("stale-a".into())),
            },
        );
        let state = reduce(state, InputUpdateCardAction::CancelSubmission(connection_a));

        assert!(state.is_submitting_for(connection_b));
        assert_eq!(state.controls.policy, InputRefreshPolicy::FORCE);
        assert_eq!(state.controls.selected_target_ids, vec![2]);

        let state = reduce(
            state,
            InputUpdateCardAction::FinishSubmission {
                connection_context: connection_b,
                outcome: PlaylistUpdateActionOutcome::Accepted(Some("current-b".into())),
            },
        );
        assert!(!state.is_submitting());
        assert_eq!(state.controls.policy, InputRefreshPolicy::NORMAL);
        assert_eq!(state.controls.selected_target_ids, vec![2]);
    }

    #[test]
    fn input_update_card_stale_force_dialog_cancel_or_ok_has_no_follow_up_action() {
        let connection_a = connection(1);
        let connection_b = connection(2);
        let mut state = InputUpdateCardInteractionState::from_model(&card(7, &[1, 2]));
        state.controls.policy = InputRefreshPolicy::FORCE;
        state.controls.selected_target_ids = vec![2];
        let state = reduce(state, InputUpdateCardAction::BeginSubmission(connection_a));
        let state = reduce(state, InputUpdateCardAction::ConnectionChanged);
        let state = reduce(state, InputUpdateCardAction::BeginSubmission(connection_b));

        for result in [DialogResult::Cancel, DialogResult::Ok] {
            assert_eq!(
                confirmation_submission_decision(connection_a, connection_b, InputRefreshPolicy::FORCE, Some(result),),
                ConfirmationSubmissionDecision::IgnoreStale
            );
            assert!(state.is_submitting_for(connection_b));
            assert_eq!(state.controls.policy, InputRefreshPolicy::FORCE);
            assert_eq!(state.controls.selected_target_ids, vec![2]);
        }
        assert_eq!(
            confirmation_submission_decision(
                connection_b,
                connection_b,
                InputRefreshPolicy::FORCE,
                Some(DialogResult::Cancel),
            ),
            ConfirmationSubmissionDecision::Cancel
        );
        assert_eq!(
            confirmation_submission_decision(
                connection_b,
                connection_b,
                InputRefreshPolicy::FORCE,
                Some(DialogResult::Ok),
            ),
            ConfirmationSubmissionDecision::Continue
        );
    }

    #[test]
    fn playlist_update_action_maps_selected_target_ids_to_configured_request_order() {
        let model = card(7, &[30, 10, 20]);

        assert_eq!(selected_current_target_ids(&model, &[20, 30]), vec![30, 20]);
    }

    #[test]
    fn playlist_update_action_distinguishes_same_name_targets_by_stable_id() {
        let model = card_with_targets(7, &[(1, "shared-name".to_string()), (4, "shared-name".to_string())]);

        assert_eq!(selected_current_target_ids(&model, &[1]), vec![1]);
        assert_eq!(selected_current_target_ids(&model, &[4]), vec![4]);
        assert_eq!(selected_current_target_ids(&model, &[4, 1]), vec![1, 4]);
    }

    #[test]
    fn input_update_card_removed_target_ids_cannot_enable_or_enter_a_request() {
        let previous = card(7, &[1]);
        let state = InputUpdateCardInteractionState::from_model(&previous);
        let replacement = card(7, &[4]);
        let targetless = card(7, &[]);

        assert!(selected_current_target_ids(&replacement, &state.controls.selected_target_ids).is_empty());
        assert!(!can_start_update(&state, &replacement, true));
        assert!(selected_current_target_ids(&targetless, &state.controls.selected_target_ids).is_empty());
        assert!(!can_start_update(&state, &targetless, true));
    }

    #[test]
    fn playlist_update_view_action_and_accessibility_translations_exist_in_every_frontend_locale(
    ) -> Result<(), serde_json::Error> {
        let locales = [
            ("en", include_str!("../../../../public/assets/i18n/en.json")),
            ("ru", include_str!("../../../../public/assets/i18n/ru.json")),
            ("ar", include_str!("../../../../public/assets/i18n/ar.json")),
        ];
        let pointers = [
            "/LABEL/ACTION",
            "/LABEL/AFFECTED_TARGETS",
            "/LABEL/FILTER",
            "/LABEL/MAPPING",
            "/LABEL/PROCESSING_ORDER",
            "/LABEL/RENAME",
            "/LABEL/TARGETS",
            "/LABEL/UPDATE_START",
            "/LABEL/REFRESH_START",
            "/LABEL/FORCE_UPDATE_START",
            "/MESSAGES/PLAYLIST_UPDATE/NO_TARGETS_ASSIGNED",
            "/MESSAGES/PLAYLIST_UPDATE/INPUT_ACCEPTED",
            "/MESSAGES/PLAYLIST_UPDATE/INPUT_BUSY",
            "/MESSAGES/PLAYLIST_UPDATE/INPUT_FAILED",
            "/MESSAGES/PLAYLIST_UPDATE/POLICY_LABEL",
            "/MESSAGES/PLAYLIST_UPDATE/START_LABEL",
            "/MESSAGES/PLAYLIST_UPDATE/POLICY_UPDATE_DESCRIPTION",
            "/MESSAGES/PLAYLIST_UPDATE/POLICY_REFRESH_DESCRIPTION",
            "/MESSAGES/PLAYLIST_UPDATE/POLICY_FORCE_DESCRIPTION",
            "/MESSAGES/PLAYLIST_UPDATE/ACTIVE_POPULATION",
            "/MESSAGES/PLAYLIST_UPDATE/BASELINE",
            "/MESSAGES/PLAYLIST_UPDATE/CACHE_BYPASSED_FORCE",
            "/MESSAGES/PLAYLIST_UPDATE/CACHE_BYPASSED_REFRESH",
            "/MESSAGES/PLAYLIST_UPDATE/CACHE_NOT_USED",
            "/MESSAGES/PLAYLIST_UPDATE/CACHE_USED",
            "/MESSAGES/PLAYLIST_UPDATE/CANDIDATE",
            "/MESSAGES/PLAYLIST_UPDATE/CLUSTER_ACCEPTED",
            "/MESSAGES/PLAYLIST_UPDATE/CLUSTER_CACHED",
            "/MESSAGES/PLAYLIST_UPDATE/CLUSTER_NOT_REQUESTED",
            "/MESSAGES/PLAYLIST_UPDATE/CLUSTER_REJECTED",
            "/MESSAGES/PLAYLIST_UPDATE/CLUSTER_TECHNICAL_ERROR",
            "/MESSAGES/PLAYLIST_UPDATE/CONFIGURED_COUNT",
            "/MESSAGES/PLAYLIST_UPDATE/DATA_UNAVAILABLE",
            "/MESSAGES/PLAYLIST_UPDATE/DECISION",
            "/MESSAGES/PLAYLIST_UPDATE/IDENTICAL_POPULATION",
            "/MESSAGES/PLAYLIST_UPDATE/MODE",
            "/MESSAGES/PLAYLIST_UPDATE/QUALITY_ACHIEVED",
            "/MESSAGES/PLAYLIST_UPDATE/QUALITY_BYPASSED",
            "/MESSAGES/PLAYLIST_UPDATE/QUALITY_GUARD",
            "/MESSAGES/PLAYLIST_UPDATE/QUALITY_THRESHOLD",
            "/MESSAGES/PLAYLIST_UPDATE/REQUIRED",
            "/MESSAGES/PLAYLIST_UPDATE/RUN_DETAILS",
            "/MESSAGES/PLAYLIST_UPDATE/UPDATE_DETAILS",
        ];

        for (locale, source) in locales {
            let translations: serde_json::Value = serde_json::from_str(source)?;
            for pointer in pointers {
                assert!(
                    translations
                        .pointer(pointer)
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|value| !value.trim().is_empty()),
                    "missing {pointer} translation for {locale}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn playlist_update_view_last_update_translations_exist_in_every_frontend_locale() -> Result<(), serde_json::Error> {
        let locales = [
            ("en", include_str!("../../../../public/assets/i18n/en.json")),
            ("ru", include_str!("../../../../public/assets/i18n/ru.json")),
            ("ar", include_str!("../../../../public/assets/i18n/ar.json")),
        ];
        let pointers = [
            "/LABEL/LAST_UPDATE",
            "/LABEL/LAST_UPDATE_NEVER",
            "/LABEL/LAST_UPDATE_JUST_NOW",
            "/LABEL/LAST_UPDATE_MINUTE_AGO",
            "/LABEL/LAST_UPDATE_MINUTES_AGO",
            "/LABEL/LAST_UPDATE_HOUR_AGO",
            "/LABEL/LAST_UPDATE_HOURS_AGO",
        ];

        for (locale, source) in locales {
            let translations: serde_json::Value = serde_json::from_str(source)?;
            for pointer in pointers {
                assert!(
                    translations
                        .pointer(pointer)
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|value| !value.trim().is_empty()),
                    "missing {pointer} translation for {locale}"
                );
            }
        }
        Ok(())
    }
}
