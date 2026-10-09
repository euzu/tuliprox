#[derive(Copy, Clone, Default, Debug)]
pub struct PlaylistItemTypeSet(u16);

macro_rules! to_m3u_non_empty_fields {
    ($header:expr, $line:expr, $(($prop:ident, $field:expr)),*;) => {
        $(
            if !$header.$prop.is_empty() {
                let _ = write!($line," {}=\"{}\"", $field, $header.$prop );
            }
         )*
    };
}

macro_rules! to_m3u_resource_non_empty_fields {
    ($header:expr, $url:expr, $line:expr, $(($prop:ident, $field:expr)),*;) => {
        $(
           if !$header.$prop.is_empty() {
               let _ = write!($line, " {}=\"{}/{}\"", $field, $url, stringify!($prop));
            }
         )*
    };
}

#[cfg(test)]
mod tests;

mod fields;
mod group;
mod header;
mod item;
mod kind;
mod m3u;
mod xtream;
pub use fields::{FieldGet, FieldGetAccessor, FieldRef, FieldSet, FieldSetAccessor};
pub use group::PlaylistGroup;
pub use header::PlaylistItemHeader;
pub use item::{PlaylistEntry, PlaylistItem};
pub use kind::{PlaylistItemType, XtreamCluster};
pub use m3u::M3uPlaylistItem;
pub use xtream::{
    ResourceOutputPolicy, XtreamMappingFlags, XtreamMappingFlagsSet, XtreamMappingOptions, XtreamPlaylistItem,
};
