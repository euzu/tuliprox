pub mod xmltv;
pub mod hls;

// Moved below this layer so the repository can parse without reaching up.
// Re-exported so `processing::parser::{ics, xtream}` keeps resolving.
pub use crate::parser::{ics, xtream};
// The stalker parser produces `iptv::stalker` DTOs and now lives beside them.
pub use crate::iptv::stalker::parser as stalker;
