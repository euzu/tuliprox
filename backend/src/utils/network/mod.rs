use std::pin::Pin;
use tokio::io::AsyncRead;

pub(crate) mod content_coding;
pub mod epg;
pub mod ip_checker;
pub mod request;

/// Type-erased asynchronous reader used by the existing network download stack.
pub type DynReader = Pin<Box<dyn AsyncRead + Send>>;

pub use request::format_http_status;
