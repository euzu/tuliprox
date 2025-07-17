#[macro_export]
macro_rules! include_modules {
    () => {
        extern crate core;
        extern crate env_logger;
        extern crate pest;
        pub mod api;
        pub mod auth;
        pub mod messaging;
        pub mod model;
        pub mod processing;
        pub mod repository;
        pub mod utils;
        pub mod tools;
    }
}

