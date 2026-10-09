use super::{coalesce_byte_stream, stream_json_array_stream};
use bytes::Bytes;
use futures::{stream, StreamExt};
use http_body_util::BodyExt;

#[tokio::test]
async fn streamed_json_array_coalesces_small_entries() {
    let response = stream_json_array_stream(stream::iter(0..4_096u32));
    let mut body = response.into_body();
    let mut frames = 0usize;
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else {
            return;
        };
        if let Ok(data) = frame.into_data() {
            frames += 1;
            bytes.extend_from_slice(&data);
        }
    }
    assert!(frames <= 2, "small JSON entries should be coalesced, got {frames} frames");
    let decoded = serde_json::from_slice::<Vec<u32>>(&bytes);
    assert!(decoded.is_ok_and(|values| values.len() == 4_096));
}

#[tokio::test]
async fn coalesced_stream_remains_finished_when_polled_again() {
    let stream = coalesce_byte_stream(stream::empty::<Result<Bytes, ()>>());
    futures::pin_mut!(stream);

    assert!(stream.next().await.is_none());
    assert!(stream.next().await.is_none());
}
