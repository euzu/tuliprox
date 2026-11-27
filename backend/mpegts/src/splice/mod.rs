//! Splicing of MPEG-TS from several sources into one continuous output.

mod aligner;
pub mod live_edge;
mod pcr_clock;
mod program_map;
mod random_access;
mod rap_detector;
mod realtime;

pub use aligner::{SpliceOutput, TsSpliceAligner};
pub use live_edge::{TrimOutput, TsLiveEdgeTrim};
pub use random_access::TsRandomAccessScanner;
pub use realtime::RealtimeMeter;

#[cfg(test)]
mod live_edge_tests;
#[cfg(test)]
mod random_access_tests;
#[cfg(test)]
mod realtime_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
