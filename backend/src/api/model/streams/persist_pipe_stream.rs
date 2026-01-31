use crate::api::model::{StreamError, STREAM_IDLE_TIMEOUT};
use crate::utils::request::DynReader;
use crate::utils::{async_file_writer, IO_BUFFER_SIZE};
use bytes::Bytes;
use log::{debug, error};
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::select;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use tokio::time::{sleep, Duration, Instant};
use crate::utils::debug_if_enabled;

pub fn tee_stream<S, W>(
    mut stream: S,
    mut writer: W,
    file_path: &Path,
    callback: Arc<dyn Fn(usize) + Send + Sync>,
) -> ReceiverStream<Result<Bytes, StreamError>>
where
    S: tokio_stream::Stream<Item=Result<Bytes, StreamError>> + Send + Unpin + 'static,
    W: tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, StreamError>>(32);
    let resource_path = file_path.to_owned();

    tokio::spawn(async move {
        let mut total_size = 0usize;
        let mut writer_active = true;
        let mut write_err: Option<StreamError> = None;
        let mut write_counter = 0usize;

        let idle_timeout = Duration::from_secs(STREAM_IDLE_TIMEOUT);
        let idle = sleep(idle_timeout);
        tokio::pin!(idle);

        loop {
            select! {
                () = &mut idle => {
                   debug!("Persist pipe stream idle for too long, closing");
                   break;
                }

                chunk = stream.next() => {
                    idle.as_mut().reset(Instant::now() + idle_timeout);
                    match chunk {
                        Some(Ok(bytes)) => {
                            if writer_active {
                                total_size += bytes.len();
                                if let Err(e) = writer.write_all(&bytes).await {
                                    writer_active = false;
                                    write_err = Some(StreamError::StdIo(e.to_string()));
                                } else {
                                    write_counter += bytes.len();
                                    if write_counter >= IO_BUFFER_SIZE {
                                        write_counter = 0;
                                        if let Err(err) = writer.flush().await {
                                            writer_active = false;
                                            write_err = Some(StreamError::StdIo(format!("Failed periodic flush of tee_stream writer {err}")));
                                        }
                                    }
                                }
                            }

                            let _ = tx.send(Ok(bytes)).await;
                        }
                        Some(Err(e)) => {
                            let _ = tx.send(Err(e)).await;
                        }
                        None => {
                            debug_if_enabled!("Persist pipe stream ended. Closing {}", resource_path.display());
                            break;
                        }
                    }
               }
            }
        }

        // final flush & shutdown
        if writer_active {
            if let Err(e) = writer.flush().await {
                writer_active = false;
                write_err = Some(StreamError::StdIo(e.to_string()));
            }
        }
        let _ = writer.shutdown().await;

        if writer_active {
            debug!("Persisted {total_size} bytes to cache resource");
            (callback)(total_size);
        } else {
            if let Some(err) = write_err {
                error!("Persisted stream error: {err}.");
            }
            drop(writer);
            let _ = tokio::fs::remove_file(&resource_path).await;
        }
    });

    ReceiverStream::new(rx)
}

pub async fn tee_dyn_reader(
    reader: DynReader,
    persist_path: &Path,
    callback: Option<Arc<dyn Fn(usize) + Send + Sync>>,
) -> DynReader {
    let file = match tokio::fs::File::create(persist_path).await {
        Ok(f) => f,
        Err(err) => {
            error!("Can't open file to write: {}, {err}", persist_path.display());
            return reader;
        }
    };

    let (mut tx, rx) = tokio::io::duplex(IO_BUFFER_SIZE);
    let mut writer = async_file_writer(file);
    let reader_arc = reader;

    tokio::spawn(async move {
        let mut total_bytes = 0usize;
        let mut buf = [0u8; 8192];

        let mut reader = reader_arc;

        loop {
            let n = match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };

            total_bytes += n;

            if tx.write_all(&buf[..n]).await.is_err() {
                break;
            }

            if writer.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }

        let _ = writer.flush().await;
        let _ = tx.shutdown().await;

        if let Some(cb) = callback {
            cb(total_bytes);
        }
    });

    Box::pin(rx) as DynReader
}