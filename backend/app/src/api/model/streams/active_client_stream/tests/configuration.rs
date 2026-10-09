use super::{assert_missing_custom_video_terminates, StreamMode};

#[tokio::test]
async fn test_provisioning_without_custom_video_terminates_immediately_with_timeout_configured() {
    assert_missing_custom_video_terminates(StreamMode::Provisioning, true).await;
}
