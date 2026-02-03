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

#[derive(Default)]
pub struct ResolveOptions {
    pub resolve: bool,
    pub resolve_delay: u16,
    pub resolve_tmdb_missing: bool,
    pub probe_requested: bool,
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
                    Some(xtream_output) => $crate::processing::processor::ResolveOptions {
                        resolve: xtream_output.[<resolve_ $cluster>] && fpl.input.input_type == InputType::Xtream,
                        resolve_delay: xtream_output.resolve_delay,
                        resolve_tmdb_missing,
                        probe_requested,
                    },
                    None => $crate::processing::processor::ResolveOptions::default(),
                }
            }
        }
    };
}
use create_resolve_options_function_for_xtream_target;
