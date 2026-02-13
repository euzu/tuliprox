use crate::app::components::{Block, BlockType, Connection};

/// Determines whether two blocks can be connected based on explicit editor rules.
/// Allowed: Input → Target, Target → Output.
/// Target can have multiple Inputs.
/// Output can only have one Target.
/// Target can connect to:
///   - 1x OutputM3u
///   - 1x OutputXtream
///   - 1x OutputHdhomerun
///   - up to 4x OutputStrm
pub fn can_connect(
    from_block: &Block,
    to_block: &Block,
    connections: &[Connection],
    blocks: &[Block],
) -> bool {
    // Prevent self-connection
    if from_block.id == to_block.id {
        return false;
    }

    // Identify block categories
    let is_input = from_block.block_type.is_input();
    let is_target = from_block.block_type.is_target();
    let to_is_target = to_block.block_type.is_target();
    let to_is_output = to_block.block_type.is_output();

    // Only allow Input → Target OR Target → Output
    let valid_direction = (is_input && to_is_target) || (is_target && to_is_output);
    if !valid_direction {
        return false;
    }

    // Prevent duplicate connection
    if connections
        .iter()
        .any(|c| c.from == from_block.id && c.to == to_block.id)
    {
        return false;
    }

    // Output can have only one incoming connection
    if to_is_output {
        let has_input_already = connections.iter().any(|c| c.to == to_block.id);
        if has_input_already {
            return false;
        }
    }

    // 6Per-target output type limits
    if is_target && to_is_output {
        let from_id = from_block.id;

        // Count how many connections this Target already has to each output type
        let mut count_m3u = 0;
        let mut count_xtream = 0;
        let mut count_hdhomerun = 0;
        let mut count_strm = 0;

        for conn in connections.iter().filter(|c| c.from == from_id) {
            if let Some(out_block) = blocks.iter().find(|b| b.id == conn.to) {
                match out_block.block_type {
                    BlockType::OutputM3u => count_m3u += 1,
                    BlockType::OutputXtream => count_xtream += 1,
                    BlockType::OutputHdHomeRun => count_hdhomerun += 1,
                    BlockType::OutputStrm => count_strm += 1,
                    _ => {}
                }
            }
        }

        match to_block.block_type {
            BlockType::OutputM3u if count_m3u >= 1 => return false,
            BlockType::OutputXtream if count_xtream >= 1 => return false,
            BlockType::OutputHdHomeRun if count_hdhomerun >= 1 => return false,
            BlockType::OutputStrm if count_strm >= 4 => return false,
            _ => {}
        }
    }

    // Passed all checks
    true
}
