use shared::create_bitset;

create_bitset!(u8, ResolveOptionsFlags, Resolve, TmdbMissing, Probe, Background);

pub struct ResolveOptions {
    pub(crate) flags: ResolveOptionsFlagsSet,
    pub resolve_delay: u16,
}

impl ResolveOptions {
    #[inline]
    pub fn has_flag(&self, flag: ResolveOptionsFlags) -> bool {
        self.flags.contains(flag)
    }

    #[inline]
    pub fn unset_flag(&mut self, flag: ResolveOptionsFlags) {
        self.flags.unset(flag);
    }
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            flags: ResolveOptionsFlags::Background.into(),
            resolve_delay: shared::utils::default_resolve_delay_secs(),
        }
    }
}
