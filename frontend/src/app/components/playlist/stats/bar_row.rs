use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct BarRowProps {
    pub label: String,
    pub meta: Html,
    #[prop_or_default]
    pub percent: Option<f64>,
    pub color_var: &'static str,
}

#[component]
pub fn BarRow(props: &BarRowProps) -> Html {
    let pct = props.percent.unwrap_or(0.0).clamp(0.0, 100.0);
    let fill_style = format!("width: {pct:.1}%; background-color: var({});", props.color_var);

    html! {
        <div class="tp__playlist-stats__bar-row">
            <div class="tp__playlist-stats__bar-row-meta">
                <span class="label">{&props.label}</span>
                <span class="counts">{props.meta.clone()}</span>
            </div>
            <div class="tp__playlist-stats__bar-row-track">
                <div class="tp__playlist-stats__bar-row-fill" style={fill_style} />
            </div>
        </div>
    }
}
