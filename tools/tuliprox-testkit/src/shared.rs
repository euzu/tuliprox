use crate::TestkitError;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Default)]
pub struct SharedStreamModel {
    subscribers: HashMap<u32, HashSet<String>>,
}

impl SharedStreamModel {
    /// Returns true only when this subscriber causes a new origin connection.
    pub fn subscribe(&mut self, marker: u32, playback_id: String) -> Result<bool, TestkitError> {
        let subscribers = self.subscribers.entry(marker).or_default();
        if !subscribers.insert(playback_id) {
            return Err(TestkitError::Protocol("shared-stream subscriber was registered twice".to_owned()));
        }
        Ok(subscribers.len() == 1)
    }

    /// Returns true only when the last subscriber closes the origin connection.
    pub fn unsubscribe(&mut self, marker: u32, playback_id: &str) -> Result<bool, TestkitError> {
        let subscribers = self
            .subscribers
            .get_mut(&marker)
            .ok_or_else(|| TestkitError::Configuration(format!("shared stream marker {marker} is not active")))?;
        if !subscribers.remove(playback_id) {
            return Err(TestkitError::Configuration(format!(
                "playback {playback_id} is not a shared-stream subscriber"
            )));
        }
        let last = subscribers.is_empty();
        if last {
            self.subscribers.remove(&marker);
        }
        Ok(last)
    }

    #[must_use]
    pub fn subscriber_count(&self, marker: u32) -> usize { self.subscribers.get(&marker).map_or(0, HashSet::len) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_first_and_last_subscriber_change_origin_lifetime() {
        let mut model = SharedStreamModel::default();
        assert!(model.subscribe(17, "a".to_owned()).unwrap());
        assert!(!model.subscribe(17, "b".to_owned()).unwrap());
        assert!(!model.unsubscribe(17, "a").unwrap());
        assert!(model.unsubscribe(17, "b").unwrap());
    }
}
