#[inline]
pub fn normalized_source_ordinal(source_ordinal: u32) -> u32 {
    if source_ordinal == 0 {
        u32::MAX
    } else {
        source_ordinal
    }
}
