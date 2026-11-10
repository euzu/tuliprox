//! Splicing of MPEG-TS from several sources into one continuous output.

mod aligner;
mod realtime;

pub use aligner::{SpliceOutput, TsSpliceAligner};
pub use realtime::RealtimeMeter;

#[cfg(test)]
mod realtime_tests;
#[cfg(test)]
mod tests;
