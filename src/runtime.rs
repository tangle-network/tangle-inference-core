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

#[derive(Debug, Serialize)]
pub struct ChildIdentity {
    pub pid: Option<u32>,
    pub executable_sha256: Option<String>,
    pub argv_sha256: Option<String>,
}

/// Hash the actual running inode; a replaced pathname cannot substitute its bytes.
pub async fn operator_executable_sha256() -> Option<String> {
    static HASH: OnceCell<Option<String>> = OnceCell::const_new();
    HASH.get_or_init(|| file_sha256(PathBuf::from("/proc/self/exe")))
        .await
        .clone()
}

/// Observe only the owned child; arguments are hashed as raw NUL-separated
/// process bytes, never returned or logged as plaintext.
pub async fn child_identity(child: &Child) -> ChildIdentity {
    let pid = child.id();
    let (executable_sha256, argv_sha256) = match pid {
        Some(pid) => (
            file_sha256(PathBuf::from(format!("/proc/{pid}/exe"))).await,
            file_sha256(PathBuf::from(format!("/proc/{pid}/cmdline"))).await,
        ),
        None => (None, None),
    };
    ChildIdentity {
        pid,
        executable_sha256,
        argv_sha256,
    }
}

async fn file_sha256(path: PathBuf) -> Option<String> {
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
