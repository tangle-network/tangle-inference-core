use std::path::PathBuf;

/// Maximum entries before the queue refuses new enqueues (prevents unbounded disk growth).
const MAX_QUEUE_DEPTH: usize = 1000;

/// Maximum retries per settlement before it's moved to the permanent failure log.
const MAX_RETRY_COUNT: u32 = 10;

/// Persistent queue of failed settlements for automatic retry.
///
/// Improvements over the original:
/// - Bounded: rejects enqueue if depth > MAX_QUEUE_DEPTH
/// - Retry-aware: tracks retry_count, marks permanently failed after MAX_RETRY_COUNT
/// - Depth reporting: `depth()` for monitoring/alerting
/// - Atomic write: writes to .tmp then renames
pub struct SettlementRecoveryQueue {
    path: PathBuf,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
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

    /// Append a failed settlement. Returns false if queue is at capacity.
    pub fn enqueue(&self, settlement: FailedSettlement) -> bool {
        let current = self.pending();
        if current.len() >= MAX_QUEUE_DEPTH {
            tracing::error!(
                depth = current.len(),
                max = MAX_QUEUE_DEPTH,
                "settlement recovery queue at capacity — DROPPING settlement for commitment {}",
                settlement.commitment
            );
            return false;
        }

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
        true
    }

    /// Read all pending failed settlements from the JSONL file.
    pub fn pending(&self) -> Vec<FailedSettlement> {
        std::fs::read_to_string(&self.path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// Number of pending entries. Use for monitoring/alerting.
    pub fn depth(&self) -> usize {
        self.pending().len()
    }

    /// Rewrite the queue with only the given entries (after retry processing).
    /// Uses atomic write (tmp → rename) to prevent corruption from concurrent access.
    pub fn rewrite(&self, entries: &[FailedSettlement]) {
        let tmp = self.path.with_extension("tmp");
        let content = entries
            .iter()
            .filter_map(|e| serde_json::to_string(e).ok())
            .collect::<Vec<_>>()
            .join("\n");
        let content = if content.is_empty() { String::new() } else { content + "\n" };

        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&tmp, &content).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    /// Remove the queue file.
    pub fn clear(&self) {
        let _ = std::fs::remove_file(&self.path);
    }

    /// Process the queue: retry each entry with the given callback.
    /// Entries that succeed are removed. Entries that fail get retry_count
    /// incremented. Entries past MAX_RETRY_COUNT are logged and removed.
    ///
    /// Returns (succeeded, failed, permanently_failed).
    pub async fn retry_all<F, Fut>(&self, retry_fn: F) -> (usize, usize, usize)
    where
        F: Fn(&FailedSettlement) -> Fut,
        Fut: std::future::Future<Output = Result<(), anyhow::Error>>,
    {
        let entries = self.pending();
        if entries.is_empty() {
            return (0, 0, 0);
        }

        tracing::info!(depth = entries.len(), "retrying failed settlements");

        let mut remaining = Vec::new();
        let mut succeeded = 0usize;
        let mut failed = 0usize;
        let mut permanently_failed = 0usize;

        for mut entry in entries {
            if entry.retry_count >= MAX_RETRY_COUNT {
                tracing::error!(
                    commitment = %entry.commitment,
                    nonce = entry.nonce,
                    amount = %entry.amount,
                    retries = entry.retry_count,
                    "settlement permanently failed after {} retries — MANUAL RECOVERY REQUIRED",
                    MAX_RETRY_COUNT
                );
                permanently_failed += 1;
                continue; // drop from queue
            }

            match retry_fn(&entry).await {
                Ok(()) => {
                    tracing::info!(
                        commitment = %entry.commitment,
                        nonce = entry.nonce,
                        retry = entry.retry_count,
                        "settlement retry succeeded"
                    );
                    succeeded += 1;
                }
                Err(e) => {
                    entry.retry_count += 1;
                    entry.error = format!("{e}");
                    entry.timestamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    tracing::warn!(
                        commitment = %entry.commitment,
                        retry = entry.retry_count,
                        error = %e,
                        "settlement retry failed"
                    );
                    remaining.push(entry);
                    failed += 1;
                }
            }
        }

        self.rewrite(&remaining);
        (succeeded, failed, permanently_failed)
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
        assert_eq!(queue.depth(), 0);

        let ok = queue.enqueue(FailedSettlement {
            commitment: "0xabc".to_string(),
            nonce: 1,
            amount: "1000".to_string(),
            operator: "0xdef".to_string(),
            service_id: 42,
            timestamp: 1700000000,
            error: "gas too high".to_string(),
            retry_count: 3,
        });
        assert!(ok);

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
        assert_eq!(queue.depth(), 2);
        assert_eq!(pending[0].commitment, "0xabc");
        assert_eq!(pending[1].nonce, 2);

        queue.clear();
        assert!(queue.pending().is_empty());
    }

    #[test]
    fn rewrite_replaces_queue() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SettlementRecoveryQueue::new(dir.path().join("dlq.jsonl"));

        queue.enqueue(FailedSettlement {
            commitment: "0x1".into(), nonce: 1, amount: "100".into(),
            operator: "0xop".into(), service_id: 1, timestamp: 0,
            error: "err".into(), retry_count: 0,
        });
        queue.enqueue(FailedSettlement {
            commitment: "0x2".into(), nonce: 2, amount: "200".into(),
            operator: "0xop".into(), service_id: 1, timestamp: 0,
            error: "err".into(), retry_count: 0,
        });
        assert_eq!(queue.depth(), 2);

        // Rewrite with only one entry
        let mut entries = queue.pending();
        entries.retain(|e| e.commitment == "0x2");
        queue.rewrite(&entries);
        assert_eq!(queue.depth(), 1);
        assert_eq!(queue.pending()[0].commitment, "0x2");
    }

    #[tokio::test]
    async fn retry_all_processes_entries() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SettlementRecoveryQueue::new(dir.path().join("dlq.jsonl"));

        // Entry 1: will succeed on retry
        queue.enqueue(FailedSettlement {
            commitment: "0xsuccess".into(), nonce: 1, amount: "100".into(),
            operator: "0xop".into(), service_id: 1, timestamp: 0,
            error: "gas".into(), retry_count: 0,
        });
        // Entry 2: will fail on retry
        queue.enqueue(FailedSettlement {
            commitment: "0xfail".into(), nonce: 2, amount: "200".into(),
            operator: "0xop".into(), service_id: 1, timestamp: 0,
            error: "rpc".into(), retry_count: 0,
        });
        // Entry 3: past max retries — permanently failed
        queue.enqueue(FailedSettlement {
            commitment: "0xperm".into(), nonce: 3, amount: "300".into(),
            operator: "0xop".into(), service_id: 1, timestamp: 0,
            error: "repeated".into(), retry_count: MAX_RETRY_COUNT,
        });

        let (succeeded, failed, perm) = queue.retry_all(|entry| {
            let is_success = entry.commitment == "0xsuccess";
            async move {
                if is_success {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("still failing"))
                }
            }
        }).await;

        assert_eq!(succeeded, 1);
        assert_eq!(failed, 1);
        assert_eq!(perm, 1);

        // Only the still-failing entry remains
        let remaining = queue.pending();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].commitment, "0xfail");
        assert_eq!(remaining[0].retry_count, 1); // incremented
    }
}
