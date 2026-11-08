//! `Channel::Plugin` delivery : the
//! [`shared::model::PluginNotifier`] that messaging reaches plugins through.
//!
//! `notification_channels` lists the plugins runnable for `SendNotification`
//! with the `ext.messaging.routing` object of their settings (the messaging
//! config extension, edited on the Web UI messaging page); `send_notification` opens a
//! fresh instance, calls `send-notification` under the normal call deadline and
//! maps the answer onto [`PluginDelivery`]. The outbox owns every retry, so a
//! trap, a deadline or an exhausted budget is a `Retry`, never an error to the
//! emitter.

use crate::{
    dispatch::HookDispatcher,
    host_fns,
    registry::{LoadedPlugin, PluginState},
    runtime::{call_with_deadline, CallError},
    PluginSystem,
};
use serde_json::Value;
use shared::model::{
    PluginDelivery, PluginDeliveryFuture, PluginHook, PluginNotification, PluginNotificationChannel, PluginNotifier,
    PluginSeverity,
};
use std::time::Duration;
use tuliprox_plugin_api::bindings::{
    exports::tuliprox::plugin::notification_hooks::{DeliveryError, Notification},
    tuliprox::plugin::status::Severity,
};

const HOOK: PluginHook = PluginHook::SendNotification;

/// How long one delivery may take. Messaging gives a send 30 s (a webhook host
/// that accepts and never answers must not stall the outbox), so the plugin
/// call gets the same: the 5 s `plugins.max_exec` of an ordinary hook would
/// turn a slow but successful provider answer into a retry. It is never below
/// the plugin's own `max_exec`, and `plugins.max_task_exec` still caps it.
const NOTIFICATION_EXEC: Duration = Duration::from_secs(30);

fn notification_exec(dispatcher: &HookDispatcher<'_>, own: Duration) -> Duration {
    let task_exec = dispatcher.runtime().config().task_exec;
    NOTIFICATION_EXEC.min(task_exec).max(own)
}

/// What `send-notification` itself answered, as opposed to the call failing.
type Answer = Result<(), DeliveryError>;

#[allow(clippy::needless_pass_by_value, reason = "used as a `map_err` function")]
fn bind(err: wasmtime::Error) -> CallError { CallError::Trap(format!("{err:#}")) }

const fn wit_severity(severity: PluginSeverity) -> Severity {
    match severity {
        PluginSeverity::Info => Severity::Info,
        PluginSeverity::Warn => Severity::Warn,
        PluginSeverity::Error => Severity::Error,
    }
}

/// The `ext.messaging.routing` object of a plugin's resolved settings JSON:
/// the routing block of its messaging config extension. `None` when the
/// settings are not an object or carry no (non-null) routing there.
fn routing_of(settings: &str) -> Option<Value> {
    let mut parsed: Value = serde_json::from_str(settings).ok()?;
    match parsed.pointer_mut("/ext/messaging/routing").map(Value::take) {
        Some(Value::Null) | None => None,
        Some(routing) => Some(routing),
    }
}

/// The guest's own answer as a delivery: `transient` and `retry-after` retry,
/// `permanent` dead-letters.
fn delivery_of(answer: Answer) -> PluginDelivery {
    match answer {
        Ok(()) => PluginDelivery::Delivered,
        Err(DeliveryError::Transient(reason)) => PluginDelivery::Retry { reason, after: None },
        Err(DeliveryError::RetryAfter(secs)) => PluginDelivery::Retry {
            reason: format!("plugin asked to retry after {secs}s"),
            after: Some(Duration::from_secs(u64::from(secs))),
        },
        Err(DeliveryError::Permanent(reason)) => PluginDelivery::Permanent { reason },
    }
}

/// One fresh instance, one `send-notification` call.
async fn call_one(
    dispatcher: &HookDispatcher<'_>,
    loaded: &LoadedPlugin,
    message: &PluginNotification<'_>,
) -> Result<Answer, CallError> {
    let indices = loaded
        .notification_hooks
        .as_ref()
        .ok_or_else(|| CallError::Instantiate("does not export notification-hooks".to_string()))?;
    let exec = notification_exec(dispatcher, loaded.prepared.limits().exec);
    let mut live = dispatcher.open_with(loaded, exec).await?;
    let instance = live.instance();
    let guest = indices.load(live.store_mut(), &instance).map_err(bind)?;
    let wit = Notification {
        title: message.title.to_string(),
        body: message.body.to_string(),
        severity: wit_severity(message.severity),
        payload: message.payload.to_string(),
    };
    let limits = live.limits();
    let result = call_with_deadline(limits, guest.call_send_notification(live.armed(), &wit)).await;
    host_fns::end_call(live.store_mut().data_mut()).await;
    result
}

async fn deliver(dispatcher: &HookDispatcher<'_>, plugin: &str, message: PluginNotification<'_>) -> PluginDelivery {
    let Some((record, loaded)) = dispatcher.named(HOOK, plugin) else {
        // Tripped by its circuit breaker: the outbox holds the message, so it
        // goes out once the plugin is re-enabled or reloaded (within the
        // outbox's own attempt budget).
        if dispatcher.record(plugin).is_some_and(|record| record.state() == PluginState::Failed) {
            return PluginDelivery::Retry {
                reason: format!("plugin `{plugin}` is failed (circuit breaker open)"),
                after: None,
            };
        }
        // Disabled, removed or reloaded invalid since the outbox enqueued it.
        return PluginDelivery::Permanent { reason: format!("plugin `{plugin}` is not loaded") };
    };
    match call_one(dispatcher, loaded, &message).await {
        Ok(answer) => {
            // The plugin ran; a refusal is the provider's verdict, not the
            // plugin's fault, so it must not trip the circuit breaker.
            record.report(Ok(()));
            if let Err(err) = &answer {
                log::warn!("[plugin:{plugin}] {} refused: {err:?}", HOOK.as_str());
            }
            delivery_of(answer)
        }
        Err(err) => {
            record.report(Err(&err));
            log::error!("[plugin:{plugin}] {} failed: {err}", HOOK.as_str());
            PluginDelivery::Retry { reason: err.to_string(), after: None }
        }
    }
}

impl PluginNotifier for PluginSystem {
    fn notification_channels(&self) -> Vec<PluginNotificationChannel> {
        self.dispatcher()
            .runnable(HOOK)
            .map(|(record, loaded)| PluginNotificationChannel {
                plugin: record.id.clone(),
                routing: routing_of(&loaded.env.settings),
            })
            .collect()
    }

    fn send_notification<'a>(&'a self, plugin: &'a str, message: PluginNotification<'a>) -> PluginDeliveryFuture<'a> {
        Box::pin(async move { deliver(&self.dispatcher(), plugin, message).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_is_the_routing_of_the_messaging_extension() {
        let routing =
            routing_of(r#"{"mode":"ok","ext":{"messaging":{"routing":{"min_severity":"error"}}}}"#).expect("routing");
        assert_eq!(routing, serde_json::json!({"min_severity": "error"}));
    }

    #[test]
    fn missing_null_top_level_or_unparsable_settings_have_no_routing() {
        for settings in [
            r#"{"mode":"ok"}"#,
            r#"{"routing":{"min_severity":"error"}}"#,
            r#"{"ext":{"messaging":{"routing":null}}}"#,
            r#"{"ext":{"messaging":{}}}"#,
            "[]",
            "not json",
            "",
        ] {
            assert!(routing_of(settings).is_none(), "{settings}");
        }
    }

    #[test]
    fn a_guest_answer_maps_onto_a_delivery() {
        assert_eq!(delivery_of(Ok(())), PluginDelivery::Delivered);
        assert_eq!(
            delivery_of(Err(DeliveryError::Transient("busy".to_string()))),
            PluginDelivery::Retry { reason: "busy".to_string(), after: None }
        );
        assert_eq!(
            delivery_of(Err(DeliveryError::Permanent("bad token".to_string()))),
            PluginDelivery::Permanent { reason: "bad token".to_string() }
        );
        match delivery_of(Err(DeliveryError::RetryAfter(90))) {
            PluginDelivery::Retry { after, .. } => assert_eq!(after, Some(Duration::from_secs(90))),
            other => panic!("unexpected {other:?}"),
        }
    }
}
