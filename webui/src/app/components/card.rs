use yew::prelude::*;
use crate::app::CardContext;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct CardProps {
    pub children: Children,
}

#[function_component]
pub fn Card(props: &CardProps) -> Html {
    let custom_class = use_state(|| String::new());
    let context = CardContext {
        custom_class: custom_class.clone(),
    };
    html! {
        <ContextProvider<CardContext> context={context}>
            <div class={classes!("card", &*custom_class)}>
                { for props.children.iter() }
            </div>
        </ContextProvider<CardContext>>
    }
}