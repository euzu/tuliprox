use crate::{
    app::components::{source_editor::editor_model::BlockType, CollapsePanel, IconButton},
    i18n::use_translation,
};
use yew::prelude::*;

pub const BLOCK_TYPES_INPUT: [BlockType; 3] = [BlockType::InputXtream, BlockType::InputM3u, BlockType::InputLibrary];

pub const BLOCK_TYPES_TARGET: [BlockType; 1] = [BlockType::Target];

pub const BLOCK_TYPES_OUTPUT: [BlockType; 4] =
    [BlockType::OutputM3u, BlockType::OutputXtream, BlockType::OutputHdHomeRun, BlockType::OutputStrm];

fn create_brick(t: &BlockType, on_drag_start: Callback<DragEvent>, label: String) -> Html {
    html! {
        <div class={format!("tp__source-editor__brick tp__source-editor__brick-{t}")}
        draggable={"true"}
        data-block-type={t.to_string()}
        ondragstart={on_drag_start}>

            { label }
        </div>
    }
}

#[derive(Properties, PartialEq)]
pub struct SourceEditorSidebarProps {
    #[prop_or_default]
    pub delete_mode: bool,
    #[prop_or_default]
    pub on_toggle_delete: Callback<(String, MouseEvent)>,
    #[prop_or_default]
    pub on_drag_start: Callback<DragEvent>,
    #[prop_or_default]
    pub on_layout: Callback<(String, MouseEvent)>,
}

#[component]
pub fn SourceEditorSidebar(props: &SourceEditorSidebarProps) -> Html {
    let translate = use_translation();

    html! {
        // Sidebar
        <div class="tp__source-editor__sidebar">
            <div class="tp__source-editor__sidebar-actions">
                <IconButton name="layout" icon="Nodes" onclick={props.on_layout.clone()} />
                // Delete mode toggle button
                <IconButton class={if props.delete_mode {"tp__source-editor__sidebar-actions-active"} else {""} } name="toggle_delete" icon="Delete" onclick={props.on_toggle_delete.clone()} />
            </div>
            <div class="tp__source-editor__sidebar-bricks">
                <CollapsePanel title={translate.t("LABEL.INPUTS")}>
                    <div class="tp__source-editor__sidebar-bricks-group">
                        {for BLOCK_TYPES_INPUT.iter().map(|t| create_brick(t, props.on_drag_start.clone(), translate.t(&format!("SOURCE_EDITOR.BRICK_{t}"))))}
                    </div>
                </CollapsePanel>
                <CollapsePanel title={translate.t("LABEL.TARGETS")}>
                    <div class="tp__source-editor__sidebar-bricks-group">
                        {for BLOCK_TYPES_TARGET.iter().map(|t| create_brick(t, props.on_drag_start.clone(), translate.t(&format!("SOURCE_EDITOR.BRICK_{t}"))))}
                    </div>
                </CollapsePanel>
                <CollapsePanel title={translate.t("LABEL.OUTPUT")}>
                     <div class="tp__source-editor__sidebar-bricks-group">
                        {for BLOCK_TYPES_OUTPUT.iter().map(|t| create_brick(t, props.on_drag_start.clone(), translate.t(&format!("SOURCE_EDITOR.BRICK_{t}"))))}
                     </div>
                </CollapsePanel>
            </div>
        </div>
    }
}
