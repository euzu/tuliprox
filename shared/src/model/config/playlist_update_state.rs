use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum PlaylistUpdateState {
    Success,
    Failure,
    Partial,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_state_has_distinct_wire_value() -> Result<(), serde_json::Error> {
        assert_eq!(serde_json::to_string(&PlaylistUpdateState::Partial)?, "\"Partial\"");
        Ok(())
    }
}
