use crate::app::components::{Block, BlockId, Connection};
use std::collections::HashMap;

const CANVAS_OFFSET: f32 = 10.0;

const Y_GAP: f32 = 25.0;
const X_GAP: f32 = 50.0;

const BLOCK_HEIGHT: f32 = 50.0;
const BLOCK_WIDTH: f32 = 200.0;

struct TargetBlock {
    id: BlockId,
    outputs: Option<Vec<BlockId>>,
    height: f32,
    position: (f32, f32),
}

impl TargetBlock {
    pub fn new(id: BlockId, outputs: Option<Vec<BlockId>>) -> Self {
        let height = Self::height(outputs.as_ref());
        TargetBlock {
            id,
            outputs,
            height,
            position: (0.0, 0.0),
        }
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

fn build_target_blocks(blocks: &mut [Block], connections: &[Connection]) -> Vec<TargetBlock> {
    let mut out_edges: HashMap<BlockId, Vec<BlockId>> = HashMap::new();
    let mut in_edges: HashMap<BlockId, Vec<BlockId>> = HashMap::new();

    for c in connections {
        out_edges.entry(c.from).or_default().push(c.to);
        in_edges.entry(c.to).or_default().push(c.from);
    }

    blocks
        .iter()
        .filter(|b| b.block_type.is_target())
        .map(|b| TargetBlock::new(b.id, out_edges.get(&b.id).cloned()))
        .collect()
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
fn count_crossings(
    input_order: &[BlockId],
    target_order: &[BlockId],
    connections: &[Connection],
) -> usize {
    let input_index: HashMap<BlockId, usize> = input_order
        .iter()
        .enumerate()
        .map(|(i, &id)| (id, i))
        .collect();
    let target_index: HashMap<BlockId, usize> = target_order
        .iter()
        .enumerate()
        .map(|(i, &id)| (id, i))
        .collect();

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
    let mut input_order: Vec<BlockId> = blocks
        .iter()
        .filter(|b| b.block_type.is_input())
        .map(|b| b.id)
        .collect();
    let mut target_order: Vec<BlockId> = blocks
        .iter()
        .filter(|b| b.block_type.is_target())
        .map(|b| b.id)
        .collect();

    let mut input_to_targets: HashMap<BlockId, Vec<BlockId>> = HashMap::new();
    let mut target_to_inputs: HashMap<BlockId, Vec<BlockId>> = HashMap::new();

    for con in connections {
        if blocks[con.from as usize - 1].block_type.is_input()
            && blocks[con.to as usize - 1].block_type.is_target()
        {
            input_to_targets.entry(con.from).or_default().push(con.to);
            target_to_inputs.entry(con.to).or_default().push(con.from);
        }
    }

    // Iterative Barycenter-Sortierung
    for _ in 0..iterations {
        // sort inputs by middle value of targets
        input_order.sort_by(|&a, &b| {
            barycenter(a, &input_to_targets, &target_order)
                .partial_cmp(&barycenter(b, &input_to_targets, &target_order))
                .unwrap()
        });

        // sort targets by middle value of inputs
        target_order.sort_by(|&a, &b| {
            barycenter(a, &target_to_inputs, &input_order)
                .partial_cmp(&barycenter(b, &target_to_inputs, &input_order))
                .unwrap()
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

    let mut target_blocks = build_target_blocks(blocks, connections);
    target_blocks.sort_by_key(|a| target_order.iter().position(|&id| id == a.id).unwrap());

    let mut start_y = CANVAS_OFFSET;
    let start_x = CANVAS_OFFSET + BLOCK_WIDTH + X_GAP;
    for target_block in &mut target_blocks {
        target_block.set_position(start_x, start_y, blocks);
        start_y += target_block.height + Y_GAP;
    }

    let target_map: HashMap<BlockId, &TargetBlock> =
        target_blocks.iter().map(|t| (t.id, t)).collect();

    let mut last_input_y = CANVAS_OFFSET;

    for block_id in &input_order {
        let connected_targets: Vec<&TargetBlock> = connections
            .iter()
            .filter(|c| c.from == *block_id)
            .filter_map(|c| target_map.get(&c.to))
            .copied()
            .collect();

        let desired_y = if connected_targets.is_empty() {
            last_input_y
        } else {
            // Center Y-Position of target
            let min_y = connected_targets
                .iter()
                .map(|t| t.position.1)
                .fold(f32::INFINITY, |a, b| a.min(b));

            let max_y = connected_targets
                .iter()
                .map(|t| t.position.1 + t.height)
                .fold(f32::NEG_INFINITY, |a, b| a.max(b));

            (min_y + max_y) / 2.0 - BLOCK_HEIGHT / 2.0
        };

        let mut final_y = desired_y;
        // prevent overlap
        if final_y < last_input_y {
            final_y = last_input_y;
        }

        let block = &mut blocks[*block_id as usize - 1];
        block.position = (CANVAS_OFFSET, final_y);
        last_input_y = final_y + BLOCK_HEIGHT + Y_GAP * 2.0;
    }
}
