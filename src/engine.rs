use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::{watch, Mutex},
};

pub const ENGINE_PROFILES: &str = include_str!("../engine-profiles.json");

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
    #[serde(default, skip_serializing)]
    pub hf_token: Option<String>,
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
            hf_token: None,
            download_dir: None,
            startup_timeout_secs: default_timeout(),
            external: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeCatalog {
    pub image: String,
    pub common_args: Vec<String>,
    pub families: BTreeMap<String, FamilyRecipe>,
    #[serde(flatten)]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FamilyRecipe {
    pub parent_model: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub arms: BTreeMap<String, RecipeArm>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(flatten)]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecipeArm {
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub gpus: Vec<String>,
    #[serde(default)]
    pub recipe_tp: BTreeMap<String, Vec<u32>>,
    #[serde(default)]
    pub gpu_overrides: BTreeMap<String, GpuRecipe>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(flatten)]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GpuRecipe {
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedRecipe {
    pub image: String,
    pub family: String,
    pub arm: String,
    pub gpu_class: Option<String>,
    pub env: BTreeMap<String, String>,
    pub model: String,
    pub args: Vec<String>,
}

impl RecipeCatalog {
    pub fn bundled() -> Result<Self> {
        Self::from_json(ENGINE_PROFILES)
    }

    pub fn from_json(json: &str) -> Result<Self> {
        let catalog: Self = serde_json::from_str(json)?;
        let digest = catalog
            .image
            .rsplit_once("@sha256:")
            .map(|(_, digest)| digest);
        ensure!(
            digest.is_some_and(|d| d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit())),
            "engine recipe image must be pinned by sha256 digest"
        );
        Ok(catalog)
    }

    /// Resolve the family's declared default node shape for planning.
    pub fn resolve(&self, family: &str, model: Option<&str>) -> Result<ResolvedRecipe> {
        let recipe = self.families.get(family).context("unknown model family")?;
        let shape = recipe
            .metadata
            .get("default_shape")
            .and_then(|v| v.as_str())
            .and_then(|v| v.rsplit_once('-'));
        let parallelism = shape
            .map(|(_, count)| count.parse())
            .transpose()?
            .unwrap_or(1);
        self.resolve_for(
            family,
            model,
            "native",
            shape.map(|(gpu, _)| gpu),
            parallelism,
        )
    }

    pub fn resolve_for(
        &self,
        family: &str,
        model: Option<&str>,
        arm: &str,
        gpu_class: Option<&str>,
        parallelism: u32,
    ) -> Result<ResolvedRecipe> {
        ensure!(parallelism > 0, "tensor parallelism must be positive");
        let recipe = self
            .families
            .get(family)
            .with_context(|| format!("unknown model family: {family}"))?;
        let model = model.unwrap_or(&recipe.parent_model);
        ensure!(
            !model.is_empty() && !model.starts_with('-'),
            "invalid model id or path"
        );
        let mut args = self.common_args.clone();
        args.extend(recipe.args.iter().cloned());
        let mut env = recipe.env.clone();
        if recipe.arms.is_empty() {
            ensure!(arm == "native", "unknown quantization arm: {arm}");
        } else {
            let selected = recipe
                .arms
                .get(arm)
                .with_context(|| format!("unknown quantization arm: {arm}"))?;
            let gpu = if selected.gpus.is_empty() {
                ensure!(
                    selected.gpu_overrides.is_empty(),
                    "GPU overlay lacks supported hardware"
                );
                None
            } else {
                let requested =
                    gpu_class.context("gpu_class is required for this family recipe")?;
                let requested = requested.strip_prefix("nvidia-").unwrap_or(requested);
                Some(
                    selected
                        .gpus
                        .iter()
                        .find(|gpu| gpu.eq_ignore_ascii_case(requested))
                        .with_context(|| {
                            format!("unsupported GPU class for {family}/{arm}: {requested}")
                        })?,
                )
            };
            if let Some(gpu) = gpu {
                let choices = selected
                    .recipe_tp
                    .get(gpu)
                    .context("GPU has no supported tensor parallel recipe")?;
                ensure!(
                    choices.contains(&parallelism),
                    "unsupported tensor parallelism for {family}/{arm}/{gpu}"
                );
                if let Some(overlay) = selected.gpu_overrides.get(gpu) {
                    args.extend(selected.args.iter().cloned());
                    args.extend(overlay.args.iter().cloned());
                    env.extend(selected.env.clone());
                    env.extend(overlay.env.clone());
                } else {
                    args.extend(selected.args.iter().cloned());
                    env.extend(selected.env.clone());
                }
            } else {
                args.extend(selected.args.iter().cloned());
                env.extend(selected.env.clone());
            }
        }
        for value in args.iter_mut().chain(env.values_mut()) {
            *value = value.replace("{tp}", &parallelism.to_string());
            ensure!(
                !value.contains('{') && !value.contains('}'),
                "unresolved recipe placeholder"
            );
        }
        set_option(
            &mut args,
            &["--model-path", "--model"],
            "--model-path",
            model,
        );
        set_option(
            &mut args,
            &["--tp", "--tp-size", "--tensor-parallel-size"],
            "--tp",
            &parallelism.to_string(),
        );
        Ok(ResolvedRecipe {
            image: self.image.clone(),
            family: family.into(),
            arm: arm.into(),
            gpu_class: gpu_class.map(str::to_owned),
            env,
            model: model.into(),
            args,
        })
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

fn set_option(args: &mut Vec<String>, aliases: &[&str], flag: &str, value: &str) {
    let mut source = std::mem::take(args).into_iter();
    while let Some(arg) = source.next() {
        if aliases.contains(&arg.as_str()) {
            source.next();
        } else if !aliases.iter().any(|name| {
            arg.strip_prefix(name)
                .is_some_and(|rest| rest.starts_with('='))
        }) {
            args.push(arg);
        }
    }
    args.extend([flag.to_owned(), value.to_owned()]);
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
        if let Some(token) = &self.config.hf_token {
            command.env("HF_TOKEN", token);
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
                let token = self.config.hf_token.clone();
                tokio::spawn(async move {
                    let mut lines = BufReader::new(pipe).lines();
                    while let Ok(Some(mut line)) = lines.next_line().await {
                        if let Some(token) = token.as_ref().filter(|token| !token.is_empty()) {
                            line = line.replace(token, "[REDACTED]");
                        }
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
