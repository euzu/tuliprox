use super::*;

#[test]
fn typed_url_never_proxied() {
    let typed_url = "https://example.com/images/custom_poster.jpg";
    let preview = resolve_preview_image_url(typed_url, false);

    assert_eq!(
        preview.as_deref(),
        Some(typed_url),
        "A user-typed URL must be loaded directly in browser and never sent to server or proxied"
    );

    let sealed_provider_url = "/api/v1/playlist/resource?token=abc123sealed";
    let provider_preview = resolve_preview_image_url(sealed_provider_url, true);
    assert_eq!(
        provider_preview.as_deref(),
        Some(sealed_provider_url),
        "Provider values with sealed resource endpoint should be kept intact"
    );
}
