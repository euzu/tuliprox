use crate::TestkitError;
use std::{collections::HashMap, time::Duration};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EventKey {
    pub command_id: String,
    pub event: String,
}

#[derive(Debug, Default)]
pub struct Timeline {
    anchors: HashMap<EventKey, tokio::time::Instant>,
}

impl Timeline {
    pub fn record(&mut self, key: EventKey) -> Result<(), TestkitError> {
        if self.anchors.contains_key(&key) {
            return Err(TestkitError::Protocol(format!("duplicate causal event {}:{}", key.command_id, key.event)));
        }
        self.anchors.insert(key, tokio::time::Instant::now());
        Ok(())
    }

    pub fn deadline(&self, key: &EventKey, within: Duration) -> Result<tokio::time::Instant, TestkitError> {
        self.anchors.get(key).copied().map(|anchor| anchor + within).ok_or_else(|| {
            TestkitError::Configuration(format!("assertion anchor {}:{} was never armed", key.command_id, key.event))
        })
    }

    pub fn require_before(
        &self,
        key: &EventKey,
        within: Duration,
        observed_at: tokio::time::Instant,
    ) -> Result<(), TestkitError> {
        if observed_at > self.deadline(key, within)? {
            return Err(TestkitError::Protocol(format!(
                "event exceeded deadline anchored at {}:{}",
                key.command_id, key.event
            )));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct ContinuousProgress {
    max_gap: Duration,
    last_frame: Option<tokio::time::Instant>,
}

impl ContinuousProgress {
    #[must_use]
    pub fn new(max_gap: Duration) -> Self { Self { max_gap, last_frame: None } }

    pub fn frame(&mut self, now: tokio::time::Instant) -> Result<(), TestkitError> {
        if self.last_frame.is_some_and(|previous| now.duration_since(previous) > self.max_gap) {
            return Err(TestkitError::Protocol("stream progress gap exceeded assertion bound".to_owned()));
        }
        self.last_frame = Some(now);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn deadline_remains_anchored_when_later_events_arrive() {
        let mut timeline = Timeline::default();
        let key = EventKey { command_id: "start-b".to_owned(), event: "sent".to_owned() };
        timeline.record(key.clone()).unwrap();
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(timeline.require_before(&key, Duration::from_secs(1), tokio::time::Instant::now()).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn continuous_progress_rejects_long_gaps() {
        let mut progress = ContinuousProgress::new(Duration::from_secs(1));
        progress.frame(tokio::time::Instant::now()).unwrap();
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(progress.frame(tokio::time::Instant::now()).is_err());
    }
}
