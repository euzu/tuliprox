use crate::model::VirtualId;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserCommand {
    Kick(SocketAddr, VirtualId, u64),
}
