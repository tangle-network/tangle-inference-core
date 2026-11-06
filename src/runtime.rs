//! Local process fingerprints and best-effort source metadata, not attestation.
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::{fs::File, io::Read};
use tokio::{process::Child, sync::OnceCell};

include!(concat!(env!("OUT_DIR"), "/core_source.rs"));

/// Source metadata captured at build time from this crate's own Git checkout.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct CoreSource {
    pub revision: Option<&'static str>,
    /// Tracked Git changes observed at compilation.
    pub dirty: Option<bool>,
    pub origin: &'static str,
}

pub fn core_source() -> CoreSource {
    CoreSource {
        revision: SOURCE_REVISION,
        dirty: SOURCE_DIRTY,
        origin: SOURCE_ORIGIN,
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ChildIdentity {
    pub pid: Option<u32>,
    pub executable_sha256: Option<String>,
    pub argv_sha256: Option<String>,
}

/// Hash the actual running inode; a replaced pathname cannot substitute its bytes.
pub async fn operator_executable_sha256() -> Option<String> {
    static HASH: OnceCell<Option<String>> = OnceCell::const_new();
    HASH.get_or_init(|| executable_hash(PathBuf::from("/proc/self/exe")))
        .await
        .clone()
}

/// argv is the exact supplied vector, including the executable. Keep the owned
/// child/drop guard alive during observation; no plaintext arguments leave here.
pub async fn child_identity(child: &Child, argv: &[String]) -> ChildIdentity {
    let pid = child.id();
    let executable_sha256 = match pid {
        Some(pid) => executable_hash(PathBuf::from(format!("/proc/{pid}/exe"))).await,
        None => None,
    };
    ChildIdentity {
        pid,
        executable_sha256,
        argv_sha256: serde_json::to_vec(argv)
            .ok()
            .map(|bytes| format!("{:x}", Sha256::digest(bytes))),
    }
}

async fn executable_hash(path: PathBuf) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        tokio::task::spawn_blocking(move || {
            let mut file = File::open(path).ok()?;
            let mut digest = Sha256::new();
            let mut buffer = vec![0; 64 * 1024];
            loop {
                let count = file.read(&mut buffer).ok()?;
                if count == 0 {
                    break;
                }
                digest.update(&buffer[..count]);
            }
            Some(format!("{:x}", digest.finalize()))
        })
        .await
        .ok()
        .flatten()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        None
    }
}
