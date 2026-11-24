//! Splicing of MPEG-TS from several sources into one continuous output.

mod aligner;
mod program_map;
mod random_access;
mod realtime;

pub use aligner::{SpliceOutput, TsSpliceAligner};
pub use random_access::TsRandomAccessScanner;
pub use realtime::RealtimeMeter;

#[cfg(test)]
mod random_access_tests;
#[cfg(test)]
mod realtime_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
