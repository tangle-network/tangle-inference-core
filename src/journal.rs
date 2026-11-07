//! Crash-safe record of settlements owed for served work.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use alloy::primitives::Address;
use serde::{Deserialize, Serialize};

use crate::server::SpendAuthPayload;

const COMPACT_AFTER: usize = 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settlement {
    pub auth_hash: String,
    pub spend_auth: SpendAuthPayload,
    pub amount: u64,
    pub metered: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Record {
    Owed(Settlement),
    Settled(String),
}

pub struct SettlementJournal {
    path: PathBuf,
    inner: Mutex<Inner>,
    _lock: File,
}

struct Inner {
    file: File,
    owed: HashMap<String, Settlement>,
    settled_since_compaction: usize,
}

impl SettlementJournal {
    /// Load this operator's owed settlements and rewrite the file to only those.
    /// None when another client holds the journal. A torn last line is skipped.
    pub fn open(path: PathBuf, operator: Address) -> anyhow::Result<Option<Self>> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path.with_extension("lock"))?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // SAFETY: flock only reads the descriptor, which `lock` keeps open.
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                tracing::warn!(path = %path.display(), "settlement journal held by another client; this client's settlements are not journaled");
                return Ok(None);
            }
        }
        let mut owed = HashMap::new();
        if let Ok(file) = File::open(&path) {
            for line in BufReader::new(file).lines() {
                match serde_json::from_str(&line?) {
                    Ok(Record::Owed(s))
                        if s.spend_auth.operator.parse::<Address>().ok() == Some(operator) =>
                    {
                        _ = owed.insert(s.auth_hash.clone(), s);
                    }
                    Ok(Record::Owed(s)) => tracing::warn!(
                        auth_hash = %s.auth_hash,
                        operator = %s.spend_auth.operator,
                        "dropping a journal line this operator will not settle"
                    ),
                    Ok(Record::Settled(hash)) => _ = owed.remove(&hash),
                    Err(e) => tracing::warn!(error = %e, "skipping unreadable settlement record"),
                }
            }
        }
        let file = rewrite(&path, owed.values())?;
        tracing::info!(owed = owed.len(), path = %path.display(), "loaded settlement journal");
        Ok(Some(Self {
            path,
            inner: Mutex::new(Inner {
                file,
                owed,
                settled_since_compaction: 0,
            }),
            _lock: lock,
        }))
    }

    /// Durably record a settlement before it is sent on-chain.
    pub fn owe(&self, settlement: &Settlement) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        append(&mut inner.file, &Record::Owed(settlement.clone()), true)?;
        inner
            .owed
            .insert(settlement.auth_hash.clone(), settlement.clone());
        Ok(())
    }

    /// Forget a confirmed settlement. Not fsynced: a lost record only makes the next start
    /// find the authorization already claimed on-chain.
    pub fn settled(&self, auth_hash: &str) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.owed.remove(auth_hash).is_none() {
            return Ok(());
        }
        append(&mut inner.file, &Record::Settled(auth_hash.into()), false)?;
        inner.settled_since_compaction += 1;
        if inner.settled_since_compaction >= COMPACT_AFTER {
            inner.file = rewrite(&self.path, inner.owed.values())?;
            inner.settled_since_compaction = 0;
        }
        Ok(())
    }

    /// Settlements recorded but not yet confirmed.
    pub fn owed(&self) -> Vec<Settlement> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.owed.values().cloned().collect()
    }
}

fn append(file: &mut File, record: &Record, sync: bool) -> anyhow::Result<()> {
    let mut line = serde_json::to_vec(record)?;
    line.push(b'\n');
    file.write_all(&line)?;
    if sync {
        file.sync_data()?;
    }
    Ok(())
}

fn rewrite<'a>(path: &Path, owed: impl Iterator<Item = &'a Settlement>) -> anyhow::Result<File> {
    let tmp = path.with_extension("tmp");
    let mut file = File::create(&tmp)?;
    for settlement in owed {
        append(&mut file, &Record::Owed(settlement.clone()), false)?;
    }
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        File::open(parent)?.sync_all()?;
    }
    Ok(OpenOptions::new().append(true).open(path)?)
}
