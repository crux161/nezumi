use anyhow::Result;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;

use nezumi::core::{MediaPacket, NezumiConsumer};

pub struct FileConsumer {
    file: File,
}

impl FileConsumer {
    pub async fn new() -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .write(true)
            .open("output.h265")
            .await?;

        Ok(Self { file })
    }
}

#[async_trait::async_trait]
impl NezumiConsumer for FileConsumer {
    async fn consume(&mut self, packet: MediaPacket) -> Result<()> {
        self.file.write_all(&packet.payload).await?;
        Ok(())
    }
}
