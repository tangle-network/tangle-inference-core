use tangle_inference_core::engine::EngineConfig;

#[test]
fn selected_gpu_recipe_reaches_sglang_instead_of_silently_using_blackwell_flags(
) -> anyhow::Result<()> {
    let mut value = serde_json::json!({
        "engine": "sglang", "base_family": "deepseek-v4.1-flash-1017",
        "model": "acme/derived-deepseek", "recipe_arm": "native",
        "gpu_class": "nvidia-h200", "tensor_parallel_size": 8
    });
    let config: EngineConfig = serde_json::from_value(value.clone())?;
    let argv = config.argv()?;
    assert!(argv
        .windows(2)
        .any(|args| args == ["--moe-runner-backend", "marlin"]));
    assert!(argv.windows(2).any(|args| args == ["--attn-dp-size", "8"]));
    assert!(argv
        .iter()
        .any(|arg| arg == "--enable-decoder-swa-bounded-replay"));
    assert!(!argv.iter().any(|arg| arg.contains("{tp}")));
    value["gpu_class"] = serde_json::json!("nvidia-a100");
    assert!(serde_json::from_value::<EngineConfig>(value.clone())?
        .argv()
        .is_err());
    value["gpu_class"] = serde_json::json!("nvidia-h200");
    value["recipe_arm"] = serde_json::json!("nvfp4");
    assert!(serde_json::from_value::<EngineConfig>(value)?
        .argv()
        .is_err());
    Ok(())
}
