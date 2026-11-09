//! Dashboard metrics for `collect-metrics`.

use crate::wit::{metrics_hooks::Metric, status::Severity};

/// One dashboard value. The host namespaces `key` as `<plugin-id>.<key>`;
/// `label` is display text or a `ui/locales` key; `value` is preformatted.
#[must_use]
pub fn metric(key: &str, label: &str, value: String, unit: Option<&str>, severity: Option<Severity>) -> Metric {
    Metric { key: key.to_string(), label: label.to_string(), value, unit: unit.map(str::to_string), severity }
}

/// Builds the `Vec<Metric>` of a `collect_metrics` hook:
///
/// ```ignore
/// fn collect_metrics() -> Result<Vec<Metric>, String> {
///     Ok(metrics! {
///         "matched", "Matched channels" => count;
///         "latency", "API latency" => ms, unit = "ms";
///         "errors", "Errors" => errors, severity = Error;
///         "quota", "Quota left" => left, unit = "req", severity = Warn;
///     })
/// }
/// ```
///
/// Values are anything `ToString`; `severity` is `Info`, `Warn` or `Error`.
#[macro_export]
macro_rules! metrics {
    (@opt) => { ::core::option::Option::None::<&str> };
    (@opt $unit:expr) => { ::core::option::Option::Some($unit) };
    (@severity) => { ::core::option::Option::None };
    (@severity $severity:ident) => { ::core::option::Option::Some($crate::wit::status::Severity::$severity) };
    ($($key:expr, $label:expr => $value:expr $(, unit = $unit:expr)? $(, severity = $severity:ident)? ;)*) => {
        ::std::vec![$(
            $crate::metrics::metric(
                $key,
                $label,
                ::std::string::ToString::to_string(&$value),
                $crate::metrics!(@opt $($unit)?),
                $crate::metrics!(@severity $($severity)?),
            )
        ),*]
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_macro_builds_metrics_with_optional_unit_and_severity() {
        let rows = metrics! {
            "matched", "Matched" => 42;
            "latency", "Latency" => 1.5, unit = "ms";
            "errors", "Errors" => 3, severity = Error;
            "quota", "Quota" => "9", unit = "req", severity = Warn;
        };
        assert_eq!(rows.len(), 4);
        assert_eq!((rows[0].key.as_str(), rows[0].value.as_str(), rows[0].unit.clone()), ("matched", "42", None));
        assert_eq!(rows[1].unit.as_deref(), Some("ms"));
        assert_eq!(rows[2].severity, Some(Severity::Error));
        assert_eq!((rows[3].unit.as_deref(), rows[3].severity), (Some("req"), Some(Severity::Warn)));
        let none: Vec<Metric> = metrics!();
        assert!(none.is_empty());
    }
}
