//! String interning utilities for memory optimization.
//!
//! Provides a global string interner to deduplicate frequently repeated
//! strings like `input_name` and `group` in playlist items.

use serde::{Deserialize, Deserializer, Serializer};
use std::sync::Arc;

macro_rules! intern_impl {
    ($s:expr, $create:expr) => {{
        if let Ok(guard) = INTERNER.read() {
            if let Some(existing) = guard.get($s) {
                return Arc::clone(existing);
            }
        }
        if let Ok(mut guard) = INTERNER.write() {
            if let Some(existing) = guard.get($s) {
                return Arc::clone(existing);
            }
            let arc: Arc<str> = $create;
            guard.insert(Arc::clone(&arc));
            return arc;
        }
        $create
    }};
}

//
// Reuses `ArcStrVisitor` / `OptionArcStrVisitor` via `deserialize_option`:
//   - null / ~ / empty  -> visit_none / visit_unit -> "" / None
//   - `ArcStrVisitor::visit_some` -> deserialize_any -> numbers/bools map into
//     typed `visit_*` methods, so raw numeric notation may be normalized
//   - `OptionArcStrVisitor::visit_some` -> deserialize_any -> numbers/bools map
//     into their typed `visit_*` methods before being interned as strings

pub use arc_str_default_on_null as arc_str_none_default_on_null;

#[cfg(test)]
mod tests;

mod pool;
mod scalar;
mod serde_adapters;
pub use pool::{interner_gc, interner_len, Internable};
pub use scalar::is_nullish;
pub use serde_adapters::{
    arc_str_default_on_null, arc_str_null_is_none_option_serde, arc_str_null_is_none_serde,
    arc_str_option_null_if_empty_serde, arc_str_option_serde, arc_str_serde, arc_str_vec_serde,
    deserialize_as_option_arc_str,
};
use serde_adapters::{ArcStrVisitor, NullishArcStrVisitor, NullishOptionArcStrVisitor, OptionArcStrVisitor};
