use crate::model::BusyStatus;
use shared::model::{
    ActiveUserConnectionChange, ConfigType, DownloadsDelta, DownloadsResponse, LibraryScanProgressEvent,
    PlaylistUpdateProgressEvent, PlaylistUpdateState, StatusCheck, StreamMeterEntry, SystemInfo,
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
    DownloadsUpdate(Rc<DownloadsResponse>),
    DownloadsDeltaUpdate(Rc<DownloadsDelta>),
}
