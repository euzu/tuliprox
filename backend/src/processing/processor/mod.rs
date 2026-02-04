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
use shared::create_bit_set;

create_bit_set!(u16, ResolveOptionsFlags, Resolve, TmdbMissing, Probe, Background);

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
                // Get input options
                let (resolve_tmdb_missing, probe_requested) = fpl.input.options.as_ref().map_or_else(||(false, false), |o| (o.resolve_tmdb, o.analyze_stream));
                match target.get_xtream_output() {
                    Some(xtream_output) => {
                        let mut flags = $crate::processing::processor::ResolveOptionsFlagsSet::new();
                        // Map config flags to resolve options flags
                        if xtream_output.flags.contains($crate::model::XtreamTargetFlags::[<Resolve $cluster:camel>]) && fpl.input.input_type == InputType::Xtream {
                            flags.add($crate::processing::processor::ResolveOptionsFlags::Resolve);
                        }
                        if resolve_tmdb_missing {
                            flags.add($crate::processing::processor::ResolveOptionsFlags::TmdbMissing);
                        }
                        if probe_requested {
                            flags.add($crate::processing::processor::ResolveOptionsFlags::Probe);
                        }
                        if xtream_output.flags.contains($crate::model::XtreamTargetFlags::ResolveBackground) {
                            flags.add($crate::processing::processor::ResolveOptionsFlags::Background);
                        }
                        
                        $crate::processing::processor::ResolveOptions {
                            flags,
                            resolve_delay: xtream_output.resolve_delay,
                        }
                    },
                    None => $crate::processing::processor::ResolveOptions::default(),
                }
            }
        }
    };
}
use create_resolve_options_function_for_xtream_target;
