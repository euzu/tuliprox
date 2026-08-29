//! Views onto [`AppState`]: the subsets each subsystem actually reads.
//!
//! Every extracted subsystem takes a small context struct - the handles it
//! needs - rather than the server's root state. Those structs live in their own
//! crates and cannot name `AppState`, so something has to build them, and this
//! is it.
//!
//! # Why the trait is defined here
//!
//! The obvious shape - a `FromAppState` trait in `tuliprox-core`, implemented by
//! each context beside its own definition - does not compile. The orphan rule
//! allows `impl ForeignTrait for LocalType` or `impl LocalTrait for ForeignType`,
//! and that arrangement is neither: the trait would be core's, the context would
//! be (say) `tuliprox-hls`'s, and the impl would have to live in this binary,
//! which owns neither. It also could not be written in `tuliprox-hls`, because
//! nothing there can name `AppState`.
//!
//! So the trait is local to the binary and the contexts are the foreign types.
//! That is the one arrangement the orphan rule permits, and it puts every view
//! in one file - which is the property worth having anyway: "what does this
//! subsystem read from the server?" is answerable without opening its crate.
//!
//! # What is not here
//!
//! `WeakHlsCtx` is derived from `HlsCtx`, not from `AppState`. `SharedStreamCtx`
//! borrows rather than owning its handles, so it cannot be built by value.
//! Neither is a view in this sense.

use super::AppState;

/// A subset of [`AppState`] that one subsystem reads.
pub trait AppStateView {
    /// Build the view by cloning the handles it names.
    fn from_app_state(state: &AppState) -> Self;
}

impl AppState {
    /// The subset that `V` reads.
    ///
    /// Prefer the named accessors below at call sites; this is what they call,
    /// and what generic code can use.
    pub fn view<V: AppStateView>(&self) -> V { V::from_app_state(self) }
}

/// Declare a view: the accessor name, the context type, and the `AppState`
/// fields it clones. Field names are the same on both sides by construction -
/// a view renaming a handle would be a view lying about what it reads.
macro_rules! app_state_views {
    ($(
        $(#[$meta:meta])*
        $accessor:ident => $ctx:ty { $($field:ident),+ $(,)? }
    )+) => {
        $(
            impl AppStateView for $ctx {
                fn from_app_state(state: &AppState) -> Self {
                    Self { $($field: ::core::clone::Clone::clone(&state.$field)),+ }
                }
            }

            impl AppState {
                $(#[$meta])*
                #[must_use]
                pub fn $accessor(&self) -> $ctx {
                    self.view()
                }
            }
        )+
    };
}

app_state_views! {
    /// The handles the DVR needs: the recording queue and what feeds it.
    recording_ctx => crate::api::model::recording::recording_ctx::RecordingCtx {
        app_config, recordings, event_manager, http_client, active_provider, connection_manager,
    }

    /// The handles the HLS proxy needs: itself, plus provider allocation and
    /// session accounting.
    hls_ctx => crate::api::model::hls_cache::HlsCtx {
        app_config, hls_proxy, active_provider, connection_manager, active_users,
    }

    /// The handles admission reads: it decides over connections and users.
    admission_ctx => tuliprox_session::admission::AdmissionCtx {
        app_config, active_users, connection_manager,
    }

    /// The handles the provider side of a stream needs: the redirect-aware
    /// HTTP clients.
    provider_stream_ctx => tuliprox_session::stream_ctx::ProviderStreamCtx {
        app_config, connection_manager, http_client_no_redirect, public_http_client_no_redirect,
    }

    /// The handles the background metadata worker reads.
    metadata_update_ctx => tuliprox_metadata::ctx::MetadataUpdateCtx {
        app_config, active_provider, connection_manager, event_manager, playlists, update_guard,
        http_client, http_client_no_redirect,
    }
}
