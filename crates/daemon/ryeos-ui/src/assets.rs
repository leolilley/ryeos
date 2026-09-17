//! Web UI static asset provider.
//!
//! Embeds files from `crates/clients/web/pkg/` at compile time via
//! `include_bytes!`. Implements the `StaticAssetProvider` trait defined
//! in `ryeos-api` so that generic static mode can resolve web assets
//! without the API crate knowing about web-specific paths.

use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use ryeos_api::routes::response_modes::static_mode::{StaticAsset, StaticAssetProvider};

/// Compute a SHA-256 ETag for the given bytes.
fn compute_etag(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let hash = hasher.finalize();
    // Quote the complete content digest so development assets have the same
    // ETag shape as manifest-pinned embedded assets.
    format!("\"{:x}\"", hash)
}

/// One member of the closed browser asset generation.
///
/// This type is deliberately private to the provider. The source of truth is
/// `web-assets.json`; `generate_asset_registry.py` validates it and emits the
/// literal `include_bytes!` table below.
struct WebAssetEntry {
    route: &'static str,
    filename: &'static str,
    content_type: &'static str,
    cache_control: &'static str,
    sha256: &'static str,
    imports: &'static [&'static str],
    bytes: &'static [u8],
}

include!("generated_web_assets.rs");

/// Web UI static asset provider — owns the embedded web client assets.
pub struct WebAssetProvider;

impl StaticAssetProvider for WebAssetProvider {
    fn get(&self, path: &str) -> Option<StaticAsset> {
        debug_assert_eq!(ASSET_MANIFEST_SCHEMA, "ryeos.ui.asset-manifest.v1");
        debug_assert_eq!(
            ASSET_UI_BINDING_REVISION,
            ryeos_client_base::UI_BINDING_CONTRACT_REVISION
        );
        let entry = asset_entry(path)?;
        debug_assert!(entry.imports.iter().all(|imported| {
            WEB_ASSETS
                .iter()
                .any(|candidate| candidate.route == *imported)
        }));
        if let Some(root) = std::env::var_os("RYEOS_UI_ASSET_DIR").map(PathBuf::from) {
            // Once an override is selected, absence fails visibly. Never mix a
            // staged source generation with embedded files from another one.
            if !dev_generation_is_complete(&root) {
                return None;
            }
            return dev_asset(&root, entry);
        }
        Some(StaticAsset {
            bytes: entry.bytes.to_vec(),
            content_type: entry.content_type,
            etag: format!("\"{}\"", entry.sha256),
            cache_control: entry.cache_control,
        })
    }
}

/// Optional local development override for browser UI assets.
///
/// Set `RYEOS_UI_ASSET_DIR=/path/to/crates/clients/web/pkg` before starting
/// `ryeosd`, then `/ui` and `/ui/assets/*` are served from that directory
/// instead of the compile-time embedded bytes. Only filenames admitted by the
/// compiled manifest can be served. Selecting an override is generation-wide:
/// a missing staged member is an error, never permission to fall back to the
/// embedded member from a different generation.
fn dev_asset(root: &Path, entry: &WebAssetEntry) -> Option<StaticAsset> {
    let relative = PathBuf::from(entry.filename);
    let path = safe_join(root, &relative)?;
    let bytes = std::fs::read(&path).ok()?;
    let etag = compute_etag(&bytes);
    Some(StaticAsset {
        bytes,
        content_type: entry.content_type,
        etag,
        cache_control: "no-store",
    })
}

fn dev_generation_is_complete(root: &Path) -> bool {
    WEB_ASSETS.iter().all(|entry| {
        safe_join(root, Path::new(entry.filename))
            .and_then(|path| std::fs::symlink_metadata(path).ok())
            .is_some_and(|metadata| metadata.file_type().is_file())
    })
}

fn asset_entry(path: &str) -> Option<&'static WebAssetEntry> {
    let trimmed = path.trim_start_matches('/');
    let route = match trimmed {
        "ui" | "ui/" | "index.html" | "ui/index.html" => "/ui".to_string(),
        value if value.starts_with("ui/assets/") => format!("/{value}"),
        value if !value.contains('/') => format!("/ui/assets/{value}"),
        _ => return None,
    };
    WEB_ASSETS.iter().find(|entry| entry.route == route)
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
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct SourceManifest {
        schema_version: String,
        ui_binding_revision: String,
        assets: Vec<SourceAsset>,
    }

    #[derive(Deserialize)]
    struct SourceAsset {
        route: String,
        filename: String,
        mime: String,
        cache: String,
        sha256: String,
        imports: Vec<String>,
    }

    fn source_manifest() -> SourceManifest {
        serde_json::from_str(include_str!("../web-assets.json"))
            .expect("browser asset manifest must parse")
    }

    fn embedded(filename: &str) -> &'static [u8] {
        WEB_ASSETS
            .iter()
            .find(|entry| entry.filename == filename)
            .unwrap_or_else(|| panic!("missing embedded asset {filename}"))
            .bytes
    }

    #[test]
    fn get_index_html() {
        let provider = WebAssetProvider;
        let asset = provider
            .get("index.html")
            .expect("index.html must be embedded");
        assert!(!asset.bytes.is_empty());
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
        assert!(!asset.bytes.is_empty());
        assert!(asset.content_type.contains("javascript"));
    }

    #[test]
    fn get_web_shell_assets() {
        let provider = WebAssetProvider;
        let css = provider
            .get("web-shell.css")
            .expect("web-shell.css must be embedded");
        assert!(!css.bytes.is_empty());
        assert!(css.content_type.contains("css"));

        let js = provider
            .get("ui/assets/ryeos_web.js")
            .expect("ryeos_web.js must be embedded");
        assert!(!js.bytes.is_empty());
        assert!(js.content_type.contains("javascript"));

        let ryeos_ui = provider
            .get("ui/assets/ryeos_shell.js")
            .expect("ryeos_shell.js must be embedded");
        assert!(!ryeos_ui.bytes.is_empty());
        assert!(ryeos_ui.content_type.contains("javascript"));

        let ambient = provider
            .get("ui/assets/ryeos_ambient_scene.js")
            .expect("ryeos_ambient_scene.js must be embedded");
        assert!(!ambient.bytes.is_empty());
        assert!(ambient.content_type.contains("javascript"));

        let wasm = provider
            .get("ui/assets/ryeos_web_bg.wasm")
            .expect("ryeos_web_bg.wasm must be embedded");
        assert!(!wasm.bytes.is_empty());
        assert!(wasm.content_type.contains("wasm"));
    }

    #[test]
    fn packaged_asset_inventory_equals_the_closed_registry() {
        let package = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../clients/web/pkg");
        let mut packaged = std::fs::read_dir(package)
            .expect("read authored web package")
            .map(|entry| entry.expect("read authored web package entry").path())
            .filter(|path| {
                matches!(
                    path.extension().and_then(|value| value.to_str()),
                    Some("html" | "js" | "css" | "wasm" | "png" | "svg" | "ico")
                )
            })
            .map(|path| {
                path.file_name()
                    .and_then(|value| value.to_str())
                    .expect("browser asset filename must be UTF-8")
                    .to_string()
            })
            .collect::<Vec<_>>();
        packaged.sort();
        let mut admitted = WEB_ASSETS
            .iter()
            .map(|entry| entry.filename.to_string())
            .collect::<Vec<_>>();
        admitted.sort();
        assert_eq!(packaged, admitted);
    }

    #[test]
    fn source_manifest_and_generated_registry_are_identical() {
        let source = source_manifest();
        assert_eq!(source.schema_version, ASSET_MANIFEST_SCHEMA);
        assert_eq!(source.ui_binding_revision, ASSET_UI_BINDING_REVISION);
        assert_eq!(source.assets.len(), WEB_ASSETS.len());
        for (source, generated) in source.assets.iter().zip(WEB_ASSETS) {
            assert_eq!(source.route, generated.route);
            assert_eq!(source.filename, generated.filename);
            assert_eq!(source.mime, generated.content_type);
            assert_eq!(source.cache, generated.cache_control);
            assert_eq!(source.sha256, generated.sha256);
            assert_eq!(
                source
                    .imports
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                generated.imports
            );
        }
    }

    #[test]
    fn embedded_bytes_match_manifest_and_import_closure_is_closed() {
        assert_eq!(
            ASSET_UI_BINDING_REVISION,
            ryeos_client_base::UI_BINDING_CONTRACT_REVISION
        );
        for entry in WEB_ASSETS {
            assert_eq!(compute_etag(entry.bytes), format!("\"{}\"", entry.sha256));
            for imported in entry.imports {
                assert!(
                    WEB_ASSETS
                        .iter()
                        .any(|candidate| candidate.route == *imported),
                    "{} imports unregistered route {imported}",
                    entry.route
                );
            }
        }
    }

    #[test]
    fn development_override_is_closed_and_never_falls_back() {
        let directory = tempfile::tempdir().expect("create staged asset directory");
        let index = asset_entry("/ui").expect("index is admitted");
        assert!(!dev_generation_is_complete(directory.path()));
        assert!(dev_asset(directory.path(), index).is_none());
        assert!(asset_entry("/ui/assets/not-admitted.js").is_none());

        for entry in WEB_ASSETS {
            std::fs::write(directory.path().join(entry.filename), entry.bytes)
                .expect("write complete staged asset generation");
        }
        assert!(dev_generation_is_complete(directory.path()));
        std::fs::write(directory.path().join(index.filename), b"development index")
            .expect("write staged index");
        let staged = dev_asset(directory.path(), index).expect("read admitted staged asset");
        assert_eq!(staged.bytes, b"development index");
        assert_eq!(staged.cache_control, "no-store");
    }

    #[test]
    fn generated_wasm_glue_exports_every_shell_import() {
        // wasm.rs and the checked-in wasm-bindgen outputs are one compiled
        // browser contract. If Rust gains an export without regenerating the
        // JS/WASM pair, static ES-module linking fails before bootstrap can
        // enter the application. Derive the required names from the shell's
        // authored import instead of maintaining a second export list here.
        let shell =
            std::str::from_utf8(embedded("ryeos_shell.js")).expect("shell JS must be UTF-8");
        let generated = std::str::from_utf8(embedded("ryeos_web.js"))
            .expect("generated wasm glue must be UTF-8");
        let named_imports = shell
            .strip_prefix("import init, {")
            .and_then(|rest| rest.split_once("} from \"/ui/assets/ryeos_web.js\";"))
            .map(|(imports, _)| imports)
            .expect("shell must begin with the generated WASM import");

        for imported in named_imports
            .lines()
            .map(|line| line.trim().trim_end_matches(','))
            .filter(|name| !name.is_empty())
        {
            assert!(
                generated.contains(&format!("export function {imported}(")),
                "generated wasm glue does not export shell import `{imported}`; regenerate ryeos_web.js and ryeos_web_bg.wasm"
            );
        }
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
}
