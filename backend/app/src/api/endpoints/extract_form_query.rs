use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
};
use serde::de::DeserializeOwned;

/// Query extractor that, unlike `axum::extract::Query`, accepts repeated keys
/// (`?field=a&field=b`) and a single value for `Vec<_>` fields.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct FormQuery<T>(pub T);

impl<T, S> FromRequestParts<S> for FormQuery<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = (StatusCode, String);

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        let result = serde_html_form::from_str(parts.uri.query().unwrap_or_default())
            .map(FormQuery)
            .map_err(|err| (StatusCode::BAD_REQUEST, format!("Failed to parse query: {err}")));
        std::future::ready(result)
    }
}

#[cfg(test)]
mod tests {
    use super::FormQuery;
    use axum::{extract::FromRequestParts, http::Request};
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Params {
        page: Option<u32>,
        #[serde(rename = "field")]
        fields: Option<Vec<String>>,
    }

    async fn extract(uri: &str) -> Result<Params, (axum::http::StatusCode, String)> {
        let (mut parts, ()) = Request::builder().uri(uri).body(()).unwrap().into_parts();
        FormQuery::<Params>::from_request_parts(&mut parts, &()).await.map(|FormQuery(params)| params)
    }

    #[tokio::test]
    async fn repeated_keys_fill_a_vec() {
        let params = extract("/x?page=2&field=a&field=b").await.unwrap();
        assert_eq!(params, Params { page: Some(2), fields: Some(vec!["a".into(), "b".into()]) });
    }

    #[tokio::test]
    async fn a_single_key_fills_a_vec() {
        let params = extract("/x?field=a").await.unwrap();
        assert_eq!(params.fields, Some(vec!["a".to_string()]));
    }

    #[tokio::test]
    async fn missing_query_yields_defaults() {
        let params = extract("/x").await.unwrap();
        assert_eq!(params, Params { page: None, fields: None });
    }

    #[tokio::test]
    async fn invalid_value_is_a_bad_request() {
        let (status, _) = extract("/x?page=abc").await.unwrap_err();
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    }
}
