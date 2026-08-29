//! Domain identifier newtypes.
//!
//! These are all `u32` on the wire and on disk, and used to be `u32` in the type
//! system too — `pub type VirtualId = u32` gave the reader a name and the
//! compiler nothing. That matters here more than usual, because the same
//! `BPlusTree<u32, XtreamPlaylistItem>` store is keyed by a *virtual* id in one
//! code path and a *provider* id in another, selected by a runtime tag. Nothing
//! stopped a lookup in one key space using an id from the other.
//!
//! # On-disk compatibility
//!
//! Both types are `#[repr(transparent)]` (same layout as the `u32`) and
//! `#[serde(transparent)]` (same encoding). The B+Tree codec is `rmp_serde`, and
//! `backend/btree/src/codec.rs` has a test asserting that a transparent newtype
//! over `u32` encodes byte-for-byte identically to the bare integer and
//! cross-reads in both directions. Existing databases are unaffected; there is
//! no migration.

use serde::{Deserialize, Serialize};
use std::{
    fmt::{Display, Formatter},
    num::ParseIntError,
    str::FromStr,
};

/// Declares an id newtype over `u32`.
///
/// Deliberately no `From<u32>` / `Into<u32>`: an implicit conversion would let a
/// provider id become a virtual id by inference, which is the confusion these
/// types exist to prevent. Crossing between an id and a raw integer is spelled
/// `new` and `get`.
macro_rules! u32_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
        )]
        #[repr(transparent)]
        #[serde(transparent)]
        pub struct $name(pub u32);

        impl $name {
            #[inline]
            #[must_use]
            pub const fn new(value: u32) -> Self { Self(value) }

            #[inline]
            #[must_use]
            pub const fn get(self) -> u32 { self.0 }

            /// Whether this is the sentinel `0`, which every id space uses to
            /// mean "unset" (real ids start at 1).
            #[inline]
            #[must_use]
            pub const fn is_unset(self) -> bool { self.0 == 0 }
        }

        impl Display for $name {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result { self.0.fmt(f) }
        }

        impl FromStr for $name {
            type Err = ParseIntError;

            fn from_str(value: &str) -> Result<Self, Self::Err> { value.parse::<u32>().map(Self) }
        }
    };
}

u32_id! {
    /// An id tuliprox assigns, unique within a target and stable across refreshes.
    ///
    /// This is what a client sees in a stream URL, and the key of the target-scoped
    /// playlist stores.
    VirtualId
}

u32_id! {
    /// An id the upstream provider assigned.
    ///
    /// Not unique across inputs and not stable if the provider renumbers, which is
    /// why it is not interchangeable with [`VirtualId`] even though both are `u32`.
    ProviderId
}

#[cfg(test)]
mod tests {
    use super::{ProviderId, VirtualId};
    use std::str::FromStr;

    #[test]
    fn ids_are_transparent_over_u32() {
        assert_eq!(size_of::<VirtualId>(), size_of::<u32>());
        assert_eq!(size_of::<Option<VirtualId>>(), size_of::<Option<u32>>());
        assert_eq!(size_of::<ProviderId>(), size_of::<u32>());
    }

    #[test]
    fn serialized_form_is_a_bare_number() {
        assert_eq!(serde_json::to_string(&VirtualId::new(7)).expect("serializes"), "7");
        assert_eq!(serde_json::from_str::<VirtualId>("7").expect("parses"), VirtualId::new(7));
        // Cross-reads: a document written when this was a bare u32 still loads.
        assert_eq!(serde_json::from_str::<ProviderId>("4294967295").expect("parses"), ProviderId::new(u32::MAX));
    }

    #[test]
    fn display_and_from_str_round_trip_for_url_paths() {
        let id = VirtualId::new(1234);
        assert_eq!(id.to_string(), "1234");
        assert_eq!(VirtualId::from_str("1234").expect("parses"), id);
        assert!(VirtualId::from_str("notanumber").is_err());
        assert!(VirtualId::from_str("").is_err());
    }

    #[test]
    fn zero_is_the_unset_sentinel() {
        assert!(VirtualId::default().is_unset());
        assert!(VirtualId::new(0).is_unset());
        assert!(!VirtualId::new(1).is_unset());
    }
}
