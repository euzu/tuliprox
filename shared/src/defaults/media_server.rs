//! Media-server catalog defaults.

default_eq_fns!(
    default_media_server_catalog_page_size, is_default_media_server_catalog_page_size, u16, 100;
    default_media_server_catalog_request_delay_ms, is_default_media_server_catalog_request_delay_ms, u64, 250;
);
