use std::io;
use std::sync::Arc;
use bytes::Bytes;
use crate::model::{ActiveUserConnectionChange, ConfigType, LibraryScanSummary, PlaylistUpdateState, StatusCheck, SystemInfo};
use serde::{Deserialize, Serialize};
use crate::model::user_command::UserCommand;

pub const PROTOCOL_VERSION: u8 = 1;

#[derive(Default, PartialOrd, PartialEq, Debug, Clone)]
pub enum UserRole {
    #[default]
    Unauthorized,
    Admin,
    User,
}

impl UserRole {
    pub fn is_admin(&self) -> bool {
        self.eq(&UserRole::Admin)
    }
    pub fn is_user(&self) -> bool {
        self.eq(&UserRole::User)
    }
}

#[derive(Default)]
pub struct ProtocolHandlerMemory {
    pub token: Option<String>,
    pub role: UserRole,
}

pub enum ProtocolHandler {
    Version(u8),
    Default(ProtocolHandlerMemory),
}

pub enum WsCloseCode {
    // Normal,
    // Away,
    Protocol,
    // Unsupported,
    // Abnormal,
    // Invalid,
    // Policy,
    // Size,
    // Extension,
    // Error,
    // Restart,
    // Again,
    // Tls,
}

impl WsCloseCode {
    pub fn code(&self) -> u16 {
        match self {
            // WsCloseCode::Normal => 1000,
            // WsCloseCode::Away => 1001,
            WsCloseCode::Protocol => 1002,
            // WsCloseCode::Unsupported => 1003,
            // WsCloseCode::Abnormal => 1006,
            // WsCloseCode::Invalid => 1007,
            // WsCloseCode::Policy => 1008,
            // WsCloseCode::Size => 1009,
            // WsCloseCode::Extension => 1010,
            // WsCloseCode::Error => 1011,
            // WsCloseCode::Restart => 1012,
            // WsCloseCode::Again => 1013,
            // WsCloseCode::Tls => 1015,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum ProtocolMessage {
    Unauthorized,
    Error(String),
    Version(u8),
    Auth(String),
    Authorized,
    ServerError(String),
    StatusRequest(String),
    UserAction(UserCommand),
    // Responses
    StatusResponse(StatusCheck),
    ActiveUserResponse(ActiveUserConnectionChange),
    ActiveProviderResponse(Arc<str>, usize), // single provider
    ActiveProviderCountRequest(String),
    ActiveProviderCountResponse(usize),
    ConfigChangeResponse(ConfigType),
    PlaylistUpdateResponse(PlaylistUpdateState),
    PlaylistUpdateProgressResponse(String, String),
    UserActionResponse(bool),
    SystemInfoResponse(SystemInfo),
    LibraryScanProgressResponse(LibraryScanSummary),
}

impl ProtocolMessage {
    pub fn to_bytes(&self) -> io::Result<Bytes> {
        match self {
            ProtocolMessage::Version(version) => {
                Ok(Bytes::from(vec![*version]))
            }
            _ => {
                //let encoded = bincode_serialize(self)?;
                let json = serde_json::to_string(self)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                Ok(Bytes::from(json.into_bytes()))
            }
        }
    }

    pub fn from_bytes(bytes: Bytes) -> io::Result<Self> {
        if bytes.len() == 1 {
            Ok(ProtocolMessage::Version(bytes[0]))
        } else {
            //bincode_deserialize::<ProtocolMessage>(bytes.as_ref())
            let s = std::str::from_utf8(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            serde_json::from_str(s)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
        }
    }
}