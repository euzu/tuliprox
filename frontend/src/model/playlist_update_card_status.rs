use crate::{model::InputUpdateCardModel, services::WebSocketConnectionContext};
use indexmap::{IndexMap, IndexSet};
use shared::model::{
    InputRefreshPolicy, LibraryScanResult, PlaylistUpdateInputTelemetry, PlaylistUpdateProgressEvent,
    PlaylistUpdateRunId, PlaylistUpdateRunOrder, PlaylistUpdateRunStateEvent, PlaylistUpdateState, SourcesConfigDto,
};
use std::{collections::HashSet, rc::Rc};
use yew::Reducible;

const MAX_CARD_DETAIL_LINES: usize = 100;
const MAX_TRACKED_RUNS: usize = 32;

/// Effective status shown in an input update card.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InputUpdateCardStatus {
    #[default]
    Ready,
    Queued,
    Updating,
    Success,
    Partial,
    Failed,
}

impl InputUpdateCardStatus {
    #[must_use]
    pub const fn label_key(self) -> &'static str {
        match self {
            Self::Ready => "LABEL.UPDATE_STATUS_READY",
            Self::Queued => "LABEL.UPDATE_STATUS_QUEUED",
            Self::Updating => "LABEL.UPDATE_STATUS_UPDATING",
            Self::Success => "LABEL.UPDATE_STATUS_SUCCESS",
            Self::Partial => "LABEL.UPDATE_STATUS_PARTIAL",
            Self::Failed => "LABEL.UPDATE_STATUS_FAILED",
        }
    }

    #[must_use]
    pub const fn modifier(self) -> &'static str {
        match self {
            Self::Ready => "tp__playlist-update-view__pipeline-status--neutral",
            Self::Queued => "tp__playlist-update-view__pipeline-status--queued",
            Self::Updating => "tp__playlist-update-view__pipeline-status--updating",
            Self::Success => "tp__playlist-update-view__pipeline-status--accepted",
            Self::Partial => "tp__playlist-update-view__pipeline-status--rejected",
            Self::Failed => "tp__playlist-update-view__pipeline-status--error",
        }
    }
}

/// Request/progress state and details for one stable input ID.
#[derive(Clone, Debug, PartialEq)]
pub struct InputUpdateCardRuntime {
    library_scan_result: Option<LibraryScanResult>,
    status: InputUpdateCardStatus,
    action_status: InputUpdateCardStatus,
    details: Vec<String>,
    run_id: Option<PlaylistUpdateRunId>,
    execution_order: Option<PlaylistUpdateRunOrder>,
    refresh_policy: Option<InputRefreshPolicy>,
    input_telemetry: Option<PlaylistUpdateInputTelemetry>,
}

impl Default for InputUpdateCardRuntime {
    fn default() -> Self {
        Self {
            library_scan_result: None,
            status: InputUpdateCardStatus::Ready,
            action_status: InputUpdateCardStatus::Ready,
            details: Vec::new(),
            run_id: None,
            execution_order: None,
            refresh_policy: None,
            input_telemetry: None,
        }
    }
}

impl InputUpdateCardRuntime {
    #[must_use]
    pub const fn status(&self) -> InputUpdateCardStatus { self.status }

    /// Completion of the correlated input-to-target run, independent of input success.
    #[must_use]
    pub const fn action_status(&self) -> InputUpdateCardStatus { self.action_status }

    #[must_use]
    pub fn details(&self) -> &[String] { &self.details }

    /// Stable identity of the run that currently determines this card's status.
    #[must_use]
    pub fn run_id(&self) -> Option<&PlaylistUpdateRunId> { self.run_id.as_ref() }

    /// Execution order of the run that currently determines this card's status.
    #[must_use]
    pub const fn execution_order(&self) -> Option<PlaylistUpdateRunOrder> { self.execution_order }

    /// Effective policy proven by request acceptance or typed backend telemetry.
    #[must_use]
    pub const fn refresh_policy(&self) -> Option<InputRefreshPolicy> { self.refresh_policy }

    /// Typed input facts emitted by the backend for this effective run.
    #[must_use]
    pub const fn input_telemetry(&self) -> Option<&PlaylistUpdateInputTelemetry> { self.input_telemetry.as_ref() }

    /// Scanner result belonging exclusively to this effective run and input.
    #[must_use]
    pub const fn library_scan_result(&self) -> Option<&LibraryScanResult> { self.library_scan_result.as_ref() }
}

struct EffectiveInputRun<'a> {
    status: InputUpdateCardStatus,
    run_id: &'a PlaylistUpdateRunId,
    execution_order: Option<PlaylistUpdateRunOrder>,
    refresh_policy: Option<InputRefreshPolicy>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InputTerminalState {
    run_id: PlaylistUpdateRunId,
    execution_order: PlaylistUpdateRunOrder,
    state: PlaylistUpdateState,
    action_state: Option<PlaylistUpdateState>,
    refresh_policy: Option<InputRefreshPolicy>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InputDetails {
    library_scan_result: Option<LibraryScanResult>,
    run_id: PlaylistUpdateRunId,
    execution_order: PlaylistUpdateRunOrder,
    lines: Vec<String>,
    input_telemetry: Option<PlaylistUpdateInputTelemetry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlaylistUpdateRunRuntime {
    execution_order: Option<PlaylistUpdateRunOrder>,
    accepted: bool,
    accepted_input_ids: IndexSet<u16>,
    observed_input_ids: IndexSet<u16>,
    input_states: IndexMap<u16, PlaylistUpdateState>,
    terminal: Option<PlaylistUpdateState>,
    refresh_policy_by_input_id: IndexMap<u16, InputRefreshPolicy>,
}

impl PlaylistUpdateRunRuntime {
    fn new() -> Self {
        Self {
            execution_order: None,
            accepted: false,
            accepted_input_ids: IndexSet::new(),
            observed_input_ids: IndexSet::new(),
            input_states: IndexMap::new(),
            terminal: None,
            refresh_policy_by_input_id: IndexMap::new(),
        }
    }

    fn scope(&self) -> IndexSet<u16> {
        self.accepted_input_ids.iter().chain(&self.observed_input_ids).copied().collect()
    }

    fn refresh_policy(&self, input_id: u16) -> Option<InputRefreshPolicy> {
        self.refresh_policy_by_input_id.get(&input_id).copied()
    }

    fn assign_execution_order(&mut self, execution_order: PlaylistUpdateRunOrder) -> bool {
        if let Some(current) = self.execution_order {
            current == execution_order
        } else {
            self.execution_order = Some(execution_order);
            true
        }
    }
}

/// Stable scope returned by one accepted card or bulk request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistUpdateAcceptedScope {
    pub connection_context: WebSocketConnectionContext,
    pub run_id: PlaylistUpdateRunId,
    pub input_ids: Vec<u16>,
    pub refresh_policy: InputRefreshPolicy,
}

/// Card runtime state correlated by update-run and stable input identity.
#[derive(Clone, Debug, PartialEq)]
pub struct PlaylistUpdateCardStatuses {
    connection_context: WebSocketConnectionContext,
    runs: IndexMap<PlaylistUpdateRunId, PlaylistUpdateRunRuntime>,
    terminal_by_input_id: IndexMap<u16, InputTerminalState>,
    details_by_input_id: IndexMap<u16, InputDetails>,
}

impl Default for PlaylistUpdateCardStatuses {
    fn default() -> Self { Self::for_connection(WebSocketConnectionContext::default()) }
}

impl PlaylistUpdateCardStatuses {
    #[must_use]
    pub fn for_connection(connection_context: WebSocketConnectionContext) -> Self {
        Self {
            connection_context,
            runs: IndexMap::new(),
            terminal_by_input_id: IndexMap::new(),
            details_by_input_id: IndexMap::new(),
        }
    }

    #[must_use]
    pub fn for_input(&self, input_id: u16) -> InputUpdateCardRuntime {
        let effective_run = self.effective_run_for_input(input_id);
        let status = effective_run.as_ref().map_or(InputUpdateCardStatus::Ready, |run| run.status);
        let action_status = effective_run.as_ref().map_or(InputUpdateCardStatus::Ready, |effective| {
            if let Some(run) = self.runs.get(effective.run_id) {
                return run.terminal.map_or_else(
                    || {
                        if effective.status == InputUpdateCardStatus::Queued {
                            InputUpdateCardStatus::Queued
                        } else {
                            InputUpdateCardStatus::Updating
                        }
                    },
                    status_from_state,
                );
            }
            self.terminal_by_input_id
                .get(&input_id)
                .and_then(|terminal| terminal.action_state)
                .map_or(effective.status, status_from_state)
        });
        let run_id = effective_run.as_ref().map(|run| run.run_id.clone());
        let refresh_policy = effective_run.as_ref().and_then(|run| run.refresh_policy);
        let execution_order = effective_run.as_ref().and_then(|run| run.execution_order);
        let details = effective_run
            .and_then(|run| self.details_by_input_id.get(&input_id).filter(|details| details.run_id == *run.run_id));
        InputUpdateCardRuntime {
            library_scan_result: details.and_then(|details| details.library_scan_result.clone()),
            status,
            action_status,
            details: details.map_or_else(Vec::new, |details| details.lines.clone()),
            run_id,
            execution_order,
            refresh_policy,
            input_telemetry: details.and_then(|details| details.input_telemetry.clone()),
        }
    }

    fn effective_run_for_input(&self, input_id: u16) -> Option<EffectiveInputRun<'_>> {
        let terminal = self.terminal_by_input_id.get(&input_id);
        let terminal_order = terminal.map(|terminal| terminal.execution_order);
        let running = self
            .runs
            .iter()
            .filter(|(run_id, run)| {
                run.terminal.is_none()
                    && run.observed_input_ids.contains(&input_id)
                    // Input completion does not finish its run's target rebuild. Only the
                    // same run may retain priority at the recorded input-terminal order.
                    && run.execution_order.is_some_and(|order| {
                        terminal.is_none_or(|terminal| {
                            order > terminal.execution_order
                                || (order == terminal.execution_order && *run_id == &terminal.run_id)
                        })
                    })
            })
            .max_by_key(|(_, run)| run.execution_order);
        if let Some((run_id, run)) = running {
            return Some(EffectiveInputRun {
                status: run
                    .input_states
                    .get(&input_id)
                    .copied()
                    .map_or(InputUpdateCardStatus::Updating, status_from_state),
                run_id,
                execution_order: run.execution_order,
                refresh_policy: run.refresh_policy(input_id),
            });
        }

        let queued = self.runs.iter().rev().find(|(_, run)| {
            run.terminal.is_none()
                && run.accepted
                && run.accepted_input_ids.contains(&input_id)
                && !run.observed_input_ids.contains(&input_id)
                && run.execution_order.is_none_or(|order| terminal_order.is_none_or(|terminal| order > terminal))
        });
        if let Some((run_id, run)) = queued {
            return Some(EffectiveInputRun {
                status: InputUpdateCardStatus::Queued,
                run_id,
                execution_order: run.execution_order,
                refresh_policy: run.refresh_policy(input_id),
            });
        }

        self.terminal_by_input_id.get(&input_id).map(|terminal| EffectiveInputRun {
            status: status_from_state(terminal.state),
            run_id: &terminal.run_id,
            execution_order: Some(terminal.execution_order),
            refresh_policy: terminal.refresh_policy,
        })
    }

    fn ensure_run(&mut self, run_id: &PlaylistUpdateRunId) {
        if self.runs.contains_key(run_id) {
            return;
        }
        self.runs.insert(run_id.clone(), PlaylistUpdateRunRuntime::new());
        self.prune_runs();
    }

    fn connection_changed(&mut self, connection_context: WebSocketConnectionContext) {
        if self.connection_context == connection_context {
            return;
        }
        self.connection_context = connection_context;
        self.runs.clear();
        self.terminal_by_input_id.clear();
        self.details_by_input_id.clear();
    }

    fn is_current_live_connection(&self, connection_context: WebSocketConnectionContext) -> bool {
        self.connection_context.is_same_live_connection(connection_context)
    }

    fn prune_runs(&mut self) {
        while self.runs.len() > MAX_TRACKED_RUNS {
            let removable = self.runs.iter().position(|(_, run)| run.terminal.is_some());
            let Some(index) = removable else {
                break;
            };
            self.runs.shift_remove_index(index);
        }
    }

    fn record_terminal(
        &mut self,
        input_id: u16,
        run_id: &PlaylistUpdateRunId,
        execution_order: PlaylistUpdateRunOrder,
        state: PlaylistUpdateState,
        refresh_policy: Option<InputRefreshPolicy>,
    ) {
        let action_state = self.runs.get(run_id).and_then(|run| run.terminal);
        match self.terminal_by_input_id.get_mut(&input_id) {
            Some(current) if current.execution_order > execution_order => {}
            Some(current) if current.execution_order == execution_order && current.run_id == *run_id => {
                current.state = strongest_state(current.state, state);
                current.action_state = action_state.or(current.action_state);
                if current.refresh_policy.is_none() {
                    current.refresh_policy = refresh_policy;
                }
            }
            Some(current) if current.execution_order == execution_order => {}
            Some(current) => {
                *current =
                    InputTerminalState { run_id: run_id.clone(), execution_order, state, action_state, refresh_policy };
            }
            None => {
                self.terminal_by_input_id.insert(
                    input_id,
                    InputTerminalState { run_id: run_id.clone(), execution_order, state, action_state, refresh_policy },
                );
            }
        }
    }

    fn activate_details_run(
        &mut self,
        input_id: u16,
        run_id: &PlaylistUpdateRunId,
        execution_order: PlaylistUpdateRunOrder,
    ) -> bool {
        match self.details_by_input_id.get_mut(&input_id) {
            Some(current) if current.execution_order > execution_order => false,
            Some(current) if current.execution_order == execution_order => current.run_id == *run_id,
            Some(current) => {
                *current = InputDetails {
                    run_id: run_id.clone(),
                    execution_order,
                    lines: Vec::new(),
                    input_telemetry: None,
                    library_scan_result: None,
                };
                true
            }
            None => {
                self.details_by_input_id.insert(
                    input_id,
                    InputDetails {
                        run_id: run_id.clone(),
                        execution_order,
                        lines: Vec::new(),
                        input_telemetry: None,
                        library_scan_result: None,
                    },
                );
                true
            }
        }
    }

    fn accept(&mut self, accepted: PlaylistUpdateAcceptedScope) {
        let PlaylistUpdateAcceptedScope { connection_context, run_id, input_ids, refresh_policy } = accepted;
        if !self.is_current_live_connection(connection_context) {
            return;
        }
        self.ensure_run(&run_id);
        let (execution_order, terminal, explicit_states, input_policies) = {
            let Some(run) = self.runs.get_mut(&run_id) else {
                return;
            };
            run.accepted = true;
            for input_id in &input_ids {
                run.accepted_input_ids.insert(*input_id);
                run.refresh_policy_by_input_id.entry(*input_id).or_insert(refresh_policy);
            }
            let input_policies = input_ids
                .iter()
                .filter_map(|input_id| {
                    run.refresh_policy_by_input_id.get(input_id).copied().map(|policy| (*input_id, policy))
                })
                .collect::<Vec<_>>();
            (run.execution_order, run.terminal, run.input_states.clone(), input_policies)
        };
        for (input_id, refresh_policy) in input_policies {
            if let Some(current) = self
                .terminal_by_input_id
                .get_mut(&input_id)
                .filter(|current| current.run_id == run_id && current.refresh_policy.is_none())
            {
                current.refresh_policy = Some(refresh_policy);
            }
            if let (Some(execution_order), Some(terminal)) = (execution_order, terminal) {
                let state = explicit_states.get(&input_id).copied().unwrap_or(terminal);
                self.activate_details_run(input_id, &run_id, execution_order);
                self.record_terminal(input_id, &run_id, execution_order, state, Some(refresh_policy));
            }
        }
    }

    fn restore_active_updates(
        &mut self,
        connection_context: WebSocketConnectionContext,
        progress: Vec<PlaylistUpdateProgressEvent>,
    ) {
        for event in progress {
            let (Some(run_id), Some(order), Some(input_id)) = (&event.run_id, event.execution_order, event.input_id)
            else {
                continue;
            };
            // A response can arrive after live progress or even run completion.
            // Hydrate missing inputs only; never roll those newer facts back.
            if self
                .runs
                .get(run_id)
                .is_some_and(|run| run.terminal.is_some() || run.observed_input_ids.contains(&input_id))
                || self
                    .terminal_by_input_id
                    .get(&input_id)
                    .is_some_and(|terminal| terminal.action_state.is_some() && terminal.execution_order >= order)
            {
                continue;
            }
            self.progress(connection_context, event);
        }
    }

    fn progress(&mut self, connection_context: WebSocketConnectionContext, progress: PlaylistUpdateProgressEvent) {
        if !self.is_current_live_connection(connection_context) {
            return;
        }
        let PlaylistUpdateProgressEvent {
            run_id: Some(run_id),
            execution_order: Some(execution_order),
            input_id: Some(input_id),
            state,
            input_telemetry,
            library_scan_result,
            message,
            ..
        } = progress
        else {
            return;
        };
        self.ensure_run(&run_id);
        let (input_state, refresh_policy) = {
            let Some(run) = self.runs.get_mut(&run_id) else {
                return;
            };
            if run.terminal.is_some() || !run.assign_execution_order(execution_order) {
                return;
            }
            run.observed_input_ids.insert(input_id);
            if let Some(input_telemetry) = input_telemetry.as_ref() {
                run.refresh_policy_by_input_id.entry(input_id).or_insert(input_telemetry.refresh_policy);
            }
            let input_state = state.map(|state| {
                let entry = run.input_states.entry(input_id).or_insert(state);
                *entry = strongest_state(*entry, state);
                *entry
            });
            (input_state, run.refresh_policy(input_id))
        };

        if self.activate_details_run(input_id, &run_id, execution_order) {
            if let Some(details) = self.details_by_input_id.get_mut(&input_id) {
                details.lines.push(message);
                if details.lines.len() > MAX_CARD_DETAIL_LINES {
                    details.lines.remove(0);
                }
                if let Some(input_telemetry) = input_telemetry {
                    details.input_telemetry = Some(input_telemetry);
                }
                if let Some(result) = library_scan_result {
                    details.library_scan_result = Some(result);
                }
            }
        }
        if let Some(state) = input_state {
            self.record_terminal(input_id, &run_id, execution_order, state, refresh_policy);
        }
    }

    fn complete(&mut self, connection_context: WebSocketConnectionContext, completed: PlaylistUpdateRunStateEvent) {
        if !self.is_current_live_connection(connection_context) {
            return;
        }
        let Some(run_id) = completed.run_id else {
            return;
        };
        self.ensure_run(&run_id);
        let (execution_order, scope, input_states, input_policies, terminal, was_known) = {
            let Some(run) = self.runs.get_mut(&run_id) else {
                return;
            };
            if let Some(execution_order) = completed.execution_order {
                if !run.assign_execution_order(execution_order) {
                    return;
                }
            }
            let was_known = run.accepted || !run.observed_input_ids.is_empty();
            if run.terminal.is_none() {
                run.terminal = Some(completed.state);
            }
            (
                run.execution_order,
                run.scope(),
                run.input_states.clone(),
                run.refresh_policy_by_input_id.clone(),
                run.terminal.unwrap_or(completed.state),
                was_known,
            )
        };
        let Some(execution_order) = execution_order else {
            return;
        };
        if !was_known {
            return;
        }
        for input_id in scope {
            let state = input_states.get(&input_id).copied().unwrap_or(terminal);
            let refresh_policy = input_policies.get(&input_id).copied();
            self.activate_details_run(input_id, &run_id, execution_order);
            self.record_terminal(input_id, &run_id, execution_order, state, refresh_policy);
        }
    }

    fn apply(mut self, action: PlaylistUpdateCardStatusAction) -> Self {
        match action {
            PlaylistUpdateCardStatusAction::ConnectionChanged(connection_context) => {
                self.connection_changed(connection_context);
            }
            PlaylistUpdateCardStatusAction::Accepted(accepted) => self.accept(accepted),
            PlaylistUpdateCardStatusAction::Progress { connection_context, progress } => {
                self.progress(connection_context, progress);
            }
            PlaylistUpdateCardStatusAction::RestoreActive { connection_context, progress } => {
                self.restore_active_updates(connection_context, progress);
            }
            PlaylistUpdateCardStatusAction::Completed { connection_context, completed } => {
                self.complete(connection_context, completed);
            }
        }
        self
    }
}

fn strongest_state(current: PlaylistUpdateState, candidate: PlaylistUpdateState) -> PlaylistUpdateState {
    match (current, candidate) {
        (PlaylistUpdateState::Failure, _) | (_, PlaylistUpdateState::Failure) => PlaylistUpdateState::Failure,
        (PlaylistUpdateState::Partial, _) | (_, PlaylistUpdateState::Partial) => PlaylistUpdateState::Partial,
        _ => PlaylistUpdateState::Success,
    }
}

pub(super) const fn status_from_state(state: PlaylistUpdateState) -> InputUpdateCardStatus {
    match state {
        PlaylistUpdateState::Success => InputUpdateCardStatus::Success,
        PlaylistUpdateState::Partial => InputUpdateCardStatus::Partial,
        PlaylistUpdateState::Failure => InputUpdateCardStatus::Failed,
    }
}

pub enum PlaylistUpdateCardStatusAction {
    ConnectionChanged(WebSocketConnectionContext),
    Accepted(PlaylistUpdateAcceptedScope),
    Progress { connection_context: WebSocketConnectionContext, progress: PlaylistUpdateProgressEvent },
    RestoreActive { connection_context: WebSocketConnectionContext, progress: Vec<PlaylistUpdateProgressEvent> },
    Completed { connection_context: WebSocketConnectionContext, completed: PlaylistUpdateRunStateEvent },
}

impl Reducible for PlaylistUpdateCardStatuses {
    type Action = PlaylistUpdateCardStatusAction;

    fn reduce(self: Rc<Self>, action: Self::Action) -> Rc<Self> { Rc::new((*self).clone().apply(action)) }
}

/// Two deliberately separate scopes for the global action: server-side
/// executability and the visible cards that can show this run as queued.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlaylistUpdateBulkScope {
    pub can_submit: bool,
    pub visible_input_ids: Vec<u16>,
}

#[must_use]
pub fn playlist_update_bulk_scope(
    sources: &SourcesConfigDto,
    cards: &[InputUpdateCardModel],
) -> PlaylistUpdateBulkScope {
    let enabled_inputs =
        sources.inputs.iter().filter(|input| input.enabled).map(|input| input.name.as_ref()).collect::<HashSet<_>>();
    let enabled_target_ids = sources
        .sources
        .iter()
        .flat_map(|source| &source.targets)
        .filter(|target| target.enabled)
        .map(|target| target.id)
        .collect::<HashSet<_>>();
    let can_submit = sources.sources.iter().any(|source| {
        source.inputs.iter().any(|input| enabled_inputs.contains(input.as_ref()))
            && source.targets.iter().any(|target| target.enabled)
    });
    // Visible cards include disabled inputs; bulk participation follows configuration IDs, not manual capabilities.
    let enabled_input_ids =
        sources.inputs.iter().filter(|input| input.enabled).map(|input| input.id).collect::<HashSet<_>>();
    let visible_input_ids = cards
        .iter()
        .filter(|card| {
            enabled_input_ids.contains(&card.input_id)
                && card.targets.iter().any(|target| enabled_target_ids.contains(&target.id))
        })
        .map(|card| card.input_id)
        .collect();
    PlaylistUpdateBulkScope { can_submit, visible_input_ids }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        build_input_update_card_models, InputUpdateCapabilities, InputUpdateCapabilitiesExt, InputUpdateCardState,
        InputUpdateCardTarget,
    };
    use shared::{
        model::{
            ConfigInputDto, ConfigSourceDto, ConfigTargetDto, InputRefreshPolicy, InputType,
            PlaylistUpdateClusterTelemetry, PlaylistUpdateDataSource, PlaylistUpdateRunId, PlaylistUpdateRunOrder,
            XtreamCluster,
        },
        utils::Internable,
    };
    use std::sync::Arc;

    fn run_id(value: &str) -> PlaylistUpdateRunId { value.into() }

    fn execution_order(value: u64) -> PlaylistUpdateRunOrder { PlaylistUpdateRunOrder::from(value) }

    const fn connection(socket_epoch: u64) -> WebSocketConnectionContext {
        WebSocketConnectionContext::new(socket_epoch, true)
    }

    const fn disconnected_connection(socket_epoch: u64) -> WebSocketConnectionContext {
        WebSocketConnectionContext::new(socket_epoch, false)
    }

    fn statuses() -> PlaylistUpdateCardStatuses { PlaylistUpdateCardStatuses::for_connection(connection(1)) }

    const fn connection_changed(connection_context: WebSocketConnectionContext) -> PlaylistUpdateCardStatusAction {
        PlaylistUpdateCardStatusAction::ConnectionChanged(connection_context)
    }

    fn card(input_id: u16, target_ids: &[u16]) -> InputUpdateCardModel {
        InputUpdateCardModel {
            input_id,
            input_name: Arc::from(format!("input-{input_id}")),
            input_type: InputType::Xtream,
            capabilities: crate::model::InputUpdateCapabilities::for_input(&ConfigInputDto {
                input_type: InputType::Xtream,
                ..ConfigInputDto::default()
            }),
            targets: target_ids
                .iter()
                .map(|target_id| InputUpdateCardTarget { id: *target_id, name: format!("target-{target_id}") })
                .collect(),
            last_update_at: None,
        }
    }

    fn reduce(state: PlaylistUpdateCardStatuses, action: PlaylistUpdateCardStatusAction) -> PlaylistUpdateCardStatuses {
        state.apply(action)
    }

    fn accepted_in(
        connection_context: WebSocketConnectionContext,
        run: &str,
        input_ids: &[u16],
    ) -> PlaylistUpdateCardStatusAction {
        accepted_with_policy_in(connection_context, run, input_ids, InputRefreshPolicy::NORMAL)
    }

    fn accepted_with_policy_in(
        connection_context: WebSocketConnectionContext,
        run: &str,
        input_ids: &[u16],
        refresh_policy: InputRefreshPolicy,
    ) -> PlaylistUpdateCardStatusAction {
        PlaylistUpdateCardStatusAction::Accepted(PlaylistUpdateAcceptedScope {
            connection_context,
            run_id: run_id(run),
            input_ids: input_ids.to_vec(),
            refresh_policy,
        })
    }

    fn accepted(run: &str, input_ids: &[u16]) -> PlaylistUpdateCardStatusAction {
        accepted_in(connection(1), run, input_ids)
    }

    fn accepted_with_policy(
        run: &str,
        input_ids: &[u16],
        refresh_policy: InputRefreshPolicy,
    ) -> PlaylistUpdateCardStatusAction {
        accepted_with_policy_in(connection(1), run, input_ids, refresh_policy)
    }

    fn progress_in(
        connection_context: WebSocketConnectionContext,
        run: &str,
        order: u64,
        input_id: u16,
        message: &str,
    ) -> PlaylistUpdateCardStatusAction {
        PlaylistUpdateCardStatusAction::Progress {
            connection_context,
            progress: PlaylistUpdateProgressEvent::for_run_input(
                run_id(run),
                execution_order(order),
                input_id,
                format!("input-{input_id}"),
                message,
            ),
        }
    }

    fn progress(run: &str, order: u64, input_id: u16, message: &str) -> PlaylistUpdateCardStatusAction {
        progress_in(connection(1), run, order, input_id, message)
    }

    fn input_completed_in(
        connection_context: WebSocketConnectionContext,
        run: &str,
        order: u64,
        input_id: u16,
        state: PlaylistUpdateState,
        message: &str,
    ) -> PlaylistUpdateCardStatusAction {
        PlaylistUpdateCardStatusAction::Progress {
            connection_context,
            progress: PlaylistUpdateProgressEvent::input_completed(
                run_id(run),
                execution_order(order),
                input_id,
                state,
                format!("input-{input_id}"),
                message,
            ),
        }
    }

    fn input_completed(
        run: &str,
        order: u64,
        input_id: u16,
        state: PlaylistUpdateState,
        message: &str,
    ) -> PlaylistUpdateCardStatusAction {
        input_completed_in(connection(1), run, order, input_id, state, message)
    }

    fn completed_in(
        connection_context: WebSocketConnectionContext,
        run: &str,
        order: u64,
        state: PlaylistUpdateState,
    ) -> PlaylistUpdateCardStatusAction {
        PlaylistUpdateCardStatusAction::Completed {
            connection_context,
            completed: PlaylistUpdateRunStateEvent::correlated(run_id(run), execution_order(order), state),
        }
    }

    fn completed(run: &str, order: u64, state: PlaylistUpdateState) -> PlaylistUpdateCardStatusAction {
        completed_in(connection(1), run, order, state)
    }

    #[test]
    fn pipeline_transparency_input_action_waits_for_target_completion_and_keeps_input_success() {
        for terminal in [PlaylistUpdateState::Success, PlaylistUpdateState::Failure, PlaylistUpdateState::Partial] {
            let state = PlaylistUpdateCardStatuses::for_connection(connection(1))
                .apply(accepted("rescan", &[2]))
                .apply(progress("rescan", 1, 2, "Rescan started"))
                .apply(input_completed("rescan", 1, 2, PlaylistUpdateState::Success, "Input acquired"));
            assert_eq!(state.for_input(2).status(), InputUpdateCardStatus::Success);
            assert_eq!(state.for_input(2).action_status(), InputUpdateCardStatus::Updating);
            let state = state.apply(completed("rescan", 1, terminal));
            assert_eq!(state.for_input(2).status(), InputUpdateCardStatus::Success);
            assert_eq!(state.for_input(2).action_status(), super::status_from_state(terminal));
            let state = state.apply(accepted("next-run", &[2]));
            assert_eq!(state.for_input(2).action_status(), InputUpdateCardStatus::Queued);
            assert!(state.for_input(2).input_telemetry().is_none());
        }
    }

    #[test]
    fn pipeline_transparency_playlist_update_status_running_rebuild_precedes_queued_follow_up() {
        let r1_telemetry = PlaylistUpdateInputTelemetry {
            refresh_policy: InputRefreshPolicy::FORCE,
            source: Some(PlaylistUpdateDataSource::Provider),
            clusters: Vec::new(),
        };
        let r2_telemetry = PlaylistUpdateInputTelemetry {
            refresh_policy: InputRefreshPolicy::NORMAL,
            source: Some(PlaylistUpdateDataSource::Cache),
            clusters: Vec::new(),
        };
        for terminal in [PlaylistUpdateState::Success, PlaylistUpdateState::Partial, PlaylistUpdateState::Failure] {
            let state = statuses()
                .apply(accepted_with_policy("r1", &[7], InputRefreshPolicy::FORCE))
                .apply(PlaylistUpdateCardStatusAction::Progress {
                    connection_context: connection(1),
                    progress: PlaylistUpdateProgressEvent::for_run_input(
                        run_id("r1"),
                        execution_order(1),
                        7,
                        "input-7",
                        "r1 provider acquisition",
                    )
                    .with_input_telemetry(r1_telemetry.clone()),
                })
                .apply(input_completed("r1", 1, 7, PlaylistUpdateState::Success, "r1 input completed"));
            let rebuilding = InputUpdateCardRuntime {
                library_scan_result: None,
                status: InputUpdateCardStatus::Success,
                action_status: InputUpdateCardStatus::Updating,
                details: vec!["r1 provider acquisition".into(), "r1 input completed".into()],
                run_id: Some(run_id("r1")),
                execution_order: Some(execution_order(1)),
                refresh_policy: Some(InputRefreshPolicy::FORCE),
                input_telemetry: Some(r1_telemetry.clone()),
            };
            assert_eq!(state.for_input(7), rebuilding);
            let state = state.apply(accepted("r2", &[7]));
            let pending = state.runs[&run_id("r2")].clone();
            assert_eq!(state.for_input(7), rebuilding, "R1 targets still running: {terminal:?}");
            let state = state.apply(completed("r1", 1, terminal));
            let queued = InputUpdateCardRuntime {
                status: InputUpdateCardStatus::Queued,
                action_status: InputUpdateCardStatus::Queued,
                run_id: Some(run_id("r2")),
                refresh_policy: Some(InputRefreshPolicy::NORMAL),
                ..InputUpdateCardRuntime::default()
            };
            assert_eq!(state.for_input(7), queued);
            assert_eq!(state.terminal_by_input_id[&7].action_state, Some(terminal));
            assert_eq!(state.runs[&run_id("r2")], pending, "R2 must not be consumed by R1 completion");
            let state = state.apply(accepted_with_policy("r1", &[7], InputRefreshPolicy::FORCE));
            assert_eq!(state.for_input(7), queued, "Late R1 acceptance must not displace queued R2");
            assert_eq!(state.runs[&run_id("r2")], pending);
            assert_eq!(state.runs.len(), 2);

            // R2 starts through normal progress, without another Accepted or a resubmission.
            let state = state.apply(PlaylistUpdateCardStatusAction::Progress {
                connection_context: connection(1),
                progress: PlaylistUpdateProgressEvent::for_run_input(
                    run_id("r2"),
                    execution_order(2),
                    7,
                    "input-7",
                    "r2 cache acquisition",
                )
                .with_input_telemetry(r2_telemetry.clone()),
            });
            let mut r2_runtime = InputUpdateCardRuntime {
                library_scan_result: None,
                status: InputUpdateCardStatus::Updating,
                action_status: InputUpdateCardStatus::Updating,
                details: vec!["r2 cache acquisition".into()],
                run_id: Some(run_id("r2")),
                execution_order: Some(execution_order(2)),
                refresh_policy: Some(InputRefreshPolicy::NORMAL),
                input_telemetry: Some(r2_telemetry.clone()),
            };
            assert_eq!(state.for_input(7), r2_runtime);
            let state = state.apply(input_completed("r2", 2, 7, PlaylistUpdateState::Success, "r2 input completed"));
            r2_runtime.status = InputUpdateCardStatus::Success;
            r2_runtime.details.push("r2 input completed".into());
            assert_eq!(state.for_input(7), r2_runtime);
            let state = state.apply(accepted_with_policy("r1", &[7], InputRefreshPolicy::FORCE));
            assert_eq!(state.for_input(7), r2_runtime);
            let state = state.apply(completed("r2", 2, PlaylistUpdateState::Success));
            r2_runtime.action_status = InputUpdateCardStatus::Success;
            assert_eq!(state.for_input(7), r2_runtime);
            let state = state.apply(accepted_with_policy("r1", &[7], InputRefreshPolicy::FORCE));
            assert_eq!(state.for_input(7), r2_runtime);
        }
    }

    fn input(id: u16, name: &str, input_type: InputType) -> ConfigInputDto {
        ConfigInputDto { id, name: name.intern(), input_type, enabled: true, ..ConfigInputDto::default() }
    }

    fn target(id: u16, name: &str) -> ConfigTargetDto {
        ConfigTargetDto { id, name: name.to_string(), ..ConfigTargetDto::default() }
    }

    #[test]
    fn playlist_update_bulk_m3u_only_target_is_executable_without_a_manual_card() {
        let sources = SourcesConfigDto {
            inputs: vec![input(50, "m3u", InputType::M3u)],
            sources: vec![ConfigSourceDto { inputs: vec!["m3u".intern()], targets: vec![target(5, "m3u-target")] }],
            ..SourcesConfigDto::default()
        };

        assert_eq!(
            playlist_update_bulk_scope(&sources, &[]),
            PlaylistUpdateBulkScope { can_submit: true, visible_input_ids: Vec::new() }
        );
    }

    #[test]
    fn playlist_update_bulk_queues_only_visible_targeted_cards_and_ignores_card_policy() {
        let sources = SourcesConfigDto {
            inputs: vec![input(7, "xtream", InputType::Xtream), input(50, "m3u", InputType::M3u)],
            sources: vec![ConfigSourceDto {
                inputs: vec!["xtream".intern(), "m3u".intern()],
                targets: vec![target(1, "shared")],
            }],
            ..SourcesConfigDto::default()
        };
        let cards = vec![card(7, &[1]), card(8, &[])];
        let local_controls =
            InputUpdateCardState { selected_target_ids: Vec::new(), policy: InputRefreshPolicy::FORCE };

        assert_eq!(
            playlist_update_bulk_scope(&sources, &cards),
            PlaylistUpdateBulkScope { can_submit: true, visible_input_ids: vec![7] }
        );
        assert!(local_controls.selected_target_ids.is_empty());
        assert_eq!(local_controls.policy, InputRefreshPolicy::FORCE);
    }

    #[test]
    fn playlist_update_bulk_without_an_executable_target_is_disabled() {
        let mut disabled_target = target(1, "disabled");
        disabled_target.enabled = false;
        let sources = SourcesConfigDto {
            inputs: vec![input(7, "xtream", InputType::Xtream)],
            sources: vec![ConfigSourceDto { inputs: vec!["xtream".intern()], targets: vec![disabled_target] }],
            ..SourcesConfigDto::default()
        };

        assert_eq!(playlist_update_bulk_scope(&sources, &[card(7, &[1])]), PlaylistUpdateBulkScope::default());

        let no_targets = SourcesConfigDto {
            inputs: vec![input(7, "xtream", InputType::Xtream)],
            sources: vec![ConfigSourceDto { inputs: vec!["xtream".intern()], targets: Vec::new() }],
            ..SourcesConfigDto::default()
        };
        assert_eq!(playlist_update_bulk_scope(&no_targets, &[card(7, &[])]), PlaylistUpdateBulkScope::default());
    }

    #[test]
    fn playlist_update_bulk_excludes_disabled_visible_inputs_by_id_not_name() {
        let sources = SourcesConfigDto {
            inputs: vec![
                ConfigInputDto { enabled: false, ..input(7, "same", InputType::Xtream) },
                input(50, "same", InputType::M3u),
            ],
            sources: vec![ConfigSourceDto {
                inputs: vec!["same".intern(), "same".intern()],
                targets: vec![target(1, "shared")],
            }],
            ..SourcesConfigDto::default()
        };
        let mut cards = build_input_update_card_models(&sources, &Default::default());
        assert_eq!(cards.iter().map(|card| card.input_id).collect::<Vec<_>>(), vec![7, 50]);
        assert_eq!(cards[0].capabilities, InputUpdateCapabilities::Disabled);
        assert_eq!(cards[0].targets, cards[1].targets);
        // A card without a corresponding configured input must not join the accepted scope either.
        cards.push(card(99, &[1]));

        assert_eq!(
            playlist_update_bulk_scope(&sources, &cards),
            PlaylistUpdateBulkScope { can_submit: true, visible_input_ids: vec![50] }
        );
    }

    #[test]
    fn playlist_update_bulk_includes_enabled_inputs_independently_of_force_capability() {
        for (input_type, supports_force) in [(InputType::M3u, true), (InputType::Plex, false)] {
            let sources = SourcesConfigDto {
                inputs: vec![input(50, "provider", input_type)],
                sources: vec![ConfigSourceDto {
                    inputs: vec!["provider".intern()],
                    targets: vec![target(5, "target")],
                }],
                ..SourcesConfigDto::default()
            };
            let cards = build_input_update_card_models(&sources, &Default::default());
            assert_eq!(cards[0].capabilities.supports(InputRefreshPolicy::FORCE), supports_force);
            assert_eq!(
                playlist_update_bulk_scope(&sources, &cards),
                PlaylistUpdateBulkScope { can_submit: true, visible_input_ids: vec![50] }
            );
        }
    }

    #[test]
    fn playlist_update_bulk_includes_enabled_update_only_cards_in_configuration_order() {
        let sources = SourcesConfigDto {
            inputs: vec![
                input(30, "m3u-batch", InputType::M3uBatch),
                input(10, "xtream-batch", InputType::XtreamBatch),
                input(20, "stalker-batch", InputType::StalkerBatch),
            ],
            sources: vec![ConfigSourceDto {
                inputs: vec!["xtream-batch".intern(), "stalker-batch".intern(), "m3u-batch".intern()],
                targets: vec![target(5, "shared")],
            }],
            ..SourcesConfigDto::default()
        };
        let cards = build_input_update_card_models(&sources, &Default::default());
        assert!(cards.iter().all(|card| card.capabilities == InputUpdateCapabilities::BulkOnly));
        assert_eq!(
            playlist_update_bulk_scope(&sources, &cards),
            PlaylistUpdateBulkScope { can_submit: true, visible_input_ids: vec![30, 10, 20] }
        );
    }

    #[test]
    fn playlist_update_bulk_excludes_targetless_and_disabled_target_cards_from_an_executable_run() {
        let sources = SourcesConfigDto {
            inputs: vec![
                input(7, "targetless", InputType::Xtream),
                input(8, "disabled-target", InputType::M3u),
                input(9, "active-target", InputType::XtreamBatch),
            ],
            sources: vec![
                ConfigSourceDto { inputs: vec!["targetless".intern()], targets: Vec::new() },
                ConfigSourceDto {
                    inputs: vec!["disabled-target".intern()],
                    targets: vec![ConfigTargetDto { enabled: false, ..target(1, "disabled") }],
                },
                ConfigSourceDto { inputs: vec!["active-target".intern()], targets: vec![target(2, "active")] },
            ],
            ..SourcesConfigDto::default()
        };
        let cards = build_input_update_card_models(&sources, &Default::default());
        assert_eq!(cards.len(), 3);
        assert_eq!(
            playlist_update_bulk_scope(&sources, &cards),
            PlaylistUpdateBulkScope { can_submit: true, visible_input_ids: vec![9] }
        );
    }

    #[test]
    fn playlist_update_bulk_only_disabled_inputs_cannot_submit_or_queue_visible_cards() {
        let sources = SourcesConfigDto {
            inputs: vec![ConfigInputDto { enabled: false, ..input(7, "disabled", InputType::Xtream) }],
            sources: vec![ConfigSourceDto { inputs: vec!["disabled".intern()], targets: vec![target(1, "active")] }],
            ..SourcesConfigDto::default()
        };
        let cards = build_input_update_card_models(&sources, &Default::default());
        assert_eq!(cards.len(), 1);
        assert_eq!(playlist_update_bulk_scope(&sources, &cards), PlaylistUpdateBulkScope::default());
    }

    #[test]
    fn playlist_update_bulk_disabled_card_is_unchanged_through_the_accepted_run() {
        let sources = SourcesConfigDto {
            inputs: vec![
                ConfigInputDto { enabled: false, ..input(7, "disabled", InputType::Xtream) },
                input(8, "enabled", InputType::M3uBatch),
            ],
            sources: vec![ConfigSourceDto {
                inputs: vec!["disabled".intern(), "enabled".intern()],
                targets: vec![target(1, "shared")],
            }],
            ..SourcesConfigDto::default()
        };
        let cards = build_input_update_card_models(&sources, &Default::default());
        assert_eq!(cards.len(), 2);
        let scope = playlist_update_bulk_scope(&sources, &cards);
        assert!(scope.can_submit);
        for terminal in [PlaylistUpdateState::Success, PlaylistUpdateState::Partial, PlaylistUpdateState::Failure] {
            let state = reduce(statuses(), accepted("bulk", &scope.visible_input_ids));
            assert_eq!(state.for_input(7), InputUpdateCardRuntime::default());
            assert_eq!(state.for_input(8).status(), InputUpdateCardStatus::Queued);
            let state = reduce(state, progress("bulk", 1, 8, "started"));
            assert_eq!(state.for_input(7), InputUpdateCardRuntime::default());
            assert_eq!(state.for_input(8).status(), InputUpdateCardStatus::Updating);
            let state = reduce(state, completed("bulk", 1, terminal));
            assert_eq!(state.for_input(7), InputUpdateCardRuntime::default());
            assert_eq!(state.for_input(8).status(), status_from_state(terminal));

            // Only an actual input event may bring the excluded ID into a run's observed scope.
            let state = reduce(state, progress("observed", 2, 7, "actual input event"));
            assert_eq!(state.for_input(7).status(), InputUpdateCardStatus::Updating);
            assert_eq!(state.for_input(7).details(), ["actual input event"]);
        }
    }

    #[test]
    fn playlist_update_status_progress_then_same_run_acceptance_does_not_create_a_phantom_queue() {
        let state = reduce(statuses(), progress("r", 1, 7, "started"));
        let state = reduce(state, accepted("r", &[7]));
        assert_eq!(state.for_input(7).status(), InputUpdateCardStatus::Updating);
        assert_eq!(state.for_input(7).refresh_policy(), Some(InputRefreshPolicy::NORMAL));
        assert_eq!(state.runs.len(), 1);
        let state = reduce(state, completed("r", 1, PlaylistUpdateState::Success));

        assert_eq!(state.for_input(7).status(), InputUpdateCardStatus::Success);
    }

    #[test]
    fn playlist_update_status_exposes_effective_run_identity_from_the_existing_lifecycle() {
        let state = reduce(statuses(), accepted("stable-run", &[7]));
        let queued = state.for_input(7);
        assert_eq!(queued.run_id().map(PlaylistUpdateRunId::as_ref), Some("stable-run"));
        assert_eq!(queued.execution_order(), None);
        assert_eq!(queued.refresh_policy(), Some(InputRefreshPolicy::NORMAL));

        let state = reduce(state, progress("stable-run", 17, 7, "started"));
        let updating = state.for_input(7);
        assert_eq!(updating.run_id().map(PlaylistUpdateRunId::as_ref), Some("stable-run"));
        assert_eq!(updating.execution_order().map(PlaylistUpdateRunOrder::get), Some(17));
        assert_eq!(updating.refresh_policy(), Some(InputRefreshPolicy::NORMAL));

        let state = reduce(state, completed("stable-run", 17, PlaylistUpdateState::Partial));
        let terminal = state.for_input(7);
        assert_eq!(terminal.status(), InputUpdateCardStatus::Partial);
        assert_eq!(terminal.run_id().map(PlaylistUpdateRunId::as_ref), Some("stable-run"));
        assert_eq!(terminal.execution_order().map(PlaylistUpdateRunOrder::get), Some(17));
        assert_eq!(terminal.refresh_policy(), Some(InputRefreshPolicy::NORMAL));
        assert_eq!(state.for_input(8), InputUpdateCardRuntime::default());
    }

    #[test]
    fn playlist_update_status_manual_policy_applies_only_to_accepted_input_ids() {
        let state = reduce(statuses(), accepted_with_policy("manual", &[7], InputRefreshPolicy::FORCE));
        let state = reduce(state, progress("manual", 17, 7, "requested input"));
        let state = reduce(state, progress("manual", 17, 9, "target dependency"));

        assert_eq!(state.for_input(7).refresh_policy(), Some(InputRefreshPolicy::FORCE));
        assert_eq!(state.for_input(9).refresh_policy(), None);
        assert_eq!(state.for_input(7).status(), InputUpdateCardStatus::Updating);
        assert_eq!(state.for_input(9).status(), InputUpdateCardStatus::Updating);

        let state = reduce(state, completed("manual", 17, PlaylistUpdateState::Success));
        assert_eq!(state.for_input(7).refresh_policy(), Some(InputRefreshPolicy::FORCE));
        assert_eq!(state.for_input(9).refresh_policy(), None);
    }

    #[test]
    fn playlist_update_status_bulk_policy_applies_to_every_explicitly_accepted_input() {
        let state = reduce(statuses(), accepted("bulk", &[7, 9]));

        assert_eq!(state.for_input(7).refresh_policy(), Some(InputRefreshPolicy::NORMAL));
        assert_eq!(state.for_input(9).refresh_policy(), Some(InputRefreshPolicy::NORMAL));
        assert_eq!(state.for_input(8).refresh_policy(), None);
    }

    #[test]
    fn playlist_update_status_late_acceptance_adds_policy_without_changing_terminal_state() {
        let state = reduce(statuses(), progress("late", 23, 7, "started before acceptance"));
        let state =
            reduce(state, input_completed("late", 23, 7, PlaylistUpdateState::Partial, "finished before acceptance"));
        let state = reduce(state, completed("late", 23, PlaylistUpdateState::Partial));
        let state = reduce(state, accepted_with_policy("late", &[7], InputRefreshPolicy::REFRESH));
        let runtime = state.for_input(7);

        assert_eq!(runtime.status(), InputUpdateCardStatus::Partial);
        assert_eq!(runtime.run_id().map(PlaylistUpdateRunId::as_ref), Some("late"));
        assert_eq!(runtime.execution_order().map(PlaylistUpdateRunOrder::get), Some(23));
        assert_eq!(runtime.details(), ["started before acceptance", "finished before acceptance"]);
        assert_eq!(runtime.refresh_policy(), Some(InputRefreshPolicy::REFRESH));
        assert_eq!(state.runs.len(), 1);
    }

    #[test]
    fn playlist_update_status_terminal_policy_survives_run_pruning() {
        let state = reduce(statuses(), accepted_with_policy("retained", &[7], InputRefreshPolicy::FORCE));
        let state =
            reduce(state, input_completed("retained", 1, 7, PlaylistUpdateState::Success, "retained terminal detail"));
        let mut state = reduce(state, completed("retained", 1, PlaylistUpdateState::Success));

        for offset in 0..MAX_TRACKED_RUNS {
            let run = format!("later-{offset}");
            let input_id = 100 + u16::try_from(offset).expect("tracked-run limit fits u16");
            state = reduce(state, accepted(&run, &[input_id]));
            state = reduce(
                state,
                completed(
                    &run,
                    u64::try_from(offset).expect("tracked-run limit fits u64") + 2,
                    PlaylistUpdateState::Success,
                ),
            );
        }

        assert!(!state.runs.contains_key(&run_id("retained")));
        let runtime = state.for_input(7);
        assert_eq!(runtime.run_id().map(PlaylistUpdateRunId::as_ref), Some("retained"));
        assert_eq!(runtime.execution_order().map(PlaylistUpdateRunOrder::get), Some(1));
        assert_eq!(runtime.status(), InputUpdateCardStatus::Success);
        assert_eq!(runtime.details(), ["retained terminal detail"]);
        assert_eq!(runtime.refresh_policy(), Some(InputRefreshPolicy::FORCE));
    }

    #[test]
    fn playlist_update_status_progress_only_run_does_not_invent_normal_policy() {
        let state = reduce(statuses(), progress("automatic", 5, 7, "automatic progress"));
        assert_eq!(state.for_input(7).refresh_policy(), None);

        let state = reduce(state, completed("automatic", 5, PlaylistUpdateState::Success));
        assert_eq!(state.for_input(7).status(), InputUpdateCardStatus::Success);
        assert_eq!(state.for_input(7).refresh_policy(), None);
    }

    #[test]
    fn playlist_update_status_follow_up_run_remains_queued_after_previous_run_completes() {
        let state = reduce(statuses(), progress("r1", 1, 7, "first"));
        let state = reduce(state, accepted("r2", &[7]));
        let state = reduce(state, completed("r1", 1, PlaylistUpdateState::Success));

        assert_eq!(state.for_input(7).status(), InputUpdateCardStatus::Queued);
    }

    #[test]
    fn pipeline_transparency_playlist_update_status_hides_previous_run_facts_from_queued_follow_up() {
        let previous_telemetry = PlaylistUpdateInputTelemetry {
            refresh_policy: InputRefreshPolicy::NORMAL,
            source: Some(PlaylistUpdateDataSource::Cache),
            clusters: Vec::new(),
        };
        let state = reduce(statuses(), accepted("r1", &[7]));
        let state = reduce(
            state,
            PlaylistUpdateCardStatusAction::Progress {
                connection_context: connection(1),
                progress: PlaylistUpdateProgressEvent::for_run_input(
                    run_id("r1"),
                    execution_order(1),
                    7,
                    "input-7",
                    "r1 cache result",
                )
                .with_input_telemetry(previous_telemetry.clone()),
            },
        );
        let state = reduce(state, input_completed("r1", 1, 7, PlaylistUpdateState::Success, "r1 completed"));
        let state = reduce(state, completed("r1", 1, PlaylistUpdateState::Success));
        let previous = state.for_input(7);
        assert_eq!(previous.input_telemetry(), Some(&previous_telemetry));
        assert_eq!(previous.details(), ["r1 cache result", "r1 completed"]);

        let state = reduce(state, accepted_with_policy("r2", &[7], InputRefreshPolicy::FORCE));
        let queued = state.for_input(7);

        assert_eq!(queued.status(), InputUpdateCardStatus::Queued);
        assert_eq!(queued.run_id().map(PlaylistUpdateRunId::as_ref), Some("r2"));
        assert_eq!(queued.refresh_policy(), Some(InputRefreshPolicy::FORCE));
        assert!(queued.details().is_empty());
        assert_eq!(queued.input_telemetry(), None);
    }

    #[test]
    fn pipeline_transparency_playlist_update_status_in_run_reuse_preserves_acquisition_telemetry() {
        let provider_telemetry = PlaylistUpdateInputTelemetry {
            refresh_policy: InputRefreshPolicy::FORCE,
            source: None,
            clusters: vec![PlaylistUpdateClusterTelemetry {
                cluster: XtreamCluster::Live,
                requested: true,
                source: Some(PlaylistUpdateDataSource::Provider),
                baseline_count: None,
                candidate_count: None,
                active_count: None,
                threshold: Some(95),
                quality: None,
                decision: None,
                technical_state: None,
            }],
        };
        let state = reduce(statuses(), accepted_with_policy("shared", &[7], InputRefreshPolicy::FORCE));
        let state = reduce(
            state,
            PlaylistUpdateCardStatusAction::Progress {
                connection_context: connection(1),
                progress: PlaylistUpdateProgressEvent::for_run_input(
                    run_id("shared"),
                    execution_order(7),
                    7,
                    "input-7",
                    "provider acquisition",
                )
                .with_input_telemetry(provider_telemetry.clone()),
            },
        );

        let state =
            reduce(state, input_completed("shared", 7, 7, PlaylistUpdateState::Success, "in-run reuse completed"));
        let runtime = state.for_input(7);

        assert_eq!(runtime.input_telemetry(), Some(&provider_telemetry));
        assert_eq!(runtime.details(), ["provider acquisition", "in-run reuse completed"]);
        assert_eq!(runtime.refresh_policy(), Some(InputRefreshPolicy::FORCE));
    }

    #[test]
    fn playlist_update_status_accepted_run_failing_before_progress_finishes_as_failed() {
        let state = reduce(statuses(), accepted("r", &[7]));
        let state = reduce(state, completed("r", 1, PlaylistUpdateState::Failure));

        assert_eq!(state.for_input(7).status(), InputUpdateCardStatus::Failed);
    }

    #[test]
    fn playlist_update_status_unknown_terminal_changes_no_active_or_queued_card() {
        let state = reduce(statuses(), progress("active", 1, 7, "started"));
        let state = reduce(state, accepted("queued", &[8]));
        let state = reduce(state, completed("unknown", 2, PlaylistUpdateState::Failure));

        assert_eq!(state.for_input(7).status(), InputUpdateCardStatus::Updating);
        assert_eq!(state.for_input(8).status(), InputUpdateCardStatus::Queued);
        assert_eq!(state.for_input(9).status(), InputUpdateCardStatus::Ready);
    }

    #[test]
    fn playlist_update_status_late_acceptance_after_early_terminal_uses_the_same_run() {
        let state = reduce(statuses(), completed("r", 1, PlaylistUpdateState::Failure));
        let state = reduce(state, accepted("r", &[7]));

        assert_eq!(state.for_input(7).status(), InputUpdateCardStatus::Failed);
    }

    #[test]
    fn playlist_update_status_later_execution_replaces_earlier_status_and_details() {
        let state = reduce(statuses(), accepted("r2", &[7]));
        assert_eq!(state.for_input(7).status(), InputUpdateCardStatus::Queued);
        let state = reduce(state, progress("r1", 1, 7, "r1 started"));
        assert_eq!(state.for_input(7).status(), InputUpdateCardStatus::Updating);
        let state = reduce(state, input_completed("r1", 1, 7, PlaylistUpdateState::Success, "r1 success"));
        let state = reduce(state, completed("r1", 1, PlaylistUpdateState::Success));
        assert_eq!(state.for_input(7).status(), InputUpdateCardStatus::Queued);

        let state = reduce(state, progress("r2", 2, 7, "r2 started"));
        assert_eq!(state.for_input(7).status(), InputUpdateCardStatus::Updating);
        let state = reduce(state, input_completed("r2", 2, 7, PlaylistUpdateState::Failure, "r2 failed"));
        let state = reduce(state, completed("r2", 2, PlaylistUpdateState::Failure));

        let runtime = state.for_input(7);
        assert_eq!(runtime.status(), InputUpdateCardStatus::Failed);
        assert_eq!(runtime.details(), ["r2 started", "r2 failed"]);
        let terminal = state.terminal_by_input_id.get(&7).expect("terminal state");
        assert_eq!(terminal.run_id, run_id("r2"));
        assert_eq!(terminal.execution_order, execution_order(2));
        let details = state.details_by_input_id.get(&7).expect("input details");
        assert_eq!(details.run_id, run_id("r2"));
        assert_eq!(details.execution_order, execution_order(2));
    }

    #[test]
    fn playlist_update_status_later_execution_early_failure_clears_queue_and_older_details() {
        let state = reduce(statuses(), accepted("r2", &[7]));
        let state = reduce(state, progress("r1", 1, 7, "r1 started"));
        let state = reduce(state, input_completed("r1", 1, 7, PlaylistUpdateState::Success, "r1 success"));
        let state = reduce(state, completed("r1", 1, PlaylistUpdateState::Success));
        let state = reduce(state, completed("r2", 2, PlaylistUpdateState::Failure));

        let runtime = state.for_input(7);
        assert_eq!(runtime.status(), InputUpdateCardStatus::Failed);
        assert!(runtime.details().is_empty());
        let terminal = state.terminal_by_input_id.get(&7).expect("terminal state");
        assert_eq!(terminal.run_id, run_id("r2"));
        assert_eq!(terminal.execution_order, execution_order(2));
    }

    #[test]
    fn playlist_update_status_late_older_acceptance_and_terminal_cannot_replace_newer_execution() {
        let state = reduce(statuses(), progress("r1", 1, 7, "r1 started"));
        let state = reduce(state, input_completed("r1", 1, 7, PlaylistUpdateState::Success, "r1 success"));
        let state = reduce(state, accepted("r2", &[7]));
        let state = reduce(state, progress("r2", 2, 7, "r2 started"));
        let state = reduce(state, input_completed("r2", 2, 7, PlaylistUpdateState::Failure, "r2 failed"));
        let state = reduce(state, completed("r2", 2, PlaylistUpdateState::Failure));
        let state = reduce(state, accepted("r1", &[7]));
        let state = reduce(state, completed("r1", 1, PlaylistUpdateState::Success));

        let runtime = state.for_input(7);
        assert_eq!(runtime.status(), InputUpdateCardStatus::Failed);
        assert_eq!(runtime.details(), ["r2 started", "r2 failed"]);
        assert_eq!(state.terminal_by_input_id.get(&7).map(|terminal| &terminal.run_id), Some(&run_id("r2")));
        assert_eq!(state.details_by_input_id.get(&7).map(|details| &details.run_id), Some(&run_id("r2")));
    }

    #[test]
    fn playlist_update_status_backend_restart_accepts_lower_order_for_badge_and_details() {
        let runtime_a = connection(1);
        let runtime_b = connection(2);
        let state = PlaylistUpdateCardStatuses::for_connection(runtime_a);
        let state = reduce(state, progress_in(runtime_a, "r1", 42, 7, "r1 started"));
        let state =
            reduce(state, input_completed_in(runtime_a, "r1", 42, 7, PlaylistUpdateState::Success, "r1 success"));
        let state = reduce(state, completed_in(runtime_a, "r1", 42, PlaylistUpdateState::Success));
        assert_eq!(state.for_input(7).status(), InputUpdateCardStatus::Success);
        assert_eq!(state.for_input(7).details(), ["r1 started", "r1 success"]);

        let state = reduce(state, connection_changed(disconnected_connection(1)));
        assert_eq!(state.for_input(7), InputUpdateCardRuntime::default());
        let state = reduce(state, connection_changed(runtime_b));
        let state = reduce(state, accepted_in(runtime_b, "r2", &[7]));
        let state = reduce(state, progress_in(runtime_b, "r2", 1, 7, "r2 started"));
        assert_eq!(state.for_input(7).status(), InputUpdateCardStatus::Updating);
        let state = reduce(state, input_completed_in(runtime_b, "r2", 1, 7, PlaylistUpdateState::Failure, "r2 failed"));
        let state = reduce(state, completed_in(runtime_b, "r2", 1, PlaylistUpdateState::Failure));

        assert_eq!(state.for_input(7).status(), InputUpdateCardStatus::Failed);
        assert_eq!(state.for_input(7).details(), ["r2 started", "r2 failed"]);
    }

    #[test]
    fn playlist_update_status_equal_orders_in_different_connections_are_not_tied() {
        let runtime_a = connection(1);
        let runtime_b = connection(2);
        let state = PlaylistUpdateCardStatuses::for_connection(runtime_a);
        let state =
            reduce(state, input_completed_in(runtime_a, "r1", 1, 7, PlaylistUpdateState::Success, "r1 success"));
        let state = reduce(state, completed_in(runtime_a, "r1", 1, PlaylistUpdateState::Success));
        let state = reduce(state, connection_changed(disconnected_connection(1)));
        let state = reduce(state, connection_changed(runtime_b));
        let state = reduce(state, accepted_in(runtime_b, "r2", &[7]));
        let state = reduce(state, input_completed_in(runtime_b, "r2", 1, 7, PlaylistUpdateState::Failure, "r2 failed"));
        let state = reduce(state, completed_in(runtime_b, "r2", 1, PlaylistUpdateState::Failure));

        assert_eq!(state.for_input(7).status(), InputUpdateCardStatus::Failed);
        assert_eq!(state.for_input(7).details(), ["r2 failed"]);
    }

    #[test]
    fn playlist_update_status_late_events_from_left_connection_are_ignored_without_queue() {
        let runtime_a = connection(1);
        let runtime_b = connection(2);
        let state = PlaylistUpdateCardStatuses::for_connection(runtime_a);
        let state = reduce(state, connection_changed(disconnected_connection(1)));
        let state = reduce(state, connection_changed(runtime_b));
        let state = reduce(state, accepted_in(runtime_b, "r2", &[7]));
        let state = reduce(state, input_completed_in(runtime_b, "r2", 1, 7, PlaylistUpdateState::Failure, "r2 failed"));
        let state = reduce(state, completed_in(runtime_b, "r2", 1, PlaylistUpdateState::Failure));
        let state = reduce(state, accepted_in(runtime_a, "r1", &[7]));
        let state = reduce(state, progress_in(runtime_a, "r1", 42, 7, "late r1 detail"));
        let state = reduce(state, completed_in(runtime_a, "r1", 42, PlaylistUpdateState::Success));

        assert_eq!(state.for_input(7).status(), InputUpdateCardStatus::Failed);
        assert_eq!(state.for_input(7).details(), ["r2 failed"]);
        assert!(!state.runs.contains_key(&run_id("r1")));
    }

    #[test]
    fn playlist_update_status_reconnect_clears_orphaned_runs_without_replay_or_terminal_guess() {
        let runtime_a = connection(1);
        let runtime_b = connection(2);
        let state = PlaylistUpdateCardStatuses::for_connection(runtime_a);
        let state = reduce(state, accepted_in(runtime_a, "queued", &[7]));
        let state = reduce(state, progress_in(runtime_a, "active", 1, 8, "active detail"));
        assert_eq!(state.for_input(7).status(), InputUpdateCardStatus::Queued);
        assert_eq!(state.for_input(8).status(), InputUpdateCardStatus::Updating);

        let state = reduce(state, connection_changed(disconnected_connection(1)));
        let state = reduce(state, connection_changed(runtime_b));
        let state = reduce(state, completed_in(runtime_a, "active", 1, PlaylistUpdateState::Success));

        assert_eq!(state.for_input(7), InputUpdateCardRuntime::default());
        assert_eq!(state.for_input(8), InputUpdateCardRuntime::default());
        assert!(state.runs.is_empty());
    }

    #[test]
    fn playlist_update_status_mixed_bulk_keeps_input_outcomes_distinct_from_global_partial() {
        let state = reduce(statuses(), accepted("bulk", &[7, 8]));
        let state = reduce(state, input_completed("bulk", 1, 7, PlaylistUpdateState::Success, "success"));
        let state = reduce(state, progress("bulk", 1, 8, "vod quality rejected"));
        let state = reduce(state, input_completed("bulk", 1, 8, PlaylistUpdateState::Partial, "partial"));
        let state = reduce(state, completed("bulk", 1, PlaylistUpdateState::Partial));

        assert_eq!(state.for_input(7).status(), InputUpdateCardStatus::Success);
        assert_eq!(state.for_input(8).status(), InputUpdateCardStatus::Partial);
    }

    #[test]
    fn playlist_update_status_technical_error_detail_is_scoped_to_the_failed_input() {
        let state = reduce(statuses(), accepted("bulk", &[7, 8]));
        let state = reduce(
            state,
            input_completed("bulk", 1, 7, PlaylistUpdateState::Failure, "Input 'a' failed during update"),
        );
        let state = reduce(state, input_completed("bulk", 1, 8, PlaylistUpdateState::Success, "Input 'b' completed"));

        assert!(state.for_input(7).details().iter().any(|line| line.contains("failed during update")));
        assert!(!state.for_input(8).details().iter().any(|line| line.contains("failed during update")));
    }

    #[test]
    fn playlist_update_status_correlated_legacy_terminal_without_order_cannot_replace_ordered_result() {
        let state = reduce(statuses(), accepted("r2", &[7]));
        let state = reduce(state, input_completed("r2", 2, 7, PlaylistUpdateState::Failure, "r2 failed"));
        let state = reduce(state, completed("r2", 2, PlaylistUpdateState::Failure));
        let state = reduce(
            state,
            PlaylistUpdateCardStatusAction::Completed {
                connection_context: connection(1),
                completed: PlaylistUpdateRunStateEvent {
                    run_id: Some(run_id("r1")),
                    execution_order: None,
                    state: PlaylistUpdateState::Success,
                },
            },
        );
        let state = reduce(state, accepted("r1", &[7]));

        let runtime = state.for_input(7);
        assert_eq!(runtime.status(), InputUpdateCardStatus::Failed);
        assert_eq!(runtime.details(), ["r2 failed"]);
    }

    #[test]
    fn playlist_update_status_legacy_events_without_run_identity_do_not_mutate_cards() {
        let state = reduce(
            statuses(),
            PlaylistUpdateCardStatusAction::Progress {
                connection_context: connection(1),
                progress: PlaylistUpdateProgressEvent::for_input(7, "legacy", "old"),
            },
        );
        let state = reduce(
            state,
            PlaylistUpdateCardStatusAction::Completed {
                connection_context: connection(1),
                completed: PlaylistUpdateRunStateEvent::uncorrelated(PlaylistUpdateState::Failure),
            },
        );

        assert_eq!(state.for_input(7), InputUpdateCardRuntime::default());
    }
}
