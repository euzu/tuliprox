use web_sys::MouseEvent;
use yew::{classes, function_component, html, Callback, Html, Properties, TargetCast};
use yew_i18n::use_translation;
use crate::html_if;
use crate::app::components::{Block, BlockId, BlockInstance, PortStatus};
#[derive(Properties, PartialEq)]
pub struct BlockProps {
    pub(crate) block: Block,
    pub(crate) edited:bool,
    pub(crate) selected:bool,
    pub(crate) delete_mode: bool,
    pub(crate) delete_block: Callback<BlockId>,
    pub(crate) port_status: PortStatus,
    pub(crate) on_edit: Callback<BlockId>,
    pub(crate) on_mouse_down: Callback<(BlockId, MouseEvent)>,
    pub(crate) on_connection_drop: Callback<BlockId>, // to_id
    pub(crate) on_connection_start:  Callback<BlockId>, // from_id
}

#[function_component]
pub fn BlockView(props: &BlockProps) -> Html {

    let translate = use_translation();

    let delete_mode = props.delete_mode;
    let delete_block = props.delete_block.clone();
    let block = &props.block;
    let port_status = props.port_status;

    let block_id = block.id;
    let block_type = block.block_type;
    let style = format!("transform: translate({}px, {}px)", block.position.0, block.position.1);
    let from_id = block_id;
    let to_id = block_id;

    let is_target = block_type.is_target();
    let is_input = !is_target && block_type.is_input();
    let is_output =  !is_input && !is_target;

    let port_style = match port_status {
        PortStatus::Valid =>  "tp__source-editor__block-port--valid",
        PortStatus::Invalid =>  "tp__source-editor__block-port--invalid",
        _ => "",
    };

    let handle_mouse_down = {
        let on_block_mouse_down = props.on_mouse_down.clone();
        Callback::from(move |e: MouseEvent| {
            e.prevent_default();
            if let Some(target) = e.target_dyn_into::<web_sys::Element>() {
                let tag = target.tag_name().to_lowercase();
                if &tag == "span" {
                    return;
                }
            }
            e.stop_propagation();
            on_block_mouse_down.emit((block_id, e))
        })
    };

    let handle_edit = {
        let on_edit = props.on_edit.clone();
        Callback::from(move |_| {
           on_edit.emit(block_id)
        })
    };

    let (title, show_type, is_batch) = {
        let (dto_title, show_type, is_batch) = match &block.instance {
            BlockInstance::Input(dto) => {
                dto.aliases.as_ref().map_or(
                    (dto.name.to_string(), true, false),
                    |a| {
                        if a.is_empty() {
                            (dto.name.to_string(), true, false)
                        } else {
                            (if dto.name.is_empty() {a[0].name.to_string()} else {dto.name.to_string()}, true, true)
                        }
                    }
                )
            },
            BlockInstance::Target(dto) => (dto.name.to_string(), true, false),
            BlockInstance::Output(_output) => {
                (translate.t(&format!("SOURCE_EDITOR.BRICK_{}", block_type)), false, false)
            }
        };
        if dto_title.is_empty() {
            (translate.t(&format!("SOURCE_EDITOR.BRICK_{}", block_type)), false, is_batch)
        } else {
            (dto_title, show_type, is_batch)
        }
    };

    html! {
        <div id={format!("block-{block_id}")} class={format!("tp__source-editor__block no-select tp__source-editor__block-{}{}{}", block_type, if props.edited {" tp__source-editor__block-editing"} else {""}, if props.selected {" tp__source-editor__block-selected"} else {""})}
              style={style} title={title.clone()}>
            <div class={"tp__source-editor__block-header"}>
                // Block handle (drag)
                <div class="tp__source-editor__block-handle" onmousedown={handle_mouse_down.clone()} />
                // Delete button for block
                {
                    html_if!(delete_mode, {
                        <div class={"tp__source-editor__block-header-actions"}>
                        <div class="tp__source-editor__block-delete" onclick={
                            Callback::from(move |_| delete_block.emit(block_id))
                        }></div>
                        </div>
                    })
                }
            </div>
            <div class={if is_batch { "tp__source-editor__block-content  tp__source-editor__block-batch" } else { "tp__source-editor__block-content" }} onmousedown={handle_mouse_down} ondblclick={handle_edit}>
                <div class={"tp__source-editor__block-content-body"}>
                    <div class="tp__source-editor__block-label">
                        { title }
                    </div>
                    {
                        html_if!(show_type, {
                          <span class="tp__source-editor__block-sub-label">{translate.t(&format!("SOURCE_EDITOR.BRICK_{}", block_type))}</span>
                        })
                    }
                </div>

               {html_if!(is_target || is_output, {
                // Left port
                <span
                    class={classes!("tp__source-editor__block-port", "tp__source-editor__block-port--left", port_style)}
                    onmouseup={{
                        let on_connection_drop = props.on_connection_drop.clone();
                        Callback::from(move |e: MouseEvent| {
                           e.prevent_default();
                           e.stop_propagation();
                           on_connection_drop.emit(to_id)
                       })
                    }} />
                })}

               {html_if!(is_target || is_input, {
                // Right port
                <span
                    class="tp__source-editor__block-port tp__source-editor__block-port--right"
                    onmousedown={{
                        let on_connection_start = props.on_connection_start.clone();
                        Callback::from(move |e: MouseEvent| {
                           e.prevent_default();
                           e.stop_propagation();
                           on_connection_start.emit(from_id);
                        })
                    }} />
                })}
            </div>
           {html_if!(is_batch, {
                <div class="tp__source-editor__block-batch-banner">
                 <div class="tp__source-editor__block-batch-banner-label">{"batch"}</div>
                </div>
           })}
        </div>
    }
}