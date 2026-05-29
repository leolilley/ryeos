//! Web UI static asset provider.
//!
//! Embeds files from `crates/clients/web/pkg/` at compile time via
//! `include_bytes!`. Implements the `StaticAssetProvider` trait defined
//! in `ryeos-api` so that generic static mode can resolve web assets
//! without the API crate knowing about web-specific paths.

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
static GRAPH_VIEW_JS: &[u8] = include_bytes!("../../../clients/web/pkg/graph-view.js");
static GRAPH_VIEW_CSS: &[u8] = include_bytes!("../../../clients/web/pkg/graph-view.css");
static COCKPIT_JS: &[u8] = include_bytes!("../../../clients/web/pkg/cockpit.js");
static COCKPIT_CSS: &[u8] = include_bytes!("../../../clients/web/pkg/cockpit.css");
static FORCE_GRAPH_3D_JS: &[u8] =
    include_bytes!("../../../clients/web/pkg/vendor/3d-force-graph.min.js");
static FORCE_GRAPH_3D_META: &[u8] =
    include_bytes!("../../../clients/web/pkg/vendor/3d-force-graph.min.js.meta");

/// Web UI static asset provider — owns the embedded web client assets.
pub struct WebAssetProvider;

impl StaticAssetProvider for WebAssetProvider {
    fn get(&self, path: &str) -> Option<StaticAsset> {
        let trimmed = path.trim_start_matches('/');
        let (bytes, cache_control) = match trimmed {
            "index.html" | "ui/index.html" => (INDEX_HTML, "no-cache"),
            "bootstrap.js" | "ui/assets/bootstrap.js" => (BOOTSTRAP_JS, "no-cache"),
            "graph-view.js" | "ui/assets/graph-view.js" => (GRAPH_VIEW_JS, "no-cache"),
            "graph-view.css" | "ui/assets/graph-view.css" => (GRAPH_VIEW_CSS, "no-cache"),
            "cockpit.js" | "ui/assets/cockpit.js" => (COCKPIT_JS, "no-cache"),
            "cockpit.css" | "ui/assets/cockpit.css" => (COCKPIT_CSS, "no-cache"),
            "3d-force-graph.v1.73.0.min.js" | "ui/assets/3d-force-graph.v1.73.0.min.js" => {
                (FORCE_GRAPH_3D_JS, "public, max-age=31536000, immutable")
            }
            "vendor/3d-force-graph.v1.73.0.min.js"
            | "ui/assets/vendor/3d-force-graph.v1.73.0.min.js" => {
                (FORCE_GRAPH_3D_JS, "public, max-age=31536000, immutable")
            }
            "3d-force-graph.min.js" | "ui/assets/3d-force-graph.min.js" => {
                (FORCE_GRAPH_3D_JS, "no-cache")
            }
            "vendor/3d-force-graph.min.js" | "ui/assets/vendor/3d-force-graph.min.js" => {
                (FORCE_GRAPH_3D_JS, "no-cache")
            }
            "3d-force-graph.min.js.meta" | "ui/assets/3d-force-graph.min.js.meta" => {
                (FORCE_GRAPH_3D_META, "no-cache")
            }
            "vendor/3d-force-graph.min.js.meta" | "ui/assets/vendor/3d-force-graph.min.js.meta" => {
                (FORCE_GRAPH_3D_META, "no-cache")
            }
            _ => return None,
        };
        Some(StaticAsset {
            bytes,
            content_type: content_type_for_path(trimmed),
            etag: compute_etag(bytes),
            cache_control,
        })
    }
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
    fn get_graph_assets() {
        let provider = WebAssetProvider;
        let js = provider
            .get("graph-view.js")
            .expect("graph-view.js must be embedded");
        assert!(js.bytes.len() > 0);
        assert!(js.content_type.contains("javascript"));

        let css = provider
            .get("graph-view.css")
            .expect("graph-view.css must be embedded");
        assert!(css.bytes.len() > 0);
        assert!(css.content_type.contains("css"));
    }

    #[test]
    fn get_vendor_graph_renderer_asset() {
        let provider = WebAssetProvider;
        let asset = provider
            .get("ui/assets/3d-force-graph.v1.73.0.min.js")
            .expect("3d-force-graph vendor asset must be embedded");
        assert!(asset.bytes.len() > 0);
        assert!(asset.content_type.contains("javascript"));
        assert!(asset.cache_control.contains("immutable"));

        let unversioned = provider
            .get("ui/assets/3d-force-graph.min.js")
            .expect("unversioned 3d-force-graph alias must be embedded");
        assert_eq!(unversioned.cache_control, "no-cache");

        let meta = provider
            .get("ui/assets/3d-force-graph.min.js.meta")
            .expect("3d-force-graph vendor metadata must be embedded");
        assert!(std::str::from_utf8(meta.bytes)
            .unwrap()
            .contains("License: MIT"));
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
