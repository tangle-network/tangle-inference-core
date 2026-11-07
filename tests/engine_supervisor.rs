use sha2::{Digest, Sha256};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tangle_inference_core::engine::{EngineConfig, EngineKind, EngineProcess, RecipeCatalog};

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

async fn isolated(test: &str, environment: &[(&str, &str)]) -> anyhow::Result<bool> {
    if std::env::var("TANGLE_ENGINE_TEST_PROBE").ok().as_deref() == Some(test) {
        return Ok(false);
    }
    let output = tokio::process::Command::new(std::env::current_exe()?)
        .args(["--exact", test, "--nocapture"])
        .env("TANGLE_ENGINE_TEST_PROBE", test)
        .envs(environment.iter().copied())
        .kill_on_drop(true)
        .output()
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(true)
}

/// Holds 3 ports for the test: bound exclusively, never listening, then reusable by engines alone.
fn config(engine: EngineKind) -> (EngineConfig, Vec<tokio::net::TcpSocket>) {
    let bind = |port: u16| {
        let socket = tokio::net::TcpSocket::new_v4().ok()?;
        socket.bind(([127, 0, 0, 1], port).into()).ok()?;
        socket.set_reuseaddr(true).ok()?;
        Some(socket)
    };
    let ports = (0..100)
        .find_map(|_| {
            let first = bind(0)?;
            let port = first.local_addr().ok()?.port();
            Some(vec![
                first,
                bind(port.checked_add(1)?)?,
                bind(port.checked_add(2)?)?,
            ])
        })
        .expect("no consecutive loopback ports");
    let config = EngineConfig {
        engine,
        model: "acme/derived-glm".into(),
        base_family: Some("glm-5.3".into()),
        host: "127.0.0.1".into(),
        port: ports[0].local_addr().unwrap().port(),
        tensor_parallel_size: 8,
        gpu_class: Some("nvidia-h200".into()),
        max_model_len: 4096,
        startup_timeout_secs: 5,
        ..EngineConfig::default()
    };
    (config, ports)
}

fn option<'a>(argv: &'a [String], flag: &str) -> &'a str {
    &argv[argv.iter().position(|arg| arg == flag).unwrap() + 1]
}

fn child_script(dir: &std::path::Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    let script = dir.join("engine.py");
    let receipt = dir.join("argv.json");
    std::fs::write(
        &script,
        r#"
import argparse, http.server, json, os, signal, sys, time, urllib.request
p = argparse.ArgumentParser()
p.add_argument("receipt")
p.add_argument("pids", nargs="?")
p.add_argument("--port", type=int)
p.add_argument("--host")
p.add_argument("--model-path")
p.add_argument("--model")
p.add_argument("--served-model-name")
p.add_argument("--worker-urls", nargs="+")
p.add_argument("--policy")
a, _ = p.parse_known_args()
if a.pids and sys.platform.startswith('linux'):
    ready, notify = os.pipe()
    if os.fork() == 0:
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        with open(a.pids, 'a') as f:
            f.write(str(os.getpid())+'\n')
        os.write(notify, b'1')
        while True:
            time.sleep(1)
    os.close(notify)
    os.read(ready, 1)
    os.close(ready)
receipt = os.path.join(a.receipt, str(a.port)+'.json') if os.path.isdir(a.receipt) else a.receipt
with open(receipt, "w") as f:
    json.dump({
        "role": "router" if a.worker_urls else "worker", "urls": a.worker_urls, "policy": a.policy,
        "devices": os.environ.get("CUDA_VISIBLE_DEVICES"), "pid": os.getpid(),
        "args": sys.argv[3 if a.pids else 2:],
        "leaked": os.environ.get("TANGLE_PARENT_ONLY_TEST") == "fixture-value",
        "path": bool(os.environ.get("PATH")), "token_in_env": os.environ.get("HF_TOKEN") == "test-only-token",
        "hopper_env": os.environ.get("SGLANG_ENABLE_DSV41_ENGRAM_HOST_TABLE"),
        "catalog_marker": os.environ.get("SGLANG_TEST_CATALOG")
    }, f)
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/crash": os._exit(42)
        if self.path == "/workers" and a.worker_urls:
            body = json.dumps([urllib.request.urlopen(url+'/health').status for url in a.worker_urls]).encode()
        elif self.path == "/v1/models":
            if a.worker_urls:
                body = urllib.request.urlopen(a.worker_urls[0]+self.path).read()
            else:
                body = json.dumps({"data": [{"id": a.served_model_name or a.model_path or a.model}]}).encode()
        else:
            body = b'ok'
        self.send_response(200 if self.path in ("/health", "/v1/models", "/workers") else 404)
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *args): pass
http.server.HTTPServer((a.host,a.port),Handler).serve_forever()
"#,
    )?;
    Ok((script, receipt))
}

#[tokio::test]
async fn recipes_reach_child_and_processes_are_reaped() -> anyhow::Result<()> {
    if isolated(
        "recipes_reach_child_and_processes_are_reaped",
        &[("TANGLE_PARENT_ONLY_TEST", "fixture-value")],
    )
    .await?
    {
        return Ok(());
    }
    for (engine, fixture) in [
        (EngineKind::Sglang, false),
        (EngineKind::Vllm, false),
        (EngineKind::Sglang, true),
    ] {
        let dir = tempfile::tempdir()?;
        let (script, receipt) = child_script(dir.path())?;
        let pids = dir.path().join("workers");
        std::fs::write(&pids, "")?;
        let _cleanup = WorkerCleanup(pids.clone());
        let (mut cfg, _ports) = config(engine);
        cfg.host = "0.0.0.0".into();
        let port = cfg.port;
        if engine == EngineKind::Sglang {
            cfg.base_family = Some("deepseek-v4.1-flash-1017".into());
            cfg.model = "acme/derived-deepseek".into();
        } else {
            cfg.base_family = Some("unknown-family".into());
        }
        cfg.command = format!(
            "python3 {} {} {}",
            script.display(),
            receipt.display(),
            pids.display()
        );
        cfg.router_command = format!(
            "python3 {} {} {}",
            script.display(),
            dir.path().join("router.json").display(),
            pids.display()
        );
        cfg.download_dir = Some(dir.path().join("model cache"));
        let catalog_path = dir.path().join("catalog.json");
        if fixture {
            let raw = include_str!("fixtures/qwen3-catalog.json");
            assert_eq!(
                RecipeCatalog::from_json(raw)?.image,
                RecipeCatalog::bundled()?.image
            );
            std::fs::write(&catalog_path, raw)?;
            let mut value = serde_json::to_value(cfg)?;
            value["recipe_catalog"] = serde_json::json!(catalog_path);
            value["base_family"] = "test-qwen3-0.6b".into();
            value["model"] = "Qwen/Qwen3-0.6B".into();
            value["tensor_parallel_size"] = 1.into();
            cfg = serde_json::from_value(value)?;
        }
        let model = cfg.model.clone();
        let process = Arc::new(EngineProcess::spawn(cfg).await?);
        process.wait_ready().await?;
        assert!(process.is_healthy().await);
        assert!(process
            .client()
            .get(format!("{}/health", process.endpoint()))
            .send()
            .await?
            .status()
            .is_success());
        let attack = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(1))
            .build()?;
        let last_port = if engine == EngineKind::Sglang {
            port + 1
        } else {
            port
        };
        for model_port in port..=last_port {
            let direct = attack
                .get(format!("http://127.0.0.2:{model_port}/v1/models"))
                .send()
                .await;
            let observed = direct.as_ref().map(|response| response.status().as_u16());
            assert!(
                direct.is_err(),
                "unauthenticated model endpoint reachable on another interface: {observed:?}"
            );
        }
        let receipt: serde_json::Value = serde_json::from_slice(&std::fs::read(receipt)?)?;
        let argv: Vec<String> = serde_json::from_value(receipt["args"].clone())?;
        let (model_flag, tp_flag, context_flag) = match engine {
            EngineKind::Sglang => ("--model-path", "--tp", "--context-length"),
            EngineKind::Vllm => ("--model", "--tensor-parallel-size", "--max-model-len"),
        };
        assert_eq!(option(&argv, model_flag), model);
        assert_eq!(option(&argv, tp_flag), if fixture { "1" } else { "8" });
        assert_eq!(option(&argv, context_flag), "4096");
        assert_eq!(
            option(&argv, "--download-dir"),
            dir.path().join("model cache").to_str().unwrap()
        );
        assert_eq!(receipt["token_in_env"], false);
        assert_eq!(
            receipt["leaked"], false,
            "parent-only environment crossed child boundary"
        );
        assert_eq!(receipt["path"], true, "approved executable path was lost");
        if engine == EngineKind::Sglang && !fixture {
            assert_eq!(receipt["hopper_env"], "0");
            assert_eq!(option(&argv, "--moe-runner-backend"), "marlin");
            assert_eq!(option(&argv, "--attention-backend"), "dsv4");
            assert_eq!(option(&argv, "--attn-dp-size"), "8");
            assert_eq!(option(&argv, "--ep-size"), "1");
            assert!(argv
                .iter()
                .any(|v| v == "--enable-decoder-swa-bounded-replay"));
            assert!(argv.iter().any(|v| v == "--enable-cache-report"));
            assert!(!argv
                .iter()
                .any(|v| v == "--max-model-len" || v.contains("{tp}")));
        }
        assert!(!argv.iter().any(|arg| arg.contains("test-only-token")));
        if fixture {
            assert_eq!(
                option(&argv, "--revision"),
                "c1899de289a04d12100db370d81485cdf75e47ca"
            );
            assert_eq!(option(&argv, "--served-model-name"), model);
            assert_eq!(receipt["catalog_marker"], "initial");
            let router: serde_json::Value =
                serde_json::from_slice(&std::fs::read(dir.path().join("router.json"))?)?;
            let router_args: Vec<String> = serde_json::from_value(router["args"].clone())?;
            assert_eq!(option(&router_args, "--policy"), "cache_aware");
            assert!(process
                .client()
                .get(format!("{}/health", option(&router_args, "--worker-urls")))
                .send()
                .await?
                .status()
                .is_success());
            let mut changed: serde_json::Value =
                serde_json::from_str(include_str!("fixtures/qwen3-catalog.json"))?;
            changed["families"]["test-qwen3-0.6b"]["args"] =
                serde_json::json!(["--served-model-name", "catalog-changed"]);
            changed["families"]["test-qwen3-0.6b"]["env"]["SGLANG_TEST_CATALOG"] = "changed".into();
            std::fs::write(&catalog_path, changed.to_string())?;
        }
        let retired_on_restart = std::fs::read_to_string(&pids)?;
        if cfg!(target_os = "linux") {
            assert_eq!(
                retired_on_restart.lines().count(),
                if engine == EngineKind::Sglang { 2 } else { 1 }
            );
        }
        process.restart().await?;
        assert!(process.is_healthy().await);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            retired_on_restart.lines().all(|pid| !running(pid)),
            "descendant survived restart"
        );
        if fixture {
            let restarted: serde_json::Value =
                serde_json::from_slice(&std::fs::read(dir.path().join("argv.json"))?)?;
            let args: Vec<String> = serde_json::from_value(restarted["args"].clone())?;
            assert_eq!(option(&args, "--served-model-name"), model);
            assert_eq!(restarted["catalog_marker"], "initial");
            let runtime = process.runtime_info().await?;
            assert_eq!(runtime["ready"], true);
            assert_eq!(runtime["served_models"], serde_json::json!([model]));
            assert_eq!(
                runtime["recipe"]["catalog_sha256"],
                format!(
                    "{:x}",
                    Sha256::digest(include_str!("fixtures/qwen3-catalog.json").as_bytes())
                )
            );
            assert!(runtime["verified_image_digest"].is_null());
        }
        process.shutdown().await;
        assert!(!process.is_healthy().await);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            std::fs::read_to_string(&pids)?
                .lines()
                .all(|pid| !running(pid)),
            "descendant survived shutdown"
        );
        process.shutdown().await;
        process.restart().await?;
        let _ = process
            .client()
            .get(format!("{}/crash", process.endpoint()))
            .send()
            .await;
        let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
        let supervised = process.clone();
        let task = tokio::spawn(async move { supervised.supervise(shutdown_rx).await });
        tokio::time::timeout(Duration::from_secs(3), async {
            while !process.is_healthy().await {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await?;
        shutdown.send(true)?;
        task.await??;
        assert!(!process.is_healthy().await);
    }
    Ok(())
}

#[tokio::test]
async fn startup_failure_is_bounded_and_external_restart_is_refused() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let script = dir.path().join("exit.py");
    std::fs::write(&script, "raise SystemExit(17)\n")?;
    let (mut cfg, _ports) = config(EngineKind::Sglang);
    cfg.command = format!("python3 {}", script.display());
    cfg.router_command = cfg.command.clone();
    let process = EngineProcess::spawn(cfg.clone()).await?;
    let failure = tokio::time::timeout(Duration::from_secs(3), process.wait_ready()).await?;
    assert!(failure.unwrap_err().to_string().contains("17"));
    process.shutdown().await;
    let external = EngineProcess::connect(cfg)?;
    assert!(external.restart().await.is_err());
    Ok(())
}

#[test]
fn tee_detection_matches_sdk_without_claiming_attestation() {
    let expected = blueprint_tee::detect_tee_provider().map(|provider| provider.to_string());
    let actual = tangle_inference_core::tee_status();
    assert_eq!(actual["provider"], serde_json::json!(expected));
    assert_eq!(actual["detected"], expected.is_some());
    assert_eq!(actual["attested"], false);
}

#[tokio::test]
async fn replica_set_owns_workers_router_and_device_partitions() -> anyhow::Result<()> {
    if isolated(
        "replica_set_owns_workers_router_and_device_partitions",
        &[("CUDA_VISIBLE_DEVICES", "0,1,2,3,4,5,6,7")],
    )
    .await?
    {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let (script, _) = child_script(dir.path())?;
    let (config, _ports) = config(EngineKind::Sglang);
    let mut value = serde_json::to_value(config)?;
    value["base_family"] = "deepseek-v4.1-flash".into();
    value["gpu_class"] = "nvidia-b200".into();
    value["tensor_parallel_size"] = 4.into();
    value["replicas"] = 2.into();
    value["command"] = format!("python3 {} {}", script.display(), dir.path().display()).into();
    value["router_command"] = value["command"].clone();
    let process = EngineProcess::spawn(serde_json::from_value(value)?).await?;
    process.wait_ready().await?;
    let result: serde_json::Value = process
        .client()
        .get(format!("{}/workers", process.endpoint()))
        .send()
        .await?
        .json()
        .await?;
    process.shutdown().await;
    assert_eq!(result, serde_json::json!([200, 200]));
    let mut devices = vec![];
    for file in std::fs::read_dir(dir.path())? {
        let file = file?;
        if file.path().extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let row: serde_json::Value = serde_json::from_slice(&std::fs::read(file.path())?)?;
        if row["role"] == "worker" {
            devices.push(row["devices"].as_str().unwrap().to_owned());
        } else {
            assert_eq!(row["policy"], "cache_aware");
            assert_eq!(row["urls"].as_array().unwrap().len(), 2);
        }
        #[cfg(target_os = "linux")]
        assert!(
            !std::path::Path::new(&format!("/proc/{}", row["pid"])).exists(),
            "managed child survived shutdown"
        );
    }
    devices.sort();
    assert_eq!(devices, ["0,1,2,3", "4,5,6,7"]);
    Ok(())
}
