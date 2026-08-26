use std::pin::Pin;
use tokio::io::AsyncRead;

pub mod content_coding;
// Tees a stream or reader to disk while it is being consumed. Built entirely on
// this module's own reader type and buffered writer, so it belongs here rather
// than in `api`, which `utils` must not depend on.
pub mod ip_checker;
pub mod persist_pipe;
pub mod request;

/// Type-erased asynchronous reader used by the existing network download stack.
pub type DynReader = Pin<Box<dyn AsyncRead + Send>>;

pub use request::format_http_status;
