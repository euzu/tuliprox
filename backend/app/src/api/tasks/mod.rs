//! Background tasks the server runs on a schedule or a watch.
//!
//! Measured 2026-08-26: this module is pinned to `api`, mostly by one type.
//!
//! - `config_watch` and `xtream_expiry` read only `app_config` (and one HTTP
//!   client), but both call `ConfigFile::load_sources`, and `ConfigFile` exists
//!   to reload configuration *into* `AppState` - thirteen references to it. It
//!   is the root state's own reload path, so it does not move, and neither do
//!   they.
//! - `scheduler` reads ten fields off `AppState`, including `forced_targets`
//!   which lives nowhere else, and calls `panel_api::sync_panel_api_exp_dates`.
//! - `hdhomerun_ssdp` (139 lines, `core` only) and `library_scan` (55 lines) are
//!   genuinely free. They stay because their natural homes would have to take a
//!   dependency to accept them - `library` is a clean `shared`+`core` leaf and
//!   would gain `session` for an event publish - and 194 lines is not worth
//!   degrading a leaf crate or minting a package to hold.

mod config_watch;
mod hdhomerun_ssdp;
mod library_scan;
mod scheduler;
mod xtream_expiry;

pub(in crate::api) use config_watch::*;
pub(in crate::api) use hdhomerun_ssdp::*;
pub(in crate::api) use library_scan::*;
pub(in crate::api) use scheduler::*;
pub(in crate::api) use xtream_expiry::*;
