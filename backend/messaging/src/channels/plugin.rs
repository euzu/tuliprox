//! A notification channel backed by a wasm plugin.
//!
//! The plugin host lives behind [`shared::model::PluginNotifier`], an object
//! safe trait `app` registers with [`super::set_plugin_notifier`], so this
//! crate names only `shared` types. Routing, quiet hours, suppression and the
//! outbox apply to a plugin channel exactly as to a built-in one.

use super::DEFAULT_SEND_TIMEOUT;
use crate::channel::{Delivery, NotificationChannel, RenderedMessage};
use log::warn;
use shared::model::{
    notification::{EventId, Severity},
    ChannelRoutingDto, PluginDelivery, PluginNotification, PluginNotificationChannel, PluginNotifier, PluginSeverity,
};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex, OnceLock, PoisonError},
};
use tuliprox_core::model::ChannelRouting;

/// Wire-id prefix of a plugin channel: `plugin.<plugin id>`.
const ID_PREFIX: &str = "plugin.";

/// The interned wire ids. `NotificationChannel::id` is `&'static str`, so each
/// distinct plugin id is leaked once; the set is bounded by the distinct plugin
/// ids ever seen, never by the number of channel-set builds.
static IDS: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();

/// `plugin.<plugin>` as a `&'static str`, the same pointer on every call.
fn intern(plugin: &str) -> &'static str {
    let wire = format!("{ID_PREFIX}{plugin}");
    let mut ids = IDS.get_or_init(|| Mutex::new(HashSet::new())).lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(known) = ids.get(wire.as_str()) {
        return known;
    }
    let leaked: &'static str = Box::leak(wire.into_boxed_str());
    ids.insert(leaked);
    leaked
}

/// The routing of a plugin channel, parsed like a built-in channel's: absent
/// means the defaults, an invalid block means the defaults and one warning.
fn parse_routing(plugin: &str, routing: Option<&serde_json::Value>) -> ChannelRouting {
    let Some(routing) = routing else {
        return ChannelRouting::default();
    };
    match serde_json::from_value::<ChannelRoutingDto>(routing.clone()) {
        Ok(dto) => ChannelRouting::from(&dto),
        Err(err) => {
            warn!("Ignoring the invalid `routing` of the notification plugin `{plugin}`: {err}");
            ChannelRouting::default()
        }
    }
}

/// A plugin has no `critical`: it is delivered as `error`.
const fn plugin_severity(severity: Severity) -> PluginSeverity {
    match severity {
        Severity::Info => PluginSeverity::Info,
        Severity::Warn => PluginSeverity::Warn,
        Severity::Error | Severity::Critical => PluginSeverity::Error,
    }
}

/// 1:1, including the provider's `retry-after` delay.
pub(crate) fn delivery_of(delivery: PluginDelivery) -> Delivery {
    match delivery {
        PluginDelivery::Delivered => Delivery::Delivered,
        PluginDelivery::Retry { reason, after } => Delivery::Retry { reason, after },
        PluginDelivery::Permanent { reason } => Delivery::Permanent { reason },
    }
}

/// The plugin channels the notifier declares now, in its order.
pub(crate) fn channels_of(notifier: &Arc<dyn PluginNotifier>) -> Vec<PluginChannel> {
    notifier
        .notification_channels()
        .into_iter()
        .map(|declared| PluginChannel::new(declared, Arc::clone(notifier)))
        .collect()
}

#[derive(Clone)]
pub struct PluginChannel {
    plugin: Arc<str>,
    id: &'static str,
    routing: ChannelRouting,
    notifier: Arc<dyn PluginNotifier>,
}

impl PluginChannel {
    fn new(declared: PluginNotificationChannel, notifier: Arc<dyn PluginNotifier>) -> Self {
        let routing = parse_routing(&declared.plugin, declared.routing.as_ref());
        Self { id: intern(&declared.plugin), plugin: declared.plugin, routing, notifier }
    }
}

impl NotificationChannel for PluginChannel {
    fn id(&self) -> &'static str { self.id }

    fn routing(&self) -> &ChannelRouting { &self.routing }

    fn wants(&self, event: EventId, severity: Severity) -> bool { self.routing.accepts(event, severity) }

    async fn send(&self, msg: &RenderedMessage<'_>) -> Delivery {
        let payload = serde_json::to_string(msg.event).unwrap_or_default();
        let notification = PluginNotification {
            title: &msg.event.title,
            body: &msg.body,
            severity: plugin_severity(msg.event.severity),
            payload: &payload,
        };
        match tokio::time::timeout(DEFAULT_SEND_TIMEOUT, self.notifier.send_notification(&self.plugin, notification))
            .await
        {
            Ok(delivery) => delivery_of(delivery),
            Err(_) => {
                Delivery::retry(format!("plugin `{}` did not answer within {DEFAULT_SEND_TIMEOUT:?}", self.plugin))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::PluginDeliveryFuture;
    use std::time::Duration;

    struct Fake(Vec<PluginNotificationChannel>);

    impl PluginNotifier for Fake {
        fn notification_channels(&self) -> Vec<PluginNotificationChannel> { self.0.clone() }

        fn send_notification<'a>(&'a self, _: &'a str, _: PluginNotification<'a>) -> PluginDeliveryFuture<'a> {
            Box::pin(async { PluginDelivery::Delivered })
        }
    }

    fn declared(plugin: &str, routing: Option<serde_json::Value>) -> PluginNotificationChannel {
        PluginNotificationChannel { plugin: Arc::from(plugin), routing }
    }

    fn notifier(channels: Vec<PluginNotificationChannel>) -> Arc<dyn PluginNotifier> { Arc::new(Fake(channels)) }

    #[test]
    fn the_wire_id_is_interned_once_per_plugin_across_builds() {
        let notifier = notifier(vec![declared("notify-intern", None), declared("notify-intern-b", None)]);
        let first = channels_of(&notifier);
        let second = channels_of(&notifier);
        assert_eq!(first[0].id(), "plugin.notify-intern");
        assert_eq!(first[1].id(), "plugin.notify-intern-b");
        // Same allocation, not just an equal string: a rebuild leaks nothing.
        assert!(std::ptr::eq(first[0].id(), second[0].id()));
        assert!(std::ptr::eq(first[1].id(), second[1].id()));
        assert!(!std::ptr::eq(first[0].id(), first[1].id()));
    }

    #[test]
    fn a_guest_delivery_maps_one_to_one() {
        let after = Some(Duration::from_secs(7));
        assert_eq!(delivery_of(PluginDelivery::Delivered), Delivery::Delivered);
        assert_eq!(
            delivery_of(PluginDelivery::Retry { reason: "r".to_string(), after }),
            Delivery::Retry { reason: "r".to_string(), after }
        );
        assert_eq!(
            delivery_of(PluginDelivery::Retry { reason: "r".to_string(), after: None }),
            Delivery::Retry { reason: "r".to_string(), after: None }
        );
        assert_eq!(
            delivery_of(PluginDelivery::Permanent { reason: "p".to_string() }),
            Delivery::Permanent { reason: "p".to_string() }
        );
    }

    #[test]
    fn critical_is_delivered_as_error() {
        assert_eq!(plugin_severity(Severity::Info), PluginSeverity::Info);
        assert_eq!(plugin_severity(Severity::Warn), PluginSeverity::Warn);
        assert_eq!(plugin_severity(Severity::Error), PluginSeverity::Error);
        assert_eq!(plugin_severity(Severity::Critical), PluginSeverity::Error);
    }

    #[test]
    fn absent_or_invalid_routing_falls_back_to_the_defaults() {
        let absent = parse_routing("p", None);
        assert!(absent.subscription.is_none() && absent.min_severity.is_none() && absent.max_per_hour.is_none());
        let invalid = parse_routing("p", Some(&serde_json::json!({"min_severity": 3, "bogus": true})));
        assert!(invalid.subscription.is_none() && invalid.min_severity.is_none() && invalid.max_per_hour.is_none());
    }

    #[test]
    fn declared_routing_is_parsed_like_a_built_in_channels() {
        let routing = parse_routing("p", Some(&serde_json::json!({"min_severity": "error", "max_per_hour": 5})));
        assert_eq!(routing.min_severity, Some(Severity::Error));
        assert_eq!(routing.max_per_hour, Some(5));
    }

    #[test]
    fn the_channel_has_no_templates_and_applies_its_severity_floor() {
        let notifier = notifier(vec![declared("notify-floor", Some(serde_json::json!({"min_severity": "error"})))]);
        let channel = channels_of(&notifier).remove(0);
        let event = shared::model::notification::registry::SYSTEM_INFO;
        assert!(!channel.wants(event, Severity::Warn));
        assert!(channel.wants(event, Severity::Error));
    }
}
