//! Public provenance exports observe running executables, including replaced paths.
#![cfg(target_os = "linux")]
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::process::Stdio;
use tangle_inference_core::runtime::{child_identity, core_source, operator_executable_sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

#[tokio::test]
async fn runtime_identity_follows_own_running_inodes() -> Result<()> {
    if std::env::var_os("TANGLE_RUNTIME_IDENTITY_PROBE").is_some() {
        let mut ready = [0];
        tokio::io::stdin().read_exact(&mut ready).await?;
        println!(
            "RUNTIME_IDENTITY={}",
            serde_json::to_string(&operator_executable_sha256().await)?
        );
        return Ok(());
    }
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).canonicalize()?;
    let root = Command::new("git")
        .arg("-C")
        .arg(&manifest)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .await?;
    if root.status.success()
        && std::path::Path::new(std::str::from_utf8(&root.stdout)?.trim()).canonicalize()?
            == manifest
    {
        let revision = Command::new("git")
            .arg("-C")
            .arg(&manifest)
            .args(["rev-parse", "HEAD"])
            .output()
            .await?;
        let dirty = Command::new("git")
            .arg("-C")
            .arg(&manifest)
            .args([
                "diff",
                "--quiet",
                "--no-ext-diff",
                "--no-textconv",
                "HEAD",
                "--",
            ])
            .status()
            .await?;
        let source = core_source();
        assert_eq!(source.origin, "git_checkout");
        assert_eq!(
            source.revision,
            Some(std::str::from_utf8(&revision.stdout)?.trim())
        );
        assert_eq!(source.dirty, Some(!dirty.success()));
    }
    let executable = std::env::current_exe()?;
    let digest = Command::new("sha256sum").arg(&executable).output().await?;
    assert!(digest.status.success());
    let expected = std::str::from_utf8(&digest.stdout)?
        .split_whitespace()
        .next()
        .unwrap();
    let directory = tempfile::tempdir()?;
    let running = directory.path().join("runtime-probe");
    std::fs::copy(&executable, &running)?;
    let argv = vec![
        running.to_str().unwrap().to_owned(),
        "--exact".into(),
        "runtime_identity_follows_own_running_inodes".into(),
        "--nocapture".into(),
    ];
    let mut child = Command::new(&running)
        .args(&argv[1..])
        .env("TANGLE_RUNTIME_IDENTITY_PROBE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let replacement = directory.path().join("replacement");
    std::fs::write(&replacement, b"replacement is not the running executable")?;
    std::fs::rename(replacement, &running)?;
    let identity = child_identity(&child, &argv).await;
    assert_eq!(identity.pid, child.id());
    assert_eq!(identity.executable_sha256.as_deref(), Some(expected));
    assert_eq!(
        identity.argv_sha256,
        Some(format!("{:x}", Sha256::digest(serde_json::to_vec(&argv)?)))
    );
    let wire = serde_json::to_string(&identity)?;
    assert!(!wire.contains("runtime-probe") && !wire.contains("--nocapture"));
    child.stdin.take().unwrap().write_all(b"g").await?;
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait_with_output())
        .await??;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let own = String::from_utf8(output.stdout)?
        .lines()
        .find_map(|line| {
            line.split_once("RUNTIME_IDENTITY=")
                .map(|(_, value)| value.to_owned())
        })
        .unwrap();
    let observed: Option<String> = serde_json::from_str(&own)?;
    assert_eq!(observed.as_deref(), Some(expected));
    Ok(())
}
