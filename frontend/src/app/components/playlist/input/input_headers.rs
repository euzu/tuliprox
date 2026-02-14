use std::collections::HashMap;
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct InputHeadersProps {
    pub headers: HashMap<String, String>,
}

#[function_component]
pub fn InputHeaders(props: &InputHeadersProps) -> Html {
    if props.headers.is_empty() {
        html! {}
    } else {
        html! {
            <div class="tp__input-headers">
                <ul>
                    { props.headers.iter().map(|(key, value)| html! { <li>{ key } {":"} {value}</li> }).collect::<Html>() }
                </ul>
            </div>
        }
    }
}
