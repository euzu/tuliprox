// Shared clippy policy: see [workspace.lints.clippy] in the root Cargo.toml.
//
// `clippy::all` + `clippy::pedantic` are switched on workspace-wide by the
// modularization plan's Phase 0. Everything clippy can rewrite mechanically has
// been applied with `cargo clippy --fix`. The lints below are the residue that
// has no machine-applicable fix and would need hand edits to DTO, serde and
// parsing code that Phase 0 is explicitly not allowed to change the behaviour of.
//
// They are crate-local debt, not workspace policy: the backend crate — the one
// this plan actually modularizes — is held to the full policy with no such list,
// and any crate extracted from it inherits the strict policy rather than this one.
// Burn these down in their own change batches, never inside an extraction.
#![allow(clippy::cast_possible_truncation)] // bounded lengths/ids in wire DTOs
#![allow(clippy::cast_possible_wrap)] // bounded lengths/ids in wire DTOs
#![allow(clippy::cast_precision_loss)] // integer -> f64 for display/ratio maths
#![allow(clippy::cast_sign_loss)] // bounded non-negative values
#![allow(clippy::trivially_copy_pass_by_ref)] // `&T` predicates required by serde `skip_serializing_if`
#![allow(clippy::ref_option)] // `&Option<T>` required by serde `skip_serializing_if`
#![allow(clippy::unreadable_literal)] // checked-in table constants
#![allow(clippy::match_same_arms)] // arms kept separate to mirror the DTO variant order
#![allow(clippy::struct_excessive_bools)] // config DTOs mirror the user-facing YAML shape
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::unnecessary_wraps)] // signatures kept uniform across DTO conversions
#![allow(clippy::manual_let_else)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::format_collect)]
#![allow(clippy::assigning_clones)]
#![allow(clippy::unused_self)]
#![allow(clippy::similar_names)]
#![allow(clippy::needless_continue)]
#![allow(clippy::float_cmp)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::missing_fields_in_debug)]
#![allow(clippy::implicit_hasher)]
#![allow(clippy::default_trait_access)]

pub mod defaults;
pub mod error;
pub mod foundation;
pub mod model;
pub mod utils;
