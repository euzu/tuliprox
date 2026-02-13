use crate::app::components::{
    BlockInstance, ConfigInputView, ConfigOutputView, ConfigTargetView, EditMode,
    SourceEditorContext,
};
use yew::{classes, function_component, html, use_context, use_effect_with, use_state, Html};

#[function_component]
pub fn SourceEditorForm() -> Html {
    let source_editor_ctx =
        use_context::<SourceEditorContext>().expect("SourceEditorContext not found");
    let visible = use_state(|| false);

    {
        let visible_set = visible.clone();
        use_effect_with(
            source_editor_ctx.edit_mode.clone(),
            move |edit_mode| match **edit_mode {
                EditMode::Inactive => {
                    visible_set.set(false);
                }
                EditMode::Active(_) => {
                    visible_set.set(true);
                }
            },
        );
    }

    html! {
        <div class={classes!("tp__source-editor-form-wrapper", if *visible { "active" } else { "" }) }>
            {
                if let EditMode::Active(block) = &*source_editor_ctx.edit_mode {
                    match &block.instance {
                        BlockInstance::Input(input) => html! { <ConfigInputView block_id={block.id} input={Some(input.clone())}></ConfigInputView> },
                        BlockInstance::Target(target) => html! { <ConfigTargetView block_id={block.id} target={Some(target.clone())}></ConfigTargetView> },
                        BlockInstance::Output(output) => html! { <ConfigOutputView block_id={block.id} output={Some(output.clone())}></ConfigOutputView> },
                    }
                } else {
                    html!{}
                }
            }
        </div>
    }
}
