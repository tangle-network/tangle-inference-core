use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::{watch, Mutex},
};

use crate::recipe::set_option;
pub use crate::recipe::{
    FamilyRecipe, GpuRecipe, RecipeArm, RecipeCatalog, ResolvedRecipe, ENGINE_PROFILES,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    #[default]
    Vllm,
    Sglang,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    #[serde(default)]
    pub engine: EngineKind,
    #[serde(default)]
    pub base_family: Option<String>,
    #[serde(default = "default_arm")]
    pub recipe_arm: String,
    #[serde(default)]
    pub gpu_class: Option<String>,
    pub model: String,
    #[serde(default = "default_context")]
    pub max_model_len: u32,
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_parallelism")]
    pub tensor_parallel_size: u32,
    #[serde(default)]
    pub extra_args: Vec<String>,
    /// Explicit executable plus initial arguments; never evaluated by a shell.
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub download_dir: Option<PathBuf>,
    #[serde(default = "default_timeout")]
    pub startup_timeout_secs: u64,
    #[serde(default)]
    pub external: bool,
}

impl std::fmt::Debug for EngineConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineConfig")
            .field("engine", &self.engine)
            .field("model", &self.model)
            .field("base_family", &self.base_family)
            .field("host", &self.host)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

fn default_arm() -> String {
    "native".into()
}
fn default_context() -> u32 {
    32768
}
fn default_host() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    8000
}
fn default_parallelism() -> u32 {
    1
}
fn default_timeout() -> u64 {
    300
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            engine: EngineKind::default(),
            base_family: None,
            recipe_arm: default_arm(),
            gpu_class: None,
            model: String::new(),
            max_model_len: default_context(),
            host: default_host(),
            port: default_port(),
            tensor_parallel_size: default_parallelism(),
            extra_args: Vec::new(),
            command: String::new(),
            download_dir: None,
            startup_timeout_secs: default_timeout(),
            external: false,
        }
    }
}

impl EngineConfig {
    pub fn recipe(&self) -> Result<ResolvedRecipe> {
        ensure!(
            self.engine == EngineKind::Sglang,
            "family catalog contains SGLang recipes"
        );
        let catalog = RecipeCatalog::bundled()?;
        let family = self
            .base_family
            .as_deref()
            .or_else(|| {
                catalog
                    .families
                    .iter()
                    .find(|(_, recipe)| recipe.parent_model == self.model)
                    .map(|(family, _)| family.as_str())
            })
            .context("SGLang requires a catalog base_family for a derivative model")?;
        catalog.resolve_for(
            family,
            Some(&self.model),
            &self.recipe_arm,
            self.gpu_class.as_deref(),
            self.tensor_parallel_size,
        )
    }

    /// The returned vector includes the executable at index zero.
    pub fn argv(&self) -> Result<Vec<String>> {
        ensure!(
            !self.model.is_empty() && !self.model.starts_with('-'),
            "invalid model id or path"
        );
        ensure!(
            self.tensor_parallel_size > 0 && self.max_model_len > 0 && self.port > 0,
            "GPU count, context length and port must be positive"
        );
        let mut command: Vec<String> = if self.command.is_empty() {
            vec![
                "python3".into(),
                "-m".into(),
                match self.engine {
                    EngineKind::Vllm => "vllm.entrypoints.openai.api_server",
                    EngineKind::Sglang => "sglang.launch_server",
                }
                .into(),
            ]
        } else {
            self.command.split_whitespace().map(str::to_owned).collect()
        };
        ensure!(!command.is_empty(), "empty engine command");
        let mut args = match self.engine {
            EngineKind::Vllm => Vec::new(),
            EngineKind::Sglang => self.recipe()?.args,
        };
        args.extend(self.extra_args.iter().cloned());
        let (model_flag, context_flag, parallel_flag) = match self.engine {
            EngineKind::Vllm => ("--model", "--max-model-len", "--tensor-parallel-size"),
            EngineKind::Sglang => ("--model-path", "--context-length", "--tp"),
        };
        set_option(
            &mut args,
            &["--model", "--model-path"],
            model_flag,
            &self.model,
        );
        set_option(&mut args, &["--host"], "--host", &self.host);
        set_option(&mut args, &["--port"], "--port", &self.port.to_string());
        set_option(
            &mut args,
            &[context_flag],
            context_flag,
            &self.max_model_len.to_string(),
        );
        set_option(
            &mut args,
            &["--tp", "--tp-size", "--tensor-parallel-size"],
            parallel_flag,
            &self.tensor_parallel_size.to_string(),
        );
        if let Some(dir) = &self.download_dir {
            set_option(
                &mut args,
                &["--download-dir"],
                "--download-dir",
                dir.to_str().context("download directory is not UTF-8")?,
            );
        }
        command.extend(args);
        Ok(command)
    }
}

struct EngineChild {
    child: Child,
    #[cfg(unix)]
    group: i32,
}

impl EngineChild {
    async fn terminate(&mut self) {
        #[cfg(unix)]
        // Each managed engine starts its own process group; workers share it.
        unsafe {
            libc::kill(-self.group, libc::SIGTERM);
        }
        if tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
        #[cfg(unix)]
        unsafe {
            libc::kill(-self.group, libc::SIGKILL);
        }
        let _ = self.child.wait().await;
    }
}

impl Drop for EngineChild {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-self.group, libc::SIGKILL);
        }
    }
}

/// Owns one local engine child, or connects to an externally managed endpoint.
pub struct EngineProcess {
    child: Mutex<Option<EngineChild>>,
    config: EngineConfig,
    client: reqwest::Client,
    managed: bool,
}

impl EngineProcess {
    pub fn connect(config: EngineConfig) -> Result<Self> {
        Ok(Self {
            child: Mutex::new(None),
            config,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()?,
            managed: false,
        })
    }

    pub async fn spawn(config: EngineConfig) -> Result<Self> {
        let mut process = Self::connect(config)?;
        process.managed = true;
        *process.child.lock().await = Some(process.spawn_child()?);
        Ok(process)
    }

    fn spawn_child(&self) -> Result<EngineChild> {
        let argv = self.config.argv()?;
        let mut command = Command::new(&argv[0]);
        command
            .env_clear()
            .envs(
                [
                    "PATH",
                    "LD_LIBRARY_PATH",
                    "CUDA_HOME",
                    "CUDA_PATH",
                    "CUDA_VISIBLE_DEVICES",
                    "NVIDIA_VISIBLE_DEVICES",
                    "NVIDIA_DRIVER_CAPABILITIES",
                    "NCCL_SOCKET_IFNAME",
                    "NCCL_IB_HCA",
                    "TMPDIR",
                ]
                .into_iter()
                .filter_map(|key| std::env::var_os(key).map(|value| (key, value))),
            )
            .args(&argv[1..])
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);
        if self.config.engine == EngineKind::Sglang {
            command.envs(self.config.recipe()?.env);
        }
        let mut child = command.spawn().context("failed to start serving engine")?;
        for (stream, pipe) in
            [
                (
                    "stdout",
                    child.stdout.take().map(|p| {
                        Box::pin(p) as std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>
                    }),
                ),
                (
                    "stderr",
                    child.stderr.take().map(|p| {
                        Box::pin(p) as std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>
                    }),
                ),
            ]
        {
            if let Some(pipe) = pipe {
                tokio::spawn(async move {
                    let mut lines = BufReader::new(pipe).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        tracing::info!(target: "engine", stream, "{line}");
                    }
                });
            }
        }
        tracing::info!(pid = child.id(), engine = ?self.config.engine, "spawned serving engine");
        Ok(EngineChild {
            #[cfg(unix)]
            group: child.id().context("serving engine has no pid")? as i32,
            child,
        })
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub fn endpoint(&self) -> String {
        let host = match self.config.host.as_str() {
            "0.0.0.0" => "127.0.0.1",
            "::" | "[::]" => "[::1]",
            host => host,
        };
        let host = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.into()
        };
        format!("http://{host}:{}", self.config.port)
    }

    pub async fn is_healthy(&self) -> bool {
        if self.managed {
            let mut guard = self.child.lock().await;
            if !matches!(
                guard.as_mut().map(|process| process.child.try_wait()),
                Some(Ok(None))
            ) {
                return false;
            }
        }
        matches!(self.client.get(format!("{}/health", self.endpoint())).timeout(Duration::from_secs(5)).send().await,
            Ok(response) if response.status().is_success())
    }

    pub async fn wait_ready(&self) -> Result<()> {
        let result = tokio::time::timeout(
            Duration::from_secs(self.config.startup_timeout_secs),
            async {
                loop {
                    {
                        let mut guard = self.child.lock().await;
                        if let Some(child) = guard.as_mut() {
                            if let Some(status) = child.child.try_wait()? {
                                bail!("serving engine exited during startup: {status}");
                            }
                        } else if self.managed {
                            bail!("serving engine has stopped");
                        }
                    }
                    if self.is_healthy().await {
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
        )
        .await
        .context("serving engine startup timed out")
        .and_then(|result| result);
        if result.is_err() && self.managed {
            self.shutdown().await;
        }
        result
    }

    pub async fn shutdown(&self) {
        if let Some(mut child) = self.child.lock().await.take() {
            child.terminate().await;
        }
    }

    pub async fn restart(&self) -> Result<()> {
        ensure!(self.managed, "cannot restart an externally managed engine");
        {
            let mut guard = self.child.lock().await;
            if let Some(mut child) = guard.take() {
                child.terminate().await;
            }
            *guard = Some(self.spawn_child()?);
        }
        self.wait_ready().await
    }

    pub async fn supervise(&self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        let mut failures = 0;
        let result = loop {
            if *shutdown.borrow() {
                break Ok(());
            }
            tokio::select! {
                result = shutdown.changed() => { if result.is_err() || *shutdown.borrow() { break Ok(()); } }
                _ = interval.tick(), if self.managed => {
                    if self.is_healthy().await { failures = 0; continue; }
                    let restart = tokio::select! {
                        changed = shutdown.changed() => {
                            if changed.is_err() || *shutdown.borrow() { break Ok(()); }
                            continue;
                        }
                        result = self.restart() => result,
                    };
                    if let Err(error) = restart {
                        failures += 1;
                        tracing::error!(%error, failures, "serving engine restart failed");
                        if failures >= 3 { break Err(error.context("serving engine restart limit reached")); }
                    }
                }
            }
        };
        self.shutdown().await;
        result
    }
}
