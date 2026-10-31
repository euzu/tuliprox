use crate::app::components::Card;
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct KpiCardProps {
    pub label: String,
    pub value: String,
    #[prop_or_default]
    pub subtext: Option<String>,
    #[prop_or_default]
    pub subtext_title: Option<String>,
    pub color_var: &'static str,
}

#[component]
pub fn KpiCard(props: &KpiCardProps) -> Html {
    let dot_style = format!("background-color: var({});", props.color_var);
    html! {
        <Card class="tp__playlist-stats__kpi-card">
            <div class="tp__playlist-stats__kpi-card-top">
                <span>{&props.label}</span>
                <span class="tp__playlist-stats__kpi-card-dot" style={dot_style} />
            </div>
            <div class="tp__playlist-stats__kpi-card-value">
                {&props.value}
            </div>
            <div class="tp__playlist-stats__kpi-card-subtext">
                {
                    if let Some(ref subtext) = props.subtext {
                        if let Some(ref title) = props.subtext_title {
                            html! { <span title={title.clone()}>{subtext}</span> }
                        } else {
                            html! { <span>{subtext}</span> }
                        }
                    } else {
                        Html::default()
                    }
                }
            </div>
        </Card>
    }
}
