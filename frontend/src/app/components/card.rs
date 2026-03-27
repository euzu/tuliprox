use crate::app::CardContext;
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct CardProps {
    #[prop_or_default]
    pub class: Classes,
    pub children: Children,
}

#[component]
pub fn Card(props: &CardProps) -> Html {
    let custom_class = use_state(String::new);
    let context = CardContext { custom_class: custom_class.clone() };
    html! {
        <ContextProvider<CardContext> context={context}>
            <div class={classes!("tp__card", props.class.clone(), &*custom_class)}>
                { for props.children.iter() }
            </div>
        </ContextProvider<CardContext>>
    }
}
