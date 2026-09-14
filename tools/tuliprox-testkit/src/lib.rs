#![forbid(unsafe_code)]

pub mod adaptive;
pub mod bootstrap;
pub mod config;
pub mod control;
pub mod custom_video;
pub mod discovery;
pub mod faults;
pub mod frame;
pub mod hls;
pub mod observation;
pub mod oracle;
pub mod origin_events;
pub mod origin_transport;
pub mod policy;
pub mod protocol;
pub mod report;
pub mod scheduler;
pub mod secret;
pub mod shared;
pub mod transport;
pub mod vod;

use std::process::ExitCode;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TestkitError {
    #[error("invalid configuration: {0}")]
    Configuration(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunExit {
    Passed,
    Failed,
    Invalid,
    Inconclusive,
}

impl From<RunExit> for ExitCode {
    fn from(value: RunExit) -> Self {
        match value {
            RunExit::Passed => ExitCode::SUCCESS,
            RunExit::Failed => ExitCode::from(1),
            RunExit::Invalid => ExitCode::from(2),
            RunExit::Inconclusive => ExitCode::from(3),
        }
    }
}
