use tangle_inference_core::engine::{EngineConfig, RecipeCatalog, ENGINE_PROFILES};

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
    value["gpu_class"] = serde_json::json!("H200");
    assert!(
        serde_json::from_value::<EngineConfig>(value.clone())?
            .argv()
            .is_err(),
        "GPU aliases must not cross the recipe boundary; use the SDK catalog id"
    );
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

#[test]
fn catalog_keeps_stock_identities_and_explicit_b300_shapes() -> anyhow::Result<()> {
    let catalog = RecipeCatalog::bundled()?;
    let glm = catalog.resolve_for("glm-5.3", None, "nvfp4", Some("nvidia-b300"), 8)?;
    assert_eq!(glm.model, "nvidia/GLM-5.3-NVFP4");
    assert!(glm
        .args
        .windows(2)
        .any(|v| v == ["--revision", "e3b8f9f2fad29727dfa95e766c5cac18ea1414bf"]));
    assert!(glm
        .args
        .windows(2)
        .any(|v| v == ["--quantization", "modelopt"]));
    let derived = catalog.resolve_for(
        "glm-5.3",
        Some("acme/derived-glm"),
        "nvfp4",
        Some("nvidia-b300"),
        8,
    )?;
    assert_eq!(derived.model, "acme/derived-glm");
    assert!(!derived.args.iter().any(|v| v == "--revision"));
    assert!(catalog
        .resolve_for("glm-5.3", None, "nvfp4", Some("nvidia-b300"), 4)
        .is_err());
    assert!(catalog
        .resolve_for("glm-5.3", None, "native", None, 8)
        .is_err());
    let flash = catalog.resolve_for(
        "deepseek-v4.1-flash",
        None,
        "native",
        Some("nvidia-b200"),
        4,
    )?;
    assert_eq!(flash.model, "deepseek-ai/DeepSeek-V4.1-Flash");
    assert!(flash
        .args
        .windows(2)
        .any(|v| v == ["--revision", "314113f8a7dadb388215396ba5fbee2c610a04d6"]));
    assert!(flash.args.windows(2).any(|v| v == ["--ep-size", "4"]));
    assert!(!flash.args.iter().any(|v| v == "--attn-dp-size"));
    let exported: serde_json::Value = serde_json::from_str(ENGINE_PROFILES)?;
    assert!(exported.get("gpus").is_none());
    assert!(exported["families"].get("qwen3").is_none());
    assert_eq!(exported["measurements"], serde_json::json!([]));
    assert_eq!(
        exported["families"]["glm-5.3"]["arms"]["nvfp4"]["quantization"]["quant_algo"],
        "MIXED_PRECISION"
    );
    Ok(())
}

#[test]
fn throughput_rows_without_run_provenance_are_rejected() -> anyhow::Result<()> {
    let mut exported: serde_json::Value = serde_json::from_str(ENGINE_PROFILES)?;
    exported["measurements"] = serde_json::json!([{
        "family": "glm-5.3", "arm": "nvfp4", "gpu_class": "nvidia-b200"
    }]);
    assert!(
        RecipeCatalog::from_json(&exported.to_string()).is_err(),
        "a family/class label is not a measured provider SKU throughput result"
    );
    Ok(())
}

#[test]
fn a_custom_recipe_cannot_invent_gpu_classes_or_node_capacity() -> anyhow::Result<()> {
    for (class, tp) in [("H200", 8), ("nvidia-h200", u32::MAX)] {
        let mut exported: serde_json::Value = serde_json::from_str(ENGINE_PROFILES)?;
        let classes = &mut exported["families"]["glm-5.3"]["arms"]["native"]["classes"];
        *classes = serde_json::json!({});
        classes[class] = serde_json::json!({"tp": [tp]});
        assert!(
            RecipeCatalog::from_json(&exported.to_string())
                .and_then(|catalog| catalog.resolve_for("glm-5.3", None, "native", Some(class), tp))
                .is_err(),
            "a recipe cannot override SDK hardware facts: {class}, TP{tp}"
        );
    }
    Ok(())
}
