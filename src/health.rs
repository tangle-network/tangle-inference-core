use serde::{Deserialize, Serialize};
use std::process::Command;

/// Information about a single GPU.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuInfo {
    pub index: u32,
    pub name: String,
    pub memory_total_mib: u32,
    pub memory_used_mib: u32,
    pub memory_free_mib: u32,
    pub temperature_c: Option<u32>,
    pub utilization_pct: Option<u32>,
    pub driver_version: String,
}

/// Parse nvidia-smi CSV output into a list of GPU info structs.
pub fn parse_nvidia_smi_output(output: &str) -> Vec<GpuInfo> {
    let mut gpus = Vec::new();
    for line in output.lines() {
        let fields: Vec<&str> = line.split(", ").collect();
        if fields.len() < 8 {
            continue;
        }
        gpus.push(GpuInfo {
            index: fields[0].trim().parse().unwrap_or(0),
            name: fields[1].trim().to_string(),
            memory_total_mib: fields[2].trim().parse().unwrap_or(0),
            memory_used_mib: fields[3].trim().parse().unwrap_or(0),
            memory_free_mib: fields[4].trim().parse().unwrap_or(0),
            temperature_c: fields[5].trim().parse().ok(),
            utilization_pct: fields[6].trim().parse().ok(),
            driver_version: fields[7].trim().to_string(),
        });
    }
    gpus
}

/// Detect available NVIDIA GPUs via nvidia-smi.
pub async fn detect_gpus() -> anyhow::Result<Vec<GpuInfo>> {
    let output = tokio::task::spawn_blocking(|| {
        Command::new("nvidia-smi")
            .args([
                "--query-gpu=index,name,memory.total,memory.used,memory.free,temperature.gpu,utilization.gpu,driver_version",
                "--format=csv,noheader,nounits",
            ])
            .output()
    })
    .await??;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("nvidia-smi failed: {stderr}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_nvidia_smi_output(&stdout))
}

/// Returns TEE status for inclusion in health endpoints.
/// Probes device files and env vars — safe to call repeatedly.
pub fn tee_status() -> serde_json::Value {
    let provider = detect_tee_provider_name();
    serde_json::json!({
        "active": provider.is_some(),
        "provider": provider,
    })
}

fn detect_tee_provider_name() -> Option<&'static str> {
    if std::path::Path::new("/dev/nsm").exists() {
        return Some("aws_nitro");
    }
    if std::path::Path::new("/dev/tdx-guest").exists() {
        return Some("intel_tdx");
    }
    if std::path::Path::new("/dev/sev-guest").exists() {
        return Some("amd_sev_snp");
    }
    if std::env::var("CONFIDENTIAL_SPACE_VERSION").is_ok() {
        return Some("gcp_confidential_space");
    }
    None
}
