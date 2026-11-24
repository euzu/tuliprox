use super::{recording_content_type, AppendRecordingVod};
use futures::StreamExt;
use shared::error::TuliproxError;

fn base_stream(
    items: Vec<Result<(String, bool), TuliproxError>>,
) -> futures::stream::Iter<std::vec::IntoIter<Result<(String, bool), TuliproxError>>> {
    futures::stream::iter(items)
}

async fn collect<S>(adapter: AppendRecordingVod<S>) -> Vec<Result<(String, bool), TuliproxError>>
where
    S: futures::Stream<Item = Result<(String, bool), TuliproxError>> + Unpin,
{
    adapter.collect().await
}

#[tokio::test]
async fn empty_extras_pass_the_base_stream_through_unchanged() {
    let base = vec![Ok((r#"{"stream_id":1}"#.to_string(), true)), Ok((r#"{"stream_id":2}"#.to_string(), false))];

    let out = collect(AppendRecordingVod::new(base_stream(base), Vec::new())).await;

    let entries: Vec<(String, bool)> = out.into_iter().map(Result::unwrap).collect();
    assert_eq!(entries, vec![(r#"{"stream_id":1}"#.to_string(), true), (r#"{"stream_id":2}"#.to_string(), false)]);
}

#[tokio::test]
async fn extras_are_appended_after_the_last_base_entry() {
    let base = vec![Ok((r#"{"stream_id":1}"#.to_string(), false))];
    let extras = vec![r#"{"stream_id":1000000000}"#.to_string(), r#"{"stream_id":1000000001}"#.to_string()];

    let out = collect(AppendRecordingVod::new(base_stream(base), extras)).await;

    let entries: Vec<(String, bool)> = out.into_iter().map(Result::unwrap).collect();
    assert_eq!(
        entries,
        vec![
            (r#"{"stream_id":1}"#.to_string(), true),
            (r#"{"stream_id":1000000000}"#.to_string(), true),
            (r#"{"stream_id":1000000001}"#.to_string(), false),
        ]
    );
}

#[tokio::test]
async fn extras_only_stream_yields_exact_element_count() {
    let extras = vec![r#"{"a":1}"#.to_string(), r#"{"a":2}"#.to_string(), r#"{"a":3}"#.to_string()];

    let out = collect(AppendRecordingVod::new(base_stream(Vec::new()), extras)).await;

    let entries: Vec<(String, bool)> = out.into_iter().map(Result::unwrap).collect();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries.iter().filter(|(_, has_next)| *has_next).count(), 2);
}

#[tokio::test]
async fn a_base_error_is_propagated_and_ends_the_stream() {
    let base = vec![
        Ok((r#"{"stream_id":1}"#.to_string(), true)),
        Err(TuliproxError::RepositoryXtream("corrupt page".to_string())),
    ];
    let extras = vec![r#"{"stream_id":1000000000}"#.to_string()];

    let out = collect(AppendRecordingVod::new(base_stream(base), extras)).await;

    assert_eq!(out.len(), 2);
    assert_eq!(out[0].as_ref().expect("first entry"), &(r#"{"stream_id":1}"#.to_string(), false));
    let error = out[1].as_ref().expect_err("second entry is the base error");
    assert_eq!(error.kind(), shared::error::ErrorKind::RepositoryXtream);
    assert_eq!(error.message(), "corrupt page");
}

#[tokio::test]
async fn a_base_error_without_a_pending_entry_is_propagated_and_drops_the_extras() {
    let base = vec![Err(TuliproxError::RepositoryXtream("open failed".to_string()))];
    let extras = vec![r#"{"stream_id":1000000000}"#.to_string()];

    let out = collect(AppendRecordingVod::new(base_stream(base), extras)).await;

    assert_eq!(out.len(), 1);
    let error = out[0].as_ref().expect_err("the only entry is the base error");
    assert_eq!(error.kind(), shared::error::ErrorKind::RepositoryXtream);
    assert_eq!(error.message(), "open failed");
}

#[test]
fn content_type_follows_the_container_extension() {
    assert_eq!(recording_content_type("show.ts"), "video/mp2t");
    assert_eq!(recording_content_type("movie.mp4"), "video/mp4");
    assert_eq!(recording_content_type("archive.mkv"), "video/x-matroska");
    assert_eq!(recording_content_type("no-extension"), "application/octet-stream");
    assert_eq!(recording_content_type("weird.xyz"), "application/octet-stream");
}
