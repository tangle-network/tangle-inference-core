use std::{net::TcpListener, path::PathBuf, sync::Arc, time::Duration};
use tangle_inference_core::engine::{EngineConfig, EngineKind, EngineProcess, RecipeCatalog};

fn config(engine: EngineKind) -> EngineConfig {
    EngineConfig {
        engine,
        model: "acme/derived-glm".into(),
        base_family: Some("glm-5.3".into()),
        host: "127.0.0.1".into(),
        port: TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port(),
        tensor_parallel_size: 8,
        gpu_class: Some("nvidia-h200".into()),
        max_model_len: 4096,
        startup_timeout_secs: 5,
        ..EngineConfig::default()
    }
}

fn option<'a>(argv: &'a [String], flag: &str) -> &'a str {
    &argv[argv.iter().position(|arg| arg == flag).unwrap() + 1]
}

#[test]
fn catalog_exports_real_family_recipes_and_sglang_argv() -> anyhow::Result<()> {
    let catalog = RecipeCatalog::bundled()?;
    let glm = catalog.resolve("glm-5.3", Some("acme/derived-glm"))?;
    assert!(glm.image.contains("@sha256:"));
    assert_eq!(option(&glm.args, "--model-path"), "acme/derived-glm");
    assert_eq!(option(&glm.args, "--tp"), "8");
    let deepseek = catalog.resolve("deepseek-v4.1-flash-1017", None)?;
    assert_eq!(option(&deepseek.args, "--moe-runner-backend"), "marlin");
    assert_eq!(option(&deepseek.args, "--attention-backend"), "dsv4");
    assert_eq!(option(&deepseek.args, "--ep-size"), "1");
    assert_eq!(
        option(&deepseek.args, "--model-path"),
        "deepseek-ai/DeepSeek-V4.1-Flash-1017"
    );
    assert!(catalog.resolve("unknown-family", None).is_err());

    let cfg = config(EngineKind::Sglang);
    let argv = cfg.argv()?;
    assert_eq!(&argv[..3], ["python3", "-m", "sglang.launch_server"]);
    assert_eq!(option(&argv, "--model-path"), cfg.model);
    assert_eq!(option(&argv, "--tp"), "8");
    assert_eq!(option(&argv, "--context-length"), "4096");
    assert!(argv.contains(&"--enable-cache-report".into()));
    assert!(!argv.contains(&"--max-model-len".into()));
    let mut invalid = cfg;
    invalid.base_family = Some("unknown-family".into());
    assert!(invalid.argv().is_err());
    invalid.engine = EngineKind::Vllm;
    let argv = invalid.argv()?;
    assert_eq!(
        &argv[..3],
        ["python3", "-m", "vllm.entrypoints.openai.api_server"]
    );
    assert_eq!(option(&argv, "--max-model-len"), "4096");
    assert_eq!(option(&argv, "--tensor-parallel-size"), "8");
    Ok(())
}

fn child_script(dir: &std::path::Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    let script = dir.join("engine.py");
    let receipt = dir.join("argv.json");
    std::fs::write(
        &script,
        r#"
import argparse, http.server, json, os, sys
p = argparse.ArgumentParser()
p.add_argument("receipt")
p.add_argument("--port", type=int)
p.add_argument("--host")
a, _ = p.parse_known_args()
with open(a.receipt, "w") as f:
    json.dump({"args": sys.argv[2:], "token_in_env": os.environ.get("HF_TOKEN") == "test-only-token", "hopper_env": os.environ.get("SGLANG_ENABLE_DSV41_ENGRAM_HOST_TABLE")}, f)
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/crash": os._exit(42)
        self.send_response(200 if self.path == "/health" else 404)
        self.end_headers()
    def log_message(self, *args): pass
http.server.HTTPServer((a.host, a.port), Handler).serve_forever()
"#,
    )?;
    Ok((script, receipt))
}

#[tokio::test]
async fn recipes_reach_child_and_processes_are_reaped() -> anyhow::Result<()> {
    for engine in [EngineKind::Sglang, EngineKind::Vllm] {
        let dir = tempfile::tempdir()?;
        let (script, receipt) = child_script(dir.path())?;
        let mut cfg = config(engine);
        if engine == EngineKind::Sglang {
            cfg.base_family = Some("deepseek-v4.1-flash-1017".into());
            cfg.model = "acme/derived-deepseek".into();
        }
        cfg.command = format!("python3 {} {}", script.display(), receipt.display());
        cfg.hf_token = Some("test-only-token".into());
        cfg.download_dir = Some(dir.path().join("model cache"));
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
        let receipt: serde_json::Value = serde_json::from_slice(&std::fs::read(receipt)?)?;
        let argv: Vec<String> = serde_json::from_value(receipt["args"].clone())?;
        assert_eq!(
            option(
                &argv,
                if engine == EngineKind::Sglang {
                    "--model-path"
                } else {
                    "--model"
                }
            ),
            model
        );
        assert_eq!(
            option(&argv, "--download-dir"),
            dir.path().join("model cache").to_str().unwrap()
        );
        assert_eq!(receipt["token_in_env"], true);
        if engine == EngineKind::Sglang {
            assert_eq!(receipt["hopper_env"], "0");
        }
        assert!(!argv.iter().any(|arg| arg.contains("test-only-token")));
        process.restart().await?;
        assert!(process.is_healthy().await);
        process.shutdown().await;
        assert!(!process.is_healthy().await);
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
    let mut cfg = config(EngineKind::Sglang);
    cfg.command = format!("python3 {}", script.display());
    let process = EngineProcess::spawn(cfg.clone()).await?;
    let failure = tokio::time::timeout(Duration::from_secs(3), process.wait_ready()).await?;
    assert!(failure.unwrap_err().to_string().contains("17"));
    process.shutdown().await;
    let external = EngineProcess::connect(cfg)?;
    assert!(external.restart().await.is_err());
    Ok(())
}

#[tokio::test]
async fn child_environment_is_allowlisted() -> anyhow::Result<()> {
    if std::env::var_os("TANGLE_ENGINE_ENV_PROBE").is_none() {
        let result = tokio::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "child_environment_is_allowlisted", "--nocapture"])
            .env("TANGLE_ENGINE_ENV_PROBE", "1")
            .env("TANGLE_PARENT_ONLY_TEST", "fixture-value")
            .kill_on_drop(true)
            .output()
            .await?;
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let output = dir.path().join("environment.json");
    let peer = dir.path().join("engine.py");
    std::fs::write(
        &peer,
        r#"import json, os, sys
with open(sys.argv[1], 'w') as output:
    json.dump({'leaked': os.environ.get('TANGLE_PARENT_ONLY_TEST') == 'fixture-value', 'path': bool(os.environ.get('PATH'))}, output)
"#,
    )?;
    let mut cfg = config(EngineKind::Vllm);
    cfg.command = format!("python3 {} {}", peer.display(), output.display());
    let process = EngineProcess::spawn(cfg).await?;
    let result: serde_json::Value = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(bytes) = std::fs::read(&output) {
                if let Ok(value) = serde_json::from_slice(&bytes) {
                    break value;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    process.shutdown().await;
    assert_eq!(
        result["leaked"], false,
        "parent-only environment crossed the child boundary"
    );
    assert_eq!(
        result["path"], true,
        "approved executable search path was lost"
    );
    Ok(())
}
