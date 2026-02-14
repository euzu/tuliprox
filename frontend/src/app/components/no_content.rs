use crate::app::components::AppIcon;
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct NoContentProps {
    #[prop_or_default]
    pub class: String,
}

#[function_component]
pub fn NoContent(props: &NoContentProps) -> Html {
    html! {
        <div class={classes!("tp__no_content", props.class.to_string())}>
            <div class="tp__no_content__indicator">
               <AppIcon name="Clear" />
            </div>
        </div>
    }
}
