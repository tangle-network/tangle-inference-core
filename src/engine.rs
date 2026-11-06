use crate::weights::{stage_weights, StagingLimits, WeightSource};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::{watch, Mutex},
};

use crate::recipe::set_option;
pub use crate::recipe::{
    FamilyRecipe, GpuRecipe, RecipeArm, RecipeCatalog, ResolvedRecipe, ThroughputMeasurement,
    ENGINE_PROFILES,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    #[default]
    Vllm,
    Sglang,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineWeights {
    pub manifest_url: String,
    #[serde(flatten)]
    pub source: WeightSource,
    pub cache_dir: PathBuf,
    #[serde(default)]
    pub allow_loopback_http: bool,
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
    #[serde(default = "default_parallelism")]
    pub replicas: u32,
    #[serde(default)]
    pub router_command: String,
    #[serde(default)]
    pub extra_args: Vec<String>,
    /// Explicit executable plus initial arguments; never evaluated by a shell.
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub download_dir: Option<PathBuf>,
    #[serde(default)]
    pub weights: Option<EngineWeights>,
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
            replicas: 1,
            router_command: String::new(),
            extra_args: Vec::new(),
            command: String::new(),
            download_dir: None,
            weights: None,
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
        self.recipe_from(&RecipeCatalog::bundled()?)
    }

    fn recipe_from(&self, catalog: &RecipeCatalog) -> Result<ResolvedRecipe> {
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
        let recipe = (self.engine == EngineKind::Sglang)
            .then(|| self.recipe())
            .transpose()?;
        self.argv_with_recipe(recipe.as_ref())
    }

    fn argv_with_recipe(&self, recipe: Option<&ResolvedRecipe>) -> Result<Vec<String>> {
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
            EngineKind::Sglang => recipe.context("SGLang recipe is missing")?.args.clone(),
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
    health_url: String,
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

/// Owns a model replica set and its router, or connects to an external endpoint.
pub struct EngineProcess {
    children: Mutex<Vec<EngineChild>>,
    launch: Mutex<()>,
    cancel_start: watch::Sender<u64>,
    stopped: AtomicBool,
    recipe: Option<ResolvedRecipe>,
    config: EngineConfig,
    client: reqwest::Client,
    managed: bool,
}

impl EngineProcess {
    pub fn connect(config: EngineConfig) -> Result<Self> {
        Ok(Self {
            children: Mutex::new(Vec::new()),
            launch: Mutex::new(()),
            cancel_start: watch::channel(0).0,
            stopped: AtomicBool::new(false),
            recipe: None,
            config,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()?,
            managed: false,
        })
    }

    pub async fn spawn(config: EngineConfig) -> Result<Self> {
        Self::spawn_with_catalog(config, &RecipeCatalog::bundled()?).await
    }

    pub async fn spawn_with_catalog(config: EngineConfig, catalog: &RecipeCatalog) -> Result<Self> {
        ensure!(config.replicas > 0, "replica count must be positive");
        let recipe = (config.engine == EngineKind::Sglang)
            .then(|| config.recipe_from(catalog))
            .transpose()?;
        if let Some(recipe) = &recipe {
            ensure!(
                config
                    .tensor_parallel_size
                    .checked_mul(config.replicas)
                    .is_some_and(|count| count <= recipe.gpu.max_per_node),
                "replica set exceeds SDK node capacity"
            );
        } else {
            ensure!(config.replicas == 1, "replica routing requires SGLang");
        }
        let mut process = Self::connect(config)?;
        process.recipe = recipe;
        process.managed = true;
        if process.config.weights.is_none() {
            *process.children.lock().await = process.spawn_children(None)?;
        }
        Ok(process)
    }

    async fn ensure_started(&self) -> Result<()> {
        let mut cancelled = self.cancel_start.subscribe();
        let generation = *cancelled.borrow();
        let _launch = self.launch.lock().await;
        ensure!(
            !self.stopped.load(Ordering::Acquire),
            "serving engine has stopped"
        );
        if !self.children.lock().await.is_empty() {
            return Ok(());
        }
        let verified = if let Some(weights) = &self.config.weights {
            let limits = StagingLimits {
                allow_loopback_http: weights.allow_loopback_http,
                timeout: Duration::from_secs(self.config.startup_timeout_secs),
                ..Default::default()
            };
            Some(tokio::select! {
                result = stage_weights(&weights.manifest_url, &weights.source, &weights.cache_dir, &limits) => result?,
                _ = cancelled.changed() => bail!("weight staging cancelled"),
            })
        } else {
            None
        };
        let mut children = self.children.lock().await;
        ensure!(
            !self.stopped.load(Ordering::Acquire) && *cancelled.borrow() == generation,
            "engine startup cancelled"
        );
        *children = self.spawn_children(verified.as_ref().map(|weights| weights.model_dir()))?;
        Ok(())
    }

    fn spawn_children(&self, model_dir: Option<&Path>) -> Result<Vec<EngineChild>> {
        let mut children = Vec::new();
        let devices: Option<Vec<_>> = std::env::var("CUDA_VISIBLE_DEVICES")
            .ok()
            .map(|value| value.split(',').map(str::trim).map(str::to_owned).collect());
        let count = (self.config.tensor_parallel_size as usize)
            .checked_mul(self.config.replicas as usize)
            .context("replica GPU count overflow")?;
        if self.config.replicas > 1 {
            ensure!(
                devices
                    .as_ref()
                    .is_some_and(|devices| devices.len() >= count
                        && devices.iter().all(|id| !id.is_empty() && id != "-1")
                        && devices
                            .iter()
                            .collect::<std::collections::HashSet<_>>()
                            .len()
                            == devices.len()),
                "replicas require a distinct CUDA_VISIBLE_DEVICES allocation"
            );
        }
        let mut urls = Vec::new();
        for replica in 0..self.config.replicas {
            let mut config = self.config.clone();
            config.host = "127.0.0.1".into();
            if config.engine == EngineKind::Sglang {
                config.port = u16::try_from(replica + 1)
                    .ok()
                    .and_then(|offset| config.port.checked_add(offset))
                    .context("replica port range overflow")?;
            }
            urls.push(format!("http://127.0.0.1:{}", config.port));
            let mut env = self
                .recipe
                .as_ref()
                .map(|recipe| recipe.env.clone())
                .unwrap_or_default();
            if self.config.replicas > 1 {
                let start = replica as usize * config.tensor_parallel_size as usize;
                env.insert(
                    "CUDA_VISIBLE_DEVICES".into(),
                    devices.as_ref().unwrap()[start..start + config.tensor_parallel_size as usize]
                        .join(","),
                );
            }
            let mut argv = config.argv_with_recipe(self.recipe.as_ref())?;
            if let Some(directory) = model_dir {
                let flag = if config.engine == EngineKind::Sglang {
                    "--model-path"
                } else {
                    "--model"
                };
                set_option(
                    &mut argv,
                    &["--model", "--model-path"],
                    flag,
                    directory.to_str().context("weights path is not UTF-8")?,
                );
                set_option(
                    &mut argv,
                    &["--served-model-name"],
                    "--served-model-name",
                    &self.config.model,
                );
            }
            let endpoint = if config.engine == EngineKind::Sglang {
                urls.last().unwrap().clone()
            } else {
                self.endpoint()
            };
            children.push(self.spawn_child(&argv, env, endpoint)?);
        }
        if self.config.engine == EngineKind::Sglang {
            let mut argv: Vec<String> = if self.config.router_command.is_empty() {
                vec![
                    "python3".into(),
                    "-m".into(),
                    "sglang_router.launch_router".into(),
                ]
            } else {
                self.config
                    .router_command
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect()
            };
            ensure!(!argv.is_empty(), "empty router command");
            argv.extend([
                "--host".into(),
                "127.0.0.1".into(),
                "--port".into(),
                self.config.port.to_string(),
                "--policy".into(),
                "cache_aware".into(),
                "--worker-urls".into(),
            ]);
            argv.extend(urls);
            children.push(self.spawn_child(&argv, Default::default(), self.endpoint())?);
        }
        Ok(children)
    }

    fn spawn_child(
        &self,
        argv: &[String],
        env: std::collections::BTreeMap<String, String>,
        endpoint: String,
    ) -> Result<EngineChild> {
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
        command.envs(env);
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
            health_url: format!("{endpoint}/health"),
            #[cfg(unix)]
            group: child.id().context("serving engine has no pid")? as i32,
            child,
        })
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub fn endpoint(&self) -> String {
        let host = match if self.managed {
            "127.0.0.1"
        } else {
            self.config.host.as_str()
        } {
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
        let urls = if self.managed {
            let mut children = self.children.lock().await;
            if children.is_empty()
                || children
                    .iter_mut()
                    .any(|process| !matches!(process.child.try_wait(), Ok(None)))
            {
                return false;
            }
            children
                .iter()
                .map(|child| child.health_url.clone())
                .collect::<Vec<_>>()
        } else {
            vec![format!("{}/health", self.endpoint())]
        };
        let mut probes = tokio::task::JoinSet::new();
        for url in urls {
            let client = self.client.clone();
            probes.spawn(async move {
                matches!(client.get(url).timeout(Duration::from_secs(1)).send().await,
                Ok(response) if response.status().is_success())
            });
        }
        while let Some(result) = probes.join_next().await {
            if !matches!(result, Ok(true)) {
                return false;
            }
        }
        true
    }

    pub async fn wait_ready(&self) -> Result<()> {
        let generation = *self.cancel_start.borrow();
        let result = tokio::time::timeout(
            Duration::from_secs(self.config.startup_timeout_secs),
            async {
                if self.managed {
                    self.ensure_started().await?;
                }
                loop {
                    if self.managed {
                        let mut children = self.children.lock().await;
                        ensure!(!children.is_empty(), "serving engine has stopped");
                        for child in children.iter_mut() {
                            if let Some(status) = child.child.try_wait()? {
                                bail!("serving engine exited during startup: {status}");
                            }
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
        if result.is_err() && self.managed && *self.cancel_start.borrow() == generation {
            self.shutdown().await;
        }
        result
    }

    pub async fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
        self.cancel_start
            .send_modify(|generation| *generation = generation.wrapping_add(1));
        let mut children = self.children.lock().await;
        while let Some(mut child) = children.pop() {
            child.terminate().await;
        }
    }

    pub async fn restart(&self) -> Result<()> {
        ensure!(self.managed, "cannot restart an externally managed engine");
        self.shutdown().await;
        self.stopped.store(false, Ordering::Release);
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
