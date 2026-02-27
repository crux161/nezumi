use anyhow::Result;
use bytes::Bytes;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub enum TrackKind {
    Video,
    Audio,
}

#[derive(Debug, Clone)]
pub struct MediaTrack {
    pub id: u32,
    pub kind: TrackKind,
    pub codec: u8, // e.g., 0x01 for HEVC
}

#[derive(Debug)]
pub struct MediaPacket {
    pub track_id: u32,
    pub pts: u64,
    pub is_keyframe: bool,
    pub payload: Bytes,
}

#[async_trait::async_trait]
pub trait NezumiProducer: Send + Sync {
    fn tracks(&self) -> Vec<MediaTrack>;
    async fn start_reading(&mut self, sender: mpsc::UnboundedSender<MediaPacket>) -> Result<()>;
}

#[async_trait::async_trait]
pub trait NezumiConsumer: Send + Sync {
    async fn consume(&mut self, packet: MediaPacket) -> Result<()>;
}
