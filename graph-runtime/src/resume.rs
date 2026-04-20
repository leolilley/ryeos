use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeState {
    pub current_node: String,
    pub step_count: u32,
    pub state: Value,
    pub graph_run_id: String,
}

pub fn load_resume_state(
    project_path: &str,
    graph_run_id: &str,
) -> Result<Option<ResumeState>> {
    let ref_path = Path::new(project_path)
        .join(".ai")
        .join("state")
        .join("objects")
        .join("refs")
        .join("graphs")
        .join(format!("{graph_run_id}.json"));

    if !ref_path.exists() {
        return Ok(None);
    }

    let ref_content = std::fs::read_to_string(&ref_path)?;
    let ref_data: Value = serde_json::from_str(&ref_content)?;

    let snapshot_hash = ref_data
        .get("checkpoint_hash")
        .and_then(|v| v.as_str());

    let Some(hash) = snapshot_hash else {
        return load_from_ref_data(&ref_data, graph_run_id);
    };

    let obj_path = Path::new(project_path)
        .join(".ai")
        .join("state")
        .join("objects")
        .join(hash);

    if !obj_path.exists() {
        return load_from_ref_data(&ref_data, graph_run_id);
    }

    let obj_content = std::fs::read_to_string(&obj_path)?;
    let checkpoint: Value = serde_json::from_str(&obj_content)?;

    let current_node = checkpoint
        .get("current_node")
        .and_then(|v| v.as_str())
        .unwrap_or("start")
        .to_string();
    let step_count = checkpoint
        .get("step_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let state = checkpoint.get("state").cloned().unwrap_or(Value::Object(Default::default()));

    Ok(Some(ResumeState {
        current_node,
        step_count,
        state,
        graph_run_id: graph_run_id.to_string(),
    }))
}

fn load_from_ref_data(ref_data: &Value, graph_run_id: &str) -> Result<Option<ResumeState>> {
    let current_node = ref_data
        .get("current_node")
        .and_then(|v| v.as_str())
        .map(String::from);
    let step_count = ref_data
        .get("step_count")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let state = ref_data.get("state").cloned();

    match (current_node, step_count, state) {
        (Some(node), Some(steps), Some(st)) => Ok(Some(ResumeState {
            current_node: node,
            step_count: steps,
            state: st,
            graph_run_id: graph_run_id.to_string(),
        })),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_resume_state_returns_none_when_no_data() {
        let result = load_resume_state("/tmp/nonexistent", "gr-1").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn load_resume_state_from_ref_file() {
        let dir = std::env::temp_dir().join(format!("graph_resume_test_{}", std::process::id()));
        let ref_dir = dir.join(".ai/state/objects/refs/graphs");
        std::fs::create_dir_all(&ref_dir).unwrap();

        let ref_data = serde_json::json!({
            "current_node": "step3",
            "step_count": 5,
            "state": {"key": "value"}
        });
        std::fs::write(
            ref_dir.join("gr-test123.json"),
            serde_json::to_string(&ref_data).unwrap(),
        ).unwrap();

        let result = load_resume_state(dir.to_str().unwrap(), "gr-test123").unwrap();
        assert!(result.is_some());
        let state = result.unwrap();
        assert_eq!(state.current_node, "step3");
        assert_eq!(state.step_count, 5);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
