use std::path::Path;

use anyhow::Result;

pub fn write_knowledge_transcript(
    project_path: &str,
    graph_id: &str,
    graph_run_id: &str,
    result_json: &str,
) -> Result<()> {
    let dir = Path::new(project_path)
        .join(".ai/knowledge/state/graphs")
        .join(graph_id);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{graph_run_id}.md"));
    let content = format!(
        "# Graph Run: {graph_run_id}\n\n\
         **Graph**: `{graph_id}`\n\
         **Completed**: {}\n\n\
         ## Result\n\n```json\n{result_json}\n```\n",
        lillux::time::iso8601_now(),
    );
    std::fs::write(&path, content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_knowledge_transcript_creates_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_string_lossy().to_string();
        write_knowledge_transcript(&path, "test/graph", "gr-abc", r#"{"success": true}"#).unwrap();
        let written = std::fs::read_to_string(
            dir.path().join(".ai/knowledge/state/graphs/test/graph/gr-abc.md"),
        )
        .unwrap();
        assert!(written.contains("gr-abc"));
        assert!(written.contains("test/graph"));
        assert!(written.contains(r#"{"success": true}"#));
    }
}
