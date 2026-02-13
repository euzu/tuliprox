use crate::app::components::{
    BlockId, BlockType, EditMode, HdHomeRunTargetOutputView, M3uTargetOutputView,
    SourceEditorContext, StrmTargetOutputView, XtreamTargetOutputView,
};
use shared::model::TargetOutputDto;
use std::rc::Rc;
use yew::{function_component, html, use_context, Html, Properties};

#[derive(Properties, PartialEq, Clone)]
pub struct ConfigOutputViewProps {
    pub(crate) block_id: BlockId,
    pub(crate) output: Option<Rc<TargetOutputDto>>,
}

#[function_component]
pub fn ConfigOutputView(props: &ConfigOutputViewProps) -> Html {
    let source_editor_ctx =
        use_context::<SourceEditorContext>().expect("SourceEditorContext not found");

    let block_id = props.block_id;

    match &*source_editor_ctx.edit_mode {
        EditMode::Active(block_instance) => match block_instance.block_type {
            BlockType::InputXtream
            | BlockType::InputM3u
            | BlockType::InputLibrary
            | BlockType::Target => html! {},
            BlockType::OutputM3u => {
                let output = props.output.as_ref().and_then(|to| {
                    if let TargetOutputDto::M3u(m3u) = &**to {
                        Some(Rc::new(m3u.clone()))
                    } else {
                        None
                    }
                });

                html! { <M3uTargetOutputView block_id={block_id} output={output} /> }
            }
            BlockType::OutputXtream => {
                let output = props.output.as_ref().and_then(|to| {
                    if let TargetOutputDto::Xtream(xtream) = &**to {
                        Some(Rc::new(xtream.clone()))
                    } else {
                        None
                    }
                });

                html! { <XtreamTargetOutputView block_id={block_id} output={output} /> }
            }
            BlockType::OutputHdHomeRun => {
                let output = props.output.as_ref().and_then(|to| {
                    if let TargetOutputDto::HdHomeRun(hdhomerun) = &**to {
                        Some(Rc::new(hdhomerun.clone()))
                    } else {
                        None
                    }
                });

                html! { <HdHomeRunTargetOutputView block_id={block_id} output={output} /> }
            }
            BlockType::OutputStrm => {
                let output = props.output.as_ref().and_then(|to| {
                    if let TargetOutputDto::Strm(strm) = &**to {
                        Some(Rc::new(strm.clone()))
                    } else {
                        None
                    }
                });

                html! { <StrmTargetOutputView block_id={block_id} output={output} /> }
            }
        },
        EditMode::Inactive => html! {},
    }
}
