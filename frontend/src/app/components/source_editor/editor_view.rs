use crate::app::components::source_editor::layout::layout;
use crate::app::components::{can_connect, Block, BlockId, BlockInstance, BlockType, BlockView, Connection, EditMode, InputRow, PortStatus, SourceEditorContext, SourceEditorForm, SourceEditorSidebar, TextButton, BLOCK_HEADER_HEIGHT, BLOCK_HEIGHT, BLOCK_PORT_HEIGHT, BLOCK_WIDTH};
use crate::app::{ConfigContext, PlaylistContext};
use crate::hooks::use_service_context;
use shared::model::{ConfigInputDto, ConfigSourceDto, ConfigTargetDto, HdHomeRunTargetOutputDto,
                    M3uTargetOutputDto, StrmTargetOutputDto, TargetOutputDto, XtreamTargetOutputDto};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use wasm_bindgen::JsCast;
use web_sys::{Element, HtmlElement, MouseEvent};
use yew::platform::spawn_local;
use yew::prelude::*;
use yew_i18n::use_translation;
use crate::model::DialogResult;
use crate::services::DialogService;

const PENDING_LINE: &str = "pending-line";
const SELECTION_RECT: &str = "selection-rect";

const BLOCK_MIDDLE_Y: f32 = (BLOCK_HEIGHT + BLOCK_HEADER_HEIGHT + BLOCK_PORT_HEIGHT) / 2.0;
const PORT_SNAP_THRESHOLD: f32 = 100.0;

const LABEL_SOURCE_EDITOR: &str = "LABEL.SOURCE_EDITOR";
const LABEL_SAVE: &str = "LABEL.SAVE";

type Position = (f32, f32);
type MoveBlockParams = (f32, f32, Position, Vec<(BlockId, Position)>);

#[derive(Clone, PartialEq)]
struct DragState {
    block_id: Option<BlockId>,
    drag_offset: Position,
    sidebar_drag_offset: Position,
    dragging_group: HashSet::<BlockId>,
}

impl Default for DragState {
    fn default() -> Self {
        Self {
            block_id: None,
            drag_offset: (0.0f32, 0.0f32),
            dragging_group: HashSet::<BlockId>::new(),
            sidebar_drag_offset: (0.0f32, 0.0f32),
        }
    }
}

impl DragState {
    pub fn with_drag_block_offset(&mut self, block_id: BlockId, drag_offset: Position) {
        self.block_id = Some(block_id);
        self.drag_offset = (drag_offset.0, drag_offset.1);
    }

    pub(crate) fn reset_dragging(&mut self) {
        self.block_id = None;
        self.drag_offset = (0.0f32, 0.0f32);
        self.dragging_group.clear();
        self.sidebar_drag_offset = (0.0f32, 0.0f32);
    }
}

#[derive(Clone, PartialEq)]
struct SelectionState {
    is_selecting: bool,
    select_rect_elem: Option<HtmlElement>,
    selection_rect: Option<(f32, f32, f32, f32)>, // x,y,w,h relative to canva;
    selection_start: Position,
    selected_blocks: HashSet::<BlockId>,
    group_initial_positions: Vec::<(BlockId, Position)>,
    group_anchor_mouse: Position,
}

impl Default for SelectionState {
    fn default() -> Self {
        Self {
            is_selecting: false,
            selection_rect: None,
            select_rect_elem: None,
            selection_start: (0.0f32, 0.0f32),
            selected_blocks: HashSet::<BlockId>::new(),
            group_initial_positions: Vec::<(BlockId, Position)>::new(),
            group_anchor_mouse: (0.0f32, 0.0f32),
        }
    }
}

impl SelectionState {
    pub fn reset_selection(&mut self) {
        self.is_selecting = false;
        if let Some(rect_elem) = &self.select_rect_elem {
            rect_elem.style().set_property("display", "none").unwrap();
        }
        self.select_rect_elem = None;
        self.selection_rect = None;
        self.selection_start = (0.0f32, 0.0f32);
        self.selected_blocks.clear();
        self.group_initial_positions.clear();
        self.group_anchor_mouse = (0.0f32, 0.0f32);
    }

    pub fn stop_selection(&mut self) {
        self.is_selecting = false;
        if let Some(rect_elem) = &self.select_rect_elem {
            rect_elem.style().set_property("display", "none").unwrap();
        }
        self.select_rect_elem = None;
        self.selection_rect = None;
        self.selection_start = (0.0f32, 0.0f32);
    }

    pub fn with_selecting_start_rect_and_clear_blocks(&mut self, is_selecting: bool,
                                                      selection_start: Position,
                                                      selection_rect: Option<(f32, f32, f32, f32)>) {
        self.is_selecting = is_selecting;
        self.selection_rect = selection_rect;
        self.selection_start = selection_start;
        self.selected_blocks.clear();
    }

    pub fn with_selecting_start_and_rect(&mut self, is_selecting: bool,
                                         selection_start: Position,
                                         selection_rect: Option<(f32, f32, f32, f32)>) {
        self.is_selecting = is_selecting;
        self.selection_rect = selection_rect;
        self.selection_start = selection_start;
    }

    pub(crate) fn with_cleared_blocks(&mut self, block_id: BlockId) {
        let updated_selections: HashSet<BlockId> = self.selected_blocks
            .iter()
            .filter(|&&id| id != block_id)
            .map(|&id| if id > block_id { id - 1 } else { id })
            .collect();

        self.selected_blocks = updated_selections;
    }
}

struct EditorState {
    canvas_offset: Position,
    pan_start: Position,
    drag: DragState,
    selection: SelectionState,
    next_id: BlockId,
    blocks: Vec::<Block>,
    connections: Vec::<Connection>,
    block_elements: HashMap::<BlockId, HtmlElement>,
    connection_elements: HashMap::<(BlockId, BlockId), Element>,
    pending_line_element: Option<Element>,
    pending_line: Option<(Position, Position)>,
    pending_connection: Option<BlockId>,
    is_panning: bool,
}

impl Default for EditorState {
    fn default() -> Self {
        Self {
            canvas_offset: (0.0f32, 0.0f32),
            pan_start: (0.0f32, 0.0f32),
            drag: DragState::default(),
            selection: SelectionState::default(),
            next_id: 1,
            blocks: Vec::<Block>::new(),
            connections: Vec::<Connection>::new(),
            block_elements: HashMap::<BlockId, HtmlElement>::new(),
            connection_elements: HashMap::<(BlockId, BlockId), Element>::new(),
            pending_line_element: None,
            pending_line: None,
            pending_connection: None,
            is_panning: false,
        }
    }
}

impl EditorState {
    pub fn get_block(&self, block_id: BlockId) -> Option<&Block> {
        self.blocks.get(block_id as usize - 1)
    }

    pub fn get_block_mut(&mut self, block_id: BlockId) -> Option<&mut Block> {
        self.blocks.get_mut(block_id as usize - 1)
    }

    pub fn clear_pending(&mut self) {
        self.pending_connection = None;
        self.pending_line = None;
        self.pending_line_element = None;
    }
}

fn create_instance(block_type: BlockType) -> BlockInstance {
    match block_type {
        BlockType::InputXtream => BlockInstance::Input(Rc::new(ConfigInputDto::default())),
        BlockType::InputM3u => BlockInstance::Input(Rc::new(ConfigInputDto::default())),
        BlockType::InputLibrary => BlockInstance::Input(Rc::new(ConfigInputDto::default())),
        BlockType::Target => {
            let dto = ConfigTargetDto { name: String::new(), ..Default::default() };
            BlockInstance::Target(Rc::new(dto))
        }
        BlockType::OutputM3u => BlockInstance::Output(Rc::new(TargetOutputDto::M3u(M3uTargetOutputDto::default()))),
        BlockType::OutputXtream => BlockInstance::Output(Rc::new(TargetOutputDto::Xtream(XtreamTargetOutputDto::default()))),
        BlockType::OutputHdHomeRun => BlockInstance::Output(Rc::new(TargetOutputDto::HdHomeRun(HdHomeRunTargetOutputDto::default()))),
        BlockType::OutputStrm => BlockInstance::Output(Rc::new(TargetOutputDto::Strm(StrmTargetOutputDto::default()))),
    }
}

fn create_block(block_id: BlockId, block_type: BlockType, instance: BlockInstance) -> Block {
    Block {
        id: block_id,
        block_type,
        position: (0.0, 0.0),
        instance,
    }
}

fn create_output_instance(output: &TargetOutputDto) -> (BlockInstance, BlockType) {
    match output {
        TargetOutputDto::Xtream(dto) => (BlockInstance::Output(Rc::new(TargetOutputDto::Xtream(dto.clone()))), BlockType::OutputXtream),
        TargetOutputDto::M3u(dto) => (BlockInstance::Output(Rc::new(TargetOutputDto::M3u(dto.clone()))), BlockType::OutputM3u),
        TargetOutputDto::Strm(dto) => (BlockInstance::Output(Rc::new(TargetOutputDto::Strm(dto.clone()))), BlockType::OutputStrm),
        TargetOutputDto::HdHomeRun(dto) => (BlockInstance::Output(Rc::new(TargetOutputDto::HdHomeRun(dto.clone()))), BlockType::OutputHdHomeRun),
    }
}

// ----------------- Component -----------------
#[function_component]
pub fn SourceEditor() -> Html {
    let canvas_ref = use_node_ref();
    let playlist_ctx = use_context::<PlaylistContext>().expect("Playlist context not found");
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let dialog = use_context::<DialogService>().expect("Dialog service not found");
    let services = use_service_context();
    let translate = use_translation();

    let force_update = use_state(|| 0);
    // ----------------- virtual canvas offset -----------------
    let editor_state_ref = use_mut_ref(EditorState::default);
    // Delete mode toggle
    let delete_mode = use_state(|| false);
    let cursor_grabbing = use_state(|| false);

    {
        let playlists = playlist_ctx.clone();
        let editor_state_ref = editor_state_ref.clone();
        let force_update = force_update.clone();
        use_effect_with(playlists.sources.clone(), move |sources| {
            if let Some(entries) = sources.as_ref() {
                let mut current_id = 1;
                let mut gen_blocks = Vec::new();
                let mut gen_connections = Vec::new();
                let mut added_inputs = HashMap::<u16, BlockId>::new();
                for (inputs, targets) in entries.as_ref() {
                    let mut input_ids = vec![];
                    for input_row in inputs {
                        match input_row.as_ref() {
                            InputRow::Input(input_config) => {
                                let block_id = if let Some(&existing_id) = added_inputs.get(&input_config.id) {
                                    existing_id
                                } else {
                                    let id = current_id;
                                    current_id += 1;
                                    added_inputs.insert(input_config.id, id);
                                    gen_blocks.push(create_block(id,
                                                                 BlockType::from(input_config.input_type),
                                                                 BlockInstance::Input(input_config.clone())));
                                    id
                                };
                                input_ids.push(block_id);
                            }
                            InputRow::Alias(_, _) => {}
                        }
                    }
                    for target_config in targets {
                        let target_id = current_id;
                        current_id += 1;
                        gen_blocks.push(create_block(target_id, BlockType::Target, BlockInstance::Target(target_config.clone())));
                        input_ids.iter().for_each(|input_id| gen_connections.push(Connection { from: *input_id, to: target_id }));

                        for output in &target_config.output {
                            let (block_instance, block_type) = create_output_instance(output);
                            let output_id = current_id;
                            current_id += 1;
                            let block = create_block(output_id, block_type, block_instance);
                            gen_blocks.push(block);
                            gen_connections.push(Connection { from: target_id, to: output_id });
                        }
                    }
                }
                layout(&mut gen_blocks, &gen_connections);
                {
                    let mut editor_state = editor_state_ref.borrow_mut();
                    editor_state.blocks = gen_blocks;
                    editor_state.connections = gen_connections;
                    editor_state.next_id = current_id;
                }
                force_update.set(*force_update + 1)
            }

            || {}
        });
    }

    let collect_block_elements = {
        let editor_state_ref = editor_state_ref.clone();

        Callback::from(move |block_ids: HashSet<BlockId>| {
            let mut editor_state = editor_state_ref.borrow_mut();
            if editor_state.block_elements.is_empty() {
                if let Some(window) = web_sys::window() {
                    if let Some(document) = window.document() {
                        for block_id in &block_ids {
                            if let Some(el) = document.get_element_by_id(&format!("block-{block_id}")) {
                                let div = el.dyn_into::<HtmlElement>().unwrap();
                                editor_state.block_elements.insert(*block_id, div);
                            }

                            // find connections
                            let connections = {
                                let mut connections = HashMap::new();
                                for conn in &editor_state.connections {
                                    if *block_id == conn.from || *block_id == conn.to
                                        || block_ids.contains(&conn.from) || block_ids.contains(&conn.to) {
                                        if let Some(el) = document.get_element_by_id(&format!("conn-{}-{}", conn.from, conn.to)) {
                                            let path_el = el.dyn_into::<Element>().unwrap();
                                            connections.insert((conn.from, conn.to), path_el);
                                        }
                                    }
                                }
                                connections
                            };
                            for (key, elem) in connections {
                                editor_state.connection_elements.insert(key, elem);
                            }
                        }
                    }
                }
            }
        })
    };

    let handle_layout = {
        let editor_state_ref = editor_state_ref.clone();
        let force_update = force_update.clone();
        Callback::from(move |_| {
            let mut editor_state = editor_state_ref.borrow_mut();
            let connections = editor_state.connections.clone();
            layout(&mut editor_state.blocks, &connections);

            force_update.set(*force_update + 1);
        })
    };

    let handle_save = {
        let config_ctx = config_ctx.clone();
        let editor_state_ref = editor_state_ref.clone();
        let services = services.clone();
        let translate = translate.clone();
        Callback::from(move |_| {
            let mut sources_config = config_ctx.config.as_ref().map(|c| c.sources.clone()).unwrap_or_default();
            let editor_state = editor_state_ref.borrow();

            // Convert diagram blocks to sources
            let mut gen_sources: Vec<ConfigSourceDto> = Vec::new();
            let mut gen_inputs: Vec<ConfigInputDto> = Vec::new();

            //find all inputs
            let input_blocks: Vec<&Block> = editor_state.blocks.iter().filter(|b| b.block_type.is_input()).collect();
            for block in &input_blocks {
                if let BlockInstance::Input(input) = &block.instance {
                    gen_inputs.push(input.as_ref().clone());
                }
            };

            // Find all targets
            let target_blocks: Vec<&Block> = editor_state.blocks.iter().filter(|b| b.block_type.is_target()).collect();

            for target_block in target_blocks {
                if let BlockInstance::Target(target_dto) = &target_block.instance {
                    let mut source_dto = ConfigSourceDto {
                        targets: vec![(**target_dto).clone()],
                        inputs: Vec::new(),
                    };

                    // Find incoming inputs
                    for conn in &editor_state.connections {
                        if conn.to == target_block.id {
                            if let Some(input_block) = editor_state.get_block(conn.from) {
                                if input_block.block_type.is_input() {
                                    if let BlockInstance::Input(input_dto) = &input_block.instance {
                                        source_dto.inputs.push(input_dto.name.clone());
                                    }
                                }
                            }
                        }
                    }

                    // Find connected outputs for this target
                    // Actually, the current ConfigTargetDto in the diagram might already have outputs in its `output` field if we edited it there.
                    // But in the diagram, we have separate Output blocks connected to Target.
                    // We need to collect them.

                    let mut outputs = Vec::new();
                    for conn in &editor_state.connections {
                        if conn.from == target_block.id {
                            if let Some(output_block) = editor_state.get_block(conn.to) {
                                if output_block.block_type.is_output() {
                                    if let BlockInstance::Output(output_dto) = &output_block.instance {
                                        outputs.push((**output_dto).clone());
                                    }
                                }
                            }
                        }
                    }

                    if let Some(target) = source_dto.targets.get_mut(0) {
                        target.output = outputs;
                    }

                    gen_sources.push(source_dto);
                }
            }

            sources_config.inputs = gen_inputs;
            sources_config.sources = gen_sources;

            let services = services.clone();
            let translate = translate.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match services.config.save_sources(sources_config).await {
                    Ok(()) => services.toastr.success(translate.t("MESSAGES.SAVE.SOURCES_CONFIG.SUCCESS")),
                    Err(err) => services.toastr.error(err.to_string()),
                }
            });
        })
    };

    let handle_confirm_save = {
        let confirm = dialog.clone();
        let on_save = handle_save.clone();
        let translator = translate.clone();
        Callback::from(move |_| {
            let confirm = confirm.clone();
            let on_save = on_save.clone();
            let translator = translator.clone();
            spawn_local(async move {
                let result = confirm.confirm(&translator.t("MESSAGES.CONFIRM_SOURCES_SAVE")).await;
                if result == DialogResult::Ok {
                   on_save.emit("dave");
                }
            });
        })
    };

    // ----------------- Drag Start from Sidebar -----------------
    let handle_drag_start = {
        let editor_state_ref = editor_state_ref.clone();
        let cursor_grabbing = cursor_grabbing.clone();
        Callback::from(move |e: DragEvent| {
            editor_state_ref.borrow_mut().selection.reset_selection();
            if let Some(target) = e.target_dyn_into::<HtmlElement>() {
                let block_type = target.get_attribute("data-block-type").unwrap_or_default();
                e.data_transfer().unwrap().set_data("text/plain", &block_type).unwrap();
                // Store mouse offset inside the element
                let rect = target.get_bounding_client_rect();
                let offset_x = e.client_x() as f32 - rect.left() as f32;
                let offset_y = e.client_y() as f32 - rect.top() as f32;
                editor_state_ref.borrow_mut().drag.sidebar_drag_offset = (offset_x, offset_y);
                cursor_grabbing.set(true);
            }
        })
    };

    // ----------------- Drop on Canvas -----------------
    let handle_drop = {
        let editor_state_ref = editor_state_ref.clone();
        let canvas_ref = canvas_ref.clone();
        let cursor_grabbing = cursor_grabbing.clone();

        Callback::from(move |e: DragEvent| {
            e.prevent_default();
            e.stop_propagation();
            cursor_grabbing.set(false);
            if let Some(canvas) = canvas_ref.cast::<HtmlElement>() {
                if let Ok(data) = e.data_transfer().unwrap().get_data("text/plain") {
                    let rect = canvas.get_bounding_client_rect();
                    let mouse_x = e.client_x() as f32 - rect.left() as f32;
                    let mouse_y = e.client_y() as f32 - rect.top() as f32;
                    let ((canvas_ox, canvas_oy), (offset_x, offset_y)) = {
                        let editor_state = editor_state_ref.borrow();
                        (editor_state.canvas_offset, editor_state.drag.sidebar_drag_offset)
                    };

                    let block_type = BlockType::from(data.as_str());
                    {
                        let mut editor_state = editor_state_ref.borrow_mut();
                        let next_id = editor_state.next_id;
                        editor_state.blocks.push(Block {
                            id: next_id,
                            block_type,
                            position: (
                                mouse_x - offset_x - canvas_ox, // <-- subtract canvas offset
                                mouse_y - offset_y - canvas_oy
                            ),
                            instance: create_instance(block_type),
                        });
                        editor_state.next_id += 1;
                    }
                }
            }
        })
    };

    let handle_drag_over = Callback::from(|e: DragEvent| {
        e.prevent_default();
        e.stop_propagation();
    });
    let handle_drag_end = {
        let cursor_grabbing = cursor_grabbing.clone();
        Callback::from(move |e: DragEvent| {
            cursor_grabbing.set(false);
            e.prevent_default();
            e.stop_propagation();
        })
    };

    // ----------------- Connection logic -----------------
    let handle_connection_start = {
        let editor_state_ref = editor_state_ref.clone();
        Callback::from(move |from_id: BlockId| {
            let pending_line = {
                let editor_state = editor_state_ref.borrow();
                if let Some(block) = editor_state.get_block(from_id) {
                    let (canvas_ox, canvas_oy) = editor_state.canvas_offset;
                    let x = block.position.0 + BLOCK_WIDTH + canvas_ox;
                    let y = block.position.1 + BLOCK_MIDDLE_Y + canvas_oy;
                    Some(((x, y), (x, y)))
                } else {
                    None
                }
            };
            {
                let mut editor_state = editor_state_ref.borrow_mut();
                editor_state.pending_connection = Some(from_id);
                editor_state.pending_line = pending_line;
            }
        })
    };

    let handle_connection_drop = {
        let editor_state_ref = editor_state_ref.clone();
        Callback::from(move |to_id: BlockId| {
            let pending_connection = editor_state_ref.borrow().pending_connection;
            if let Some(from_id) = pending_connection {
                if from_id != to_id {
                    if let (Some(from_block), Some(to_block)) = {
                        let editor_state = editor_state_ref.borrow();
                        (editor_state.get_block(from_id).cloned(),
                         editor_state.get_block(to_id).cloned())
                    } {
                        // Check connection rules before adding
                        let connection = {
                            let editor_state = editor_state_ref.borrow();
                            if can_connect(&from_block, &to_block, &editor_state.connections, &editor_state.blocks) {
                                Some(Connection { from: from_id, to: to_id })
                            } else {
                                None
                            }
                        };
                        if let Some(con) = connection {
                            editor_state_ref.borrow_mut().connections.push(con);
                        }
                    }
                }
            }
            {
                editor_state_ref.borrow_mut().clear_pending();
            }
        })
    };

    // ----------------- Drag block logic  -----------------
    let handle_block_mouse_down = {
        let editor_state_ref = editor_state_ref.clone();
        let canvas_ref = canvas_ref.clone();
        let cursor_grabbing = cursor_grabbing.clone();

        Callback::from(move |(block_id, e): (BlockId, MouseEvent)| {
            e.prevent_default();
            e.stop_propagation();

            if editor_state_ref.borrow().pending_line.is_some() {
                return;
            }

            let ctrl_key = e.ctrl_key();
            if let Some(canvas) = canvas_ref.cast::<HtmlElement>() {
                cursor_grabbing.set(true);
                let rect = canvas.get_bounding_client_rect();
                let mouse_x = e.client_x() as f32 - rect.left() as f32;
                let mouse_y = e.client_y() as f32 - rect.top() as f32;

                let possible_block = editor_state_ref.borrow().get_block(block_id).cloned();
                let mut editor_state = editor_state_ref.borrow_mut();

                if let Some(block) = possible_block {
                    // Prepare group
                    editor_state.selection.group_initial_positions.clear();
                    editor_state.drag.dragging_group.clear();

                    // Neue Auswahllogik:
                    let (selected_blocks, new_selection) = {
                        let is_selected = editor_state.selection.selected_blocks.contains(&block_id);

                        if is_selected && ctrl_key {
                            // Ctrl + Click on existing block -> remove from selection
                            editor_state.selection.selected_blocks.remove(&block_id);
                            (editor_state.selection.selected_blocks.clone(), None)
                        } else if !is_selected {
                            // Block not selected, select only this block
                            (HashSet::from([block_id]), Some(block_id))
                        } else {
                            // The block is selected and Ctrl is not pressed -> the selection remains as is.
                            (editor_state.selection.selected_blocks.clone(), None)
                        }
                    };

                    // initial positions for drag
                    let mut initial_pos = Vec::new();
                    for id in &selected_blocks {
                        if let Some(b) = editor_state.get_block(*id) {
                            initial_pos.push((*id, b.position));
                        }
                    }

                    editor_state.drag.dragging_group = selected_blocks.clone();
                    editor_state.selection.group_initial_positions = initial_pos;

                    // Drag-Offset calculation
                    editor_state.drag.with_drag_block_offset(block_id, (mouse_x - block.position.0, mouse_y - block.position.1));

                    // update selection
                    if let Some(block) = new_selection {
                        if !ctrl_key {
                            editor_state.selection.selected_blocks.clear();
                        }
                        editor_state.selection.selected_blocks.insert(block);
                    }

                    editor_state.selection.group_anchor_mouse = (mouse_x, mouse_y);
                }
            }
        })
    };


    // ----------------- Canvas mouse down (start panning or marquee selection) -----------------
    let handle_canvas_mouse_down = {
        let editor_state_ref = editor_state_ref.clone();
        let canvas_ref = canvas_ref.clone();
        let cursor_grabbing = cursor_grabbing.clone();

        Callback::from(move |e: MouseEvent| {
            let mouse_button = e.button();
            if mouse_button != 0 && mouse_button != 2 {
                return;
            }
            if let Some(target) = e.target_dyn_into::<web_sys::Element>() {
                if let Some(canvas) = canvas_ref.cast::<web_sys::Element>() {
                    let tag = target.tag_name().to_lowercase();
                    if target.is_same_node(Some(&canvas)) || tag == "svg" {
                        e.prevent_default();
                        e.stop_propagation();
                        let mut editor_state = editor_state_ref.borrow_mut();
                        if e.button() == 0 { // left button
                            if editor_state.selection.is_selecting {
                                editor_state.selection.reset_selection();
                            } else {
                                // selection area mode
                                if let Some(rect_el) = canvas_ref.cast::<HtmlElement>() {
                                    let rect = rect_el.get_bounding_client_rect();
                                    let mouse_x = e.client_x() as f32 - rect.left() as f32;
                                    let mouse_y = e.client_y() as f32 - rect.top() as f32;
                                    if e.ctrl_key() {
                                        editor_state.selection
                                            .with_selecting_start_and_rect(true, (mouse_x, mouse_y), Some((mouse_x, mouse_y, 0.0, 0.0)));
                                    } else {
                                        editor_state.selection
                                            .with_selecting_start_rect_and_clear_blocks(true, (mouse_x, mouse_y), Some((mouse_x, mouse_y, 0.0, 0.0)));
                                    }
                                }
                            }
                        } else if e.button() == 2 { // right button
                            editor_state.selection.reset_selection();
                            // Right button panning
                            cursor_grabbing.set(true);
                            editor_state.is_panning = true;
                            editor_state.pan_start = (e.client_x() as f32, e.client_y() as f32);
                        }
                    }
                }
            }
        })
    };

    let move_blocks = {
        let editor_state_ref = editor_state_ref.clone();
        Callback::from(move |(mouse_x, mouse_y, offset, initial_positions): MoveBlockParams| {
            {
                let to_collect: HashSet<BlockId> = initial_positions.iter().map(|(block_id, _)| *block_id).collect();
                collect_block_elements.emit(to_collect);
            }

            let mut moved_block_ids = HashSet::new();
            {
                let (anchor_x, anchor_y) = offset;
                let dx = mouse_x - anchor_x;
                let dy = mouse_y - anchor_y;

                let mut editor_state = editor_state_ref.borrow_mut();
                for (id, (ix, iy)) in initial_positions {
                    if let Some(b) = editor_state.get_block_mut(id) {
                        b.position = (ix + dx, iy + dy);
                        moved_block_ids.insert(b.id);
                    }
                }
            }

            if !moved_block_ids.is_empty() {
                let editor_state = editor_state_ref.borrow();
                let (canvas_ox, canvas_oy) = editor_state.canvas_offset;

                for block_id in &moved_block_ids {
                    if let Some(div) = editor_state.block_elements.get(block_id) {
                        if let Some(block) = editor_state.get_block(*block_id) {
                            let (x, y) = block.position;
                            div.style().set_property("transform", &format!("translate({}px,{}px)", x + canvas_ox, y + canvas_oy)).unwrap();
                        }
                    }
                }

                let mut move_connections = HashMap::<String, (BlockId, BlockId)>::new();
                for conn in &editor_state.connections {
                    if moved_block_ids.contains(&conn.from) || moved_block_ids.contains(&conn.to) {
                        move_connections.insert(format!("conn-{}-{}", conn.from, conn.to), (conn.from, conn.to));
                    }
                }
                for (from, to) in move_connections.values() {
                    if let Some(path_el) = editor_state.connection_elements.get(&(*from, *to)) {
                        if let (Some(from_block), Some(to_block)) =
                            (&editor_state.get_block(*from), &editor_state.get_block(*to)) {
                            let (d, _) = update_connection(canvas_ox, canvas_oy, from_block, to_block);
                            path_el.set_attribute("d", &d).unwrap();
                        }
                    }
                }
            }
        })
    };

    // ----------------- Mouse move for pending line, block drag, canvas panning, marquee update -----------------
    let handle_canvas_mouse_move = {
        let editor_state_ref = editor_state_ref.clone();
        let canvas_ref = canvas_ref.clone();
        let last_frame = RefCell::new(0.0);
        let move_blocks = move_blocks.clone();

        Callback::from(move |e: MouseEvent| {
            let now = web_sys::js_sys::Date::now();
            if now - *last_frame.borrow() < 16.0 { return; }
            *last_frame.borrow_mut() = now;

            let client_x = e.client_x();
            let client_y = e.client_y();

            let is_panning = {
                editor_state_ref.borrow().is_panning
            };

            if is_panning {
                let initial_positions: Vec<(BlockId, Position)> = {
                    editor_state_ref.borrow().blocks.iter().map(|b| (b.id, b.position)).collect()
                };

                {
                    let mut editor_state = editor_state_ref.borrow_mut();
                    let (start_x, start_y) = editor_state.pan_start;
                    let dx = client_x as f32 - start_x;
                    let dy = client_y as f32 - start_y;
                    let (canvas_ox, canvas_oy) = editor_state.canvas_offset;
                    editor_state.canvas_offset = (canvas_ox + dx, canvas_oy + dy);
                    editor_state.pan_start = (client_x as f32, client_y as f32);
                };

                move_blocks.emit((0.0, 0.0, (0.0, 0.0), initial_positions));
                return;
            }

            if let Some(canvas) = canvas_ref.cast::<HtmlElement>() {
                let rect = canvas.get_bounding_client_rect();
                let mouse_x = client_x as f32 - rect.left() as f32;
                let mouse_y = client_y as f32 - rect.top() as f32;

                {
                    let mut editor_state = editor_state_ref.borrow_mut();
                    // Pending line snap
                    if let Some(((from_x, from_y), _)) = editor_state.pending_line {
                        let mut snapped = (mouse_x, mouse_y);
                        let (canvas_ox, canvas_oy) = editor_state.canvas_offset;
                        for block in &editor_state.blocks {
                            if let Some(port_snap) = compute_port_snap_distance(block.position, mouse_x, mouse_y, canvas_ox, canvas_oy) {
                                snapped = port_snap;
                                break;
                            }
                        }
                        editor_state.pending_line = Some(((from_x, from_y), snapped));

                        if editor_state.pending_line_element.is_none() {
                            if let Some(window) = web_sys::window() {
                                if let Some(document) = window.document() {
                                    if let Some(el) = document.get_element_by_id(PENDING_LINE) {
                                        let line = el.dyn_into::<Element>().unwrap();
                                        editor_state.pending_line_element = Some(line);
                                    }
                                }
                            }
                        }

                        if let Some(line) = editor_state.pending_line_element.as_ref() {
                            update_line(line, from_x, from_y, snapped.0, snapped.1);
                        }
                    }
                }

                let (is_selecting, selection_start, canvas_offset) = {
                    let editor_state = editor_state_ref.borrow();
                    (editor_state.selection.is_selecting,
                     editor_state.selection.selection_start,
                     editor_state.canvas_offset
                    )
                };

                if is_selecting {
                    let (x, y, w, h) = compute_normalized_selection_rect(selection_start, mouse_x, mouse_y);
                    let ctrl_key = e.ctrl_key();

                    // Update selected_blocks: block intersects rect?
                    let selected_blocks: Vec<BlockId> = {
                        editor_state_ref.borrow().blocks.iter()
                            .filter(|b| b.intersects_rect((x, y), (x + w, y + h), canvas_offset)).map(|b| b.id).collect()
                    };

                    {
                        let mut editor_state = editor_state_ref.borrow_mut();
                        if !ctrl_key {
                            editor_state.selection.selected_blocks.clear();
                        }
                        editor_state.selection.selected_blocks.extend(selected_blocks);

                        editor_state.selection.selection_rect = Some((x, y, w, h));
                        if editor_state.selection.select_rect_elem.is_none() {
                            if let Some(window) = web_sys::window() {
                                if let Some(document) = window.document() {
                                    if let Some(el) = document.get_element_by_id(SELECTION_RECT) {
                                        editor_state.selection.select_rect_elem = el.dyn_into::<HtmlElement>().ok();
                                    }
                                }
                            }
                        };

                        if let Some(rect_div) = editor_state.selection.select_rect_elem.as_ref() {
                            update_selection_rect(rect_div, x, y, w, h);
                        }
                    }
                }

                let to_move = {
                    let editor_state = editor_state_ref.borrow();
                    // Update dragging block (Single or Group)
                    if let Some(block_id) = editor_state.drag.block_id {
                        // If the dragged block is member of a selection  -> move group
                        if editor_state.drag.dragging_group.contains(&block_id) && !editor_state.selection.group_initial_positions.is_empty() {
                            Some((mouse_x, mouse_y, editor_state.selection.group_anchor_mouse, editor_state.selection.group_initial_positions.clone()))
                        } else {
                            // Single drag block
                            if let Some(block) = {
                                editor_state.get_block(block_id)
                            } {
                                let positions = vec![(block_id, block.position)];
                                Some((mouse_x, mouse_y, editor_state.selection.group_anchor_mouse, positions))
                            } else {
                                None
                            }
                        }
                    } else {
                        None
                    }
                };
                if let Some(move_it) = to_move {
                    move_blocks.emit(move_it);
                }
            }
        })
    };

    let handle_canvas_mouse_up = {
        let editor_state_ref = editor_state_ref.clone();
        let cursor_grabbing = cursor_grabbing.clone();
        Callback::from(move |_e: MouseEvent| {
            let mut editor_state = editor_state_ref.borrow_mut();
            editor_state.block_elements.clear();
            editor_state.connection_elements.clear();
            // Stop any block dragging and stop panning
            if editor_state.drag.block_id.is_some() {
                editor_state.drag.reset_dragging();
            }
            editor_state.selection.stop_selection();
            editor_state.is_panning = false;
            cursor_grabbing.set(false);
        })
    };

    let handle_canvas_right_click = {
        let editor_state_ref = editor_state_ref.clone();
        Callback::from(move |e: MouseEvent| {
            e.prevent_default(); // prevent default browser context menu
            e.stop_propagation();
            editor_state_ref.borrow_mut().clear_pending();
        })
    };

    // ----------------- Delete handlers -----------------
    let handle_toggle_delete_mode = {
        let delete_mode = delete_mode.clone();
        Callback::from(move |_| delete_mode.set(!*delete_mode))
    };

    // Deleting a Block means updating the following block ids,
    // because a BlockId is the index in the blocks list.
    let handle_delete_block = {
        let editor_state_ref = editor_state_ref.clone();
        let force_update = force_update.clone();
        Callback::from(move |block_id: BlockId| {
            let mut editor_state = editor_state_ref.borrow_mut();
            editor_state.blocks.retain(|b| b.id != block_id);
            editor_state.connections.retain(|c| c.from != block_id && c.to != block_id);

            for block in editor_state.blocks.iter_mut() {
                if block.id >= block_id {
                    block.id -= 1;
                }
            }

            // udpate connection ids
            for conn in editor_state.connections.iter_mut() {
                if conn.from >= block_id {
                    conn.from -= 1;
                }
                if conn.to >= block_id {
                    conn.to -= 1;
                }
            }

            let max_id = editor_state.blocks.iter().map(|b| b.id).max().unwrap_or(0);
            editor_state.next_id = max_id + 1;

            editor_state.selection.with_cleared_blocks(block_id);
            force_update.set(*force_update + 1);
        })
    };

    let handle_delete_connection = {
        let editor_state_ref = editor_state_ref.clone();
        Callback::from(move |(from, to): (BlockId, BlockId)| {
            editor_state_ref.borrow_mut().connections.retain(|c| !(c.from == from && c.to == to));
            force_update.set(*force_update + 1);
        })
    };

    let get_port_status = {
        |block: &Block| {
            if let Some(from_id) = editor_state_ref.borrow().pending_connection {
                let editor_state = editor_state_ref.borrow();
                if let Some(from_block) = editor_state.get_block(from_id) {
                    return if can_connect(from_block, block, &editor_state.connections, &editor_state.blocks) {
                        PortStatus::Valid
                    } else {
                        PortStatus::Invalid
                    };
                }
            }
            PortStatus::Inactive
        }
    };

    let form_changed = {
        let editor_state_ref = editor_state_ref.clone();
        Callback::<(BlockId, BlockInstance)>::from(move |(block_id, instance): (BlockId, BlockInstance)| {
            if let Some(block) = editor_state_ref.borrow_mut().get_block_mut(block_id) {
                block.instance = instance;
            }
        })
    };

    let edit_mode = use_state(|| EditMode::Inactive);

    let handle_block_edit = {
        let edit_mode_set = edit_mode.clone();
        let editor_state_ref = editor_state_ref.clone();
        Callback::from(move |block_id: BlockId| {
            let mut editor_state = editor_state_ref.borrow_mut();
            if let Some(block) = editor_state.get_block(block_id) {
                edit_mode_set.set(EditMode::Active(block.clone()));
                editor_state.selection.reset_selection();
            }
        })
    };

    let editor_context = SourceEditorContext {
        on_form_change: form_changed,
        edit_mode: edit_mode.clone(),
    };

    let edited_block_id = match *edit_mode {
        EditMode::Inactive => 0,
        EditMode::Active(ref b) => b.id,
    };
    let grabbed = *cursor_grabbing;

    let editor_state = editor_state_ref.borrow();
    let ((canvas_off_x, canvas_off_y), select_rect_style, pending_line) = {
        let canvas_offset = editor_state.canvas_offset; // Apply virtual canvas offset
        let select_rect_style = editor_state.selection.selection_rect.as_ref().map(
            |(x, y, w, h)| format!("left:{x}px; top:{y}px; width:{w}px; height:{h}px;"));
        (canvas_offset, select_rect_style, editor_state.pending_line)
    };

    // ----------------- Render -----------------
    html! {
        <ContextProvider<SourceEditorContext> context={editor_context}>
        <div class="tp__source-editor">
            <div class="tp__source-editor__header tp__config-view__header">
                <h1>{ translate.t(LABEL_SOURCE_EDITOR) } </h1>
                <div class="tp__config-view__header-tools">
                </div>
               <TextButton name="sources_save"
                    class={ "secondary" }
                    icon={ "Save" }
                    title={ translate.t(LABEL_SAVE) }
                    onclick={handle_confirm_save.clone()}></TextButton>
            </div>
        <div class="tp__source-editor__content">
            <SourceEditorSidebar
                delete_mode={*delete_mode}
                on_drag_start={handle_drag_start.clone()}
                on_toggle_delete={handle_toggle_delete_mode.clone()}
                on_layout={handle_layout.clone()}
            />
            // Canvas
            <div class="tp__source-editor__canvas-wrapper">
            <div
                ref={canvas_ref.clone()}
                class={classes!("tp__source-editor__canvas", "graph-paper-advanced",
                      if grabbed {"grabbed"} else {""},
                      if editor_state.selection.is_selecting {"selection_mode"} else {""})}
                ondrop={handle_drop.clone()}
                ondragend={handle_drag_end.clone()}
                ondragover={handle_drag_over.clone()}
                onmousemove={handle_canvas_mouse_move.clone()}
                onmousedown={handle_canvas_mouse_down.clone()}
                onmouseup={handle_canvas_mouse_up.clone()}
                oncontextmenu={handle_canvas_right_click.clone()}>

                // SVG for connections
                <svg class={classes!("tp__source-editor__connections",
                               if grabbed {"grabbed"} else {""},
                               if editor_state.selection.is_selecting {"selection_mode"} else {""})}>
                    { for editor_state.connections.iter().filter_map(|c| {
                        let from_block = editor_state.get_block(c.from)?;
                        let to_block = editor_state.get_block(c.to)?;
                        let (d, (from_x, from_y, to_x, to_y)) = update_connection(canvas_off_x, canvas_off_y, from_block, to_block);

                        Some(html! {
                            <g>
                                <path id={format!("conn-{}-{}", c.from, c.to)} d={d} stroke="var(--source-editor-line-color)" fill="transparent" stroke-width="2"/>
                                { if *delete_mode {
                                    let mid_x = (from_x + to_x) / 2.0;
                                    let mid_y = (from_y + to_y) / 2.0;
                                    let on_delete_connection = handle_delete_connection.clone();
                                    html! {
                                        <circle cx={mid_x.to_string()} cy={mid_y.to_string()} r="6" fill="var(--source-editor-delete-color)" class="clickable"
                                            onclick={
                                                let from = c.from;
                                                let to = c.to;
                                                Callback::from(move |_| on_delete_connection.emit((from, to)))
                                            }
                                        />
                                    }
                                } else {
                                    html!{}
                                } }
                            </g>
                        })
                    }) }

                    // Pending line straight
                    { if let Some(((x1, y1), (x2, y2))) = pending_line {
                        html! {
                            <line id={PENDING_LINE}
                                x1={x1.to_string()} y1={y1.to_string()}
                                x2={x2.to_string()} y2={y2.to_string()}
                                stroke="var(--source-editor-pending-line-color)"
                                stroke-width="2"
                                stroke-dasharray="4 2" />
                        }
                    } else { html!{} } }
                </svg>

                // Selection rectangle overlay
                <div id={SELECTION_RECT} class="tp__source-editor__selection-rect" style={select_rect_style}></div>

                // Render blocks with canvas offset
                { for editor_state.blocks.iter().map(|b|{
                    let port_status = get_port_status(b);
                    let mut shifted_block = b.clone();
                    let block_id = shifted_block.id;
                    shifted_block.position = (b.position.0 + canvas_off_x, b.position.1 + canvas_off_y);
                    let is_block_selected = editor_state.selection.selected_blocks.contains(&block_id);
                    html! {
                    <BlockView
                        key={block_id}
                        block={shifted_block}
                        edited={edited_block_id == block_id}
                        selected={is_block_selected}
                        delete_mode={*delete_mode}
                        delete_block={handle_delete_block.clone()}
                        port_status={port_status}
                        on_edit={handle_block_edit.clone()}
                        on_mouse_down={handle_block_mouse_down.clone()}
                        on_connection_drop={handle_connection_drop.clone()}
                        on_connection_start={handle_connection_start.clone()}
                    />
                }}) }
            </div>
            </div>
            <SourceEditorForm />
          </div>
        </div>
        </ContextProvider<SourceEditorContext>>
    }
}

fn update_connection(ox: f32, oy: f32, from_block: &Block, to_block: &Block) -> (String, (f32, f32, f32, f32)) {
    let from_x = from_block.position.0 + BLOCK_WIDTH + ox;
    let from_y = from_block.position.1 + BLOCK_MIDDLE_Y + oy;
    let to_x = to_block.position.0 + ox;
    let to_y = to_block.position.1 + BLOCK_MIDDLE_Y + oy;
    let dx = to_x - from_x;
    let ctrl = dx * 0.5;
    (format!(
        "M {} {} C {} {}, {} {}, {} {}",
        from_x, from_y,
        from_x + ctrl, from_y,
        to_x - ctrl, to_y,
        to_x, to_y
    ), (from_x, from_y, to_x, to_y))
}

fn update_line(line: &Element, from_x: f32, from_y: f32, to_x: f32, to_y: f32) {
    line.set_attribute("x1", &from_x.to_string()).unwrap();
    line.set_attribute("y1", &from_y.to_string()).unwrap();
    line.set_attribute("x2", &to_x.to_string()).unwrap();
    line.set_attribute("y2", &to_y.to_string()).unwrap();
}

fn update_selection_rect(rect_div: &HtmlElement, x: f32, y: f32, w: f32, h: f32) {
    let div_style = rect_div.style();
    div_style.set_property("display", "block").unwrap();
    div_style.set_property("left", &format!("{x}px")).unwrap();
    div_style.set_property("top", &format!("{y}px")).unwrap();
    div_style.set_property("width", &format!("{w}px")).unwrap();
    div_style.set_property("height", &format!("{h}px")).unwrap();
}

fn compute_normalized_selection_rect(selection_start: Position, mouse_x: f32, mouse_y: f32) -> (f32, f32, f32, f32) {
    // compute normalized rect
    let (start_x, start_y) = selection_start;
    let x = start_x.min(mouse_x);
    let y = start_y.min(mouse_y);
    let w = (mouse_x - start_x).abs();
    let h = (mouse_y - start_y).abs();
    (x, y, w, h)
}

fn compute_port_snap_distance(block_position: Position, mouse_x: f32, mouse_y: f32, canvas_ox: f32, canvas_oy: f32) -> Option<Position> {
    let port_x = block_position.0 + canvas_ox;
    let port_y = block_position.1 + BLOCK_MIDDLE_Y + canvas_oy;
    let dx = mouse_x - port_x;
    let dy = mouse_y - port_y;
    let dist_sq = dx * dx + dy * dy;
    if dist_sq < PORT_SNAP_THRESHOLD {
        Some((port_x, port_y))
    } else {
        None
    }
}