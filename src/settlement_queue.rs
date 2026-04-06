use std::path::PathBuf;

/// Persistent queue of failed settlements for manual or automatic retry.
pub struct SettlementRecoveryQueue {
    path: PathBuf,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct FailedSettlement {
    pub commitment: String,
    pub nonce: u64,
    pub amount: String,
    pub operator: String,
    pub service_id: u64,
    pub timestamp: u64,
    pub error: String,
    pub retry_count: u32,
}

impl SettlementRecoveryQueue {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Append a failed settlement to the JSONL file (one JSON object per line).
    pub fn enqueue(&self, settlement: FailedSettlement) {
        let line = serde_json::to_string(&settlement).unwrap_or_default();
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(f, "{line}");
        }
    }

    /// Read all pending failed settlements from the JSONL file.
    pub fn pending(&self) -> Vec<FailedSettlement> {
        std::fs::read_to_string(&self.path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// Remove the queue file (e.g. after all entries have been retried).
    pub fn clear(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueue_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SettlementRecoveryQueue::new(dir.path().join("dlq.jsonl"));

        assert!(queue.pending().is_empty());

        queue.enqueue(FailedSettlement {
            commitment: "0xabc".to_string(),
            nonce: 1,
            amount: "1000".to_string(),
            operator: "0xdef".to_string(),
            service_id: 42,
            timestamp: 1700000000,
            error: "gas too high".to_string(),
            retry_count: 3,
        });

        queue.enqueue(FailedSettlement {
            commitment: "0x123".to_string(),
            nonce: 2,
            amount: "2000".to_string(),
            operator: "0x456".to_string(),
            service_id: 42,
            timestamp: 1700000001,
            error: "rpc timeout".to_string(),
            retry_count: 1,
        });

        let pending = queue.pending();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].commitment, "0xabc");
        assert_eq!(pending[1].nonce, 2);

        queue.clear();
        assert!(queue.pending().is_empty());
    }
}
