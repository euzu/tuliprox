use super::{
    download_text_content_with_headers_and_options, identity_fetch_options, make_epg_test_client, request_header_value,
    start_recording_http_server, test_input_source, test_retry_config, TextContentBodyOptions, TextContentFetchOptions,
};
use std::time::Duration;
use url::Url;

#[tokio::test]
async fn text_content_manifest_repeated_decoder_failures_stop_at_configured_budget() {
    let corrupt_gzip =
        "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 7\r\nConnection: close\r\n\r\ncorrupt";
    let (addr, requests, handle) =
        start_recording_http_server(vec![corrupt_gzip.to_string()]).await.expect("recording origin should start");
    let url = Url::parse(&format!("http://{addr}/manifest.m3u8")).expect("origin URL");
    let input = test_input_source(url.to_string(), None);
    let options = TextContentFetchOptions::new(
        identity_fetch_options(),
        TextContentBodyOptions::hls_manifest(1024, Duration::from_secs(1)),
    );

    download_text_content_with_headers_and_options(
        &test_retry_config(3),
        &make_epg_test_client(),
        &input,
        None,
        false,
        options,
    )
    .await
    .expect_err("decoder failures must exhaust the configured budget");

    let captured = requests.lock().await;
    assert_eq!(captured.len(), 3);
    assert!(captured.iter().all(|request| request_header_value(request, "accept-encoding") == Some("identity")));
    handle.abort();
}
