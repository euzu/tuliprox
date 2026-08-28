// Shared clippy policy: see [workspace.lints.clippy] in the root Cargo.toml.
//
// `clippy::all` + `clippy::pedantic` reach this crate for the first time. Everything
// with a machine-applicable fix has already been applied via `cargo clippy --fix`.
// What remains needs hand edits inside Yew `html!` macro bodies and view state,
// which this policy must not touch: it is a behaviour-preserving lint-policy
// change, not a frontend rewrite.
//
// The list below is tracked debt, not workspace policy — the backend is held
// to the full policy without any such list.
#![allow(clippy::cast_precision_loss)] // integer -> f64 for chart/layout maths
#![allow(clippy::cast_possible_truncation)] // f64 -> integer pixel coordinates
#![allow(clippy::cast_sign_loss)] // clamped non-negative pixel/scroll values
#![allow(clippy::cast_possible_wrap)] // bounded indices and pixel offsets
#![allow(clippy::cast_lossless)] // explicit casts keep chart calculations visually aligned
#![allow(clippy::trivially_copy_pass_by_ref)] // `&T` signatures required by Yew callbacks
#![allow(clippy::ref_option)] // `&Option<T>` signatures required by Yew props
#![allow(clippy::needless_pass_by_value)] // Yew props and callbacks take values by design
#![allow(clippy::match_same_arms)] // arms kept separate to mirror view variants
#![allow(clippy::assigning_clones)] // component state retains values supplied by properties
#![allow(clippy::too_many_lines)] // `html!` view bodies
#![allow(clippy::struct_excessive_bools)] // props mirror the backend config DTOs
#![allow(clippy::fn_params_excessive_bools)] // form helpers mirror boolean component properties
#![allow(clippy::struct_field_names)] // DTO-backed view models keep domain field names
#![allow(clippy::map_unwrap_or)] // existing option mapping keeps view fallbacks adjacent
#![allow(clippy::manual_let_else)] // branches follow Yew rendering flow
#![allow(clippy::unnecessary_wraps)] // callback and validation signatures stay uniform
#![allow(clippy::items_after_statements)] // local Yew helpers remain next to their use
#![allow(clippy::similar_names)] // UI domain terms intentionally differ only by qualifier
#![allow(clippy::option_option)] // nested options distinguish unchanged, cleared and set values
#![allow(clippy::needless_for_each)] // side-effecting DOM and state iteration
#![allow(clippy::default_trait_access)] // explicit defaults identify component state types
#![allow(clippy::needless_continue)] // rendering loops keep skipped variants explicit
#![allow(clippy::match_wildcard_for_single_variants)] // wildcard keeps views forward-compatible with DTO variants
#![allow(clippy::many_single_char_names)] // coordinate maths uses conventional axis names
#![allow(clippy::format_push_string)] // incremental HTML/text previews are intentionally assembled in place
#![allow(clippy::case_sensitive_file_extension_comparisons)] // generated filenames use canonical lowercase extensions
#![allow(clippy::float_cmp)] // exact comparisons in view-model unit tests

pub mod app;
pub mod error;
pub mod hooks;
pub mod i18n;
pub mod model;
pub mod provider;
pub mod services;

pub mod utils;
