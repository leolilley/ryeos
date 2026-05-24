//! Embedded web UI assets.
//!
//! Static files from `crates/clients/web/pkg/` are embedded at compile time
//! via `include_bytes!`. The `source: embedded_asset` static mode resolves
//! asset paths at dispatch time through this module.

use sha2::{Digest, Sha256};

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
    format!("\"{:x}", hash)
}

/// An embedded asset with its bytes, content type, and ETag.
pub struct EmbeddedAsset {
    pub bytes: &'static [u8],
    pub content_type: &'static str,
    pub etag: String,
    /// Whether this asset has a content-hashed name (eligible for immutable caching).
    pub is_hashed: bool,
}

// ── Compile-time embedded bytes ─────────────────────────────────────────

static INDEX_HTML: &[u8] = include_bytes!("../../../../clients/web/pkg/index.html");
static BOOTSTRAP_JS: &[u8] = include_bytes!("../../../../clients/web/pkg/bootstrap.js");

/// Look up an embedded asset by path (without leading slash).
///
/// Returns `None` if no asset matches the given path.
pub fn get_asset(path: &str) -> Option<EmbeddedAsset> {
    let trimmed = path.trim_start_matches('/');
    let (bytes, is_hashed) = match trimmed {
        "index.html" | "ui/index.html" => (INDEX_HTML, false),
        "bootstrap.js" | "ui/assets/bootstrap.js" => (BOOTSTRAP_JS, false),
        _ => return None,
    };
    Some(EmbeddedAsset {
        bytes,
        content_type: content_type_for_path(trimmed),
        etag: compute_etag(bytes),
        is_hashed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_index_html() {
        let asset = get_asset("index.html").expect("index.html must be embedded");
        assert!(asset.bytes.len() > 0);
        assert!(asset.content_type.contains("text/html"));
        assert!(asset.etag.starts_with('"'));
    }

    #[test]
    fn get_bootstrap_js() {
        let asset = get_asset("bootstrap.js").expect("bootstrap.js must be embedded");
        assert!(asset.bytes.len() > 0);
        assert!(asset.content_type.contains("javascript"));
    }

    #[test]
    fn get_asset_leading_slash_stripped() {
        assert!(get_asset("/index.html").is_some());
    }

    #[test]
    fn get_unknown_asset_returns_none() {
        assert!(get_asset("nonexistent.css").is_none());
    }

    #[test]
    fn etag_is_deterministic() {
        let a1 = get_asset("index.html").unwrap();
        let a2 = get_asset("index.html").unwrap();
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
