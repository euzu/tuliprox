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


//
// fn get_resolve_<cluster>_options(target: &ConfigTarget, fpl: &FetchedPlaylist) -> (bool, u16)
//
#[macro_export]
macro_rules! create_resolve_options_function_for_xtream_target {
    ($cluster:ident) => {
        paste::paste! {
            fn [<get_resolve_ $cluster _options>](target: &ConfigTarget, fpl: &FetchedPlaylist) -> (bool, u16) {
                match target.get_xtream_output() {
                    Some(xtream_output) => (xtream_output.[<resolve_ $cluster>] && fpl.input.input_type == InputType::Xtream,
                                           xtream_output.[<resolve_ $cluster _delay>]),
                    None => (false, 0)
                }
            }
        }
    };
}
use create_resolve_options_function_for_xtream_target;
