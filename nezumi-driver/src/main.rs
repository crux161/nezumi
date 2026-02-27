mod file_dump;

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio::time::timeout;

use file_dump::FileConsumer;
use nezumi::core::{NezumiConsumer, NezumiProducer};
use nezumi::windows_mf::WindowsMfProducer;

#[tokio::main]
async fn main() -> Result<()> {
    let mut producer =
        WindowsMfProducer::new().context("Failed to initialize Windows MF producer")?;
    let mut consumer = FileConsumer::new()
        .await
        .context("Failed to initialize file consumer for output.h265")?;

    let (tx, mut rx) = mpsc::unbounded_channel();

    let router_task = tokio::spawn(async move {
        while let Some(packet) = rx.recv().await {
            consumer
                .consume(packet)
                .await
                .context("Failed writing packet payload to output.h265")?;
        }

        Ok::<(), anyhow::Error>(())
    });

    match timeout(Duration::from_secs(10), producer.start_reading(tx)).await {
        Ok(result) => {
            result.context("Producer exited before timeout with an error")?;
        }
        Err(_) => {
            println!("INFO: Nezumi Driver shutting down. H.265 dump complete.");
        }
    }

    drop(producer);

    match timeout(Duration::from_secs(2), router_task).await {
        Ok(joined) => joined.context("Router task join failure")??,
        Err(_) => {}
    }

    Ok(())
}
