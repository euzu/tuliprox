pub mod playlist;
mod xtream;
// mod affix;
mod xtream_vod;
mod xtream_series;
pub mod epg;
mod sort;

#[macro_export]
macro_rules! handle_error {
    ($stmt:expr, $map_err:expr) => {
        if let Err(err) = $stmt {
            $map_err(err);
        }
    };
}
use handle_error;

#[macro_export]
macro_rules! handle_error_and_return {
    ($stmt:expr, $map_err:expr) => {
        if let Err(err) = $stmt {
            $map_err(err);
            return Default::default();
        }
    };
}
use handle_error_and_return;


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

