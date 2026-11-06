use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const ENGINE_PROFILES: &str = include_str!("../engine-profiles.json");

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
