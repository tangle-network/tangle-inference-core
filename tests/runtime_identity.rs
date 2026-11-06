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
    let argv = [
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
    let identity = child_identity(&child).await;
    assert_eq!(identity.pid, child.id());
    assert_eq!(identity.executable_sha256.as_deref(), Some(expected));
    assert_eq!(
        identity.argv_sha256,
        Some(format!(
            "{:x}",
            Sha256::digest(std::fs::read(format!(
                "/proc/{}/cmdline",
                child.id().unwrap()
            ))?)
        ))
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
    assert_eq!(
        operator_executable_sha256().await.as_deref(),
        Some(expected)
    );
    // A launcher may exec a different image without changing its supervised PID.
    use tangle_inference_core::engine::{EngineConfig, EngineProcess};
    let python = Command::new("python3")
        .args([
            "-c",
            "import os,sys; print(os.path.realpath(sys.executable))",
        ])
        .output()
        .await?;
    assert!(python.status.success());
    let second_python = directory.path().join("python-second");
    std::fs::copy(std::str::from_utf8(&python.stdout)?.trim(), &second_python)?;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&second_python)?
        .set_len(256 * 1024 * 1024)?;
    let switch = directory.path().join("switch");
    let peer = directory.path().join("exec-peer.py");
    std::fs::write(
        &peer,
        r#"import argparse,http.server,json,os,sys,time
p=argparse.ArgumentParser()
p.add_argument('switch');p.add_argument('second');p.add_argument('--active',action='store_true')
p.add_argument('--host');p.add_argument('--port',type=int)
a,_=p.parse_known_args()
if not a.active:
    while not os.path.exists(a.switch):time.sleep(.01)
    os.execv(a.second,[a.second,__file__]+sys.argv[1:]+['--active'])
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        value={'data':[{'id':'fixture/runtime-model'}]} if self.path=='/v1/models' else {'version':'exec-image-two'}
        body=b'' if self.path=='/health' else json.dumps(value).encode()
        self.send_response(200);self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
    def log_message(self,*args):pass
http.server.HTTPServer((a.host,a.port),H).serve_forever()
"#,
    )?;
    let port = std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port();
    let process = EngineProcess::spawn(EngineConfig {
        model: "fixture/runtime-model".into(),
        port,
        startup_timeout_secs: 5,
        command: format!(
            "python3 {} {} {}",
            peer.display(),
            switch.display(),
            second_python.display()
        ),
        ..Default::default()
    })
    .await?;
    std::fs::write(&switch, b"exec now")?;
    process.wait_ready().await?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(1))
        .build()?;
    let mut samples = 0;
    let mut false_health = 0;
    let mut metadata = Box::pin(process.runtime_info());
    let observation = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            tokio::select! {
                facts = &mut metadata => break facts,
                healthy = process.is_healthy() => {
                    client.get(format!("{}/health", process.endpoint()))
                        .send().await?.error_for_status()?;
                    samples += 1;
                    false_health += usize::from(!healthy);
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        }
    })
    .await;
    drop(metadata);
    if !matches!(observation, Ok(Ok(_))) {
        process.shutdown().await;
    }
    let facts = observation??;
    let pid = facts["processes"][0]["pid"].as_u64().unwrap();
    let actual_executable = std::fs::read(format!("/proc/{pid}/exe"))?;
    let actual_argv = std::fs::read(format!("/proc/{pid}/cmdline"))?;
    let expected = serde_json::json!({
        "executable_sha256":format!("{:x}",Sha256::digest(actual_executable)),
        "argv_sha256":format!("{:x}",Sha256::digest(actual_argv)),
    });
    let observed = serde_json::json!({
        "executable_sha256":facts["processes"][0]["executable_sha256"],
        "argv_sha256":facts["processes"][0]["argv_sha256"],
    });
    process.shutdown().await;
    assert!(
        samples > 0,
        "metadata did not overlap a public health observation"
    );
    assert_eq!(
        false_health, 0,
        "metadata made a healthy HTTP worker report unready: {false_health}/{samples} observations"
    );
    assert_eq!(facts["ready"], true);
    assert_eq!(
        observed, expected,
        "runtime identity retained the pre-exec launcher fingerprint"
    );
    Ok(())
}
