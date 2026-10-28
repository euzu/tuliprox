//! Rhai hosts for tuliprox. Each host registers its own types and functions and reuses the
//! shared runtime in `tuliprox-scripting`.

pub mod mapper;
pub use mapper::*;
