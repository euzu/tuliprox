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
    pub flags: ResolveOptionsFlagsSet,
    pub resolve_delay: u16,
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
                        let target_options = target.options.as_ref();
                        let input_is_xtream = fpl.input.input_type.is_xtream();

                        let (
                            resolve_tmdb_missing,
                            probe_requested,
                            input_resolve_enabled,
                            input_probe_enabled,
                            input_resolve_background,
                            input_resolve_delay,
                        ) = if let Some(options) = input_options {
                            (
                                options.flags.contains($crate::model::ConfigInputFlags::ResolveTmdb),
                                options.flags.contains($crate::model::ConfigInputFlags::ProbeStream),
                                options.flags.contains($crate::model::ConfigInputFlags::[<Resolve $cluster:camel>]),
                                options.flags.contains($crate::model::ConfigInputFlags::[<Probe $cluster:camel>]),
                                options.flags.contains($crate::model::ConfigInputFlags::ResolveBackground),
                                options.resolve_delay,
                            )
                        } else {
                            (
                                false,
                                false,
                                false,
                                false,
                                shared::utils::default_as_true(),
                                shared::utils::default_resolve_delay_secs(),
                            )
                        };

                        let (
                            legacy_resolve_enabled,
                            legacy_probe_enabled,
                            legacy_resolve_background,
                            legacy_resolve_delay,
                        ) = if let Some(options) = target_options {
                            (
                                options.[<legacy_resolve_ $cluster>](),
                                options.[<legacy_probe_ $cluster>](),
                                options.legacy_resolve_background(),
                                options.legacy_resolve_delay(),
                            )
                        } else {
                            (
                                false,
                                false,
                                shared::utils::default_as_true(),
                                shared::utils::default_resolve_delay_secs(),
                            )
                        };

                        let resolve_enabled = input_resolve_enabled || legacy_resolve_enabled;
                        let probe_enabled = input_probe_enabled || legacy_probe_enabled;
                        let resolve_background = input_resolve_background && legacy_resolve_background;
                        let resolve_delay =
                            if shared::utils::is_default_resolve_delay_secs(&input_resolve_delay) {
                                legacy_resolve_delay
                            } else {
                                input_resolve_delay
                            };

                        let mut flags = $crate::processing::processor::ResolveOptionsFlagsSet::new();
                        if resolve_enabled && input_is_xtream {
                            flags.add($crate::processing::processor::ResolveOptionsFlags::Resolve);
                        }
                        if resolve_tmdb_missing {
                            flags.add($crate::processing::processor::ResolveOptionsFlags::TmdbMissing);
                        }
                        if probe_requested
                            && input_is_xtream
                            && probe_enabled {
                            flags.add($crate::processing::processor::ResolveOptionsFlags::Probe);
                        }
                        if resolve_background {
                            flags.add($crate::processing::processor::ResolveOptionsFlags::Background);
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
