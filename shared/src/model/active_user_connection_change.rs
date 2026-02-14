use crate::model::StreamInfo;
use std::net::SocketAddr;

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(tag = "type", content = "payload", rename_all = "camelCase")]
pub enum ActiveUserConnectionChange {
    Updated(StreamInfo),
    Disconnected(SocketAddr),  // addr
    Connections(usize, usize), // user_count, connection_count
}
