#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::dbg_macro))]

//! Provider allocation and the streaming-session runtime.
//!
//! This is the layer that decides *who gets a stream and from which provider*:
//! provider allocation and lineup rotation, admission and eviction of
//! connections, per-user session accounting, the event bus those parts talk
//! over, connection metering, and the shared-stream fan-out that lets several
//! clients ride one upstream connection.
//!
//! These pieces are one package rather than several because they are mutually
//! recursive at the value level - `ConnectionManager` holds an
//! `Arc<SharedStreamManager>`, which holds an `Arc<ActiveProviderManager>`,
//! which holds a back-reference to the `SharedStreamManager`. Splitting them
//! would require callback traits solely to break the ownership cycle without
//! improving the runtime boundary.
//!
//! Nothing here names `AppState`: the runtime takes the handles it needs.

pub mod active_provider_manager;
pub mod active_user_manager;
pub mod admission;
pub mod admission_strategy;
pub mod connection_manager;
pub mod event_manager;
pub mod meter;
pub mod meter_registry;
pub mod provider_dns_manager;
pub mod provider_lineup_manager;
pub mod qos_aggregation_manager;
pub mod response_headers;
pub mod stream;
pub mod stream_ctx;
pub mod stream_options;
pub mod streams;

pub use self::{
    active_provider_manager::*, active_user_manager::*, admission::*, admission_strategy::*, connection_manager::*,
    event_manager::*, meter::*, meter_registry::*, provider_lineup_manager::*, stream::*, streams::*,
};
