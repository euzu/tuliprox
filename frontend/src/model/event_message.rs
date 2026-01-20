use std::rc::Rc;
use std::sync::Arc;
use shared::model::{ActiveUserConnectionChange, ConfigType, LibraryScanSummary, PlaylistUpdateState, StatusCheck, SystemInfo};
use crate::model::BusyStatus;

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum EventMessage {
    Unauthorized,
    ServerError(String),
    ServerStatus(Rc<StatusCheck>),
    ActiveUser(ActiveUserConnectionChange),
    ActiveProvider(Arc<str>, usize), // single provider
    ActiveProviderCount(usize), // all provider
    ConfigChange(ConfigType),
    Busy(BusyStatus),
    PlaylistUpdate(PlaylistUpdateState),
    PlaylistUpdateProgress(String, String),
    WebSocketStatus(bool),
    SystemInfoUpdate(SystemInfo),
    LibraryScanProgress(LibraryScanSummary)
}