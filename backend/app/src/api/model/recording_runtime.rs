//! Adapts the server's connection managers to the DVR's capacity port.
//!
//! Everything provider-shaped stays on this side of the boundary. The adapter
//! does not write files, choose between ffmpeg and HTTP, or change a
//! recording's state; it answers whether there is a connection to record with
//! and hands one over. That is the whole of its job, and keeping it that small
//! is what lets the DVR be tested without a provider.

use futures::future::BoxFuture;
use std::sync::Arc;
use tokio::sync::Notify;
use tuliprox_core::model::ProviderHandle;
use tuliprox_dvr::recording::recording_capacity::{ProviderCapacity, RecordingCapacityPort};
use tuliprox_session::{ActiveProviderManager, ConnectionManager};

/// The running server's provider capacity, as the DVR sees it.
pub struct ProviderCapacityAdapter {
    active_provider: Arc<ActiveProviderManager>,
    connection_manager: Arc<ConnectionManager>,
}

impl ProviderCapacityAdapter {
    pub fn new(active_provider: Arc<ActiveProviderManager>, connection_manager: Arc<ConnectionManager>) -> Arc<Self> {
        Arc::new(Self { active_provider, connection_manager })
    }
}

impl RecordingCapacityPort for ProviderCapacityAdapter {
    fn capacities_for_input<'a>(&'a self, input_name: &'a Arc<str>) -> BoxFuture<'a, Vec<ProviderCapacity>> {
        Box::pin(self.active_provider.provider_capacities_for_input(input_name))
    }

    fn acquire<'a>(&'a self, input_name: &'a Arc<str>, priority: i8) -> BoxFuture<'a, Option<ProviderHandle>> {
        Box::pin(self.active_provider.acquire_connection_for_download(input_name, priority))
    }

    fn release(&self, handle: Option<ProviderHandle>) -> BoxFuture<'_, ()> {
        Box::pin(self.connection_manager.release_provider_handle(handle))
    }

    fn capacity_changed(&self) -> Arc<Notify> { self.connection_manager.capacity_notified() }
}
