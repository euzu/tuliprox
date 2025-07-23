use log::error;
use tokio::task;
use crate::api::model::active_user_manager::{ActiveUserConnectionChangeReceiver};
use crate::api::model::provider_config::{ProviderConnectionChangeReceiver};

#[derive(Clone, PartialEq)]
pub enum EventMessage {
    ActiveUserChange(usize, usize), // user_count, connection count
    ActiveProviderChange(String, usize) // provider name, connections
}

pub struct EventManager {
    channel_tx: tokio::sync::broadcast::Sender<EventMessage>,
    #[allow(dead_code)]
    channel_rx: tokio::sync::broadcast::Receiver<EventMessage>,
}

impl EventManager {
    pub fn new(mut active_user_change_rx: ActiveUserConnectionChangeReceiver,
               mut provider_change_rx: ProviderConnectionChangeReceiver,
    ) -> Self {
        let (channel_tx, channel_rx) = tokio::sync::broadcast::channel(10);

        let channel_tx_clone = channel_tx.clone();
        task::spawn(async move {
            loop {
               tokio::select! {
                    Some((user_count, connection_count)) = active_user_change_rx.recv() => {
                        if let Err(e) = channel_tx_clone.send(EventMessage::ActiveUserChange(user_count, connection_count)) {
                            error!("Failed to send active user change event: {e}");
                            break;
                        }
                    }

                    Some((provider, connection_count)) = provider_change_rx.recv() => {
                        if let Err(e) = channel_tx_clone.send(EventMessage::ActiveProviderChange(provider, connection_count)) {
                            error!("Failed to send active provider change event: {e}");
                            break;
                        }
                    }
               }
           }
        });

        Self {
            channel_tx,
            channel_rx,
        }
    }

    pub fn get_event_channel(&self) -> tokio::sync::broadcast::Receiver<EventMessage> {
        self.channel_tx.subscribe()
    }

    pub fn send_event(&self, event: EventMessage) {
        if let Err(err) = self.channel_tx.send(event) {
            error!("Failed to send event: {err}");
        }
    }

}

