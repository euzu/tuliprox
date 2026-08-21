use crate::app::components::{Block, BlockId, BlockInstance, Connection};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

const CANVAS_OFFSET: f32 = 10.0;

const Y_GAP: f32 = 25.0;
const X_GAP: f32 = 50.0;

const BLOCK_HEIGHT: f32 = 50.0;
const BLOCK_WIDTH: f32 = 200.0;
const INPUT_VERTICAL_STEP_BASE: f32 = BLOCK_HEIGHT + Y_GAP * 2.0;
const INPUT_BATCH_EXTRA_HEIGHT: f32 = 16.0;

struct TargetBlock {
    id: BlockId,
    outputs: Option<Vec<BlockId>>,
    height: f32,
    position: (f32, f32),
}

impl TargetBlock {
    pub fn new(id: BlockId, outputs: Option<Vec<BlockId>>) -> Self {
        let height = Self::height(outputs.as_ref());
        TargetBlock { id, outputs, height, position: (0.0, 0.0) }
    }

    fn height(outputs: Option<&Vec<BlockId>>) -> f32 {
        if let Some(outs) = outputs {
            let len = outs.len() as f32;
            ((len * BLOCK_HEIGHT) + ((len - 1.0) * Y_GAP)).max(BLOCK_HEIGHT)
        } else {
            BLOCK_HEIGHT
        }
    }

    pub fn set_position(&mut self, x: f32, y: f32, blocks: &mut [Block]) {
        self.position = (x, y);
        let out_x = x + BLOCK_WIDTH + X_GAP;
        let mut out_y = y;
        if let Some(outputs) = self.outputs.as_ref() {
            for out in outputs {
                blocks[*out as usize - 1].position = (out_x, out_y);
                out_y += BLOCK_HEIGHT + Y_GAP;
            }
        }
        blocks[self.id as usize - 1].position = (x, y + (self.height - BLOCK_HEIGHT) / 2.0);
    }
}

#[derive(Default)]
struct LayoutComponent {
    input_ids: Vec<BlockId>,
    target_ids: Vec<BlockId>,
}

type EdgeMap = HashMap<BlockId, Vec<BlockId>>;

struct InputTargetMaps {
    input_to_targets: EdgeMap,
    /// Staged input -> provider input edges.
    input_to_child_inputs: EdgeMap,
    adjacency: EdgeMap,
}

fn build_target_blocks(blocks: &mut [Block], connections: &[Connection]) -> Vec<TargetBlock> {
    let mut out_edges: HashMap<BlockId, Vec<BlockId>> = HashMap::new();

    for c in connections {
        out_edges.entry(c.from).or_default().push(c.to);
    }

    blocks
        .iter()
        .filter(|b| b.block_type.is_target())
        .map(|b| TargetBlock::new(b.id, out_edges.get(&b.id).cloned()))
        .collect()
}

fn push_unique(map: &mut EdgeMap, key: BlockId, value: BlockId) {
    let values = map.entry(key).or_default();
    if !values.contains(&value) {
        values.push(value);
    }
}

fn build_input_target_maps(blocks: &[Block], connections: &[Connection]) -> InputTargetMaps {
    let mut input_to_targets: EdgeMap = HashMap::new();
    let mut input_to_child_inputs: EdgeMap = HashMap::new();
    let mut adjacency: EdgeMap = HashMap::new();

    for con in connections {
        // Skip connections referencing missing blocks instead of panicking
        let Some(from) = usize::from(con.from).checked_sub(1).and_then(|i| blocks.get(i)) else { continue };
        let Some(to) = usize::from(con.to).checked_sub(1).and_then(|i| blocks.get(i)) else { continue };

        if from.block_type.is_input() && to.block_type.is_target() {
            push_unique(&mut input_to_targets, con.from, con.to);
            push_unique(&mut adjacency, con.from, con.to);
            push_unique(&mut adjacency, con.to, con.from);
        } else if from.block_type.is_target() && to.block_type.is_input() {
            push_unique(&mut input_to_targets, con.to, con.from);
            push_unique(&mut adjacency, con.from, con.to);
            push_unique(&mut adjacency, con.to, con.from);
        } else if from.block_type.is_input() && to.block_type.is_input() {
            // Staged input chain: parent input feeds a child (staged) input.
            push_unique(&mut input_to_child_inputs, con.from, con.to);
            push_unique(&mut adjacency, con.from, con.to);
            push_unique(&mut adjacency, con.to, con.from);
        }
    }

    InputTargetMaps { input_to_targets, input_to_child_inputs, adjacency }
}

fn compute_input_columns(input_ids: &[BlockId], input_to_child_inputs: &EdgeMap) -> HashMap<BlockId, usize> {
    let input_set: HashSet<BlockId> = input_ids.iter().copied().collect();
    let mut columns: HashMap<BlockId, usize> = input_ids.iter().map(|&id| (id, 0usize)).collect();

    let max_iterations = input_ids.len() + 1;
    for _ in 0..max_iterations {
        let mut changed = false;
        for (&parent, children) in input_to_child_inputs {
            if !input_set.contains(&parent) {
                continue;
            }
            let parent_column = columns.get(&parent).copied().unwrap_or(0);
            for &child in children {
                if !input_set.contains(&child) {
                    continue;
                }
                let candidate = parent_column + 1;
                if candidate > columns.get(&child).copied().unwrap_or(0) {
                    columns.insert(child, candidate);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    columns
}

fn build_connected_components(
    input_ids: &[BlockId],
    target_ids: &[BlockId],
    adjacency: &HashMap<BlockId, Vec<BlockId>>,
) -> Vec<LayoutComponent> {
    let input_set: HashSet<BlockId> = input_ids.iter().copied().collect();
    let target_set: HashSet<BlockId> = target_ids.iter().copied().collect();
    let mut visited: HashSet<BlockId> = HashSet::new();
    let mut components = Vec::new();

    for &start in input_ids.iter().chain(target_ids.iter()) {
        if visited.contains(&start) {
            continue;
        }

        let mut queue = VecDeque::from([start]);
        let mut component = LayoutComponent::default();

        while let Some(node) = queue.pop_front() {
            if !visited.insert(node) {
                continue;
            }

            if input_set.contains(&node) {
                component.input_ids.push(node);
            } else if target_set.contains(&node) {
                component.target_ids.push(node);
            }

            if let Some(neighbors) = adjacency.get(&node) {
                for &neighbor in neighbors {
                    if !visited.contains(&neighbor) {
                        queue.push_back(neighbor);
                    }
                }
            }
        }

        components.push(component);
    }

    components
}

fn average_connected_targets(
    input_id: BlockId,
    input_to_targets: &HashMap<BlockId, Vec<BlockId>>,
    target_centers: &HashMap<BlockId, f32>,
) -> Option<f32> {
    let connected = input_to_targets.get(&input_id)?;
    let mut sum = 0.0;
    let mut count = 0;

    for target_id in connected {
        if let Some(y) = target_centers.get(target_id) {
            sum += y;
            count += 1;
        }
    }

    if count == 0 {
        None
    } else {
        // Inputs are positioned by top-left y, while targets are represented as center y.
        Some((sum / count as f32) - BLOCK_HEIGHT / 2.0)
    }
}

fn input_extra_height(block: &Block) -> f32 {
    match &block.instance {
        BlockInstance::Input(dto) => {
            dto.aliases.as_ref().map_or(0.0, |aliases| if aliases.is_empty() { 0.0 } else { INPUT_BATCH_EXTRA_HEIGHT })
        }
        _ => 0.0,
    }
}

fn component_sort_key(
    component: &LayoutComponent,
    input_rank: &HashMap<BlockId, usize>,
    target_rank: &HashMap<BlockId, usize>,
) -> (usize, usize, usize) {
    let min_input_rank =
        component.input_ids.iter().filter_map(|id| input_rank.get(id).copied()).min().unwrap_or(usize::MAX);
    let min_target_rank =
        component.target_ids.iter().filter_map(|id| target_rank.get(id).copied()).min().unwrap_or(usize::MAX);
    (min_input_rank.min(min_target_rank), min_target_rank, min_input_rank)
}

#[allow(clippy::too_many_arguments)]
fn layout_component(
    component: &LayoutComponent,
    input_rank: &HashMap<BlockId, usize>,
    target_rank: &HashMap<BlockId, usize>,
    input_to_targets: &HashMap<BlockId, Vec<BlockId>>,
    input_to_child_inputs: &HashMap<BlockId, Vec<BlockId>>,
    input_columns: &HashMap<BlockId, usize>,
    target_heights: &HashMap<BlockId, f32>,
    input_steps: &HashMap<BlockId, f32>,
    input_visual_heights: &HashMap<BlockId, f32>,
) -> (HashMap<BlockId, f32>, HashMap<BlockId, f32>, f32) {
    let mut input_ids = component.input_ids.clone();
    input_ids.sort_by_key(|id| input_rank.get(id).copied().unwrap_or(usize::MAX));

    let mut target_ids = component.target_ids.clone();
    target_ids.sort_by_key(|id| target_rank.get(id).copied().unwrap_or(usize::MAX));

    let mut target_centers: HashMap<BlockId, f32> = HashMap::new();
    let mut current_top = 0.0;
    for target_id in &target_ids {
        let height = target_heights.get(target_id).copied().unwrap_or(BLOCK_HEIGHT);
        target_centers.insert(*target_id, current_top + height / 2.0);
        current_top += height + Y_GAP;
    }

    // Desired y per input. Inputs wired to targets align to those targets; staged
    // inputs without a direct target align to their provider inputs. Process deepest
    // columns first so a parent can read its already-resolved children's y.
    let mut by_column_desc = input_ids.clone();
    by_column_desc.sort_by(|a, b| {
        let column_a = input_columns.get(a).copied().unwrap_or(0);
        let column_b = input_columns.get(b).copied().unwrap_or(0);
        column_b.cmp(&column_a)
    });
    let mut input_desired: HashMap<BlockId, f32> = HashMap::new();
    for input_id in &by_column_desc {
        let desired = average_connected_targets(*input_id, input_to_targets, &target_centers)
            .or_else(|| {
                let children = input_to_child_inputs.get(input_id)?;
                let mut sum = 0.0;
                let mut count = 0;
                for child in children {
                    if let Some(&child_y) = input_desired.get(child) {
                        sum += child_y;
                        count += 1;
                    }
                }
                if count == 0 {
                    None
                } else {
                    Some(sum / count as f32)
                }
            })
            .unwrap_or(0.0);
        input_desired.insert(*input_id, desired);
    }

    // Stack inputs per column so siblings in the same column never overlap while
    // still honoring their desired y.
    let mut columns: BTreeMap<usize, Vec<BlockId>> = BTreeMap::new();
    for input_id in &input_ids {
        columns.entry(input_columns.get(input_id).copied().unwrap_or(0)).or_default().push(*input_id);
    }
    let mut input_y: HashMap<BlockId, f32> = HashMap::new();
    for column_ids in columns.values_mut() {
        column_ids.sort_by_key(|id| input_rank.get(id).copied().unwrap_or(usize::MAX));
        let mut next_input_y = 0.0;
        for input_id in column_ids.iter() {
            let desired = input_desired.get(input_id).copied().unwrap_or(next_input_y);
            let aligned = desired.max(next_input_y);
            input_y.insert(*input_id, aligned);
            let step = input_steps.get(input_id).copied().unwrap_or(INPUT_VERTICAL_STEP_BASE);
            next_input_y = aligned + step;
        }
    }

    let mut target_tops: HashMap<BlockId, f32> = HashMap::new();
    for target_id in &target_ids {
        let height = target_heights.get(target_id).copied().unwrap_or(BLOCK_HEIGHT);
        let center = target_centers.get(target_id).copied().unwrap_or(height / 2.0);
        target_tops.insert(*target_id, center - height / 2.0);
    }

    let min_input_top = input_y.values().copied().fold(f32::INFINITY, f32::min);
    let min_target_top = target_tops.values().copied().fold(f32::INFINITY, f32::min);
    let min_top = min_input_top.min(min_target_top);
    let shift = if min_top.is_finite() { -min_top } else { 0.0 };

    for y in input_y.values_mut() {
        *y += shift;
    }
    for y in target_tops.values_mut() {
        *y += shift;
    }

    let max_input_bottom = input_y
        .iter()
        .map(|(id, &y)| y + input_visual_heights.get(id).copied().unwrap_or(BLOCK_HEIGHT))
        .fold(0.0, f32::max);
    let max_target_bottom = target_tops
        .iter()
        .map(|(id, &top)| top + target_heights.get(id).copied().unwrap_or(BLOCK_HEIGHT))
        .fold(0.0, f32::max);

    let component_height = max_input_bottom.max(max_target_bottom).max(BLOCK_HEIGHT);

    (input_y, target_tops, component_height)
}

/// calcuates Barycenter for a Block, based on connected Blocks in given Order-Array
fn barycenter(id: BlockId, map: &HashMap<BlockId, Vec<BlockId>>, order: &[BlockId]) -> f32 {
    if let Some(connected) = map.get(&id) {
        let mut sum = 0.0;
        let mut count = 0;
        for &c in connected {
            if let Some(pos) = order.iter().position(|&x| x == c) {
                sum += pos as f32;
                count += 1;
            }
        }
        if count == 0 {
            f32::INFINITY
        } else {
            sum / count as f32
        }
    } else {
        f32::INFINITY
    }
}

/// Counts crossings
fn count_crossings(input_order: &[BlockId], target_order: &[BlockId], connections: &[Connection]) -> usize {
    let input_index: HashMap<BlockId, usize> = input_order.iter().enumerate().map(|(i, &id)| (id, i)).collect();
    let target_index: HashMap<BlockId, usize> = target_order.iter().enumerate().map(|(i, &id)| (id, i)).collect();

    let mut count = 0;
    for (i, c1) in connections.iter().enumerate() {
        if !input_index.contains_key(&c1.from) || !target_index.contains_key(&c1.to) {
            continue;
        }
        for c2 in &connections[i + 1..] {
            if !input_index.contains_key(&c2.from) || !target_index.contains_key(&c2.to) {
                continue;
            }
            let i1 = input_index[&c1.from];
            let j1 = target_index[&c1.to];
            let i2 = input_index[&c2.from];
            let j2 = target_index[&c2.to];

            if (i1 < i2 && j1 > j2) || (i1 > i2 && j1 < j2) {
                count += 1;
            }
        }
    }
    count
}

/// Barycentric Sort
pub fn barycentric_sort(
    blocks: &[Block],
    connections: &[Connection],
    iterations: usize,
) -> (Vec<BlockId>, Vec<BlockId>) {
    // Initiale Reihenfolge
    let mut input_order: Vec<BlockId> = blocks.iter().filter(|b| b.block_type.is_input()).map(|b| b.id).collect();
    let mut target_order: Vec<BlockId> = blocks.iter().filter(|b| b.block_type.is_target()).map(|b| b.id).collect();

    let mut input_to_targets: HashMap<BlockId, Vec<BlockId>> = HashMap::new();
    let mut target_to_inputs: HashMap<BlockId, Vec<BlockId>> = HashMap::new();

    for con in connections {
        let from = usize::from(con.from).checked_sub(1).and_then(|i| blocks.get(i));
        let to = usize::from(con.to).checked_sub(1).and_then(|i| blocks.get(i));
        if let (Some(from), Some(to)) = (from, to) {
            if from.block_type.is_input() && to.block_type.is_target() {
                input_to_targets.entry(con.from).or_default().push(con.to);
                target_to_inputs.entry(con.to).or_default().push(con.from);
            }
        }
    }

    // Iterative Barycenter-Sortierung
    for _ in 0..iterations {
        // sort inputs by middle value of targets
        input_order.sort_by(|&a, &b| {
            barycenter(a, &input_to_targets, &target_order).total_cmp(&barycenter(b, &input_to_targets, &target_order))
        });

        // sort targets by middle value of inputs
        target_order.sort_by(|&a, &b| {
            barycenter(a, &target_to_inputs, &input_order).total_cmp(&barycenter(b, &target_to_inputs, &input_order))
        });
    }

    // simple local cross optimisation for inputs
    let mut improved = true;
    for _ in 0..10 {
        if !improved {
            break;
        }
        improved = false;
        for i in 0..input_order.len().saturating_sub(1) {
            let mut swapped = input_order.clone();
            swapped.swap(i, i + 1);
            if count_crossings(&swapped, &target_order, connections)
                < count_crossings(&input_order, &target_order, connections)
            {
                input_order.swap(i, i + 1);
                improved = true;
            }
        }
    }

    // simple local cross optimisation for targets
    improved = true;
    for _ in 0..10 {
        if !improved {
            break;
        }
        improved = false;
        for i in 0..target_order.len().saturating_sub(1) {
            let mut swapped = target_order.clone();
            swapped.swap(i, i + 1);
            if count_crossings(&input_order, &swapped, connections)
                < count_crossings(&input_order, &target_order, connections)
            {
                target_order.swap(i, i + 1);
                improved = true;
            }
        }
    }

    (input_order, target_order)
}

pub fn layout(blocks: &mut [Block], connections: &[Connection]) {
    let (input_order, target_order) = barycentric_sort(blocks, connections, 5);
    let input_rank: HashMap<BlockId, usize> = input_order.iter().enumerate().map(|(idx, &id)| (id, idx)).collect();
    let target_rank: HashMap<BlockId, usize> = target_order.iter().enumerate().map(|(idx, &id)| (id, idx)).collect();

    let mut target_blocks: HashMap<BlockId, TargetBlock> =
        build_target_blocks(blocks, connections).into_iter().map(|target| (target.id, target)).collect();
    let target_heights: HashMap<BlockId, f32> = target_blocks.iter().map(|(id, target)| (*id, target.height)).collect();

    let input_ids: Vec<BlockId> = blocks.iter().filter(|b| b.block_type.is_input()).map(|b| b.id).collect();
    let target_ids: Vec<BlockId> = blocks.iter().filter(|b| b.block_type.is_target()).map(|b| b.id).collect();
    let input_extra_heights: HashMap<BlockId, f32> = input_ids
        .iter()
        .map(|&id| {
            let block = &blocks[id as usize - 1];
            (id, input_extra_height(block))
        })
        .collect();
    let input_steps: HashMap<BlockId, f32> = input_ids
        .iter()
        .map(|&id| (id, INPUT_VERTICAL_STEP_BASE + input_extra_heights.get(&id).copied().unwrap_or(0.0)))
        .collect();
    let input_visual_heights: HashMap<BlockId, f32> =
        input_ids.iter().map(|&id| (id, BLOCK_HEIGHT + input_extra_heights.get(&id).copied().unwrap_or(0.0))).collect();

    let maps = build_input_target_maps(blocks, connections);
    let input_columns = compute_input_columns(&input_ids, &maps.input_to_child_inputs);
    let max_input_column = input_columns.values().copied().max().unwrap_or(0);
    let column_stride = BLOCK_WIDTH + X_GAP;
    let mut components = build_connected_components(&input_ids, &target_ids, &maps.adjacency);
    components.sort_by_key(|component| component_sort_key(component, &input_rank, &target_rank));

    let start_x = CANVAS_OFFSET + (max_input_column as f32 + 1.0) * column_stride;
    let mut start_y = CANVAS_OFFSET;

    for component in components {
        let (component_input_y, component_target_tops, component_height) = layout_component(
            &component,
            &input_rank,
            &target_rank,
            &maps.input_to_targets,
            &maps.input_to_child_inputs,
            &input_columns,
            &target_heights,
            &input_steps,
            &input_visual_heights,
        );

        let mut sorted_targets = component.target_ids.clone();
        sorted_targets.sort_by_key(|id| target_rank.get(id).copied().unwrap_or(usize::MAX));
        for target_id in sorted_targets {
            if let (Some(target_block), Some(local_top)) =
                (target_blocks.get_mut(&target_id), component_target_tops.get(&target_id))
            {
                target_block.set_position(start_x, start_y + local_top, blocks);
            }
        }

        let mut sorted_inputs = component.input_ids.clone();
        sorted_inputs.sort_by_key(|id| input_rank.get(id).copied().unwrap_or(usize::MAX));
        for input_id in sorted_inputs {
            if let Some(local_y) = component_input_y.get(&input_id) {
                let column = input_columns.get(&input_id).copied().unwrap_or(0);
                let x = CANVAS_OFFSET + column as f32 * column_stride;
                blocks[input_id as usize - 1].position = (x, start_y + local_y);
            }
        }

        start_y += component_height + Y_GAP;
    }
}
