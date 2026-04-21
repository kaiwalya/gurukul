use crossbeam_channel::{Receiver, Sender};
use std::collections::HashMap;

/// A single block of audio data with its sample-accurate timestamp.
pub type Block = (u64, Vec<f32>);

/// A receiver end of a port subscription.
pub type Subscription = Receiver<Block>;

/// Manages per-port broadcast channels.
pub struct SubscriptionHub {
    // port_path -> list of senders
    senders: HashMap<String, Vec<Sender<Block>>>,
}

impl SubscriptionHub {
    pub fn new() -> Self {
        Self {
            senders: HashMap::new(),
        }
    }

    /// Subscribe to a port by path (e.g. `"src.audio_out"`). Returns the receiver end.
    pub fn subscribe(&mut self, path: impl Into<String>) -> Subscription {
        let (tx, rx) = crossbeam_channel::unbounded();
        self.senders.entry(path.into()).or_default().push(tx);
        rx
    }

    /// Send a block to all subscribers of a port. No-op if no subscribers.
    pub fn send(&self, path: &str, block: Block) {
        if let Some(senders) = self.senders.get(path) {
            for tx in senders {
                // Ignore send errors — subscribers may have dropped.
                let _ = tx.send(block.clone());
            }
        }
    }

    pub fn has_subscribers(&self, path: &str) -> bool {
        self.senders
            .get(path)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    }
}

impl Default for SubscriptionHub {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_delivers_exactly_as_many_blocks_as_produced() {
        let mut hub = SubscriptionHub::new();
        let rx = hub.subscribe("src.audio_out");

        let n_blocks = 5;
        for i in 0..n_blocks {
            let block: Block = (i as u64 * 512, vec![0.0f32; 512]);
            hub.send("src.audio_out", block);
        }
        drop(hub);

        let received: Vec<Block> = rx.try_iter().collect();
        assert_eq!(received.len(), n_blocks);
    }

    #[test]
    fn no_subscribers_is_a_noop() {
        let hub = SubscriptionHub::new();
        // Should not panic
        hub.send("nowhere.port", (0, vec![0.0f32; 512]));
    }
}
