use shared::model::ConfigTargetDto;
use std::rc::Rc;
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct TargetWatchProps {
    pub target: Rc<ConfigTargetDto>,
}

#[component]
pub fn TargetWatch(props: &TargetWatchProps) -> Html {
    match props.target.watch.as_ref() {
        None => html! {},
        Some(watch) => html! {
            <div class="tp__target-watch">
                <ul>
                    for item in watch.iter() {
                        <li key={item.clone()}>{ item }</li>
                    }
                </ul>
            </div>
        },
    }
}
