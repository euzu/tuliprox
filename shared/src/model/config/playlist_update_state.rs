use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum PlaylistUpdateState {
    Success,
    Failure,
}
