use std::sync::Arc;
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct PanelProps {
    pub value: Arc<str>,
    pub active: Arc<str>,
    pub children: Children,
    #[prop_or_default]
    pub class: String,
}

#[component]
pub fn Panel(props: &PanelProps) -> Html {
    html! {
        <div class={classes!("tp__panel", &props.class, if props.value == props.active {""} else {"tp__hidden"} )}>
            { for props.children.iter() }
        </div>
    }
}
