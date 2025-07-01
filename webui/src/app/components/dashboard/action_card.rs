use yew::prelude::*;
use crate::app::components::AppIcon;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct ActionProps {
    #[prop_or_default]
    pub icon: String,
    #[prop_or_default]
    pub title: String,
    #[prop_or_default]
    pub subtitle: String,
    #[prop_or_default]
    pub subtitle_html: String,
    #[prop_or_default]
    pub onaction: Callback<()>,
    pub children: Children,
    #[prop_or_default]
    pub classname: String,
}

#[function_component]
pub fn ActionCard(props: &ActionProps) -> Html {

    html! {
        <div class={if props.classname.is_empty() {"action-card".to_string()} else {format!("action-card {}", props.classname)}}>
            <div class="action-card__icon">
                <AppIcon name={props.icon.clone()} />
            </div>
            <div class="action-card__body">
                <span class="action-card__title">
                    {props.title.clone()}
                </span>
                <span class="action-card__content">
                    { props.subtitle.clone() }
                    {
                        if !props.subtitle_html.is_empty() {
                            Html::from_html_unchecked(AttrValue::from((*props.subtitle_html).to_string()))
                        } else {
                            Html::default()
                        }
                    }
                </span>
            </div>
            {for props.children.iter() }
        </div>
    }
}