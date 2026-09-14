use crate::{
    protocol::{PlaybackId, RunId},
    TestkitError,
};
use std::collections::HashMap;

/// Tracks the semantic start identity for a run independently from transport message IDs.
/// A replay with identical parameters is harmless; a conflicting replay is rejected.
#[derive(Debug, Default)]
pub struct PlaybackRegistry {
    starts: HashMap<(RunId, u64, PlaybackId), String>,
}

impl PlaybackRegistry {
    pub fn register_start(
        &mut self,
        run_id: RunId,
        generation: u64,
        playback_id: PlaybackId,
        fingerprint: String,
    ) -> Result<StartDisposition, TestkitError> {
        let key = (run_id, generation, playback_id);
        match self.starts.get(&key) {
            None => {
                self.starts.insert(key, fingerprint);
                Ok(StartDisposition::New)
            }
            Some(existing) if existing == &fingerprint => Ok(StartDisposition::Replay),
            Some(_) => Err(TestkitError::Protocol("playback ID was reused with different start parameters".to_owned())),
        }
    }

    pub fn stop(&mut self, run_id: &RunId, generation: u64, playback_id: &PlaybackId) -> bool {
        self.starts.remove(&(run_id.clone(), generation, playback_id.clone())).is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartDisposition {
    New,
    Replay,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_is_safe_but_conflict_is_rejected() {
        let mut registry = PlaybackRegistry::default();
        let run = RunId::new("run");
        let playback = PlaybackId::new("playback");
        assert!(matches!(
            registry.register_start(run.clone(), 1, playback.clone(), "first".to_owned()),
            Ok(StartDisposition::New)
        ));
        assert!(matches!(
            registry.register_start(run.clone(), 1, playback.clone(), "first".to_owned()),
            Ok(StartDisposition::Replay)
        ));
        assert!(registry.register_start(run, 1, playback, "different".to_owned()).is_err());
    }
}
