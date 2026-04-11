use serde::Serialize;
use tokio::sync::broadcast;

use crate::db::PersistedEventRecord;

pub const DEFAULT_BROKER_CAPACITY: usize = 4096;

#[derive(Debug)]
pub struct LiveBroker {
    capacity: usize,
    sender: broadcast::Sender<PersistedEventRecord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LiveBrokerStatus {
    pub capacity: usize,
    pub subscribers: usize,
}

impl LiveBroker {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { capacity, sender }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PersistedEventRecord> {
        self.sender.subscribe()
    }

    pub fn publish_batch(&self, events: &[PersistedEventRecord]) {
        for event in events {
            let _ = self.sender.send(event.clone());
        }
    }

    pub fn status(&self) -> LiveBrokerStatus {
        LiveBrokerStatus {
            capacity: self.capacity,
            subscribers: self.sender.receiver_count(),
        }
    }
}
