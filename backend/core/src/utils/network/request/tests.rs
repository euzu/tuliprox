mod tmdb_profile;

use super::{
    classify_resource_destination, download_text_content, download_text_content_with_headers_and_options,
    get_input_epg_content_as_file, get_remote_content_as_stream, is_safe_cross_origin_redirect_header,
    next_provider_url_index, preview_request_diagnostics_for_logging, preview_request_target_for_logging,
    resolve_attempt_target, resolve_resource_socket_addrs, same_origin,
    send_input_with_retry_and_provider_policy_with_manual_redirects_and_options_result,
    send_input_with_retry_and_provider_policy_with_options_result, send_with_retry_and_provider,
    send_with_retry_and_provider_policy, should_retry_text_body_error, should_try_next_ip_on_connect_error,
    strip_sensitive_headers_for_cross_origin_redirect, text_response_error_log_label, InputEpgFileRequest,
    PublicIpResolver, RequestFetchOptions, ResourceDestination, ResourceDestinationResolver, TextContentBodyOptions,
    TextContentFetchOptions, STREAM_IDLE_TIMEOUT,
};

mod behavior;
mod configuration;
mod lifecycle;
mod persistence;
mod protocol;
mod retry;
mod support;

use self::{
    lifecycle::start_hanging_http_server,
    support::{
        atomic_download_temp_files, identity_fetch_options, make_epg_test_client, make_provider_with_dns,
        make_test_app_config, request_header_value, response_with_body, start_plain_http_server_with_body,
        start_plain_http_server_with_response, start_recording_http_byte_server, start_recording_http_server,
        test_input_source, test_retry_config,
    },
};
