use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Result};

pub fn write_knowledge_transcript(
    project_path: &str,
    graph_id: &str,
    graph_run_id: &str,
    result_json: &str,
) -> Result<()> {
    let base = Path::new(project_path).join(".ai/knowledge/state/graphs");
    let dir = base.join(safe_relative_subpath(graph_id)?);
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

/// Return `graph_id` as a relative `PathBuf` rebuilt from its
/// `Normal` components only. Rejects absolute paths, drive prefixes,
/// `..` traversal, and Windows verbatim/UNC roots so an attacker (or
/// a buggy upstream that accepts a malformed `category`) cannot
/// escape the `.ai/knowledge/state/graphs/` base on `Path::join`.
///
/// Empty input or input that contains no `Normal` components yields
/// an error rather than silently writing to the base directory.
fn safe_relative_subpath(graph_id: &str) -> Result<PathBuf> {
    let raw = Path::new(graph_id);
    let mut out = PathBuf::new();
    for component in raw.components() {
        match component {
            Component::Normal(seg) => out.push(seg),
            // Anchor / drive / current-dir / parent-dir all forbidden:
            // rebuilding only from Normal components keeps the result
            // strictly relative and inside the base on `join`.
            Component::ParentDir => bail!(
                "graph_id contains '..' traversal which would escape the knowledge base: {graph_id:?}"
            ),
            Component::RootDir | Component::Prefix(_) => continue,
            Component::CurDir => continue,
        }
    }
    if out.as_os_str().is_empty() {
        bail!("graph_id is empty after stripping unsafe path components: {graph_id:?}");
    }
    Ok(out)
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

    /// The original `/flow` failure mode (5.5δ-flakes root cause): a
    /// leading slash in `graph_id` once made `Path::join` clobber the
    /// base path and write to filesystem root (EACCES). Rebuilding
    /// from Normal components only neutralises this.
    #[test]
    fn leading_slash_does_not_escape_base() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_string_lossy().to_string();
        write_knowledge_transcript(&path, "/flow", "gr-1", r#"{"ok": 1}"#).unwrap();
        let written = std::fs::read_to_string(
            dir.path().join(".ai/knowledge/state/graphs/flow/gr-1.md"),
        )
        .unwrap();
        assert!(written.contains("gr-1"));
    }

    /// Defense against a buggy upstream (or attacker-controlled
    /// category) that smuggles `..` into the graph_id.
    #[test]
    fn parent_dir_traversal_is_rejected() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_string_lossy().to_string();
        let err = write_knowledge_transcript(&path, "../escape", "gr-x", "{}").unwrap_err();
        assert!(err.to_string().contains(".."), "expected traversal error: {err}");
    }

    #[test]
    fn embedded_parent_dir_traversal_is_rejected() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_string_lossy().to_string();
        let err = write_knowledge_transcript(&path, "ok/../escape", "gr-x", "{}").unwrap_err();
        assert!(err.to_string().contains(".."), "expected traversal error: {err}");
    }

    /// Empty / pure-anchor graph_ids cannot be silently written to
    /// the base directory itself — they must hard-error.
    #[test]
    fn empty_or_root_only_graph_id_is_rejected() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_string_lossy().to_string();
        assert!(write_knowledge_transcript(&path, "", "gr-x", "{}").is_err());
        assert!(write_knowledge_transcript(&path, "/", "gr-x", "{}").is_err());
    }
}
