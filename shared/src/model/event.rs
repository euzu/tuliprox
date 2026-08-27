//! The in-process event taxonomy.
//!
//! `EventMessage` used to live in `tuliprox-session`, which meant that
//! `metadata`, `dvr` and `processing` all had to depend on the *streaming
//! session runtime* just to name an event. A metadata refresh has nothing
//! to do with provider allocation, so the type lives here instead: `shared`
//! is the one crate every emitter already sees.
//!
//! The bus implementation (`EventManager`) stays in `session`, where the
//! stream-meter registry it feeds also lives.

use crate::model::{
    ActiveUserConnectionChange, ConfigType, DownloadsDelta, DownloadsResponse, LibraryScanProgressEvent,
    PlaylistUpdateProgressEvent, PlaylistUpdateState, SystemInfo,
};
use std::sync::Arc;

/// Everything that happens in the server that someone outside the emitting
/// module might want to know about.
///
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum EventMessage {
    ServerError(String),
    ActiveUser(ActiveUserConnectionChange),
    ActiveProvider(Arc<str>, usize),
    ConfigChange(ConfigType),
    PlaylistUpdate(PlaylistUpdateState),
    PlaylistUpdateProgress(PlaylistUpdateProgressEvent),
    SystemInfoUpdate(SystemInfo),
    LibraryScanProgress(LibraryScanProgressEvent),
    DownloadsUpdate(DownloadsResponse),
    DownloadsDeltaUpdate(DownloadsDelta),
    RecordingChanged,
    RecordingRulesChanged,
    InputMetadataUpdatesCompleted(Arc<str>),
    InputMetadataUpdatesStarted(Arc<str>),
}
