//! The tuliprox plugin contract.
//!
//! `wit/tuliprox-plugin.wit` is the single source of truth, frozen at ABI
//! [`ABI_VERSION`]. A guest SDK generates its bindings from that directory; the
//! host (the `host` feature) generates its bindings here with
//! `wasmtime::component::bindgen!`.
//!
//! The WIT is pinned to wasmtime 50.0.1: `wit/deps` holds `wasi:*@0.2.12`
//! exactly as `wasmtime-wasi-http` 50.0.1 ships it.
#![recursion_limit = "256"]

/// Package id of the contract.
pub const WIT_PACKAGE: &str = "tuliprox:plugin@1.0.0";

/// ABI version of the contract. Equal to the WIT package version.
pub const ABI_VERSION: &str = "1.0.0";

/// ABI major. A manifest's `api_version` must equal it.
pub const ABI_MAJOR: u32 = 1;

/// The WIT source of the whole contract.
pub const WIT_SOURCE: &str = include_str!("../wit/tuliprox-plugin.wit");

/// Every interface a plugin may export, as the fully qualified component export
/// name the registry looks up. `lifecycle` is mandatory; each other entry is
/// one manifest hook family.
pub const EXPORT_INTERFACES: [&str; 24] = [
    "tuliprox:plugin/lifecycle@1.0.0",
    "tuliprox:plugin/playlist-hooks@1.0.0",
    "tuliprox:plugin/source-hooks@1.0.0",
    "tuliprox:plugin/sort-hooks@1.0.0",
    "tuliprox:plugin/metadata-hooks@1.0.0",
    "tuliprox:plugin/sort-key-hooks@1.0.0",
    "tuliprox:plugin/chno-hooks@1.0.0",
    "tuliprox:plugin/dedupe-hooks@1.0.0",
    "tuliprox:plugin/group-merge-hooks@1.0.0",
    "tuliprox:plugin/filter-hooks@1.0.0",
    "tuliprox:plugin/epg-load-hooks@1.0.0",
    "tuliprox:plugin/epg-transform-hooks@1.0.0",
    "tuliprox:plugin/epg-enrich-hooks@1.0.0",
    "tuliprox:plugin/epg-match-hooks@1.0.0",
    "tuliprox:plugin/epg-conflict-hooks@1.0.0",
    "tuliprox:plugin/stream-hooks@1.0.0",
    "tuliprox:plugin/event-hooks@1.0.0",
    "tuliprox:plugin/metrics-hooks@1.0.0",
    "tuliprox:plugin/notification-hooks@1.0.0",
    "tuliprox:plugin/input-provider@1.0.0",
    "tuliprox:plugin/output-generator@1.0.0",
    "tuliprox:plugin/rpc@1.0.0",
    "tuliprox:plugin/tasks@1.0.0",
    "tuliprox:plugin/settings-migration@1.0.0",
];

/// Host bindings for the whole contract.
///
/// They are generated over a host-only world (`tuliprox:host/union`, defined
/// inline here and never shipped to guests) that includes every capability
/// world. That yields ONE set of types and ONE `host::Host` trait for all
/// plugins. A component implements only the worlds it targets, so never
/// instantiate through the union's world-level `UnionPre`, which demands every
/// export. Bind per interface instead: each
/// `exports::tuliprox::plugin::<iface>::GuestIndices::new(&instance_pre)`
/// fails with "no exported instance" when the component lacks that export,
/// which is exactly the "hook absent" signal.
///
/// Every host import (`host`, `wasi:http/outgoing-handler`) is async and every
/// export call is async, so a hook runs under the caller's runtime and never
/// blocks a thread. The `wasi:*` packages are mapped onto the types
/// `wasmtime-wasi` and `wasmtime-wasi-http` already define, so one linker
/// serves both.
#[cfg(feature = "host")]
#[allow(clippy::all, clippy::pedantic, missing_docs, unused_qualifications)]
pub mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        inline: "
            package tuliprox:host;

            world union {
                include tuliprox:plugin/playlist-plugin@1.0.0;
                include tuliprox:plugin/source-plugin@1.0.0;
                include tuliprox:plugin/sort-plugin@1.0.0;
                include tuliprox:plugin/metadata-plugin@1.0.0;
                include tuliprox:plugin/sort-key-plugin@1.0.0;
                include tuliprox:plugin/chno-plugin@1.0.0;
                include tuliprox:plugin/dedupe-plugin@1.0.0;
                include tuliprox:plugin/group-merge-plugin@1.0.0;
                include tuliprox:plugin/filter-plugin@1.0.0;
                include tuliprox:plugin/epg-load-plugin@1.0.0;
                include tuliprox:plugin/epg-transform-plugin@1.0.0;
                include tuliprox:plugin/epg-enrich-plugin@1.0.0;
                include tuliprox:plugin/epg-match-plugin@1.0.0;
                include tuliprox:plugin/epg-conflict-plugin@1.0.0;
                include tuliprox:plugin/stream-plugin@1.0.0;
                include tuliprox:plugin/event-plugin@1.0.0;
                include tuliprox:plugin/metrics-plugin@1.0.0;
                include tuliprox:plugin/notifier-plugin@1.0.0;
                include tuliprox:plugin/input-plugin@1.0.0;
                include tuliprox:plugin/output-plugin@1.0.0;
                include tuliprox:plugin/rpc-plugin@1.0.0;
                include tuliprox:plugin/task-plugin@1.0.0;
                include tuliprox:plugin/settings-migration-plugin@1.0.0;
            }
        ",
        world: "tuliprox:host/union",
        imports: { default: async },
        exports: { default: async },
        with: {
            "wasi:io": wasmtime_wasi::p2::bindings::io,
            "wasi:clocks": wasmtime_wasi::p2::bindings::clocks,
            "wasi:http": wasmtime_wasi_http::p2::bindings::http,
        },
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wit_source_declares_the_frozen_package() {
        assert!(WIT_SOURCE.contains(&format!("package {WIT_PACKAGE};")));
        assert_eq!(WIT_PACKAGE, format!("tuliprox:plugin@{ABI_VERSION}"));
        assert!(ABI_VERSION.starts_with(&format!("{ABI_MAJOR}.")));
    }

    #[test]
    fn every_export_interface_is_declared_and_has_a_world() {
        for export in EXPORT_INTERFACES {
            let name = export
                .strip_prefix("tuliprox:plugin/")
                .and_then(|rest| rest.strip_suffix(&format!("@{ABI_VERSION}")))
                .expect("fully qualified 1.0.0 export name");
            assert!(WIT_SOURCE.contains(&format!("interface {name} {{")), "{name} is not declared");
            assert!(WIT_SOURCE.contains(&format!("export {name};")), "{name} has no capability world");
        }
    }

    #[test]
    fn the_wit_declares_no_unlisted_export() {
        let exported = WIT_SOURCE.lines().filter_map(|line| line.split("export ").nth(1)).count();
        assert_eq!(exported, EXPORT_INTERFACES.len());
    }

    #[cfg(feature = "host")]
    #[test]
    fn host_bindings_expose_one_accessor_per_interface() {
        // Compile-time check: each export has a per-interface GuestIndices.
        use bindings::exports::tuliprox::plugin as e;
        let _ = (
            e::lifecycle::GuestIndices::new::<()>,
            e::playlist_hooks::GuestIndices::new::<()>,
            e::source_hooks::GuestIndices::new::<()>,
            e::sort_hooks::GuestIndices::new::<()>,
            e::metadata_hooks::GuestIndices::new::<()>,
            e::sort_key_hooks::GuestIndices::new::<()>,
            e::chno_hooks::GuestIndices::new::<()>,
            e::dedupe_hooks::GuestIndices::new::<()>,
            e::group_merge_hooks::GuestIndices::new::<()>,
            e::filter_hooks::GuestIndices::new::<()>,
            e::epg_load_hooks::GuestIndices::new::<()>,
            e::epg_transform_hooks::GuestIndices::new::<()>,
        );
        let _ = (
            e::epg_enrich_hooks::GuestIndices::new::<()>,
            e::epg_match_hooks::GuestIndices::new::<()>,
            e::epg_conflict_hooks::GuestIndices::new::<()>,
            e::stream_hooks::GuestIndices::new::<()>,
            e::event_hooks::GuestIndices::new::<()>,
            e::metrics_hooks::GuestIndices::new::<()>,
            e::notification_hooks::GuestIndices::new::<()>,
            e::input_provider::GuestIndices::new::<()>,
            e::output_generator::GuestIndices::new::<()>,
            e::rpc::GuestIndices::new::<()>,
            e::tasks::GuestIndices::new::<()>,
            e::settings_migration::GuestIndices::new::<()>,
        );
    }
}
