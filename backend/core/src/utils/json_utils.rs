use crate::utils::async_file_writer;
use serde::Serialize;
use tokio::io::AsyncWriteExt;

pub async fn json_write_documents_to_file<T>(path: &std::path::Path, value: &T) -> std::io::Result<()>
where
    T: Serialize + Sync,
{
    // TODO this is not so optimal for memory usage, serde do not support async
    let file = tokio::fs::File::create(path).await?;
    let mut writer = async_file_writer(file);
    let json = serde_json::to_vec(value)?;
    writer.write_all(&json).await?;
    writer.flush().await?;
    Ok(())
}
