use std::path::Path;

use serde_json::{json, Value};

use crate::model::NodeReceipt;

pub fn write_checkpoint(
    project_path: &str,
    graph_run_id: &str,
    current_node: &str,
    step: u32,
    state: &Value,
) -> anyhow::Result<String> {
    let checkpoint = json!({
        "current_node": current_node,
        "step_count": step,
        "state": state,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });

    let checkpoint_json = serde_json::to_string(&checkpoint)?;
    let hash = lillux::cas::sha256_hex(checkpoint_json.as_bytes());

    let obj_dir = Path::new(project_path)
        .join(".ai")
        .join("state")
        .join("objects");
    std::fs::create_dir_all(&obj_dir)?;

    let obj_path = obj_dir.join(&hash);
    let tmp = obj_path.with_extension("tmp");
    std::fs::write(&tmp, &checkpoint_json)?;
    std::fs::rename(&tmp, &obj_path)?;

    let ref_dir = Path::new(project_path)
        .join(".ai")
        .join("state")
        .join("objects")
        .join("refs")
        .join("graphs");
    std::fs::create_dir_all(&ref_dir)?;

    let ref_data = json!({
        "graph_run_id": graph_run_id,
        "checkpoint_hash": hash,
        "current_node": current_node,
        "step_count": step,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });

    let ref_path = ref_dir.join(format!("{graph_run_id}.json"));
    let ref_tmp = ref_path.with_extension("tmp");
    std::fs::write(&ref_tmp, serde_json::to_string(&ref_data)?)?;
    std::fs::rename(&ref_tmp, &ref_path)?;

    Ok(hash)
}

pub fn write_node_receipt(
    project_path: &str,
    graph_run_id: &str,
    receipt: &NodeReceipt,
) -> anyhow::Result<Value> {
    let receipt_json = json!({
        "node": receipt.node,
        "step": receipt.step,
        "graph_run_id": graph_run_id,
        "node_result_hash": receipt.result_hash,
        "cache_hit": receipt.cache_hit,
        "elapsed_ms": receipt.elapsed_ms,
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "error": receipt.error,
    });

    let receipt_str = serde_json::to_string(&receipt_json)?;
    let hash = lillux::cas::sha256_hex(receipt_str.as_bytes());

    let obj_dir = Path::new(project_path)
        .join(".ai")
        .join("state")
        .join("objects");
    std::fs::create_dir_all(&obj_dir)?;

    let obj_path = obj_dir.join(&hash);
    if !obj_path.exists() {
        let tmp = obj_path.with_extension("tmp");
        std::fs::write(&tmp, &receipt_str)?;
        std::fs::rename(&tmp, &obj_path)?;
    }

    Ok(receipt_json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NodeReceipt;
    use std::path::PathBuf;

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "graph_persist_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_node_receipt_formats_correctly() {
        let receipt = NodeReceipt {
            node: "step1".to_string(),
            step: 1,
            result_hash: Some("abc123".to_string()),
            cache_hit: false,
            elapsed_ms: 142,
            error: None,
        };
        let dir = tempdir();
        let output = write_node_receipt(dir.to_str().unwrap(), "gr-1", &receipt).unwrap();
        assert_eq!(output["cache_hit"], false);
        assert_eq!(output["elapsed_ms"], 142);
        assert_eq!(output["node_result_hash"], "abc123");
        assert!(output.get("timestamp").is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_checkpoint_creates_files() {
        let dir = tempdir();
        let hash = write_checkpoint(
            dir.to_str().unwrap(),
            "gr-test",
            "step3",
            5,
            &json!({"key": "val"}),
        )
        .unwrap();
        assert!(!hash.is_empty());

        let obj_path = dir.join(".ai/state/objects").join(&hash);
        assert!(obj_path.exists());

        let ref_path = dir.join(".ai/state/objects/refs/graphs/gr-test.json");
        assert!(ref_path.exists());

        let ref_data: Value =
            serde_json::from_str(&std::fs::read_to_string(&ref_path).unwrap()).unwrap();
        assert_eq!(ref_data["checkpoint_hash"], hash);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
