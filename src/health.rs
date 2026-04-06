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

#[cfg(test)]
mod tests {
    use super::*;

    const SINGLE_GPU: &str = "0, NVIDIA A100-SXM4-80GB, 81920, 1024, 80896, 42, 15, 535.129.03";

    const MULTI_GPU: &str = "\
0, NVIDIA A100-SXM4-80GB, 81920, 1024, 80896, 42, 15, 535.129.03
1, NVIDIA A100-SXM4-80GB, 81920, 512, 81408, 38, 0, 535.129.03";

    #[test]
    fn test_parse_single_gpu() {
        let gpus = parse_nvidia_smi_output(SINGLE_GPU);
        assert_eq!(gpus.len(), 1);
        let gpu = &gpus[0];
        assert_eq!(gpu.index, 0);
        assert_eq!(gpu.name, "NVIDIA A100-SXM4-80GB");
        assert_eq!(gpu.memory_total_mib, 81920);
        assert_eq!(gpu.memory_used_mib, 1024);
        assert_eq!(gpu.memory_free_mib, 80896);
        assert_eq!(gpu.temperature_c, Some(42));
        assert_eq!(gpu.utilization_pct, Some(15));
        assert_eq!(gpu.driver_version, "535.129.03");
    }

    #[test]
    fn test_parse_multi_gpu() {
        let gpus = parse_nvidia_smi_output(MULTI_GPU);
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].index, 0);
        assert_eq!(gpus[1].index, 1);
        assert_eq!(gpus[1].memory_used_mib, 512);
    }

    #[test]
    fn test_parse_empty_output() {
        let gpus = parse_nvidia_smi_output("");
        assert!(gpus.is_empty());
    }

    #[test]
    fn test_parse_malformed_line_skipped() {
        let output = "0, NVIDIA A100\ngarbage\n0, NVIDIA A100-SXM4-80GB, 81920, 1024, 80896, 42, 15, 535.129.03";
        let gpus = parse_nvidia_smi_output(output);
        assert_eq!(gpus.len(), 1);
    }

    #[test]
    fn test_parse_missing_optional_fields() {
        let output = "0, Tesla T4, 16384, 0, 16384, [N/A], [N/A], 535.129.03";
        let gpus = parse_nvidia_smi_output(output);
        assert_eq!(gpus.len(), 1);
        assert!(gpus[0].temperature_c.is_none());
        assert!(gpus[0].utilization_pct.is_none());
    }

    #[test]
    fn test_gpu_info_serialization() {
        let gpu = GpuInfo {
            index: 0,
            name: "Test GPU".to_string(),
            memory_total_mib: 8192,
            memory_used_mib: 100,
            memory_free_mib: 8092,
            temperature_c: Some(55),
            utilization_pct: None,
            driver_version: "535.0".to_string(),
        };
        let json = serde_json::to_value(&gpu).unwrap();
        assert_eq!(json["name"], "Test GPU");
        assert_eq!(json["memory_total_mib"], 8192);
        assert!(json["utilization_pct"].is_null());
    }
}
