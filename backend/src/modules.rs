#[macro_export]
macro_rules! include_modules {
    () => {
        extern crate core;
        extern crate env_logger;
        extern crate pest;
        pub mod api;
        pub mod auth;
        pub mod config_loader;
        pub mod iptv;
        pub mod library;
        pub mod media_enrichment;
        pub mod messaging;
        pub mod processing;
        pub mod ptt;
        pub mod repository;
        pub mod runtime_config_report;
    };
}
