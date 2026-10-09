use super::*;

#[tokio::test]
async fn enqueue_with_backpressure_delivers_events_after_queue_full() {
    let (tx, mut rx) = mpsc::channel(1);
    let sender = BackpressureSender::new(tx.clone(), "test", 2);
    assert!(tx.send(1_u8).await.is_ok());

    sender.enqueue(2_u8);
    sender.enqueue(3_u8);

    assert_eq!(rx.recv().await, Some(1));
    assert_eq!(tokio::time::timeout(Duration::from_secs(1), rx.recv()).await.ok().flatten(), Some(2));
    assert_eq!(tokio::time::timeout(Duration::from_secs(1), rx.recv()).await.ok().flatten(), Some(3));
}
