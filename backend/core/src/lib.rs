//! Resolved configuration model and shared utilities.
//!
//! `model` holds the runtime configuration and domain types; `utils` holds the
//! helpers that read, write and transform them. The two reference each other -
//! configured helpers need the configuration, and configuration resolution uses
//! the helpers - so they are one package rather than two joined by an interface
//! invented to keep a layer diagram.
//!
//! Nothing here names the API layer, a repository, or the playlist pipeline.

pub mod response_macros;

pub mod model;
pub mod utils;
