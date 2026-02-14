mod playlist;
mod xtream;
// mod affix;
mod xtream_vod;
mod xtream_series;
mod epg;
mod sort;
mod trakt;
mod library;
mod stream_probe;
pub use self::playlist::*;
pub use self::epg::*;
pub use self::xtream::*;
pub use self::xtream_vod::*;
pub use self::xtream_series::*;
pub use self::stream_probe::*;
use shared::create_bitset;

create_bitset!(u8, ResolveOptionsFlags, Resolve, TmdbMissing, Probe, Background);

pub struct ResolveOptions {
    flags: ResolveOptionsFlagsSet,
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
            flags: ResolveOptionsFlagsSet::from_variants(&[ResolveOptionsFlags::Background]),
            resolve_delay: 0,
        }
    }
}


//
// fn get_resolve_<cluster>_options(target: &ConfigTarget, fpl: &FetchedPlaylist) -> bool
//
#[macro_export]
macro_rules! create_resolve_options_function_for_xtream_target {
    ($cluster:ident) => {
        paste::paste! {
            fn [<get_resolve_ $cluster _options>](target: &ConfigTarget, fpl: &FetchedPlaylist) -> $crate::processing::processor::ResolveOptions {
                match target.get_xtream_output() {
                    Some(_) => {
                        let input_options = fpl.input.options.as_ref();
                        let input_is_xtream = fpl.input.input_type.is_xtream();

                        let (
                            resolve_tmdb_missing,
                            input_resolve_enabled,
                            input_probe_enabled,
                            input_resolve_background,
                            input_resolve_delay,
                        ) = if let Some(options) = input_options {
                            (
                                options.has_flag($crate::model::ConfigInputFlags::ResolveTmdb),
                                options.has_flag($crate::model::ConfigInputFlags::[<Resolve $cluster:camel>]),
                                options.has_flag($crate::model::ConfigInputFlags::[<Probe $cluster:camel>]),
                                options.has_flag($crate::model::ConfigInputFlags::ResolveBackground),
                                options.resolve_delay,
                            )
                        } else {
                            (
                                false,
                                false,
                                false,
                                shared::utils::default_as_true(),
                                shared::utils::default_resolve_delay_secs(),
                            )
                        };

                        let resolve_enabled = input_resolve_enabled;
                        let probe_enabled = input_probe_enabled;
                        let resolve_background = input_resolve_background;
                        let resolve_delay = input_resolve_delay;

                        let mut flags = $crate::processing::processor::ResolveOptionsFlagsSet::new();
                        if resolve_enabled && input_is_xtream {
                            flags.set($crate::processing::processor::ResolveOptionsFlags::Resolve);
                        }
                        if resolve_tmdb_missing {
                            flags.set($crate::processing::processor::ResolveOptionsFlags::TmdbMissing);
                        }
                        if input_is_xtream && probe_enabled {
                            flags.set($crate::processing::processor::ResolveOptionsFlags::Probe);
                        }
                        if resolve_background {
                            flags.set($crate::processing::processor::ResolveOptionsFlags::Background);
                        }
                        $crate::processing::processor::ResolveOptions {
                            flags,
                            resolve_delay,
                        }
                    },
                    None => $crate::processing::processor::ResolveOptions::default(),
                }
            }
        }
    };
}
use create_resolve_options_function_for_xtream_target;
