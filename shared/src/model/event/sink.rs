use super::EventMessage;
use std::sync::Arc;

/// Somewhere an [`EventMessage`] can be published.
///
/// A bound, not a trait object: emitters are generic over their sink and
/// monomorphise against the one they were built with. Three things fall out
/// of that:
///
/// * the pipeline no longer threads `Option<Arc<EventManager>>` around and
///   branches on it at every emit site - [`NoopSink`] is the absent case,
///   and its `emit` is an empty function the optimiser deletes outright;
/// * tests can assert on what was emitted without standing up a broadcast
///   channel and racing a subscriber;
/// * `dvr` and the notification path can name a sink without depending on
///   the streaming-session runtime that implements it.
///
/// Implementations must not block. The bus is reached from the streaming
/// data path, and an emitter that can stall is an emitter that will.
pub trait EventSink: Send + Sync {
    /// Publish. Best-effort by contract: no subscribers, a full buffer or a
    /// closed channel are all normal and none of them are the emitter's
    /// problem.
    fn emit(&self, event: EventMessage);
    /// Forward a confirmed account response to the runtime provider manager.
    /// Sinks without a provider runtime deliberately ignore the observation.
    fn observe_provider_account(
        &self,
        _name: Arc<str>,
        _identity: super::super::ProviderAccountIdentity,
        _status: Option<super::super::ProxyUserStatus>,
        _exp_date: Option<i64>,
    ) {
    }
}

/// The sink that drops everything.
///
/// Replaces the `Option` at call sites that only ever had a `None` case
/// because tests and one-shot CLI runs have no bus. Monomorphised against
/// this, every emit site compiles to nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopSink;

impl EventSink for NoopSink {
    fn emit(&self, _event: EventMessage) {}
}

impl<T: EventSink + ?Sized> EventSink for Arc<T> {
    fn emit(&self, event: EventMessage) { (**self).emit(event); }
    fn observe_provider_account(
        &self,
        name: Arc<str>,
        identity: super::super::ProviderAccountIdentity,
        status: Option<super::super::ProxyUserStatus>,
        exp_date: Option<i64>,
    ) {
        (**self).observe_provider_account(name, identity, status, exp_date);
    }
}

impl<T: EventSink> EventSink for &T {
    fn emit(&self, event: EventMessage) { (**self).emit(event); }
    fn observe_provider_account(
        &self,
        name: Arc<str>,
        identity: super::super::ProviderAccountIdentity,
        status: Option<super::super::ProxyUserStatus>,
        exp_date: Option<i64>,
    ) {
        (**self).observe_provider_account(name, identity, status, exp_date);
    }
}
