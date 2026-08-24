// Shared clippy policy: see [workspace.lints.clippy] in the root Cargo.toml.
//
// `clippy::all` + `clippy::pedantic` reach this crate for the first time in the
// modularization plan's Phase 0. Everything with a machine-applicable fix has
// already been applied via `cargo clippy --fix`. What remains needs hand edits
// inside Yew `html!` macro bodies and view state, which Phase 0 must not touch:
// it is a behaviour-preserving lint-policy change, not a frontend rewrite.
//
// This crate is out of scope for the modularization plan (see "The frontend is a
// separate plan"). The list is tracked debt for that plan, not workspace policy —
// the backend is held to the full policy without any such list.
#![allow(clippy::cast_precision_loss)] // integer -> f64 for chart/layout maths
#![allow(clippy::cast_possible_truncation)] // f64 -> integer pixel coordinates
#![allow(clippy::cast_sign_loss)] // clamped non-negative pixel/scroll values
#![allow(clippy::cast_possible_wrap)] // bounded indices and pixel offsets
#![allow(clippy::cast_lossless)]
#![allow(clippy::trivially_copy_pass_by_ref)] // `&T` signatures required by Yew callbacks
#![allow(clippy::ref_option)] // `&Option<T>` signatures required by Yew props
#![allow(clippy::needless_pass_by_value)] // Yew props and callbacks take values by design
#![allow(clippy::match_same_arms)] // arms kept separate to mirror view variants
#![allow(clippy::assigning_clones)]
#![allow(clippy::too_many_lines)] // `html!` view bodies
#![allow(clippy::struct_excessive_bools)] // props mirror the backend config DTOs
#![allow(clippy::fn_params_excessive_bools)]
#![allow(clippy::struct_field_names)]
#![allow(clippy::map_unwrap_or)]
#![allow(clippy::manual_let_else)]
#![allow(clippy::unnecessary_wraps)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::similar_names)]
#![allow(clippy::option_option)]
#![allow(clippy::needless_for_each)]
#![allow(clippy::default_trait_access)]
#![allow(clippy::needless_continue)]
#![allow(clippy::match_wildcard_for_single_variants)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::format_push_string)]
#![allow(clippy::case_sensitive_file_extension_comparisons)]
#![allow(clippy::float_cmp)] // exact comparisons in view-model unit tests

pub mod app;
pub mod error;
pub mod hooks;
pub mod i18n;
pub mod model;
pub mod provider;
pub mod services;

pub mod utils;
