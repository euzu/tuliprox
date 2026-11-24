use shared::model::{
    HEADER_TULIPROX_FALLBACK, HEADER_TULIPROX_PLAYER_API_PATH, HEADER_TULIPROX_REJECTION, HEADER_TULIPROX_STREAM_UID,
};

pub(crate) fn create_cors_layer<const N: usize>(methods: [axum::http::Method; N]) -> tower_http::cors::CorsLayer {
    tower_http::cors::CorsLayer::new()
        .allow_origin(tower_http::cors::Any)
        .allow_methods(methods)
        .allow_headers(tower_http::cors::Any)
        .expose_headers([
            axum::http::HeaderName::from_static(HEADER_TULIPROX_FALLBACK),
            axum::http::HeaderName::from_static(HEADER_TULIPROX_REJECTION),
            axum::http::HeaderName::from_static(HEADER_TULIPROX_STREAM_UID),
            axum::http::HeaderName::from_static(HEADER_TULIPROX_PLAYER_API_PATH),
        ])
        .max_age(std::time::Duration::from_hours(1))
}

#[cfg(test)]
mod tests {
    use super::create_cors_layer;
    use axum::{
        body::Body,
        http::{header, Method, Request, StatusCode},
        response::IntoResponse,
        routing::get,
        Router,
    };
    use shared::model::{
        HEADER_TULIPROX_FALLBACK, HEADER_TULIPROX_PLAYER_API_PATH, HEADER_TULIPROX_REJECTION,
        HEADER_TULIPROX_STREAM_UID,
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn cors_exposes_fallback_and_rejection_headers() {
        let app = Router::new()
            .route("/test", get(|| async { StatusCode::OK.into_response() }))
            .layer(create_cors_layer([Method::GET]));

        let request = Request::builder()
            .method(Method::GET)
            .uri("/test")
            .header(header::ORIGIN, "http://example.com")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let expose = response
            .headers()
            .get(header::ACCESS_CONTROL_EXPOSE_HEADERS)
            .expect("access-control-expose-headers present")
            .to_str()
            .unwrap();

        assert!(
            expose.contains(HEADER_TULIPROX_FALLBACK),
            "expected expose headers to contain {HEADER_TULIPROX_FALLBACK}, got {expose}"
        );
        assert!(
            expose.contains(HEADER_TULIPROX_REJECTION),
            "expected expose headers to contain {HEADER_TULIPROX_REJECTION}, got {expose}"
        );
        assert!(
            expose.contains(HEADER_TULIPROX_STREAM_UID),
            "expected expose headers to contain {HEADER_TULIPROX_STREAM_UID}, got {expose}"
        );
        assert!(
            expose.contains(HEADER_TULIPROX_PLAYER_API_PATH),
            "expected expose headers to contain {HEADER_TULIPROX_PLAYER_API_PATH}, got {expose}"
        );
    }

    #[tokio::test]
    async fn cors_allows_delete_method() {
        let app = Router::new()
            .route("/test", get(|| async { StatusCode::OK.into_response() }))
            .layer(create_cors_layer([Method::GET, Method::POST, Method::DELETE]));

        let request = Request::builder()
            .method(Method::OPTIONS)
            .uri("/test")
            .header(header::ORIGIN, "http://example.com")
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "DELETE")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let allow_methods = response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_METHODS)
            .expect("access-control-allow-methods present")
            .to_str()
            .unwrap();

        assert!(allow_methods.contains("DELETE"), "expected allow methods to contain DELETE, got {allow_methods}");
    }
}
