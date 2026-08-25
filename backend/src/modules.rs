#[macro_export]
macro_rules! include_modules {
    () => {
        extern crate core;
        extern crate env_logger;
        extern crate pest;
        pub mod api;
        pub mod auth;
        pub mod iptv;
        pub mod library;
        pub mod media_enrichment;
        pub mod messaging;
        pub mod model;
        pub mod mpegts;
        pub mod processing;
        pub mod ptt;
        pub mod repository;
        pub mod runtime_config_report;
        pub mod utils;
    };
}
