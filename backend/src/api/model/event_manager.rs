use std::sync::Arc;
use log::{trace};
use shared::model::{ActiveUserConnectionChange, ConfigType, LibraryScanSummary, PlaylistUpdateState, SystemInfo};

#[allow(clippy::large_enum_variant)]
#[derive(Clone, PartialEq)]
pub enum EventMessage {
    ServerError(String),
    ActiveUser(ActiveUserConnectionChange), // user_count, connection count
    ActiveProvider(Arc<str>, usize), // provider name, connections
    ConfigChange(ConfigType),
    PlaylistUpdate(PlaylistUpdateState),
    PlaylistUpdateProgress(String, String),
    SystemInfoUpdate(SystemInfo),
    LibraryScanProgress(LibraryScanSummary),
}

pub struct EventManager {
    channel_tx: tokio::sync::broadcast::Sender<EventMessage>,
}

impl EventManager {
    pub fn new() -> Self {
        let (channel_tx, _channel_rx) = tokio::sync::broadcast::channel(10);

        Self {
            channel_tx,
            //channel_rx,
        }
    }

    pub fn get_event_channel(&self) -> tokio::sync::broadcast::Receiver<EventMessage> {
        self.channel_tx.subscribe()
    }

    pub fn send_event(&self, event: EventMessage) -> bool {
        if let Err(err) = self.channel_tx.send(event) {
            trace!("Failed to send event: {err}");
            false
        } else {
            true
        }
    }

    pub fn send_provider_event(&self, provider: &Arc<str>, connection_count: usize) {
        if !self.send_event(EventMessage::ActiveProvider(Arc::clone(provider), connection_count)) {
            trace!("Failed to send connection change: {provider}: {connection_count}");
        }
    }

    pub fn send_system_info(&self, system_info: SystemInfo) {
        if !self.send_event(EventMessage::SystemInfoUpdate(system_info)) {
            trace!("Failed to send system info");
        }
    }
}

impl Default for EventManager {
    fn default() -> Self {
        Self::new()
    }
}
