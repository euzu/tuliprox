use std::rc::Rc;
use yew::prelude::*;
use shared::model::ProcessingOrder;
use crate::app::components::{Tag, TagList};

#[derive(Properties, PartialEq, Clone)]
pub struct PlaylistProcessingProps {
  pub order: ProcessingOrder,
}

#[function_component]
pub fn PlaylistProcessing(props: &PlaylistProcessingProps) -> Html {
    let tags = use_memo(props.order, |order| {
        let text = match order {
            ProcessingOrder::Frm => vec!["F", "R", "M"],
            ProcessingOrder::Fmr => vec!["F", "M", "R"],
            ProcessingOrder::Rfm => vec!["R", "F", "M"],
            ProcessingOrder::Rmf => vec!["R", "M", "F"],
            ProcessingOrder::Mfr => vec!["M", "F", "R"],
            ProcessingOrder::Mrf => vec!["M", "R", "F"],
        };
        text.iter().map(|s| Rc::new(Tag { label: s.to_string(), class: None })).collect::<Vec<Rc<Tag>>>()
    });

    html! {
      <div class="tp__playlist-processing">
        <TagList tags={(*tags).clone()} />
      </div>
    }
}