use std::f64::consts::PI;
use yew::prelude::*;

const RADIUS: f64 = 36.0;
const CIRCUMFERENCE: f64 = 2.0 * PI * RADIUS;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct EpgCoverageRingProps {
    #[prop_or_default]
    pub percent: Option<f64>,
}

#[component]
pub fn EpgCoverageRing(props: &EpgCoverageRingProps) -> Html {
    let (dash, text) = props.percent.map_or((0.0, "—".to_string()), |p| {
        let clamped = p.clamp(0.0, 100.0);
        ((clamped / 100.0) * CIRCUMFERENCE, format!("{clamped:.0}%"))
    });

    let stroke_dasharray = format!("{dash:.1} {CIRCUMFERENCE:.1}");

    html! {
        <div class="ring-container">
            <svg viewBox="0 0 100 100">
                <circle
                    cx="50"
                    cy="50"
                    r={RADIUS.to_string()}
                    fill="none"
                    stroke="var(--playlist-stats-track-color)"
                    stroke-width="8"
                />
                <circle
                    cx="50"
                    cy="50"
                    r={RADIUS.to_string()}
                    fill="none"
                    stroke="var(--playlist-stats-epg-color)"
                    stroke-width="8"
                    stroke-dasharray={stroke_dasharray}
                    stroke-linecap="round"
                    transform="rotate(-90 50 50)"
                />
                <text
                    x="50"
                    y="50"
                    text-anchor="middle"
                    dominant-baseline="central"
                    fill="var(--text-color)"
                    font-size="20"
                    font-weight="700"
                >
                    {text}
                </text>
            </svg>
        </div>
    }
}
