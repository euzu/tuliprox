use super::*;

#[tokio::test]
async fn ts_inspector_sync_async_and_aes_prefix_paths_share_signature() {
    let budget = HlsTsProbeBudget { read_chunk_bytes: 191, ..HlsTsProbeBudget::default() };
    let mut plaintext = track_stream(400);
    let clear_signature = found(
        inspect_mpeg_ts(Cursor::new(&plaintext), HlsTsProbeProtection::Clear, budget).expect("sync probe succeeds"),
    );
    let async_clear_signature = found(
        inspect_mpeg_ts_async(&plaintext[..], HlsTsProbeProtection::Clear, budget)
            .await
            .expect("async clear probe succeeds"),
    );
    plaintext.resize(plaintext.len().next_multiple_of(AES_128_BLOCK_BYTES), 0xFF);
    let key = *b"0123456789abcdef";
    let iv = [0xA5; AES_128_BLOCK_BYTES];
    let ciphertext = encrypt_aes128_cbc(&plaintext, &key, iv);
    let async_aes_signature = found(
        inspect_mpeg_ts_async(&ciphertext[..], HlsTsProbeProtection::Aes128Cbc { key: &key, iv }, budget)
            .await
            .expect("async AES probe succeeds"),
    );

    assert_eq!(async_clear_signature, clear_signature);
    assert_eq!(async_aes_signature, clear_signature);
}

#[test]
fn ts_inspector_does_not_mutate_source_bytes() {
    let bytes = track_stream(0);
    let before = bytes.clone();
    let _ = inspect_mpeg_ts(Cursor::new(&bytes), HlsTsProbeProtection::Clear, HlsTsProbeBudget::default())
        .expect("probe succeeds");
    assert_eq!(bytes, before);
}
