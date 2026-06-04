use crate::app::components::{AppIcon, TextButton};
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct NoContentProps {
    #[prop_or_default]
    pub class: String,
    #[prop_or_default]
    pub icon: String,
    #[prop_or_default]
    pub text: String,
    #[prop_or_default]
    pub hint: String,
    #[prop_or_default]
    pub action_label: String,
    #[prop_or_default]
    pub action_icon: String,
    #[prop_or_default]
    pub onaction: Option<Callback<()>>,
}

#[component]
pub fn NoContent(props: &NoContentProps) -> Html {
    let icon = if props.icon.is_empty() { "Clear" } else { props.icon.as_str() };

    let action = match (&props.onaction, props.action_label.is_empty()) {
        (Some(cb), false) => {
            let cb = cb.clone();
            let onclick = Callback::from(move |_: String| cb.emit(()));
            html! {
                <TextButton
                    class="primary tp__no_content__action"
                    name="no-content-action"
                    icon={props.action_icon.clone()}
                    title={props.action_label.clone()}
                    onclick={onclick}
                />
            }
        }
        _ => Html::default(),
    };

    html! {
        <div class={classes!("tp__no_content", props.class.to_string())} role="status">
            <div class="tp__no_content__indicator">
               <AppIcon name={icon.to_string()} />
            </div>
            if !props.text.is_empty() {
                <p class="tp__no_content__text">{&props.text}</p>
            }
            if !props.hint.is_empty() {
                <p class="tp__no_content__hint">{&props.hint}</p>
            }
            { action }
        </div>
    }
}
