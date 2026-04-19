use std::path::{Path, PathBuf};

pub const AI_DIR: &str = ".ai";
pub const STATE_THREADS_REL: &str = "state/threads";
pub const KNOWLEDGE_THREADS_REL: &str = "knowledge/agent/threads";

pub fn safe_rel_path(id: &str) -> anyhow::Result<PathBuf> {
    let mut parts = Vec::new();
    for segment in id.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." || segment.contains('\\') {
            anyhow::bail!("invalid path segment in id {:?}: {:?}", id, segment);
        }
        parts.push(segment);
    }
    if parts.is_empty() {
        anyhow::bail!("empty id");
    }
    Ok(parts.iter().fold(PathBuf::new(), |acc, p| acc.join(p)))
}

pub fn thread_state_dir(project_root: &Path, thread_id: &str) -> anyhow::Result<PathBuf> {
    let rel = safe_rel_path(thread_id)?;
    Ok(project_root.join(AI_DIR).join(STATE_THREADS_REL).join(rel))
}

pub fn thread_transcript_path(project_root: &Path, thread_id: &str) -> anyhow::Result<PathBuf> {
    Ok(thread_state_dir(project_root, thread_id)?.join("transcript.jsonl"))
}

pub fn thread_knowledge_path(project_root: &Path, thread_id: &str) -> anyhow::Result<PathBuf> {
    let rel = safe_rel_path(thread_id)?;
    let mut path = project_root
        .join(AI_DIR)
        .join(KNOWLEDGE_THREADS_REL)
        .join(&rel);
    let file_name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    path.set_file_name(format!("{file_name}.md"));
    Ok(path)
}

pub fn user_hooks_path(user_space: &Path) -> PathBuf {
    user_space.join(AI_DIR).join("config/agent/hooks.yaml")
}

pub fn project_hooks_path(project_root: &Path) -> PathBuf {
    project_root.join(AI_DIR).join("config/agent/hooks.yaml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_rel_path_allows_nested_thread_ids() {
        assert_eq!(
            safe_rel_path("team/sub/thread-1").unwrap(),
            PathBuf::from("team").join("sub").join("thread-1")
        );
    }

    #[test]
    fn safe_rel_path_rejects_parent_traversal() {
        assert!(safe_rel_path("../evil").is_err());
        assert!(safe_rel_path("ok/../../evil").is_err());
    }

    #[test]
    fn safe_rel_path_rejects_empty() {
        assert!(safe_rel_path("").is_err());
        assert!(safe_rel_path("a//b").is_err());
    }

    #[test]
    fn thread_knowledge_path_mirrors_nested_structure() {
        let root = Path::new("/tmp/project");
        assert_eq!(
            thread_knowledge_path(root, "group/thread-1").unwrap(),
            PathBuf::from("/tmp/project/.ai/knowledge/agent/threads/group/thread-1.md")
        );
    }

    #[test]
    fn thread_knowledge_path_simple_id() {
        let root = Path::new("/tmp/project");
        assert_eq!(
            thread_knowledge_path(root, "thread-1").unwrap(),
            PathBuf::from("/tmp/project/.ai/knowledge/agent/threads/thread-1.md")
        );
    }

    #[test]
    fn thread_state_dir_constructs_correctly() {
        let root = Path::new("/tmp/project");
        assert_eq!(
            thread_state_dir(root, "t-1").unwrap(),
            PathBuf::from("/tmp/project/.ai/state/threads/t-1")
        );
    }

    #[test]
    fn thread_transcript_path_constructs_correctly() {
        let root = Path::new("/tmp/project");
        assert_eq!(
            thread_transcript_path(root, "t-1").unwrap(),
            PathBuf::from("/tmp/project/.ai/state/threads/t-1/transcript.jsonl")
        );
    }
}
