use std::collections::HashSet;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

pub struct VerifiedLoader {
    project_root: PathBuf,
    user_root: Option<PathBuf>,
    system_roots: Vec<PathBuf>,
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct ResolvedPath {
    pub path: PathBuf,
    pub root: PathBuf,
    pub space: String,
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct VerifiedContent {
    pub content: String,
    pub hash: String,
    pub path: PathBuf,
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct ScannedItem {
    pub name: String,
    pub path: PathBuf,
    pub root: PathBuf,
}

impl VerifiedLoader {
    pub fn new(
        project_root: PathBuf,
        user_root: Option<PathBuf>,
        system_roots: Vec<PathBuf>,
    ) -> Self {
        Self {
            project_root,
            user_root,
            system_roots,
        }
    }

    fn kind_subdir(kind: &str) -> &'static str {
        match kind {
            "directive" => ".ai/directives/",
            "tool" => ".ai/tools/",
            "knowledge" => ".ai/knowledge/",
            "config" => ".ai/config/rye-runtime/",
            _ => ".ai/",
        }
    }

    fn strip_kind_prefix(item_id: &str) -> (&str, &str) {
        if let Some(rest) = item_id.split_once(':') {
            (rest.0, rest.1)
        } else {
            (item_id, item_id)
        }
    }

    pub fn resolve_item(&self, kind: &str, item_id: &str) -> Result<ResolvedPath> {
        let (effective_kind, bare_id) = Self::strip_kind_prefix(item_id);
        let kind = if effective_kind != bare_id { effective_kind } else { kind };
        let subdir = Self::kind_subdir(kind);

        let item_path = PathBuf::from(format!("{subdir}{bare_id}.md"));

        if self.project_root.join(&item_path).exists() {
            return Ok(ResolvedPath {
                path: self.project_root.join(&item_path),
                root: self.project_root.clone(),
                space: "project".to_string(),
            });
        }

        if let Some(ref user_root) = self.user_root {
            if user_root.join(&item_path).exists() {
                return Ok(ResolvedPath {
                    path: user_root.join(&item_path),
                    root: user_root.clone(),
                    space: "user".to_string(),
                });
            }
        }

        for system_root in &self.system_roots {
            if system_root.join(&item_path).exists() {
                return Ok(ResolvedPath {
                    path: system_root.join(&item_path),
                    root: system_root.clone(),
                    space: "system".to_string(),
                });
            }
        }

        bail!("item not found: {kind}:{bare_id}");
    }

    pub fn load_verified(&self, _kind: &str, path: &Path) -> Result<VerifiedContent> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;

        let content = lillux::signature::strip_signature_lines(&raw);

        let hash = {
            let digest = Sha256::digest(content.as_bytes());
            let mut hex = String::with_capacity(64);
            for byte in digest.iter() {
                let _ = write!(&mut hex, "{byte:02x}");
            }
            hex
        };

        Ok(VerifiedContent {
            content,
            hash,
            path: path.to_path_buf(),
        })
    }

    pub fn load_config<T: DeserializeOwned>(&self, config_id: &str) -> Option<T> {
        let subdir = Self::kind_subdir("config");
        let item_path = PathBuf::from(format!("{subdir}{config_id}.yaml"));

        let mut candidate_paths = Vec::new();

        for system_root in &self.system_roots {
            let p = system_root.join(&item_path);
            if p.exists() {
                candidate_paths.push(p);
            }
        }

        if let Some(ref user_root) = self.user_root {
            let p = user_root.join(&item_path);
            if p.exists() {
                candidate_paths.push(p);
            }
        }

        let p = self.project_root.join(&item_path);
        if p.exists() {
            candidate_paths.push(p);
        }

        if candidate_paths.is_empty() {
            return None;
        }

        for path in &candidate_paths {
            if let Ok(verified) = self.load_verified("config", path) {
                if let Ok(value) = serde_yaml::from_str::<T>(&verified.content) {
                    return Some(value);
                }
            }
        }

        None
    }

    pub fn scan_kind(&self, kind: &str) -> Result<Vec<ScannedItem>> {
        let subdir = Self::kind_subdir(kind);
        let mut seen_names: HashSet<String> = HashSet::new();
        let mut results = Vec::new();

        let roots_to_scan: Vec<(&Path, &str)> = {
            let mut v = Vec::new();
            for sr in &self.system_roots {
                v.push((sr.as_path(), "system"));
            }
            if let Some(ref ur) = self.user_root {
                v.push((ur.as_path(), "user"));
            }
            v.push((self.project_root.as_path(), "project"));
            v
        };

        for (root, _space) in &roots_to_scan {
            let dir = root.join(subdir);
            if !dir.is_dir() {
                continue;
            }

            let entries = fs::read_dir(&dir)
                .with_context(|| format!("scanning {}", dir.display()))?;

            for entry in entries {
                let entry = entry?;
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }

                let name = match path.file_stem().and_then(|s| s.to_str()) {
                    Some(n) => n.to_string(),
                    None => continue,
                };

                if !seen_names.contains(&name) {
                    seen_names.insert(name.clone());
                    results.push(ScannedItem {
                        name,
                        path: path.clone(),
                        root: root.to_path_buf(),
                    });
                }
            }
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn create_file(dir: &Path, relative: &str, content: &str) -> PathBuf {
        let p = dir.join(relative);
        p.parent().map(|d| fs::create_dir_all(d).unwrap());
        fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn resolve_item_finds_in_project_first() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let system = tmp.path().join("system");

        create_file(
            &project,
            ".ai/directives/hello.md",
            "# Project Hello\n",
        );
        create_file(
            &system,
            ".ai/directives/hello.md",
            "# System Hello\n",
        );

        let loader = VerifiedLoader::new(project, None, vec![system]);
        let resolved = loader.resolve_item("directive", "hello").unwrap();

        assert_eq!(resolved.space, "project");
        assert!(resolved.path.to_string_lossy().contains("project"));
    }

    #[test]
    fn resolve_item_falls_back_to_user() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let user = tmp.path().join("user");
        let system = tmp.path().join("system");

        create_file(
            &user,
            ".ai/directives/shared.md",
            "# User Shared\n",
        );
        create_file(
            &system,
            ".ai/directives/shared.md",
            "# System Shared\n",
        );

        let loader = VerifiedLoader::new(project, Some(user), vec![system]);
        let resolved = loader.resolve_item("directive", "shared").unwrap();

        assert_eq!(resolved.space, "user");
    }

    #[test]
    fn resolve_item_falls_back_to_system() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let user = tmp.path().join("user");
        let system = tmp.path().join("system");

        create_file(
            &system,
            ".ai/tools/run.md",
            "# System Tool\n",
        );

        let loader = VerifiedLoader::new(project, Some(user), vec![system]);
        let resolved = loader.resolve_item("tool", "run").unwrap();

        assert_eq!(resolved.space, "system");
    }

    #[test]
    fn resolve_item_strips_kind_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");

        create_file(
            &project,
            ".ai/directives/agent.md",
            "# Agent Directive\n",
        );

        let loader = VerifiedLoader::new(project, None, vec![]);
        let resolved = loader.resolve_item("directive", "directive:agent").unwrap();

        assert_eq!(resolved.space, "project");
        assert!(resolved.path.to_string_lossy().ends_with("agent.md"));
    }

    #[test]
    fn resolve_item_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");

        let loader = VerifiedLoader::new(project, None, vec![]);
        let result = loader.resolve_item("directive", "nonexistent");

        assert!(result.is_err());
    }

    #[test]
    fn load_config_system_user_project_override() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let user = tmp.path().join("user");
        let system = tmp.path().join("system");

        create_file(
            &system,
            ".ai/config/rye-runtime/test.yaml",
            "name: system\n",
        );
        create_file(
            &user,
            ".ai/config/rye-runtime/test.yaml",
            "name: user\n",
        );
        create_file(
            &project,
            ".ai/config/rye-runtime/test.yaml",
            "name: project\n",
        );

        let loader = VerifiedLoader::new(project, Some(user), vec![system]);
        let config: serde_yaml::Value = loader.load_config("test").unwrap();

        assert_eq!(config["name"], "system");
    }

    #[test]
    fn load_config_missing_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");

        let loader = VerifiedLoader::new(project, None, vec![]);
        let config: Option<serde_yaml::Value> = loader.load_config("nonexistent");

        assert!(config.is_none());
    }

    #[test]
    fn load_config_bad_yaml_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");

        create_file(
            &project,
            ".ai/config/rye-runtime/bad.yaml",
            "not valid yaml: [",
        );

        let loader = VerifiedLoader::new(project, None, vec![]);
        let config: Option<serde_yaml::Value> = loader.load_config("bad");

        assert!(config.is_none());
    }

    #[test]
    fn load_config_system_only() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let system = tmp.path().join("system");

        create_file(
            &system,
            ".ai/config/rye-runtime/defaults.yaml",
            "key: from_system\n",
        );

        let loader = VerifiedLoader::new(project, None, vec![system]);
        let config: serde_yaml::Value = loader.load_config("defaults").unwrap();

        assert_eq!(config["key"], "from_system");
    }

    #[test]
    fn load_verified_strips_signature_and_hashes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.md");
        let content = "--- rye:signed:2024-01-01T00:00:00Z:abc123:base64sig:fingerprint ---\n# Hello\n\nBody text.\n";
        fs::write(&path, content).unwrap();

        let loader = VerifiedLoader::new(tmp.path().to_path_buf(), None, vec![]);
        let verified = loader.load_verified("directive", &path).unwrap();

        assert!(!verified.content.contains("rye:signed:"));
        assert!(verified.content.contains("# Hello"));
        assert_eq!(verified.hash.len(), 64);
    }

    #[test]
    fn load_verified_unsigned_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("plain.md");
        let content = "# Plain Directive\n\nSome content here.\n";
        fs::write(&path, content).unwrap();

        let loader = VerifiedLoader::new(tmp.path().to_path_buf(), None, vec![]);
        let verified = loader.load_verified("directive", &path).unwrap();

        assert_eq!(verified.content, content);
        assert_eq!(verified.hash.len(), 64);
    }

    #[test]
    fn load_verified_hash_is_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("det.md");
        let content = "deterministic content";
        fs::write(&path, content).unwrap();

        let loader = VerifiedLoader::new(tmp.path().to_path_buf(), None, vec![]);
        let v1 = loader.load_verified("directive", &path).unwrap();
        let v2 = loader.load_verified("directive", &path).unwrap();

        assert_eq!(v1.hash, v2.hash);
    }

    #[test]
    fn scan_kind_finds_across_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let user = tmp.path().join("user");
        let system = tmp.path().join("system");

        create_file(
            &system,
            ".ai/tools/sys_tool.md",
            "# System Tool\n",
        );
        create_file(
            &system,
            ".ai/tools/shared.md",
            "# System Shared\n",
        );
        create_file(
            &user,
            ".ai/tools/user_tool.md",
            "# User Tool\n",
        );
        create_file(
            &user,
            ".ai/tools/shared.md",
            "# User Shared\n",
        );
        create_file(
            &project,
            ".ai/tools/proj_tool.md",
            "# Project Tool\n",
        );

        let system_clone = system.clone();
        let loader = VerifiedLoader::new(project, Some(user), vec![system]);
        let items = loader.scan_kind("tool").unwrap();
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();

        assert!(names.contains(&"sys_tool"));
        assert!(names.contains(&"user_tool"));
        assert!(names.contains(&"proj_tool"));
        assert!(names.contains(&"shared"));

        let shared = items.iter().find(|i| i.name == "shared").unwrap();
        assert_eq!(shared.root, system_clone);
    }

    #[test]
    fn scan_kind_empty_when_no_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");

        let loader = VerifiedLoader::new(project, None, vec![]);
        let items = loader.scan_kind("directive").unwrap();

        assert!(items.is_empty());
    }
}
