use crate::model::BusyStatus;
use shared::model::{
    ActiveUserConnectionChange, ConfigType, LibraryScanProgressEvent, PlaylistUpdateProgressEvent, PlaylistUpdateState,
    RecordingTaskDto, StatusCheck, StreamMeterEntry, SystemInfo,
};
use std::{rc::Rc, sync::Arc};

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum EventMessage {
    Unauthorized,
    ServerError(String),
    ServerStatus(Rc<StatusCheck>),
    ActiveUser(ActiveUserConnectionChange),
    ActiveProvider(Arc<str>, usize), // single provider
    ActiveProviderCount(usize),      // all provider
    ConfigChange(ConfigType),
    Busy(BusyStatus),
    PlaylistUpdate(PlaylistUpdateState),
    PlaylistUpdateProgress(PlaylistUpdateProgressEvent),
    WebSocketStatus(bool),
    SystemInfoUpdate(SystemInfo),
    LibraryScanProgress(LibraryScanProgressEvent),
    StreamMeterBatch(Vec<StreamMeterEntry>),
    RecordingSnapshot {
        revision: u64,
        tasks: Rc<Vec<RecordingTaskDto>>,
    },
    RecordingRulesChanged,
    /// The socket refused to serve recordings for an actionable reason.
    /// Carries the same stable code the REST routes use, so views map
    /// both through one i18n table.
    RecordingUnavailable {
        code: String,
    },
}
