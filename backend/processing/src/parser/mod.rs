pub mod xmltv;
// The pure HLS manifest parser moved to `tuliprox-parser` beside the other
// format parsers. Re-exported so `processing::parser::hls` keeps resolving.
// The stalker parser produces `iptv::stalker` DTOs and now lives beside them.
pub use tuliprox_iptv::stalker::parser as stalker;
pub use tuliprox_parser::hls;
// Moved below this layer so the repository can parse without reaching up.
// Re-exported so `processing::parser::{ics, xtream}` keeps resolving.
pub use tuliprox_parser::{ics, xtream};
