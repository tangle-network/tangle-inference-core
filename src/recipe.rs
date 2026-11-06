use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const ENGINE_PROFILES: &str = include_str!("../engine-profiles.json");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeCatalog {
    pub image: String,
    pub common_args: Vec<String>,
    pub families: BTreeMap<String, FamilyRecipe>,
    #[serde(default)]
    pub measurements: Vec<ThroughputMeasurement>,
    #[serde(flatten)]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FamilyRecipe {
    pub parent_model: String,
    #[serde(default)]
    pub parent_revision: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub arms: BTreeMap<String, RecipeArm>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(flatten)]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecipeArm {
    #[serde(default)]
    pub parent_model: Option<String>,
    #[serde(default)]
    pub parent_revision: Option<String>,
    pub classes: BTreeMap<String, GpuRecipe>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(flatten)]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GpuRecipe {
    pub tp: Vec<u32>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(flatten)]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

/// Raw run counters belong to one model, serving topology and provider SKU.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThroughputMeasurement {
    pub family: String,
    pub arm: String,
    pub gpu_class: String,
    pub tp: u32,
    pub replicas: u32,
    pub allocated_gpu_count: u32,
    pub provider: String,
    pub provider_sku: String,
    pub region: String,
    pub concurrency: u32,
    pub model: String,
    pub revision: String,
    pub weights_manifest_sha256: String,
    pub image: String,
    pub measured_at: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub elapsed_ms: u64,
    pub raw_result_uri: String,
    pub raw_result_sha256: String,
}

impl ThroughputMeasurement {
    pub fn output_tokens_per_second(&self) -> Option<f64> {
        (self.elapsed_ms > 0 && self.output_tokens > 0)
            .then(|| self.output_tokens as f64 * 1000.0 / self.elapsed_ms as f64)
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedRecipe {
    pub image: String,
    pub family: String,
    pub arm: String,
    pub gpu_class: Option<String>,
    pub env: BTreeMap<String, String>,
    pub model: String,
    pub revision: Option<String>,
    pub args: Vec<String>,
}

impl RecipeCatalog {
    pub fn bundled() -> Result<Self> {
        Self::from_json(ENGINE_PROFILES)
    }

    pub fn from_json(json: &str) -> Result<Self> {
        let catalog: Self = serde_json::from_str(json)?;
        ensure!(
            catalog
                .image
                .rsplit_once("@sha256:")
                .is_some_and(|(_, d)| is_digest(d, 64)),
            "engine recipe image must be pinned by sha256 digest"
        );
        ensure!(
            !catalog.metadata.contains_key("gpus"),
            "GPU facts belong to the SDK catalog"
        );
        for row in &catalog.measurements {
            let plan = catalog.resolve_for(
                &row.family,
                Some(&row.model),
                &row.arm,
                Some(&row.gpu_class),
                row.tp,
            )?;
            ensure!(
                row.replicas > 0 && row.concurrency > 0
                    && row.tp.checked_mul(row.replicas).is_some_and(|n| n <= row.allocated_gpu_count)
                    && row.output_tokens_per_second().is_some(),
                "measurement requires a positive, fully allocated serving topology and raw counters"
            );
            ensure!(
                row.image == catalog.image
                    && plan
                        .revision
                        .as_ref()
                        .is_none_or(|revision| revision == &row.revision)
                    && is_digest(&row.weights_manifest_sha256, 64)
                    && is_digest(&row.raw_result_sha256, 64)
                    && (is_digest(&row.revision, 40) || is_digest(&row.revision, 64))
                    && [
                        &row.provider,
                        &row.provider_sku,
                        &row.region,
                        &row.measured_at,
                        &row.raw_result_uri
                    ]
                    .iter()
                    .all(|s| !s.trim().is_empty()),
                "measurement requires immutable model, engine, manifest and raw-result provenance"
            );
        }
        Ok(catalog)
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
        let selected = recipe
            .arms
            .get(arm)
            .with_context(|| format!("unknown quantization arm: {arm}"))?;
        let gpu_class = gpu_class.context("gpu_class requires an exact SDK catalog id")?;
        let gpu = selected
            .classes
            .get(gpu_class)
            .with_context(|| format!("unsupported GPU class for {family}/{arm}: {gpu_class}"))?;
        ensure!(
            gpu.tp.contains(&parallelism),
            "unsupported tensor parallelism for {family}/{arm}/{gpu_class}"
        );
        let parent = selected
            .parent_model
            .as_deref()
            .unwrap_or(&recipe.parent_model);
        let model = model.unwrap_or(parent);
        ensure!(
            !model.is_empty() && !model.starts_with('-'),
            "invalid model id or path"
        );
        let revision = if model != parent {
            None
        } else if selected.parent_model.is_some() {
            selected.parent_revision.clone()
        } else {
            selected
                .parent_revision
                .as_ref()
                .or(recipe.parent_revision.as_ref())
                .cloned()
        };
        let mut args = self.common_args.clone();
        args.extend(
            recipe
                .args
                .iter()
                .chain(&selected.args)
                .chain(&gpu.args)
                .cloned(),
        );
        let mut env = recipe.env.clone();
        env.extend(selected.env.clone());
        env.extend(gpu.env.clone());
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
        if let Some(revision) = &revision {
            set_option(&mut args, &["--revision"], "--revision", revision);
        }
        Ok(ResolvedRecipe {
            image: self.image.clone(),
            family: family.into(),
            arm: arm.into(),
            gpu_class: Some(gpu_class.into()),
            env,
            model: model.into(),
            revision,
            args,
        })
    }
}

fn is_digest(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(crate) fn set_option(args: &mut Vec<String>, aliases: &[&str], flag: &str, value: &str) {
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
