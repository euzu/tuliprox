use super::{
    super::{Aes128, Arc, AsyncReadExt, AsyncWriteExt, AtomicUsize, Block, Ordering, RwLock, TcpListener},
    path_has_extension, TestSegmentOrigin,
};
use aes::cipher::{BlockEncrypt, KeyInit as _};

pub(in crate::api::endpoints::hls_api::tests) const AES_TEST_MANIFEST: &[u8] = b"#EXTM3U\n#EXT-X-VERSION:5\n#EXT-X-TARGETDURATION:12\n#EXT-X-MEDIA-SEQUENCE:77\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",KEYFORMAT=\"identity\",KEYFORMATVERSIONS=\"1\"\n#EXTINF:12,\n77.ts\n#EXTINF:12,\n78.ts\n#EXTINF:12,\n79.ts\n#EXTINF:12,\n80.ts\n#EXTINF:12,\n81.ts\n#EXTINF:12,\n82.ts\n";

pub(in crate::api::endpoints::hls_api::tests) const AES_TEST_KEY_BYTES: &[u8] = b"0123456789abcdef";

pub(in crate::api::endpoints::hls_api::tests) const AES_TEST_PLAINTEXT_SEGMENT: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"));

pub(in crate::api::endpoints::hls_api::tests) async fn spawn_test_encrypted_hls_origin(
    manifest: &'static [u8],
    key_bytes: Arc<[u8]>,
    plaintext_segment: Arc<[u8]>,
) -> TestSegmentOrigin {
    let manifest = Arc::<[u8]>::from(manifest);
    let key_bytes = Arc::new(RwLock::new(key_bytes));
    let key_bytes_for_task = Arc::clone(&key_bytes);
    let key_requests = Arc::new(AtomicUsize::new(0));
    let key_requests_for_task = Arc::clone(&key_requests);
    let manifest_requests = Arc::new(AtomicUsize::new(0));
    let manifest_requests_for_task = Arc::clone(&manifest_requests);
    let segment_requests = Arc::new(AtomicUsize::new(0));
    let segment_requests_for_task = Arc::clone(&segment_requests);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
    let addr = listener.local_addr().expect("local addr");
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let manifest = Arc::clone(&manifest);
            let key_bytes = Arc::clone(&key_bytes_for_task);
            let key_requests = Arc::clone(&key_requests_for_task);
            let manifest_requests = Arc::clone(&manifest_requests_for_task);
            let segment_requests = Arc::clone(&segment_requests_for_task);
            let plaintext_segment = Arc::clone(&plaintext_segment);
            tokio::spawn(async move {
                let mut request = vec![0_u8; 2048];
                let Ok(read) = socket.read(&mut request).await else {
                    return;
                };
                if read == 0 {
                    return;
                }
                let path = String::from_utf8_lossy(&request[..read])
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .map_or_else(|| "/".to_string(), str::to_owned);
                let current_key_bytes = Arc::clone(&*key_bytes.read().await);
                let body = if path_has_extension(&path, "m3u8") {
                    manifest_requests.fetch_add(1, Ordering::SeqCst);
                    manifest
                } else if path.ends_with("key.bin") {
                    key_requests.fetch_add(1, Ordering::SeqCst);
                    current_key_bytes
                } else if let Some(origin_sequence) = path
                    .rsplit('/')
                    .next()
                    .and_then(|file| file.strip_suffix(".ts"))
                    .and_then(|value| value.parse::<u64>().ok())
                {
                    segment_requests.fetch_add(1, Ordering::SeqCst);
                    Arc::from(encrypt_test_aes128_cbc_pkcs7(
                        &plaintext_segment,
                        &current_key_bytes,
                        test_hls_sequence_iv(origin_sequence),
                    ))
                } else {
                    Arc::<[u8]>::from([])
                };
                let response =
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                if socket.write_all(response.as_bytes()).await.is_ok() {
                    let _ = socket.write_all(&body).await;
                }
            });
        }
    });
    TestSegmentOrigin {
        base_url: format!("http://{addr}"),
        key_requests,
        manifest_requests,
        segment_requests,
        key_bytes: Some(key_bytes),
        task,
    }
}

pub(in crate::api::endpoints::hls_api::tests) fn encrypt_test_aes128_cbc_pkcs7(
    plaintext: &[u8],
    key: &[u8],
    iv: [u8; 16],
) -> Vec<u8> {
    let padding_len = 16 - (plaintext.len() % 16);
    let mut ciphertext = plaintext.to_vec();
    ciphertext.resize(
        plaintext.len().saturating_add(padding_len),
        u8::try_from(padding_len).expect("PKCS#7 AES-128 padding fits in u8"),
    );
    let cipher = Aes128::new_from_slice(key).expect("test key has AES-128 length");
    let mut previous = iv;
    for block in ciphertext.as_chunks_mut::<16>().0 {
        for (byte, previous) in block.iter_mut().zip(previous) {
            *byte ^= previous;
        }
        let mut encrypted = Block::<Aes128>::default();
        encrypted.copy_from_slice(block);
        cipher.encrypt_block(&mut encrypted);
        block.copy_from_slice(&encrypted);
        previous.copy_from_slice(block);
    }
    ciphertext
}

pub(in crate::api::endpoints::hls_api::tests) fn test_hls_sequence_iv(sequence: u64) -> [u8; 16] {
    let mut iv = [0_u8; 16];
    iv[8..].copy_from_slice(&sequence.to_be_bytes());
    iv
}
