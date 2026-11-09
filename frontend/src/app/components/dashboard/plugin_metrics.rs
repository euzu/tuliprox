//! Plugin metrics as native entries of the stats cards: label, value, unit and a severity
//! accent, with a sparkline for numeric values. No plugin JavaScript is involved.

use crate::app::components::{
    resolve_text, use_plugin_metrics, use_plugin_ui, Card, Sparkline, SparklineFormat, SparklineSeries, StatusCard,
};
use shared::model::{PluginMetricDto, PluginMetricSeverityDto};
use std::rc::Rc;
use yew::prelude::*;

/// The card modifier of a severity, for the accent.
fn severity_class(severity: Option<PluginMetricSeverityDto>) -> &'static str {
    match severity {
        Some(PluginMetricSeverityDto::Info) => "tp__status-card--info",
        Some(PluginMetricSeverityDto::Warn) => "tp__status-card--warn",
        Some(PluginMetricSeverityDto::Error) => "tp__status-card--error",
        None => "",
    }
}

/// `value` followed by its unit, if it has one.
fn value_with_unit(metric: &PluginMetricDto) -> String {
    match metric.unit.as_deref().map(str::trim).filter(|unit| !unit.is_empty()) {
        Some(unit) => format!("{} {unit}", metric.value),
        None => metric.value.clone(),
    }
}

/// One card per plugin metric. Renders nothing while no plugin reports a metric.
#[component]
pub fn PluginMetricCards() -> Html {
    let ui = use_plugin_ui();
    let metrics = use_plugin_metrics();
    if metrics.latest.is_empty() {
        return html! {};
    }
    html! {
        <div class="tp__stats__body-group tp__stats__body-group-plugins">
            { for metrics.latest.iter().map(|metric| {
                let strings = ui.as_ref().map(|ui| ui.strings_for(&metric.plugin)).unwrap_or_default();
                let plugin_name = ui
                    .as_ref()
                    .and_then(|ui| ui.plugin(&metric.plugin))
                    .map_or_else(|| metric.plugin.clone(), |plugin| plugin.name.clone());
                let chart = metrics.history.series.get(&metric.key).map(|points| {
                    let values: Rc<[f64]> = points.iter().copied().collect::<Vec<_>>().into();
                    let series: Rc<[SparklineSeries]> = Rc::from([SparklineSeries::new(values)]);
                    html! { <Sparkline class="tp__sparkline--plugin" format={SparklineFormat::Plain} series={series} /> }
                });
                html! {
                    <Card key={metric.key.clone()}>
                        <StatusCard
                            title={resolve_text(&strings, &metric.label)}
                            data={value_with_unit(metric)}
                            footer={plugin_name}
                            classname={severity_class(metric.severity)}
                            chart={chart}
                        />
                    </Card>
                }
            }) }
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metric(unit: Option<&str>) -> PluginMetricDto {
        PluginMetricDto {
            key: "demo.queue".to_string(),
            plugin: "demo".to_string(),
            label: "Queue".to_string(),
            value: "12".to_string(),
            unit: unit.map(ToString::to_string),
            severity: None,
        }
    }

    #[test]
    fn the_unit_follows_the_value() {
        assert_eq!(value_with_unit(&metric(Some("ms"))), "12 ms");
        assert_eq!(value_with_unit(&metric(Some("  "))), "12");
        assert_eq!(value_with_unit(&metric(None)), "12");
    }

    #[test]
    fn every_severity_has_its_own_accent() {
        assert_eq!(severity_class(None), "");
        assert_eq!(severity_class(Some(PluginMetricSeverityDto::Info)), "tp__status-card--info");
        assert_eq!(severity_class(Some(PluginMetricSeverityDto::Warn)), "tp__status-card--warn");
        assert_eq!(severity_class(Some(PluginMetricSeverityDto::Error)), "tp__status-card--error");
    }
}
