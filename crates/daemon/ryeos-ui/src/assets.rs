//! Web UI static asset provider.
//!
//! Embeds files from `crates/clients/web/pkg/` at compile time via
//! `include_bytes!`. Implements the `StaticAssetProvider` trait defined
//! in `ryeos-api` so that generic static mode can resolve web assets
//! without the API crate knowing about web-specific paths.

use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use ryeos_api::routes::response_modes::static_mode::{StaticAsset, StaticAssetProvider};

/// Content type inferred from file extension.
fn content_type_for_path(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".wasm") {
        "application/wasm"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else if path.ends_with(".json") {
        "application/json"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "application/octet-stream"
    }
}

/// Compute a SHA-256 ETag for the given bytes.
fn compute_etag(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let hash = hasher.finalize();
    // Use first 32 hex chars (128 bits) — sufficient for ETag uniqueness.
    format!("\"{:x}\"", hash)
}

// ── Compile-time embedded bytes ─────────────────────────────────────────

static INDEX_HTML: &[u8] = include_bytes!("../../../clients/web/pkg/index.html");
static BOOTSTRAP_JS: &[u8] = include_bytes!("../../../clients/web/pkg/bootstrap.js");
static RYEOS_UI_COMPONENTS_CHROME_JS: &[u8] =
    include_bytes!("../../../clients/web/pkg/ryeos_components_chrome.js");
static RYEOS_UI_COMPONENTS_HOME_JS: &[u8] =
    include_bytes!("../../../clients/web/pkg/ryeos_components_home.js");
static RYEOS_UI_COMPONENTS_PRIMITIVES_JS: &[u8] =
    include_bytes!("../../../clients/web/pkg/ryeos_components_primitives.js");
static RYEOS_UI_COMPONENTS_WORKSPACE_JS: &[u8] =
    include_bytes!("../../../clients/web/pkg/ryeos_components_workspace.js");
static RYEOS_UI_DOM_ADAPTER_JS: &[u8] =
    include_bytes!("../../../clients/web/pkg/ryeos_dom_adapter.js");
static RYEOS_UI_AMBIENT_SCENE_JS: &[u8] =
    include_bytes!("../../../clients/web/pkg/ryeos_ambient_scene.js");
static RYEOS_UI_EFFECTS_JS: &[u8] = include_bytes!("../../../clients/web/pkg/ryeos_effects.js");
static RYEOS_UI_MOTION_JS: &[u8] = include_bytes!("../../../clients/web/pkg/ryeos_motion.js");
static RYEOS_UI_PRESENTATION_STATE_JS: &[u8] =
    include_bytes!("../../../clients/web/pkg/ryeos_presentation_state.js");
static RYEOS_UI_SHELL_JS: &[u8] = include_bytes!("../../../clients/web/pkg/ryeos_shell.js");
static WEB_SHELL_CSS: &[u8] = include_bytes!("../../../clients/web/pkg/web-shell.css");
static RYEOS_WEB_JS: &[u8] = include_bytes!("../../../clients/web/pkg/ryeos_web.js");
static RYEOS_WEB_WASM: &[u8] = include_bytes!("../../../clients/web/pkg/ryeos_web_bg.wasm");

/// Web UI static asset provider — owns the embedded web client assets.
pub struct WebAssetProvider;

impl StaticAssetProvider for WebAssetProvider {
    fn get(&self, path: &str) -> Option<StaticAsset> {
        let trimmed = path.trim_start_matches('/');
        if let Some(asset) = dev_asset(trimmed) {
            return Some(asset);
        }
        let (bytes, cache_control) = match trimmed {
            "index.html" | "ui/index.html" => (INDEX_HTML, "no-cache"),
            "bootstrap.js" | "ui/assets/bootstrap.js" => (BOOTSTRAP_JS, "no-cache"),
            "ryeos_components_chrome.js" | "ui/assets/ryeos_components_chrome.js" => {
                (RYEOS_UI_COMPONENTS_CHROME_JS, "no-cache")
            }
            "ryeos_components_home.js" | "ui/assets/ryeos_components_home.js" => {
                (RYEOS_UI_COMPONENTS_HOME_JS, "no-cache")
            }
            "ryeos_components_primitives.js" | "ui/assets/ryeos_components_primitives.js" => {
                (RYEOS_UI_COMPONENTS_PRIMITIVES_JS, "no-cache")
            }
            "ryeos_components_workspace.js" | "ui/assets/ryeos_components_workspace.js" => {
                (RYEOS_UI_COMPONENTS_WORKSPACE_JS, "no-cache")
            }
            "ryeos_dom_adapter.js" | "ui/assets/ryeos_dom_adapter.js" => {
                (RYEOS_UI_DOM_ADAPTER_JS, "no-cache")
            }
            "ryeos_ambient_scene.js" | "ui/assets/ryeos_ambient_scene.js" => {
                (RYEOS_UI_AMBIENT_SCENE_JS, "no-cache")
            }
            "ryeos_effects.js" | "ui/assets/ryeos_effects.js" => (RYEOS_UI_EFFECTS_JS, "no-cache"),
            "ryeos_motion.js" | "ui/assets/ryeos_motion.js" => (RYEOS_UI_MOTION_JS, "no-cache"),
            "ryeos_presentation_state.js" | "ui/assets/ryeos_presentation_state.js" => {
                (RYEOS_UI_PRESENTATION_STATE_JS, "no-cache")
            }
            "ryeos_shell.js" | "ui/assets/ryeos_shell.js" => (RYEOS_UI_SHELL_JS, "no-cache"),
            "web-shell.css" | "ui/assets/web-shell.css" => (WEB_SHELL_CSS, "no-cache"),
            "ryeos_web.js" | "ui/assets/ryeos_web.js" => (RYEOS_WEB_JS, "no-cache"),
            "ryeos_web_bg.wasm" | "ui/assets/ryeos_web_bg.wasm" => (RYEOS_WEB_WASM, "no-cache"),
            _ => return None,
        };
        Some(StaticAsset {
            bytes: bytes.to_vec(),
            content_type: content_type_for_path(trimmed),
            etag: compute_etag(bytes),
            cache_control,
        })
    }
}

/// Optional local development override for browser UI assets.
///
/// Set `RYEOS_UI_ASSET_DIR=/path/to/crates/clients/web/pkg` before starting
/// `ryeosd`, then `/ui` and `/ui/assets/*` are served from that directory
/// instead of the compile-time embedded bytes. This is intentionally an env-gated
/// development escape hatch so UI JS/CSS can be refreshed without repopulating
/// bundles or recompiling the daemon for every edit.
fn dev_asset(trimmed: &str) -> Option<StaticAsset> {
    let root = std::env::var_os("RYEOS_UI_ASSET_DIR").map(PathBuf::from)?;
    let relative = asset_relative_path(trimmed)?;
    let path = safe_join(&root, &relative)?;
    let bytes = std::fs::read(&path).ok()?;
    let etag = compute_etag(&bytes);
    Some(StaticAsset {
        bytes,
        content_type: content_type_for_path(relative.to_str().unwrap_or(trimmed)),
        etag,
        cache_control: "no-store",
    })
}

fn asset_relative_path(trimmed: &str) -> Option<PathBuf> {
    let relative = trimmed.strip_prefix("ui/assets/").unwrap_or(trimmed);
    if relative == "ui/index.html" {
        return Some(PathBuf::from("index.html"));
    }
    Some(PathBuf::from(relative))
}

fn safe_join(root: &Path, relative: &Path) -> Option<PathBuf> {
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return None;
    }
    Some(root.join(relative))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_index_html() {
        let provider = WebAssetProvider;
        let asset = provider
            .get("index.html")
            .expect("index.html must be embedded");
        assert!(asset.bytes.len() > 0);
        assert!(asset.content_type.contains("text/html"));
        assert!(asset.etag.starts_with('"'));
        assert!(asset.etag.ends_with('"'));
    }

    #[test]
    fn get_bootstrap_js() {
        let provider = WebAssetProvider;
        let asset = provider
            .get("bootstrap.js")
            .expect("bootstrap.js must be embedded");
        assert!(asset.bytes.len() > 0);
        assert!(asset.content_type.contains("javascript"));
    }

    #[test]
    fn get_web_shell_assets() {
        let provider = WebAssetProvider;
        let css = provider
            .get("web-shell.css")
            .expect("web-shell.css must be embedded");
        assert!(css.bytes.len() > 0);
        assert!(css.content_type.contains("css"));

        let js = provider
            .get("ui/assets/ryeos_web.js")
            .expect("ryeos_web.js must be embedded");
        assert!(js.bytes.len() > 0);
        assert!(js.content_type.contains("javascript"));

        let ryeos_ui = provider
            .get("ui/assets/ryeos_shell.js")
            .expect("ryeos_shell.js must be embedded");
        assert!(ryeos_ui.bytes.len() > 0);
        assert!(ryeos_ui.content_type.contains("javascript"));

        let ambient = provider
            .get("ui/assets/ryeos_ambient_scene.js")
            .expect("ryeos_ambient_scene.js must be embedded");
        assert!(ambient.bytes.len() > 0);
        assert!(ambient.content_type.contains("javascript"));

        let wasm = provider
            .get("ui/assets/ryeos_web_bg.wasm")
            .expect("ryeos_web_bg.wasm must be embedded");
        assert!(wasm.bytes.len() > 0);
        assert!(wasm.content_type.contains("wasm"));
    }

    #[test]
    fn get_asset_leading_slash_stripped() {
        let provider = WebAssetProvider;
        assert!(provider.get("/index.html").is_some());
    }

    #[test]
    fn get_unknown_asset_returns_none() {
        let provider = WebAssetProvider;
        assert!(provider.get("nonexistent.css").is_none());
    }

    #[test]
    fn etag_is_deterministic() {
        let provider = WebAssetProvider;
        let a1 = provider.get("index.html").unwrap();
        let a2 = provider.get("index.html").unwrap();
        assert_eq!(a1.etag, a2.etag);
    }

    #[test]
    fn content_type_for_extensions() {
        assert!(content_type_for_path("foo.wasm").contains("wasm"));
        assert!(content_type_for_path("foo.css").contains("css"));
        assert!(content_type_for_path("foo.ico").contains("icon"));
        assert_eq!(content_type_for_path("foo.bin"), "application/octet-stream");
    }
}
