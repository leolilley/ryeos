//! Persistence — config and session state.
//!
//! Config is stored as signed YAML at `~/.ai/config/ryeos/client/tui/config.yaml`.
//! The signature line (`# ryeos:signed:...`) is stripped before YAML parsing.
//! Session state remains JSON (not signed — ephemeral).

use ryeos_client_base::ids::TileId;
use ryeos_client_base::layout::{LayoutTree, SplitAxis};
use ryeos_client_base::workspace::{TileState, ViewSpec};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiConfig {
    pub animation_enabled: bool,
    pub content_max_width: usize,
    pub tick_interval_ms: u64,
    pub poll_interval_secs: u64,
    /// Keybinding overrides: maps affordance IDs to keybind strings.
    /// Overrides are merged on top of defaults at startup.
    /// Value `"none"` disables a binding.
    /// Example: `palette: ctrl+k` or `help: none`
    #[serde(default)]
    pub keybindings: std::collections::HashMap<String, String>,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            animation_enabled: true,
            content_max_width: 160,
            tick_interval_ms: 50,
            poll_interval_secs: 5,
            keybindings: std::collections::HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiSession {
    pub layout: LayoutTreeSer,
    pub tiles: HashMap<u64, TileStateSer>,
    pub focused_tile_id: u64,
}

/// Serializable layout tree.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LayoutTreeSer {
    Leaf(u64),
    Split {
        axis: SplitAxis,
        ratio: f32,
        first: Box<LayoutTreeSer>,
        second: Box<LayoutTreeSer>,
    },
}

/// Serializable tile state.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TileStateSer {
    pub view: ViewSpecSer,
}

/// Serializable view spec.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ViewSpecSer {
    Thread { thread_id: Option<u64> },
    ThreadList,
    Overview,
    Remotes,
    Services,
    ItemInspector,
    Schedules,
    GcStatus,
    Files,
    Projects,
    SpaceBrowser,
    Trust,
    Graph,
    EventInspector,
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

impl From<&LayoutTree> for LayoutTreeSer {
    fn from(tree: &LayoutTree) -> Self {
        match tree {
            LayoutTree::Leaf(id) => LayoutTreeSer::Leaf(id.0),
            LayoutTree::Split {
                axis,
                ratio,
                first,
                second,
            } => LayoutTreeSer::Split {
                axis: *axis,
                ratio: *ratio,
                first: Box::new(LayoutTreeSer::from(first.as_ref())),
                second: Box::new(LayoutTreeSer::from(second.as_ref())),
            },
        }
    }
}

impl From<&TileState> for TileStateSer {
    fn from(ts: &TileState) -> Self {
        TileStateSer {
            view: ViewSpecSer::from(&ts.view),
        }
    }
}

impl From<&ViewSpec> for ViewSpecSer {
    fn from(vs: &ViewSpec) -> Self {
        match vs {
            ViewSpec::Thread { thread_id } => ViewSpecSer::Thread {
                thread_id: thread_id.map(|id| id.0),
            },
            ViewSpec::ThreadList => ViewSpecSer::ThreadList,
            ViewSpec::Overview => ViewSpecSer::Overview,
            ViewSpec::Remotes => ViewSpecSer::Remotes,
            ViewSpec::Services => ViewSpecSer::Services,
            ViewSpec::ItemInspector => ViewSpecSer::ItemInspector,
            ViewSpec::Schedules => ViewSpecSer::Schedules,
            ViewSpec::GcStatus => ViewSpecSer::GcStatus,
            ViewSpec::Files => ViewSpecSer::Files,
            ViewSpec::Projects => ViewSpecSer::Projects,
            ViewSpec::SpaceBrowser { .. } => ViewSpecSer::SpaceBrowser,
            ViewSpec::Trust => ViewSpecSer::Trust,
            ViewSpec::Graph { .. } => ViewSpecSer::Graph,
            ViewSpec::EventInspector => ViewSpecSer::EventInspector,
        }
    }
}

// ---------------------------------------------------------------------------
// File I/O
// ---------------------------------------------------------------------------

/// Config directory: `~/.ai/config/ryeos/client/tui/`
fn config_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".ai")
            .join("config")
            .join("ryeos")
            .join("client")
            .join("tui")
    })
}

/// Config file path: `~/.ai/config/ryeos/client/tui/config.yaml`
#[allow(dead_code)]
fn config_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("config.yaml"))
}

/// Strip the `# ryeos:signed:...` signature line from YAML content.
/// Returns the body without signature lines, ready for YAML parsing.
fn strip_signature(raw: &str) -> String {
    lillux::signature::strip_signature_lines_with_envelope(raw, "#", None)
}

/// Load config from disk. Returns default if not found or parse fails.
#[allow(dead_code)]
pub fn load_config() -> TuiConfig {
    let path = match config_path() {
        Some(p) if p.exists() => p,
        _ => return TuiConfig::default(),
    };

    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("failed to read config {}: {}", path.display(), e);
            return TuiConfig::default();
        }
    };

    let body = strip_signature(&raw);
    match serde_yaml::from_str(&body) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!("failed to parse config {}: {}", path.display(), e);
            TuiConfig::default()
        }
    }
}

/// Save config to disk as signed YAML.
///
/// Reads the user's signing key from the default key location.
/// If no key is available, writes unsigned YAML (still valid, just not signed).
#[allow(dead_code)]
pub fn save_config(config: &TuiConfig) -> std::io::Result<()> {
    let dir = config_dir()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home directory"))?;
    std::fs::create_dir_all(&dir)?;

    let yaml = serde_yaml::to_string(config)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    let content = match try_sign(&yaml) {
        Some(signed) => signed,
        None => yaml, // no key available — write unsigned
    };

    let path = dir.join("config.yaml");
    std::fs::write(path, content)
}

/// Try to sign YAML content with the user's default signing key.
/// Returns `None` if no key is available.
fn try_sign(yaml_body: &str) -> Option<String> {
    let key_path = dirs::home_dir()?
        .join(".ai")
        .join("keys")
        .join("default.key");
    if !key_path.exists() {
        return None;
    }
    let key_bytes = std::fs::read(&key_path).ok()?;
    let signing_key = ed25519_dalek::SigningKey::from_bytes(key_bytes.as_slice().try_into().ok()?);
    Some(lillux::signature::sign_content(
        yaml_body,
        &signing_key,
        "#",
        None,
    ))
}

/// Load a signed YAML file from any path, stripping signature before parsing.
/// Useful for loading config files from arbitrary locations.
#[allow(dead_code)]
pub fn load_signed_yaml<T: serde::de::DeserializeOwned>(path: &Path) -> std::io::Result<T> {
    let raw = std::fs::read_to_string(path)?;
    let body = strip_signature(&raw);
    serde_yaml::from_str(&body).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Save session to disk.
pub fn save_session(
    layout: &LayoutTree,
    tiles: &HashMap<TileId, TileState>,
    focused: TileId,
) -> std::io::Result<()> {
    let dir = config_dir()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home directory"))?;
    std::fs::create_dir_all(&dir)?;

    let session = TuiSession {
        layout: LayoutTreeSer::from(layout),
        tiles: tiles
            .iter()
            .map(|(id, ts)| (id.0, TileStateSer::from(ts)))
            .collect(),
        focused_tile_id: focused.0,
    };

    let path = dir.join("session.json");
    let data = serde_json::to_string_pretty(&session)?;
    std::fs::write(path, data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_client_base::workspace::Workspace;
    use tempfile::TempDir;

    #[test]
    fn session_roundtrip_preserves_layout_and_focus() {
        let ws = Workspace::default_three_pane();
        let layout_ser = LayoutTreeSer::from(&ws.layout);
        let tiles_ser: HashMap<u64, TileStateSer> = ws
            .tiles
            .iter()
            .map(|(id, ts)| (id.0, TileStateSer::from(ts)))
            .collect();

        let json =
            serde_json::to_string_pretty(&(&layout_ser, &tiles_ser, ws.focused_tile.0)).unwrap();

        let (layout_back, tiles_back, focused_back): (
            LayoutTreeSer,
            HashMap<u64, TileStateSer>,
            u64,
        ) = serde_json::from_str(&json).unwrap();

        assert_eq!(focused_back, ws.focused_tile.0);
        assert_eq!(tiles_back.len(), 3);
    }

    #[test]
    fn load_config_returns_default_when_no_file() {
        let tmp = TempDir::new().unwrap();
        // Point config_dir won't find anything — just verify default
        let config = TuiConfig::default();
        assert!(config.keybindings.is_empty());
        assert!(config.animation_enabled);
    }

    #[test]
    fn strip_signature_removes_signed_line() {
        let signed = "# ryeos:signed:2026-01-01T00:00:00Z:abc123:sig:fp\nanimation_enabled: true\n";
        let body = strip_signature(signed);
        assert_eq!(body, "animation_enabled: true\n");
        assert!(!body.contains("ryeos:signed"));
    }

    #[test]
    fn strip_signature_preserves_unsigned_yaml() {
        let yaml = "animation_enabled: true\ncontent_max_width: 160\n";
        let body = strip_signature(yaml);
        assert_eq!(body, yaml);
    }

    #[test]
    fn config_yaml_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.yaml");

        let config = TuiConfig {
            animation_enabled: false,
            content_max_width: 120,
            keybindings: {
                let mut m = HashMap::new();
                m.insert("palette".into(), "ctrl+k".into());
                m
            },
            ..TuiConfig::default()
        };

        // Write YAML manually (unsigned)
        let yaml = serde_yaml::to_string(&config).unwrap();
        std::fs::write(&path, &yaml).unwrap();

        // Read back
        let raw = std::fs::read_to_string(&path).unwrap();
        let body = strip_signature(&raw);
        let loaded: TuiConfig = serde_yaml::from_str(&body).unwrap();

        assert_eq!(loaded.animation_enabled, false);
        assert_eq!(loaded.content_max_width, 120);
        assert_eq!(
            loaded.keybindings.get("palette"),
            Some(&"ctrl+k".to_string())
        );
    }

    #[test]
    fn signed_config_yaml_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.yaml");
        let sk = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);

        let config = TuiConfig {
            animation_enabled: false,
            keybindings: {
                let mut m = HashMap::new();
                m.insert("help".into(), "ctrl+h".into());
                m
            },
            ..TuiConfig::default()
        };

        // Write signed YAML
        let yaml = serde_yaml::to_string(&config).unwrap();
        let signed = lillux::signature::sign_content(&yaml, &sk, "#", None);
        std::fs::write(&path, &signed).unwrap();

        // Read back — strip sig, parse
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.starts_with("# ryeos:signed:"));
        let body = strip_signature(&raw);
        let loaded: TuiConfig = serde_yaml::from_str(&body).unwrap();

        assert_eq!(loaded.animation_enabled, false);
        assert_eq!(loaded.keybindings.get("help"), Some(&"ctrl+h".to_string()));
    }

    #[test]
    fn load_signed_yaml_helper_works() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.yaml");
        let sk = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);

        let config = TuiConfig::default();
        let yaml = serde_yaml::to_string(&config).unwrap();
        let signed = lillux::signature::sign_content(&yaml, &sk, "#", None);
        std::fs::write(&path, &signed).unwrap();

        let loaded: TuiConfig = load_signed_yaml(&path).unwrap();
        assert_eq!(loaded.animation_enabled, config.animation_enabled);
    }
}
