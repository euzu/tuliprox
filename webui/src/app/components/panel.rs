use yew::{function_component, html, Children, Html, Properties};

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct PanelProps {
    pub value: String,
    pub active: String,
    pub children: Children,
}

#[function_component]
pub fn Panel(props: &PanelProps) -> Html {
    html! {
        <div class={format!("panel{}", if props.value == props.active {""} else {" hidden"} ).to_lowercase()}>
            { for props.children.iter() }
        </div>
    }
}