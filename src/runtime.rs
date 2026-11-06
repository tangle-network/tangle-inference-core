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

/// Open the owned child's process files before asynchronous hashing.
/// Arguments are hashed as raw NUL-separated bytes; no plaintext leaves here.
pub fn child_identity(child: &Child) -> impl std::future::Future<Output = ChildIdentity> + use<> {
    let pid = child.id();
    let files = pid.map(|pid| {
        (
            file_sha256(PathBuf::from(format!("/proc/{pid}/exe"))),
            file_sha256(PathBuf::from(format!("/proc/{pid}/cmdline"))),
        )
    });
    async move {
        let (executable_sha256, argv_sha256) = match files {
            Some((executable, argv)) => (executable.await, argv.await),
            None => (None, None),
        };
        ChildIdentity {
            pid,
            executable_sha256,
            argv_sha256,
        }
    }
}

fn file_sha256(path: PathBuf) -> impl std::future::Future<Output = Option<String>> + use<> {
    #[cfg(target_os = "linux")]
    let file = File::open(path);
    async move {
        #[cfg(target_os = "linux")]
        {
            tokio::task::spawn_blocking(move || {
                let mut file = file.ok()?;
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
}
