use shared::model::ConfigTargetDto;
use std::rc::Rc;
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct TargetWatchProps {
    pub target: Rc<ConfigTargetDto>,
}

#[function_component]
pub fn TargetWatch(props: &TargetWatchProps) -> Html {
    match props.target.watch.as_ref() {
        None => html! {},
        Some(watch) => html! {
            <div class="tp__target-watch">
                <ul>
                    { watch.iter().map(|item| html! { <li>{ item }</li> }).collect::<Html>() }
                </ul>
            </div>
        },
    }
}
