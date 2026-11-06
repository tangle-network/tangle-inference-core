use std::path::PathBuf;

/// Maximum entries before the queue refuses new enqueues (prevents unbounded disk growth).
const MAX_QUEUE_DEPTH: usize = 1000;

/// Persistent failed settlements for operator recovery.
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
        let content = if content.is_empty() {
            String::new()
        } else {
            content + "\n"
        };

        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&tmp, &content).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}
