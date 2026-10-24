use crate::api::model::RecordingQueue;
use std::sync::Arc;
use tuliprox_dvr::recording::recording_capacity::RecordingCapacityPort;

/// The recording queue and the provider capacity the DVR sees.
pub struct RecordingState {
    pub queue: Arc<RecordingQueue>,
    /// The adapter that keeps provider details out of the recording engine.
    pub capacity: Arc<dyn RecordingCapacityPort>,
}

impl RecordingState {
    pub fn new(queue: Arc<RecordingQueue>, capacity: Arc<dyn RecordingCapacityPort>) -> Self {
        Self { queue, capacity }
    }
}
