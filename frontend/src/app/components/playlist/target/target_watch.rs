use crate::{app::components::source_editor::watch_entry_label, i18n::use_translation};
use shared::model::ConfigTargetDto;
use std::rc::Rc;
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct TargetWatchProps {
    pub target: Rc<ConfigTargetDto>,
}

#[component]
pub fn TargetWatch(props: &TargetWatchProps) -> Html {
    let translate = use_translation();
    match props.target.watch.as_ref() {
        None => html! {},
        Some(watch) => html! {
            <div class="tp__target-watch">
                <ul>
                    for entry in watch.iter() {
                        <li key={entry.pattern.clone()}>{ watch_entry_label(entry, |key| translate.t(key)) }</li>
                    }
                </ul>
            </div>
        },
    }
}
