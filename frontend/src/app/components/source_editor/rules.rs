use crate::app::components::{Block, BlockType, Connection};

/// Determines whether two blocks can be connected based on explicit editor rules.
/// Allowed: Provider Input -> Target, Staged Input -> Provider Input, Target -> Output.
/// Target can have multiple Inputs.
/// Output can only have one Target.
/// Target can connect to:
///   - 1x `OutputM3u`
///   - 1x `OutputXtream`
///   - 1x `OutputHdhomerun`
///   - up to 4x `OutputStrm`
pub fn can_connect(from_block: &Block, to_block: &Block, connections: &[Connection], blocks: &[Block]) -> bool {
    // Prevent self-connection
    if from_block.id == to_block.id {
        return false;
    }

    // Identify block categories
    let is_target_input = from_block.block_type.is_input() && !matches!(from_block.block_type, BlockType::InputStaged);
    let from_is_staged = matches!(from_block.block_type, BlockType::InputStaged);
    let is_target = from_block.block_type.is_target();
    let to_is_target = to_block.block_type.is_target();
    let to_is_child_input = to_block.block_type.is_chainable_input();
    let to_is_output = to_block.block_type.is_output();

    // Only allow Provider Input -> Target OR Staged Input -> Provider Input OR Target -> Output
    let valid_direction =
        (is_target_input && to_is_target) || (from_is_staged && to_is_child_input) || (is_target && to_is_output);
    if !valid_direction {
        return false;
    }
    // A provider input can have only one staged overlay connection.
    if from_is_staged && to_is_child_input {
        let stage_has_provider = connections.iter().any(|c| c.from == from_block.id);
        let provider_has_stage = connections.iter().any(|c| c.to == to_block.id);
        if stage_has_provider || provider_has_stage {
            return false;
        }
    }

    // Prevent duplicate connection
    if connections.iter().any(|c| c.from == from_block.id && c.to == to_block.id) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::components::BlockInstance;
    use shared::model::{ConfigInputDto, ConfigTargetDto};
    use std::rc::Rc;

    fn block(id: u16, block_type: BlockType) -> Block {
        let instance = if block_type.is_target() {
            BlockInstance::Target(Rc::new(ConfigTargetDto::default()))
        } else {
            BlockInstance::Input(Rc::new(ConfigInputDto::default()))
        };

        Block { id, block_type, position: (0.0, 0.0), instance }
    }

    #[test]
    fn staged_input_cannot_connect_directly_to_target() {
        let staged = block(1, BlockType::InputStaged);
        let target = block(2, BlockType::Target);

        assert!(!can_connect(&staged, &target, &[], &[staged.clone(), target.clone()]));
    }

    #[test]
    fn staged_input_can_connect_to_provider_input() {
        let staged = block(1, BlockType::InputStaged);
        let provider = block(2, BlockType::InputXtream);

        assert!(can_connect(&staged, &provider, &[], &[staged.clone(), provider.clone()]));
    }

    #[test]
    fn staged_input_can_connect_to_only_one_provider_input() {
        let staged = block(1, BlockType::InputStaged);
        let first_provider = block(2, BlockType::InputXtream);
        let second_provider = block(3, BlockType::InputXtream);
        let connections = [Connection { from: staged.id, to: first_provider.id }];

        assert!(!can_connect(
            &staged,
            &second_provider,
            &connections,
            &[staged.clone(), first_provider, second_provider.clone()],
        ));
    }
}
