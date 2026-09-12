use crate::app::components::{Tag, TagList};
use shared::model::ProcessingOrder;
use std::rc::Rc;
use yew::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PlaylistProcessingStage {
    Filter,
    Rename,
    Mapping,
}

impl PlaylistProcessingStage {
    const fn short_label(self) -> &'static str {
        match self {
            Self::Filter => "F",
            Self::Rename => "R",
            Self::Mapping => "M",
        }
    }
}

pub(super) const fn processing_stages(order: ProcessingOrder) -> [PlaylistProcessingStage; 3] {
    use PlaylistProcessingStage::{Filter, Mapping, Rename};

    match order {
        ProcessingOrder::Frm => [Filter, Rename, Mapping],
        ProcessingOrder::Fmr => [Filter, Mapping, Rename],
        ProcessingOrder::Rfm => [Rename, Filter, Mapping],
        ProcessingOrder::Rmf => [Rename, Mapping, Filter],
        ProcessingOrder::Mfr => [Mapping, Filter, Rename],
        ProcessingOrder::Mrf => [Mapping, Rename, Filter],
    }
}

#[derive(Properties, PartialEq, Clone)]
pub struct PlaylistProcessingProps {
    pub order: ProcessingOrder,
}

#[component]
pub fn PlaylistProcessing(props: &PlaylistProcessingProps) -> Html {
    let tags = use_memo(props.order, |order| {
        processing_stages(*order)
            .iter()
            .map(|stage| Rc::new(Tag { label: stage.short_label().to_string(), class: None }))
            .collect::<Vec<Rc<Tag>>>()
    });

    html! {
      <div class="tp__playlist-processing">
        <TagList tags={(*tags).clone()} />
      </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use PlaylistProcessingStage::{Filter, Mapping, Rename};

    #[test]
    fn pipeline_transparency_target_processing_stages_follow_every_configured_order() {
        assert_eq!(processing_stages(ProcessingOrder::Frm), [Filter, Rename, Mapping]);
        assert_eq!(processing_stages(ProcessingOrder::Fmr), [Filter, Mapping, Rename]);
        assert_eq!(processing_stages(ProcessingOrder::Rfm), [Rename, Filter, Mapping]);
        assert_eq!(processing_stages(ProcessingOrder::Rmf), [Rename, Mapping, Filter]);
        assert_eq!(processing_stages(ProcessingOrder::Mfr), [Mapping, Filter, Rename]);
        assert_eq!(processing_stages(ProcessingOrder::Mrf), [Mapping, Rename, Filter]);
    }
}
