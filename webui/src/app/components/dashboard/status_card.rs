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
}

#[function_component]
pub fn StatusCard(props: &StatusCardProps) -> Html {

    html! {
        <div class={if props.classname.is_empty() {"status-card".to_string()} else {format!("status-card {}", props.classname)}}>
            <span class="status-card__title">
                {props.title.clone()}
            </span>
            <div class="status-card__body">
                {props.data.clone()}
            </div>
            <div class="status-card__footer">
                {props.footer.clone()}
            </div>
        </div>
    }
}