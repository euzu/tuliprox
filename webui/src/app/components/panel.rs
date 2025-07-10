use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct PanelProps {
    pub value: String,
    pub active: String,
    pub children: Children,
}

#[function_component]
pub fn Panel(props: &PanelProps) -> Html {
    html! {
        <div class={classes!("tp__panel", if props.value == props.active {""} else {"tp__hidden"} )}>
            { for props.children.iter() }
        </div>
    }
}