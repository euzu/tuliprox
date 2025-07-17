use shared::model::{ConfigInputDto};
use std::rc::Rc;
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct InputHeadersProps {
    pub input: Rc<ConfigInputDto>,
}

#[function_component]
pub fn InputHeaders(props: &InputHeadersProps) -> Html {
    if props.input.headers.is_empty() {
        html! {}
    } else {
       html! {
            <div class="tp__input-headers">
                <ul>
                    { props.input.headers.iter().map(|(key, value)| html! { <li>{ key } {":"} {value}</li> }).collect::<Html>() }
                </ul>
            </div>
        }
    }
}