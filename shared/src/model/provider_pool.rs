use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// One provider's capacity at the moment the pool was inspected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderPoolEntry {
    pub name: Arc<str>,
    pub current_connections: usize,
    pub max_connections: usize,
    pub expired: bool,
}

/// Every provider behind an input was at capacity, so a request was refused.
///
/// The snapshot was already being built - per-provider `current/max` plus
/// expiry - and then thrown away unless debug logging happened to be on.
/// `ActiveProvider` says connection counts moved; nothing said a stream was
/// turned away because there was nowhere to put it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderPoolExhausted {
    /// The input whose pool ran out, already sanitized.
    pub input: Arc<str>,
    pub providers: Vec<ProviderPoolEntry>,
}

impl ProviderPoolExhausted {
    #[must_use]
    pub fn new(input: Arc<str>, providers: Vec<ProviderPoolEntry>) -> Self { Self { input, providers } }

    /// Per input: when a provider goes down every request behind it is
    /// refused, and an operator wants to hear that once.
    #[must_use]
    pub fn dedup_key(&self) -> String { format!("provider-pool-exhausted:{}", self.input) }
}

/// An input started being served from a different priority group.
///
/// `acquire` walks priority groups highest to lowest and silently falls
/// through when the preferred ones are at capacity, so "you are on your
/// backup provider" was invisible.
///
/// Emitted on *transition* rather than per allocation. The fallthrough
/// happens on every request while the primary is full, and one event per
/// stream start would drown the signal it exists to carry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderPriorityFallback {
    /// The input being allocated, already sanitized.
    pub input: Arc<str>,
    /// The provider now serving it.
    pub provider: Arc<str>,
    /// Which priority group that provider sits in. Zero is the preferred
    /// group, so a non-zero value is a fallback and a return to zero is a
    /// recovery.
    pub group_index: usize,
    /// How many priority groups the input has.
    pub group_count: usize,
    /// The group that was serving this input before, when there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_group_index: Option<usize>,
}

impl ProviderPriorityFallback {
    #[must_use]
    pub fn new(
        input: Arc<str>,
        provider: Arc<str>,
        group_index: usize,
        group_count: usize,
        previous_group_index: Option<usize>,
    ) -> Self {
        Self { input, provider, group_index, group_count, previous_group_index }
    }

    /// Whether this is a move back towards the preferred group.
    #[must_use]
    pub fn is_recovery(&self) -> bool { self.previous_group_index.is_some_and(|previous| self.group_index < previous) }
}
