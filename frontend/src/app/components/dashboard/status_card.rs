use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct StatusCardProps {
    #[prop_or_default]
    pub title: String,
    #[prop_or_default]
    pub data: String,
    #[prop_or_default]
    pub footer: String,
    #[prop_or_default]
    pub classname: String,
    #[prop_or_default]
    pub chart: Option<Html>,
}

#[component]
pub fn StatusCard(props: &StatusCardProps) -> Html {
    html! {
        <div class={classes!("tp__status-card", if props.classname.is_empty() { String::new() } else {props.classname.to_string()})}>
            <span class="tp__status-card__title">
                {props.title.clone()}
            </span>
            <div class="tp__status-card__body">
                {props.data.clone()}
            </div>
            {
                if let Some(chart) = props.chart.clone() {
                    html! { <div class="tp__status-card__chart">{ chart }</div> }
                } else {
                    Html::default()
                }
            }
            <div class="tp__status-card__footer">
                {props.footer.clone()}
            </div>
        </div>
    }
}
