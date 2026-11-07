use crate::{
    app::components::{
        plugin_bricks,
        source_editor::editor_model::{BlockSpec, BlockType},
        use_plugin_ui, AppIcon, CollapsePanel, IconButton, PluginBrick, PluginTileKind, PluginUiContext,
    },
    hooks::{use_icon_context, Icons},
    i18n::{use_translation, YewI18n},
};
use yew::prelude::*;

pub const BLOCK_TYPES_INPUT: [BlockType; 9] = [
    BlockType::InputXtream,
    BlockType::InputM3u,
    BlockType::InputLibrary,
    BlockType::InputStalker,
    BlockType::InputStaged,
    BlockType::InputJellyfin,
    BlockType::InputEmby,
    BlockType::InputPlex,
    BlockType::InputPlugin,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_exposes_all_first_class_input_types() {
        assert!(BLOCK_TYPES_INPUT.contains(&BlockType::InputXtream));
        assert!(BLOCK_TYPES_INPUT.contains(&BlockType::InputM3u));
        assert!(BLOCK_TYPES_INPUT.contains(&BlockType::InputLibrary));
        assert!(BLOCK_TYPES_INPUT.contains(&BlockType::InputStaged));
        assert!(BLOCK_TYPES_INPUT.contains(&BlockType::InputEmby));
        assert!(BLOCK_TYPES_INPUT.contains(&BlockType::InputJellyfin));
        assert!(BLOCK_TYPES_INPUT.contains(&BlockType::InputPlex));
        assert!(BLOCK_TYPES_INPUT.contains(&BlockType::InputPlugin));
    }

    #[test]
    fn sidebar_exposes_the_plugin_output() {
        assert!(BLOCK_TYPES_OUTPUT.contains(&BlockType::OutputPlugin));
        assert!(BlockType::OutputPlugin.is_output());
    }
}

pub const BLOCK_TYPES_TARGET: [BlockType; 1] = [BlockType::Target];

pub const BLOCK_TYPES_OUTPUT: [BlockType; 5] = [
    BlockType::OutputXtream,
    BlockType::OutputM3u,
    BlockType::OutputHdHomeRun,
    BlockType::OutputStrm,
    BlockType::OutputPlugin,
];

/// One sidebar tile. A disabled tile is shown but neither drags nor adds.
struct Brick {
    spec: BlockSpec,
    label: String,
    hint: Option<String>,
    icon: Option<String>,
    disabled: bool,
}

impl Brick {
    fn builtin(block_type: BlockType, translate: &YewI18n) -> Self {
        Self {
            spec: BlockSpec::of(block_type),
            label: translate.t(&format!("SOURCE_EDITOR.BRICK_{block_type}")),
            hint: None,
            icon: None,
            disabled: false,
        }
    }

    fn plugin(kind: PluginTileKind, brick: PluginBrick, icons: &Icons) -> Self {
        Self {
            spec: BlockSpec::with_plugin(kind.block_type(), &brick.plugin_id),
            label: brick.title,
            hint: brick.description,
            // A plugin-supplied name is only ever a lookup key: `AppIcon` would load
            // an unknown one as an image URL.
            icon: brick.icon.filter(|name| icons.get_icon(name).is_some()),
            disabled: false,
        }
    }
}

/// The tiles that stand for the plugin block type `block_type`: one per enabled
/// provider; a greyed-out one with a note when none is installed; the generic
/// tile while the plugin list is unknown, so nothing is hidden on a guess.
fn plugin_type_bricks(
    block_type: BlockType,
    ui: Option<&PluginUiContext>,
    icons: &Icons,
    translate: &YewI18n,
) -> Vec<Brick> {
    let Some(kind) = PluginTileKind::of(block_type) else {
        return vec![Brick::builtin(block_type, translate)];
    };
    let Some(ui) = ui.filter(|ui| ui.loaded) else {
        return vec![Brick::builtin(block_type, translate)];
    };
    let bricks = plugin_bricks(ui, kind);
    if bricks.is_empty() {
        let mut none = Brick::builtin(block_type, translate);
        none.hint = Some(translate.t(kind.none_installed_key()));
        none.disabled = true;
        return vec![none];
    }
    bricks.into_iter().map(|brick| Brick::plugin(kind, brick, icons)).collect()
}

fn bricks_for(
    block_types: &[BlockType],
    ui: Option<&PluginUiContext>,
    icons: &Icons,
    translate: &YewI18n,
) -> Vec<Brick> {
    block_types
        .iter()
        .flat_map(|block_type| {
            if block_type.is_plugin() {
                plugin_type_bricks(*block_type, ui, icons, translate)
            } else {
                vec![Brick::builtin(*block_type, translate)]
            }
        })
        .collect()
}

fn create_brick(brick: Brick, on_drag_start: Callback<DragEvent>, on_add_block: Callback<BlockSpec>) -> Html {
    let Brick { spec, label, hint, icon, disabled } = brick;
    let block_type = spec.block_type;
    let key = spec.to_drag_data();
    let plugin = spec.plugin.clone();
    let is_plugin = plugin.is_some();
    let handle_touch_end = Callback::from(move |e: TouchEvent| {
        e.stop_propagation();
        if !disabled {
            on_add_block.emit(spec.clone());
        }
    });
    let handle_drag_start = Callback::from(move |e: DragEvent| {
        if disabled {
            e.prevent_default();
        } else {
            on_drag_start.emit(e);
        }
    });

    html! {
        <div key={key} class={classes!(
            "tp__source-editor__brick",
            format!("tp__source-editor__brick-{block_type}"),
            is_plugin.then_some("tp__source-editor__brick--plugin"),
            disabled.then_some("tp__source-editor__brick--disabled"),
        )}
        draggable={(!disabled).to_string()}
        aria-disabled={disabled.to_string()}
        title={hint}
        data-block-type={block_type.to_string()}
        data-plugin={plugin}
        ondragstart={handle_drag_start}
        ontouchend={handle_touch_end}>
            if let Some(icon) = icon {
                <span class="tp__source-editor__brick-icon"><AppIcon name={icon} /></span>
            }
            <span class="tp__source-editor__brick-label">{ label }</span>
        </div>
    }
}

#[derive(Properties, PartialEq)]
pub struct SourceEditorSidebarProps {
    #[prop_or_default]
    pub allow_write: bool,
    #[prop_or_default]
    pub collapsed: bool,
    #[prop_or_default]
    pub is_mobile: bool,
    #[prop_or_default]
    pub delete_mode: bool,
    #[prop_or_default]
    pub on_toggle_delete: Callback<(String, MouseEvent)>,
    #[prop_or_default]
    pub on_toggle_sidebar: Callback<(String, MouseEvent)>,
    #[prop_or_default]
    pub on_drag_start: Callback<DragEvent>,
    #[prop_or_default]
    pub on_add_block: Callback<BlockSpec>,
    #[prop_or_default]
    pub on_layout: Callback<(String, MouseEvent)>,
}

#[component]
pub fn SourceEditorSidebar(props: &SourceEditorSidebarProps) -> Html {
    let translate = use_translation();
    let plugin_ui = use_plugin_ui();
    let icons = use_icon_context();
    let group = |block_types: &[BlockType]| -> Html {
        bricks_for(block_types, plugin_ui.as_ref(), &icons, &translate)
            .into_iter()
            .map(|brick| create_brick(brick, props.on_drag_start.clone(), props.on_add_block.clone()))
            .collect()
    };

    html! {
        // Sidebar
        <div class={classes!(
            "tp__source-editor__sidebar",
            if props.collapsed { "collapsed" } else { "expanded" },
            if props.is_mobile { "mobile" } else { "" }
        )}>
            <div class="tp__source-editor__sidebar-actions">
                <IconButton
                    class={if props.collapsed {"tp__source-editor__sidebar-actions-active"} else {""}}
                    name="toggle_sidebar"
                    icon="Sidebar"
                    onclick={props.on_toggle_sidebar.clone()}
                />
                <IconButton name="layout" icon="Nodes" onclick={props.on_layout.clone()} />
                // Delete mode toggle button
                { html! {
                    if props.allow_write {
                        <IconButton class={if props.delete_mode {"tp__source-editor__sidebar-actions-active"} else {""} } name="toggle_delete" icon="Delete" onclick={props.on_toggle_delete.clone()} />
                    } else {
                        <></>
                    }
                }}
            </div>
            {
                if props.collapsed {
                    html! {}
                } else {
                    html! {
                        <div class="tp__source-editor__sidebar-bricks">
                            <CollapsePanel title={translate.t("LABEL.INPUTS")}>
                                <div class="tp__source-editor__sidebar-bricks-group">
                                    { group(&BLOCK_TYPES_INPUT) }
                                </div>
                            </CollapsePanel>
                            <CollapsePanel title={translate.t("LABEL.TARGETS")}>
                                <div class="tp__source-editor__sidebar-bricks-group">
                                    { group(&BLOCK_TYPES_TARGET) }
                                </div>
                            </CollapsePanel>
                            <CollapsePanel title={translate.t("LABEL.OUTPUT")}>
                                 <div class="tp__source-editor__sidebar-bricks-group">
                                    { group(&BLOCK_TYPES_OUTPUT) }
                                 </div>
                            </CollapsePanel>
                        </div>
                    }
                }
            }
        </div>
    }
}
