pub mod playlist;
pub mod xtream;
// mod affix;
pub mod xtream_vod;
pub mod xtream_series;
pub mod epg;
mod sort;
pub mod trakt;
mod library;
pub mod stream_probe;

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