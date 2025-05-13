use std::sync::Arc;
use arc_swap::ArcSwap;
use chrono::{Local, DateTime};
use solana_sdk::hash::Hash;
use flume::Receiver;
use yellowstone_grpc_proto::prelude::SubscribeUpdateBlockMeta;
use std::str::FromStr;

/// Structure to hold the latest block metadata.
#[derive(Debug)]
pub struct BlockMetaEntry {
    pub slot: u64,
    pub detected_at: DateTime<Local>,
    pub blockhash: Hash,
}

/// Type alias for the shared block meta storage using arc-swap.
pub type SharedBlockMeta = Arc<ArcSwap<BlockMetaEntry>>;

/// Spawns a task that listens for block meta updates and stores only the latest value in a lock-free manner.
pub async fn spawn_blockmeta_data(rx: Receiver<SubscribeUpdateBlockMeta>, storage: SharedBlockMeta) {
        
        while let Ok(entry) = rx.recv_async().await {
            
            let detected_at = Local::now();
            if let Ok(blockhash) = Hash::from_str(&entry.blockhash) {
                let meta_entry = BlockMetaEntry {
                    slot: entry.slot,
                    detected_at,
                    blockhash,
                };
                // Update the latest block meta atomically.
                storage.store(Arc::new(meta_entry));
            }
        };
}
