#![cfg(target_os = "linux")]

use std::{net::TcpListener, path::PathBuf, time::Duration};
use tangle_inference_core::engine::{EngineConfig, EngineProcess};

struct WorkerCleanup(PathBuf);
impl Drop for WorkerCleanup {
    fn drop(&mut self) {
        for pid in std::fs::read_to_string(&self.0).unwrap_or_default().lines() {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", pid])
                .status();
        }
    }
}
fn running(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rsplit_once(") ").map(|(_, rest)| rest.starts_with('Z')))
        .is_some_and(|zombie| !zombie)
}
#[tokio::test]
async fn shutdown_and_restart_retire_engine_workers() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let pid_file = dir.path().join("workers");
    let _cleanup = WorkerCleanup(pid_file.clone());
    let script = dir.path().join("engine.py");
    std::fs::write(
        &script,
        r#"
import argparse, http.server, os, signal, time
p=argparse.ArgumentParser()
p.add_argument('pids')
p.add_argument('--host')
p.add_argument('--port',type=int)
a,_=p.parse_known_args()
if os.fork()==0:
    signal.signal(signal.SIGTERM,signal.SIG_IGN)
    with open(a.pids,'a') as f: f.write(str(os.getpid())+'\n')
    while True: time.sleep(1)
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self): self.send_response(200); self.end_headers()
    def log_message(self,*args): pass
http.server.HTTPServer((a.host,a.port),H).serve_forever()
"#,
    )?;
    let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let process = EngineProcess::spawn(EngineConfig {
        model: "public-fixture".into(),
        host: "127.0.0.1".into(),
        port,
        command: format!("python3 {} {}", script.display(), pid_file.display()),
        startup_timeout_secs: 5,
        ..EngineConfig::default()
    })
    .await?;
    process.wait_ready().await?;
    let old = std::fs::read_to_string(&pid_file)?
        .lines()
        .next()
        .unwrap()
        .to_owned();
    process.restart().await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!running(&old), "engine worker {old} survived restart");
    let pids = std::fs::read_to_string(&pid_file)?;
    let new = pids.lines().last().unwrap();
    process.shutdown().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!running(new), "engine worker {new} survived shutdown");
    Ok(())
}
