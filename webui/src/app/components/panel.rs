use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct PanelProps {
    pub value: String,
    pub active: String,
    pub children: Children,
    #[prop_or_default]
    pub class: String,
}

#[function_component]
pub fn Panel(props: &PanelProps) -> Html {
    html! {
        <div class={classes!("tp__panel", props.class.to_string(), if props.value == props.active {""} else {"tp__hidden"} )}>
            { for props.children.iter() }
        </div>
    }
}