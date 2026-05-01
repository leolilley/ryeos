use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

// NOTE: deny_unknown_fields blocked by #[serde(flatten)]/#[serde(untagged)]. Tracked in 04-FUTURE-WORK.md.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookDefinition {
    pub id: String,
    pub event: String,
    #[serde(default)]
    pub layer: Option<u8>,
    #[serde(default)]
    pub condition: Option<Value>,
    pub action: Value,
    #[serde(default, flatten)]
    pub extra: Map<String, Value>,
}

// NOTE: deny_unknown_fields blocked by #[serde(flatten)]/#[serde(untagged)]. Tracked in 04-FUTURE-WORK.md.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HookConditionsConfig {
    #[serde(default)]
    pub builtin_hooks: Vec<HookDefinition>,
    #[serde(default)]
    pub infra_hooks: Vec<HookDefinition>,
    #[serde(default)]
    pub context_hooks: Vec<HookDefinition>,
    #[serde(default, flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HooksFile {
    #[serde(default)]
    pub hooks: Vec<HookDefinition>,
}

pub struct HooksLoader {
    system_hook_conditions_path: PathBuf,
    user_space: Option<PathBuf>,
}

impl HooksLoader {
    pub fn new(system_hook_conditions_path: PathBuf, user_space: Option<PathBuf>) -> Self {
        Self {
            system_hook_conditions_path,
            user_space,
        }
    }

    pub fn load(&self) -> anyhow::Result<HookConditionsConfig> {
        let content = std::fs::read_to_string(&self.system_hook_conditions_path)
            .map_err(|e| anyhow::anyhow!(
                "failed to read system hook conditions {}: {}",
                self.system_hook_conditions_path.display(),
                e
            ))?;
        let cleaned = lillux::signature::strip_signature_lines(&content);
        let config: HookConditionsConfig = serde_yaml::from_str(&cleaned)?;
        Ok(config)
    }

    pub fn get_builtin_hooks(&self) -> anyhow::Result<Vec<HookDefinition>> {
        Ok(self.load()?.builtin_hooks)
    }

    pub fn get_context_hooks(&self) -> anyhow::Result<Vec<HookDefinition>> {
        Ok(self.load()?.context_hooks)
    }

    pub fn get_infra_hooks(&self) -> anyhow::Result<Vec<HookDefinition>> {
        Ok(self.load()?.infra_hooks)
    }

    pub fn get_user_hooks(&self) -> anyhow::Result<Vec<HookDefinition>> {
        let user_space = match &self.user_space {
            Some(p) => p,
            None => return Ok(vec![]),
        };
        let path = crate::paths::user_hooks_path(user_space);
        load_hooks_file(&path)
    }

    pub fn get_project_hooks(&self, project_root: &Path) -> anyhow::Result<Vec<HookDefinition>> {
        let path = crate::paths::project_hooks_path(project_root);
        load_hooks_file(&path)
    }
}

fn load_hooks_file(path: &Path) -> anyhow::Result<Vec<HookDefinition>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let content = std::fs::read_to_string(path)?;
    let cleaned = lillux::signature::strip_signature_lines(&content);
    let file: HooksFile = serde_yaml::from_str(&cleaned)?;
    Ok(file.hooks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_strips_signature_and_parses() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hook_conditions.yaml");
        std::fs::write(
            &path,
            "# rye:signed:test\nbuiltin_hooks:\n  - id: retry\n    event: error\n    action: {primary: execute}\n"
        ).unwrap();
        let loader = HooksLoader::new(path, None);
        let config = loader.load().unwrap();
        assert_eq!(config.builtin_hooks.len(), 1);
        assert_eq!(config.builtin_hooks[0].id, "retry");
    }

    #[test]
    fn get_user_hooks_returns_empty_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hook_conditions.yaml");
        std::fs::write(&path, "builtin_hooks: []\n").unwrap();
        let loader = HooksLoader::new(path, Some(tmp.path().join("no-such-user")));
        assert!(loader.get_user_hooks().unwrap().is_empty());
    }

    #[test]
    fn get_project_hooks_loads_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        let sys_path = tmp.path().join("hook_conditions.yaml");
        std::fs::write(&sys_path, "builtin_hooks: []\n").unwrap();
        let project = tmp.path().join("project");
        let hooks_dir = project.join(".ai/config/agent");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::write(
            hooks_dir.join("hooks.yaml"),
            "hooks:\n  - id: inject\n    event: thread_started\n    action: {item_id: \"tool:rye/core/fetch\"}\n",
        )
        .unwrap();
        let loader = HooksLoader::new(sys_path, None);
        let hooks = loader.get_project_hooks(&project).unwrap();
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].id, "inject");
    }

    #[test]
    fn load_missing_file_returns_error() {
        let loader = HooksLoader::new(PathBuf::from("/nonexistent/file.yaml"), None);
        let err = loader.load().unwrap_err();
        assert!(err.to_string().contains("failed to read system hook conditions"));
    }

    #[test]
    fn hook_definition_deserializes() {
        let yaml = "id: test\nevent: start\naction:\n  primary: execute\n";
        let hook: HookDefinition = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(hook.id, "test");
        assert_eq!(hook.event, "start");
    }
}
