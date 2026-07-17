//! Unified bundle binary resolution for runtimes + handlers.
//!
//! Accepts two intentional cross-consumer ref shapes — neither is a
//! compatibility shim; they're the canonical forms used by different
//! authoring surfaces and the resolver normalizes both to the same
//! verified `<bundle>/.ai/bin/<host_triple>/<name>` path:
//!
//!   - `bin:<name>` — the canonical short form used by tool YAMLs.
//!     The triple is implicit (always the host triple), so authors
//!     don't have to mention it; it's also the form `ryeos publish`
//!     emits when describing tool binary refs.
//!
//!   - `bin/<triple>/<name>` — the path-style form used by runtime
//!     YAMLs and handler descriptors. The triple is explicit so a
//!     bundle can ship multiple architectures side-by-side and the
//!     descriptor unambiguously names which one it covers.
//!
//!   - `bin/{triple}/<name>` — the literal-placeholder variant of the
//!     path-style form. Authors who want the explicit `bin/<triple>/<name>`
//!     shape (so a descriptor visibly declares it's pointing at an
//!     architecture-namespaced binary) but don't want to hard-code one
//!     architecture write `{triple}` and the resolver substitutes the
//!     host triple before the normal explicit-path verification runs.
//!
//! All three shapes go through the same manifest-hash + trust-store
//! verification path below.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use lillux::crypto::VerifyingKey;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::contracts::{ItemSpace, SignatureEnvelope, TrustClass as ContractTrustClass};
use crate::error::EngineError;
use crate::item_resolution::ResolutionRoots;
use crate::resolution::TrustClass;
use crate::trust::TrustStore;

/// Result of resolving a binary reference.
#[derive(Debug)]
pub struct ResolvedBinary {
    pub absolute_path: PathBuf,
    /// Raw SHA-256 of the exact verified executable bytes.
    pub content_hash: String,
    pub manifest_hash: String,
    pub signer_fingerprint: String,
}

#[derive(Debug)]
pub struct CapturedExecutable {
    pub identity: ResolvedBinary,
    pub handle: std::sync::Arc<std::fs::File>,
}

/// Signed identity of one bundle's complete native-executor authorization
/// manifest. A bundle without native executables has no such identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleExecutorManifestIdentity {
    pub manifest_hash: String,
    pub signer_fingerprint: String,
}

struct VerifiedBundleExecutorSet {
    identity: Option<BundleExecutorManifestIdentity>,
    item_refs: HashSet<String>,
}

/// Resolve, verify, open without following symlinks, and re-hash an installed
/// bundle executable. The verified bytes are copied into a sealed anonymous
/// file so later mutation of the installed path cannot change what executes.
pub fn capture_bundle_executable(
    executable_name: &str,
    bundle_root: &Path,
    node_trust_store: &TrustStore,
) -> Result<CapturedExecutable, EngineError> {
    let identity = resolve_bundle_binary_ref(
        &format!("bin:{executable_name}"),
        bundle_root,
        |fingerprint| {
            node_trust_store
                .get(fingerprint)
                .map(|signer| signer.verifying_key)
        },
        TrustClass::TrustedBundle,
    )?;

    #[cfg(unix)]
    let handle = {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&identity.absolute_path)
    };
    #[cfg(not(unix))]
    let handle = std::fs::OpenOptions::new()
        .read(true)
        .open(&identity.absolute_path);
    let mut handle = handle.map_err(|error| {
        EngineError::Internal(format!(
            "capture verified executable {}: {error}",
            identity.absolute_path.display()
        ))
    })?;
    validate_captured_executable(&handle, &identity.absolute_path)?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut handle, &mut bytes).map_err(|error| {
        EngineError::Internal(format!(
            "read captured executable {}: {error}",
            identity.absolute_path.display()
        ))
    })?;
    let observed = lillux::sha256_hex(&bytes);
    if observed != identity.content_hash {
        return Err(EngineError::BinHashMismatch {
            bin: executable_name.to_string(),
            declared: identity.content_hash,
            computed: observed,
        });
    }
    let handle =
        lillux::sealed_executable_memfd(c"ryeos-bundle-executable", &bytes).map_err(|error| {
            EngineError::Internal(format!(
                "materialize immutable executable {}: {error}",
                identity.absolute_path.display()
            ))
        })?;
    Ok(CapturedExecutable { identity, handle })
}

#[cfg(target_os = "linux")]
fn validate_captured_executable(handle: &std::fs::File, path: &Path) -> Result<(), EngineError> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::PermissionsExt as _;

    let metadata = handle.metadata().map_err(|error| {
        EngineError::Internal(format!("inspect executable {}: {error}", path.display()))
    })?;
    if !metadata.file_type().is_file() {
        return Err(EngineError::Internal(format!(
            "refuse executable {}: captured object is not a regular file",
            path.display()
        )));
    }
    let mode = metadata.permissions().mode();
    if mode & 0o111 == 0 {
        return Err(EngineError::Internal(format!(
            "refuse executable {}: no execute bit is set",
            path.display()
        )));
    }
    if mode & (libc::S_ISUID | libc::S_ISGID) != 0 {
        return Err(EngineError::Internal(format!(
            "refuse executable {}: setuid and setgid files are forbidden",
            path.display()
        )));
    }

    // An exact byte digest does not cover Linux file capabilities. Refuse the
    // xattr before copying the verified bytes into the unprivileged memfd.
    let result = unsafe {
        libc::fgetxattr(
            handle.as_raw_fd(),
            c"security.capability".as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    if result >= 0 {
        return Err(EngineError::Internal(format!(
            "refuse executable {}: Linux file capabilities are forbidden",
            path.display()
        )));
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::ENODATA) {
        return Err(EngineError::Internal(format!(
            "inspect Linux file capabilities on {}: {error}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn validate_captured_executable(_handle: &std::fs::File, path: &Path) -> Result<(), EngineError> {
    Err(EngineError::Internal(format!(
        "immutable executable capture is unsupported on this platform: {}",
        path.display()
    )))
}

/// Resolve a runtime command binary ref for a concrete wrapper item.
///
/// Accepted shapes:
///   - `bin:<name>` — existing behavior, resolved in the wrapper item's bundle.
///   - `bin:<bundle>/<name>` — qualified behavior, resolved in the registered
///     signed bundle whose manifest name is `<bundle>`.
///
/// Qualified refs intentionally change only executable materialization. Runtime
/// callback authority remains attached to the wrapper item that requested the
/// subprocess, not to the bundle that ships the binary.
pub fn resolve_runtime_binary_command_ref(
    binary_ref: &str,
    wrapper_source_path: &Path,
    roots: &ResolutionRoots,
    trust_store: &TrustStore,
    root_trust_class: TrustClass,
) -> Result<ResolvedBinary, EngineError> {
    let Some(rest) = binary_ref.strip_prefix("bin:") else {
        return Err(EngineError::InvalidBinPrefix {
            raw: binary_ref.to_string(),
            detail: "runtime command binary_ref must start with `bin:`".into(),
        });
    };

    let slash_count = rest.matches('/').count();
    match slash_count {
        0 => {
            let bundle_root = registered_bundle_root_for_source(wrapper_source_path, roots)
                .or_else(|| find_bundle_root(wrapper_source_path))
                .ok_or_else(|| EngineError::InvalidBinPrefix {
                    raw: binary_ref.to_string(),
                    detail: format!(
                        "cannot find bundle root (no registered bundle or .ai/ ancestor of {})",
                        wrapper_source_path.display()
                    ),
                })?;
            resolve_bundle_binary_ref(
                binary_ref,
                &bundle_root,
                |fp| trust_store.get(fp).map(|signer| signer.verifying_key),
                root_trust_class,
            )
        }
        1 => {
            let (target_bundle, bin_name) = rest.split_once('/').expect("one slash checked above");
            validate_bundle_name(target_bundle, binary_ref)?;
            validate_bin_name(bin_name, binary_ref)?;

            let source_root = registered_bundle_root_for_source(wrapper_source_path, roots)
                .ok_or_else(|| EngineError::InvalidBinPrefix {
                    raw: binary_ref.to_string(),
                    detail: format!(
                        "qualified `bin:<bundle>/<name>` refs are only allowed from registered bundle items; {} is not under a registered bundle root",
                        wrapper_source_path.display()
                    ),
                })?;
            let source_manifest = load_minimal_bundle_manifest(&source_root, trust_store)?;
            let (target_root, target_manifest) =
                find_qualified_target_bundle(target_bundle, roots, trust_store)?;

            ensure_qualified_binary_dependency(&source_manifest, &target_manifest)?;

            resolve_bundle_binary_ref(
                &format!("bin:{bin_name}"),
                &target_root,
                |fp| trust_store.get(fp).map(|signer| signer.verifying_key),
                root_trust_class,
            )
        }
        _ => Err(EngineError::InvalidBinPrefix {
            raw: binary_ref.to_string(),
            detail: "qualified binary refs must be `bin:<bundle>/<binary>`; bundle and binary names cannot contain `/`".into(),
        }),
    }
}

/// Resolve a binary reference relative to a bundle root.
///
/// Accepted shapes:
///   - `bin:<name>` — name resolved against `<bundle>/.ai/bin/<host_triple>/`
///   - `bin/<triple>/<name>` — explicit triple path (must equal host triple)
///
/// Verification:
///   - The file must exist at the resolved path.
///   - The bundle's manifest must contain an entry for the binary
///     whose hash matches the on-disk content.
///   - The manifest's signer must be present in the trust store.
///
/// Errors are EngineError variants — InvalidBinPrefix, BinNotFound,
/// BinHashMismatch, BinUntrusted (existing variants from foundation
/// wave).
pub fn resolve_bundle_binary_ref(
    binary_ref: &str,
    bundle_root: &Path,
    trusted_verifying_key: impl Fn(&str) -> Option<VerifyingKey>,
    root_trust_class: TrustClass,
) -> Result<ResolvedBinary, EngineError> {
    let triple = env!("RYEOS_ENGINE_HOST_TRIPLE");

    // Determine the binary name and item_ref based on ref shape.
    let (bin_name, item_ref, bin_path) = if let Some(name) = binary_ref.strip_prefix("bin:") {
        // Canonical short shape: bin:<name> (triple implicit = host)
        let name = name.trim();
        validate_bin_name(name, binary_ref)?;
        let path = bundle_root
            .join(crate::AI_DIR)
            .join("bin")
            .join(triple)
            .join(name);
        let iref = format!("bin/{triple}/{name}");
        (name.to_string(), iref, path)
    } else if binary_ref.starts_with("bin/") {
        // Path-style shape: bin/<triple>/<name>
        //
        // Authors may write the literal placeholder `{triple}` in the
        // triple segment; we substitute the host triple here so the
        // descriptor stays portable across architectures while still
        // visibly carrying the architecture-namespaced shape.
        let parts: Vec<&str> = binary_ref.splitn(4, '/').collect();
        if parts.len() != 3 {
            return Err(EngineError::InvalidBinPrefix {
                raw: binary_ref.to_string(),
                detail: "path-style binary_ref must be `bin/<triple>/<name>`".into(),
            });
        }
        let raw_ref_triple = parts[1];
        let name = parts[2];

        let ref_triple = if raw_ref_triple == "{triple}" {
            triple
        } else {
            raw_ref_triple
        };

        if ref_triple != triple {
            return Err(EngineError::InvalidBinPrefix {
                raw: binary_ref.to_string(),
                detail: format!(
                    "binary_ref triple `{ref_triple}` doesn't match host triple `{triple}`"
                ),
            });
        }

        validate_bin_name(name, binary_ref)?;

        let path = bundle_root
            .join(crate::AI_DIR)
            .join("bin")
            .join(triple)
            .join(name);
        // Normalize the item_ref used for manifest lookup to the
        // resolved triple so manifests don't have to track placeholders.
        let iref = format!("bin/{triple}/{name}");
        (name.to_string(), iref, path)
    } else {
        return Err(EngineError::InvalidBinPrefix {
            raw: binary_ref.to_string(),
            detail: "binary_ref must start with `bin:` or `bin/<triple>/`".into(),
        });
    };

    // --- Common verification path ---

    let bin_dir = bin_path.parent().ok_or_else(|| EngineError::BinNotFound {
        bin: bin_name.clone(),
        searched: "cannot determine binary directory".into(),
    })?;

    if !bin_dir.is_dir() {
        return Err(EngineError::BinNotFound {
            bin: bin_name.clone(),
            searched: format!("expected triple dir {}", bin_dir.display()),
        });
    }

    if !bin_path.exists() {
        return Err(EngineError::BinNotFound {
            bin: bin_name.clone(),
            searched: bin_path.display().to_string(),
        });
    }

    // --- Confinement: resolved path must be under the canonical bin dir ---

    let canonical_bin_dir = bin_dir.canonicalize().map_err(|e| {
        EngineError::Internal(format!(
            "failed to canonicalize bin dir {}: {e}",
            bin_dir.display()
        ))
    })?;
    let canonical_resolved = bin_path.canonicalize().map_err(|e| {
        EngineError::Internal(format!(
            "failed to canonicalize resolved path {}: {e}",
            bin_path.display()
        ))
    })?;

    // Verify the canonical resolved path starts with the canonical bin dir.
    if !canonical_resolved.starts_with(&canonical_bin_dir) {
        return Err(EngineError::BinOutsideBundle {
            bin: bin_name.clone(),
            resolved: canonical_resolved.display().to_string(),
            bin_dir: canonical_bin_dir.display().to_string(),
        });
    }

    // --- Regular file check: reject symlinks that escape, FIFOs, dirs, devices ---

    let metadata = std::fs::symlink_metadata(&bin_path).map_err(|e| {
        EngineError::Internal(format!(
            "failed to stat resolved binary {}: {e}",
            bin_path.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(EngineError::BinNotRegularFile {
            bin: bin_name.clone(),
        });
    }

    let manifest_ref_path = bundle_root
        .join(crate::AI_DIR)
        .join("refs")
        .join("bundles")
        .join("manifest");

    require_executor_manifest_ref(&manifest_ref_path, bundle_root, &bin_name)?;

    let signed_manifest_ref = std::fs::read_to_string(&manifest_ref_path).map_err(|_| {
        EngineError::BinManifestMissing {
            bundle_root: bundle_root.display().to_string(),
        }
    })?;
    let verified_manifest_ref = crate::executor_resolution::verify_signed_executor_manifest_ref(
        &signed_manifest_ref,
        &trusted_verifying_key,
        root_trust_class,
    )
    .map_err(|error| match error {
        crate::executor_resolution::ExecutorResolutionError::ManifestSignerUntrusted {
            fingerprint,
        } => EngineError::BinUntrusted {
            bin: bin_name.clone(),
            fingerprint,
        },
        other => EngineError::BinManifestInvalid {
            bin: bin_name.clone(),
            reason: other.to_string(),
        },
    })?;
    if !is_dispatchable_trust_class(verified_manifest_ref.trust_class) {
        return Err(EngineError::BinUntrusted {
            bin: bin_name.clone(),
            fingerprint: verified_manifest_ref.signer_fingerprint.clone(),
        });
    }
    let manifest_hash = verified_manifest_ref.manifest_hash.clone();

    let objects_dir = bundle_root.join(crate::AI_DIR).join("objects");
    let cas = lillux::cas::CasStore::new(objects_dir);

    let manifest_value = cas
        .get_object(&manifest_hash)
        .map_err(|e| {
            EngineError::Internal(format!("CAS read error for manifest {manifest_hash}: {e}"))
        })?
        .ok_or_else(|| EngineError::BinManifestMissing {
            bundle_root: bundle_root.display().to_string(),
        })?;

    let item_source_hashes = crate::executor_resolution::verify_executor_manifest_object(
        &manifest_value,
        &manifest_hash,
    )
    .map_err(|error| EngineError::BinManifestInvalid {
        bin: bin_name.clone(),
        reason: error.to_string(),
    })?;

    let item_source_hash =
        item_source_hashes
            .get(&item_ref)
            .ok_or_else(|| EngineError::BinNotInManifest {
                bin: bin_name.clone(),
                triple: triple.to_string(),
            })?;

    let item_source = cas
        .get_object(item_source_hash)
        .map_err(|e| {
            EngineError::Internal(format!(
                "CAS read error for item_source {item_source_hash}: {e}"
            ))
        })?
        .ok_or_else(|| {
            EngineError::Internal(format!(
                "item_source {item_source_hash} for {item_ref} not found in CAS"
            ))
        })?;

    let signed_sidecar_fingerprint = verify_item_source_sidecar(
        &bin_name,
        &bin_path,
        &item_ref,
        &item_source,
        &trusted_verifying_key,
    )?;

    let (content_blob_hash, signed_mode) = crate::executor_resolution::verify_executor_item_source(
        &item_source,
        item_source_hash,
        &item_ref,
    )
    .map_err(|error| EngineError::BinManifestInvalid {
        bin: bin_name.clone(),
        reason: error.to_string(),
    })?;
    verify_installed_mode(&bin_name, &bin_path, signed_mode)?;

    let blob_bytes = cas
        .get_blob(&content_blob_hash)
        .map_err(|error| EngineError::BinManifestInvalid {
            bin: bin_name.clone(),
            reason: format!("read content blob {content_blob_hash}: {error}"),
        })?
        .ok_or_else(|| EngineError::BinManifestInvalid {
            bin: bin_name.clone(),
            reason: format!("content blob {content_blob_hash} is missing from bundle CAS"),
        })?;
    let computed_blob_hash = lillux::sha256_hex(&blob_bytes);
    if computed_blob_hash != content_blob_hash {
        return Err(EngineError::BinHashMismatch {
            bin: bin_name.clone(),
            declared: content_blob_hash.clone(),
            computed: computed_blob_hash,
        });
    }

    let bin_bytes = std::fs::read(&bin_path).map_err(|e| {
        EngineError::Internal(format!("failed to read binary {}: {e}", bin_path.display()))
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&bin_bytes);
    let computed_hash = format!("{:x}", hasher.finalize());

    if computed_hash != content_blob_hash {
        return Err(EngineError::BinHashMismatch {
            bin: bin_name.clone(),
            declared: content_blob_hash,
            computed: computed_hash,
        });
    }
    if bin_bytes != blob_bytes {
        return Err(EngineError::BinHashMismatch {
            bin: bin_name.clone(),
            declared: content_blob_hash,
            computed: computed_hash,
        });
    }

    let signer_fingerprint = verified_manifest_ref.signer_fingerprint;
    if signer_fingerprint != signed_sidecar_fingerprint {
        return Err(EngineError::BinSidecarInvalid {
            bin: bin_name,
            reason: format!(
                "sidecar signer `{signed_sidecar_fingerprint}` does not match signed manifest-ref signer `{signer_fingerprint}`"
            ),
        });
    }

    Ok(ResolvedBinary {
        absolute_path: bin_path,
        content_hash: computed_hash,
        manifest_hash,
        signer_fingerprint,
    })
}

/// Verify the complete node-installed executable set for a bundle.
///
/// If a bundle ships `.ai/bin`, the only accepted authorization chain is the
/// current signed executor-manifest ref format. Every manifest entry must name
/// one regular on-disk executable and its regular signed ItemSource sidecar;
/// every executable and sidecar on disk must be named by that manifest. The
/// manifest object, ItemSource objects, CAS blobs, sidecars, file bytes, Unix
/// modes, and signer continuity are all checked before the bundle is admitted.
pub fn verify_bundle_executor_manifest(
    bundle_root: &Path,
    node_trust_store: &TrustStore,
) -> Result<(), EngineError> {
    verify_bundle_executor_manifest_items(bundle_root, node_trust_store).map(|_| ())
}

/// Verify the executable set and retain its signed manifest identity for a
/// multi-reader bundle-generation snapshot.
pub fn verify_bundle_executor_manifest_identity(
    bundle_root: &Path,
    node_trust_store: &TrustStore,
) -> Result<Option<BundleExecutorManifestIdentity>, EngineError> {
    verify_bundle_executor_manifest_items(bundle_root, node_trust_store)
        .map(|verified| verified.identity)
}

/// Re-verify only the signed executor-manifest reference identity. Generation
/// guards use this after initial full admission to detect replacement without
/// re-hashing every executable on every launch.
pub fn verify_bundle_executor_manifest_ref_identity(
    bundle_root: &Path,
    node_trust_store: &TrustStore,
) -> Result<Option<BundleExecutorManifestIdentity>, EngineError> {
    let ai_dir = bundle_root.join(crate::AI_DIR);
    let bin_root = ai_dir.join("bin");
    let manifest_ref_path = ai_dir.join("refs").join("bundles").join("manifest");
    let bin_exists = match std::fs::symlink_metadata(&bin_root) {
        Ok(metadata) if metadata.file_type().is_dir() => true,
        Ok(_) => {
            return Err(bundle_executor_error(
                bundle_root,
                format!("{} must be a regular directory", bin_root.display()),
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(bundle_executor_error(
                bundle_root,
                format!("stat {}: {error}", bin_root.display()),
            ))
        }
    };
    let manifest_ref_exists = match std::fs::symlink_metadata(&manifest_ref_path) {
        Ok(metadata) if metadata.file_type().is_file() => true,
        Ok(_) => {
            return Err(bundle_executor_error(
                bundle_root,
                format!("{} must be a regular file", manifest_ref_path.display()),
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(bundle_executor_error(
                bundle_root,
                format!("stat {}: {error}", manifest_ref_path.display()),
            ))
        }
    };
    if !bin_exists && !manifest_ref_exists {
        return Ok(None);
    }
    if !bin_exists || !manifest_ref_exists {
        return Err(EngineError::BinManifestMissing {
            bundle_root: bundle_root.display().to_string(),
        });
    }
    let signed_manifest_ref = lillux::read_regular_file_to_string_no_follow(&manifest_ref_path)
        .map_err(|error| {
            bundle_executor_error(
                bundle_root,
                format!("read {}: {error}", manifest_ref_path.display()),
            )
        })?;
    let verified = crate::executor_resolution::verify_signed_executor_manifest_ref(
        &signed_manifest_ref,
        |fingerprint| {
            node_trust_store
                .get(fingerprint)
                .map(|signer| signer.verifying_key)
        },
        TrustClass::TrustedBundle,
    )
    .map_err(|error| bundle_executor_error(bundle_root, error.to_string()))?;
    if !is_dispatchable_trust_class(verified.trust_class) {
        return Err(EngineError::BinUntrusted {
            bin: bundle_root.display().to_string(),
            fingerprint: verified.signer_fingerprint,
        });
    }
    Ok(Some(BundleExecutorManifestIdentity {
        manifest_hash: verified.manifest_hash,
        signer_fingerprint: verified.signer_fingerprint,
    }))
}

/// Verify every admitted bundle's executable set and require one owner for
/// each native-executor identity.
///
/// Native executor references are globally addressed as
/// `native:<bare-name>` at launch time, with the target triple supplied by the
/// node. Consequently, two bundles may not both publish the same
/// `bin/<target-triple>/<bare-name>` identity. Checking every target triple at
/// admission time keeps a bundle set portable: a collision cannot remain
/// dormant merely because the current node has a different host triple.
pub fn verify_bundle_executor_manifests(
    bundle_roots: &[PathBuf],
    node_trust_store: &TrustStore,
) -> Result<(), EngineError> {
    let mut owners: HashMap<String, &Path> = HashMap::new();

    for bundle_root in bundle_roots {
        let mut item_refs: Vec<String> =
            verify_bundle_executor_manifest_items(bundle_root, node_trust_store)?
                .item_refs
                .into_iter()
                .collect();
        item_refs.sort_unstable();
        for item_ref in item_refs {
            if let Some(first_root) = owners.insert(item_ref.clone(), bundle_root.as_path()) {
                return Err(EngineError::BinManifestInvalid {
                    bin: item_ref.clone(),
                    reason: format!(
                        "native executor identity `{item_ref}` is published by both {} and {}; each target-triple/name identity must have exactly one admitted bundle owner",
                        first_root.display(),
                        bundle_root.display(),
                    ),
                });
            }
        }
    }

    Ok(())
}

fn verify_bundle_executor_manifest_items(
    bundle_root: &Path,
    node_trust_store: &TrustStore,
) -> Result<VerifiedBundleExecutorSet, EngineError> {
    let ai_dir = bundle_root.join(crate::AI_DIR);
    require_regular_bundle_ai_tree(bundle_root, &ai_dir, true)?;
    let bin_root = ai_dir.join("bin");
    let manifest_ref_path = ai_dir.join("refs").join("bundles").join("manifest");

    let bin_root_metadata = match std::fs::symlink_metadata(&bin_root) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                return Err(bundle_executor_error(
                    bundle_root,
                    format!("{} must be a regular directory", bin_root.display()),
                ));
            }
            Some(metadata)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(bundle_executor_error(
                bundle_root,
                format!("stat {}: {error}", bin_root.display()),
            ));
        }
    };
    let manifest_ref_exists = match std::fs::symlink_metadata(&manifest_ref_path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                return Err(bundle_executor_error(
                    bundle_root,
                    format!("{} must be a regular file", manifest_ref_path.display()),
                ));
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(bundle_executor_error(
                bundle_root,
                format!("stat {}: {error}", manifest_ref_path.display()),
            ));
        }
    };

    if bin_root_metadata.is_none() && !manifest_ref_exists {
        return Ok(VerifiedBundleExecutorSet {
            identity: None,
            item_refs: HashSet::new(),
        });
    }
    if !manifest_ref_exists {
        return Err(EngineError::BinManifestMissing {
            bundle_root: bundle_root.display().to_string(),
        });
    }

    let signed_manifest_ref = std::fs::read_to_string(&manifest_ref_path).map_err(|error| {
        bundle_executor_error(
            bundle_root,
            format!("read {}: {error}", manifest_ref_path.display()),
        )
    })?;
    let verified_manifest_ref = crate::executor_resolution::verify_signed_executor_manifest_ref(
        &signed_manifest_ref,
        |fingerprint| {
            node_trust_store
                .get(fingerprint)
                .map(|signer| signer.verifying_key)
        },
        TrustClass::TrustedBundle,
    )
    .map_err(|error| match error {
        crate::executor_resolution::ExecutorResolutionError::ManifestSignerUntrusted {
            fingerprint,
        } => EngineError::BinUntrusted {
            bin: bundle_root.display().to_string(),
            fingerprint,
        },
        other => bundle_executor_error(bundle_root, other.to_string()),
    })?;
    if !is_dispatchable_trust_class(verified_manifest_ref.trust_class) {
        return Err(EngineError::BinUntrusted {
            bin: bundle_root.display().to_string(),
            fingerprint: verified_manifest_ref.signer_fingerprint,
        });
    }

    let cas = lillux::cas::CasStore::new(ai_dir.join("objects"));
    let manifest_value = cas
        .get_object(&verified_manifest_ref.manifest_hash)
        .map_err(|error| {
            bundle_executor_error(
                bundle_root,
                format!(
                    "read executor manifest object {}: {error}",
                    verified_manifest_ref.manifest_hash
                ),
            )
        })?
        .ok_or_else(|| {
            bundle_executor_error(
                bundle_root,
                format!(
                    "executor manifest object {} is missing from bundle CAS",
                    verified_manifest_ref.manifest_hash
                ),
            )
        })?;
    let item_source_hashes = crate::executor_resolution::verify_executor_manifest_object(
        &manifest_value,
        &verified_manifest_ref.manifest_hash,
    )
    .map_err(|error| bundle_executor_error(bundle_root, error.to_string()))?;

    let expected_item_refs: HashSet<String> = item_source_hashes.keys().cloned().collect();
    let mut ordered_item_refs: Vec<&String> = item_source_hashes.keys().collect();
    ordered_item_refs.sort_unstable();
    for item_ref in ordered_item_refs {
        let (triple, bin_name) = parse_executor_item_ref(item_ref).map_err(|detail| {
            bundle_executor_error(
                bundle_root,
                format!("invalid executor item ref `{item_ref}`: {detail}"),
            )
        })?;
        let bin_path = bin_root.join(triple).join(bin_name);
        require_regular_artifact(bundle_root, &bin_path, "installed executable")?;

        let item_source_hash = &item_source_hashes[item_ref];
        let item_source = cas
            .get_object(item_source_hash)
            .map_err(|error| {
                bundle_executor_error(
                    bundle_root,
                    format!("read ItemSource {item_source_hash} for {item_ref}: {error}"),
                )
            })?
            .ok_or_else(|| {
                bundle_executor_error(
                    bundle_root,
                    format!("ItemSource {item_source_hash} for {item_ref} is missing from CAS"),
                )
            })?;
        let sidecar_signer = verify_item_source_sidecar(
            bin_name,
            &bin_path,
            item_ref,
            &item_source,
            &|fingerprint| {
                node_trust_store
                    .get(fingerprint)
                    .map(|signer| signer.verifying_key)
            },
        )?;
        if sidecar_signer != verified_manifest_ref.signer_fingerprint {
            return Err(EngineError::BinSidecarInvalid {
                bin: bin_name.to_string(),
                reason: format!(
                    "sidecar signer `{sidecar_signer}` does not match signed manifest-ref signer `{}`",
                    verified_manifest_ref.signer_fingerprint
                ),
            });
        }

        let (blob_hash, signed_mode) = crate::executor_resolution::verify_executor_item_source(
            &item_source,
            item_source_hash,
            item_ref,
        )
        .map_err(|error| bundle_executor_error(bundle_root, error.to_string()))?;
        verify_installed_mode(bin_name, &bin_path, signed_mode)?;

        let blob_bytes = cas
            .get_blob(&blob_hash)
            .map_err(|error| {
                bundle_executor_error(
                    bundle_root,
                    format!("read content blob {blob_hash} for {item_ref}: {error}"),
                )
            })?
            .ok_or_else(|| {
                bundle_executor_error(
                    bundle_root,
                    format!("content blob {blob_hash} for {item_ref} is missing from CAS"),
                )
            })?;
        let actual_blob_hash = lillux::sha256_hex(&blob_bytes);
        if actual_blob_hash != blob_hash {
            return Err(EngineError::BinHashMismatch {
                bin: bin_name.to_string(),
                declared: blob_hash,
                computed: actual_blob_hash,
            });
        }

        let installed_bytes = std::fs::read(&bin_path).map_err(|error| {
            bundle_executor_error(
                bundle_root,
                format!("read installed executable {}: {error}", bin_path.display()),
            )
        })?;
        let installed_hash = lillux::sha256_hex(&installed_bytes);
        if installed_hash != blob_hash || installed_bytes != blob_bytes {
            return Err(EngineError::BinHashMismatch {
                bin: bin_name.to_string(),
                declared: blob_hash,
                computed: installed_hash,
            });
        }
    }

    let mut installed_item_refs = HashSet::new();
    let mut installed_sidecar_refs = HashSet::new();
    if bin_root_metadata.is_some() {
        for triple_entry in sorted_dir_entries(&bin_root, bundle_root)? {
            let triple_type = triple_entry.file_type().map_err(|error| {
                bundle_executor_error(
                    bundle_root,
                    format!("inspect {}: {error}", triple_entry.path().display()),
                )
            })?;
            let triple = triple_entry.file_name().into_string().map_err(|_| {
                bundle_executor_error(bundle_root, "binary triple directory is not UTF-8")
            })?;
            if triple_type.is_symlink()
                || !triple_type.is_dir()
                || !is_safe_executor_path_segment(&triple)
            {
                return Err(bundle_executor_error(
                    bundle_root,
                    format!(
                        "{} must be a regular, safely named target-triple directory",
                        triple_entry.path().display()
                    ),
                ));
            }

            for artifact in sorted_dir_entries(&triple_entry.path(), bundle_root)? {
                let artifact_type = artifact.file_type().map_err(|error| {
                    bundle_executor_error(
                        bundle_root,
                        format!("inspect {}: {error}", artifact.path().display()),
                    )
                })?;
                if artifact_type.is_symlink() || !artifact_type.is_file() {
                    return Err(bundle_executor_error(
                        bundle_root,
                        format!("{} must be a regular file", artifact.path().display()),
                    ));
                }
                let artifact_name = artifact.file_name().into_string().map_err(|_| {
                    bundle_executor_error(bundle_root, "binary artifact name is not UTF-8")
                })?;
                let (bin_name, is_sidecar) = match artifact_name.strip_suffix(".item_source.json") {
                    Some(bin_name) => (bin_name, true),
                    None => (artifact_name.as_str(), false),
                };
                if !is_safe_executor_path_segment(bin_name) {
                    return Err(bundle_executor_error(
                        bundle_root,
                        format!("unsafe binary artifact name `{artifact_name}`"),
                    ));
                }
                let item_ref = format!("bin/{triple}/{bin_name}");
                if !expected_item_refs.contains(&item_ref) {
                    return Err(bundle_executor_error(
                        bundle_root,
                        format!(
                            "on-disk executable artifact {} is not authorized by the signed executor manifest",
                            artifact.path().display()
                        ),
                    ));
                }
                if is_sidecar {
                    installed_sidecar_refs.insert(item_ref);
                } else {
                    installed_item_refs.insert(item_ref);
                }
            }
        }
    }

    if installed_item_refs != expected_item_refs || installed_sidecar_refs != expected_item_refs {
        return Err(bundle_executor_error(
            bundle_root,
            "signed executor manifest, installed executables, and ItemSource sidecars do not form a one-to-one set",
        ));
    }

    Ok(VerifiedBundleExecutorSet {
        identity: Some(BundleExecutorManifestIdentity {
            manifest_hash: verified_manifest_ref.manifest_hash,
            signer_fingerprint: verified_manifest_ref.signer_fingerprint,
        }),
        item_refs: expected_item_refs,
    })
}

fn bundle_executor_error(bundle_root: &Path, reason: impl Into<String>) -> EngineError {
    EngineError::BinManifestInvalid {
        bin: bundle_root.display().to_string(),
        reason: reason.into(),
    }
}

/// Require the entire bundle `.ai` control tree to remain inside the admitted
/// generation. In particular, CAS objects must not be reached through an
/// ancestor symlink whose target can change independently after installation.
fn require_regular_bundle_ai_tree(
    bundle_root: &Path,
    path: &Path,
    tree_root: bool,
) -> Result<(), EngineError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        bundle_executor_error(
            bundle_root,
            format!("stat bundle control-tree path {}: {error}", path.display()),
        )
    })?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(bundle_executor_error(
            bundle_root,
            format!("bundle control tree contains symlink at {}", path.display()),
        ));
    }
    if file_type.is_dir() {
        for entry in sorted_dir_entries(path, bundle_root)? {
            require_regular_bundle_ai_tree(bundle_root, &entry.path(), false)?;
        }
        return Ok(());
    }
    if !tree_root && file_type.is_file() {
        return Ok(());
    }

    Err(bundle_executor_error(
        bundle_root,
        if tree_root {
            format!(
                "bundle control tree root must be a real directory at {}",
                path.display()
            )
        } else {
            format!(
                "bundle control tree contains non-regular entry at {}",
                path.display()
            )
        },
    ))
}

fn require_executor_manifest_ref(
    path: &Path,
    bundle_root: &Path,
    bin_name: &str,
) -> Result<(), EngineError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.file_type().is_file() => {
            Ok(())
        }
        Ok(_) => Err(EngineError::BinManifestInvalid {
            bin: bin_name.to_string(),
            reason: format!("{} must be a regular file", path.display()),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(EngineError::BinManifestMissing {
                bundle_root: bundle_root.display().to_string(),
            })
        }
        Err(error) => Err(EngineError::BinManifestInvalid {
            bin: bin_name.to_string(),
            reason: format!("stat {}: {error}", path.display()),
        }),
    }
}

fn require_regular_artifact(
    bundle_root: &Path,
    path: &Path,
    artifact_kind: &str,
) -> Result<(), EngineError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        bundle_executor_error(
            bundle_root,
            format!("stat {artifact_kind} {}: {error}", path.display()),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(bundle_executor_error(
            bundle_root,
            format!("{artifact_kind} {} must be a regular file", path.display()),
        ));
    }
    Ok(())
}

fn sorted_dir_entries(
    path: &Path,
    bundle_root: &Path,
) -> Result<Vec<std::fs::DirEntry>, EngineError> {
    let mut entries = std::fs::read_dir(path)
        .map_err(|error| {
            bundle_executor_error(bundle_root, format!("read {}: {error}", path.display()))
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            bundle_executor_error(bundle_root, format!("read {}: {error}", path.display()))
        })?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    Ok(entries)
}

fn parse_executor_item_ref(item_ref: &str) -> Result<(&str, &str), &'static str> {
    let mut parts = item_ref.split('/');
    if parts.next() != Some("bin") {
        return Err("must start with `bin/`");
    }
    let triple = parts.next().ok_or("missing target triple")?;
    let bin_name = parts.next().ok_or("missing binary name")?;
    if parts.next().is_some()
        || !is_safe_executor_path_segment(triple)
        || !is_safe_executor_path_segment(bin_name)
    {
        return Err("must be exactly `bin/<safe-triple>/<safe-name>`");
    }
    Ok((triple, bin_name))
}

fn is_safe_executor_path_segment(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('.')
        && !value.contains("..")
        && !value.contains('/')
        && !value.contains('\\')
        && !value.chars().any(char::is_whitespace)
        && !value.chars().any(char::is_control)
}

fn is_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(unix)]
fn verify_installed_mode(
    bin_name: &str,
    bin_path: &Path,
    signed_mode: u32,
) -> Result<(), EngineError> {
    use std::os::unix::fs::PermissionsExt;

    let actual_mode = std::fs::symlink_metadata(bin_path)
        .map_err(|error| EngineError::BinManifestInvalid {
            bin: bin_name.to_string(),
            reason: format!("stat {} for Unix mode: {error}", bin_path.display()),
        })?
        .permissions()
        .mode()
        & 0o7777;
    if actual_mode != signed_mode {
        return Err(EngineError::BinManifestInvalid {
            bin: bin_name.to_string(),
            reason: format!(
                "installed Unix mode {actual_mode:#o} does not match signed mode {signed_mode:#o}"
            ),
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_installed_mode(
    _bin_name: &str,
    _bin_path: &Path,
    _signed_mode: u32,
) -> Result<(), EngineError> {
    Ok(())
}

fn registered_bundle_root_for_source(
    source_path: &Path,
    roots: &ResolutionRoots,
) -> Option<PathBuf> {
    roots
        .ordered
        .iter()
        .filter(|root| root.space == ItemSpace::Bundle)
        .filter_map(|root| root.ai_root.parent().map(Path::to_path_buf))
        .find(|bundle_root| source_path.starts_with(bundle_root))
}

/// Walk up from `path` to find the first ancestor containing `.ai/`.
fn find_bundle_root(path: &Path) -> Option<PathBuf> {
    let mut current = path;
    loop {
        if current.join(crate::AI_DIR).is_dir() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

fn find_qualified_target_bundle(
    target_name: &str,
    roots: &ResolutionRoots,
    trust_store: &TrustStore,
) -> Result<(PathBuf, MinimalBundleManifest), EngineError> {
    let mut searched = Vec::new();
    let mut skipped = Vec::new();
    let mut matches = Vec::new();

    for root in roots
        .ordered
        .iter()
        .filter(|root| root.space == ItemSpace::Bundle)
    {
        let Some(bundle_root) = root.ai_root.parent().map(Path::to_path_buf) else {
            continue;
        };
        searched.push(bundle_root.display().to_string());
        // Skip bundles whose signed manifest doesn't verify: an unverifiable
        // manifest can't be a trusted target, and one malformed/unsigned bundle
        // in the registered set must not break qualified resolution of others.
        // The reason is retained so a broken registration surfaces in the
        // not-found diagnostic rather than looking like a genuine absence.
        match load_minimal_bundle_manifest(&bundle_root, trust_store) {
            Ok(manifest) if manifest.name == target_name => {
                matches.push((bundle_root, manifest));
            }
            Ok(_) => {}
            Err(reason) => skipped.push(reason.to_string()),
        }
    }

    match matches.len() {
        0 => Err(EngineError::QualifiedBinBundleNotFound {
            bundle: target_name.to_string(),
            searched,
            skipped,
        }),
        1 => Ok(matches.pop().expect("len checked above")),
        _ => Err(EngineError::QualifiedBinBundleAmbiguous {
            bundle: target_name.to_string(),
            roots: matches.into_iter().map(|(root, _)| root).collect(),
        }),
    }
}

/// The subset of a signed bundle manifest that qualified binary resolution
/// needs: the bundle identity and its kind-dependency surface. Intentionally a
/// lenient projection (no `deny_unknown_fields`) so it does not break when the
/// full manifest schema gains fields — the manifest is already signature- and
/// trust-verified before it is parsed here, so extra fields are not a hazard.
#[derive(Debug, Deserialize)]
struct MinimalBundleManifest {
    name: String,
    #[serde(default)]
    provides_kinds: Vec<String>,
    #[serde(default)]
    requires_kinds: Vec<String>,
    #[serde(default)]
    uses_kinds: Vec<String>,
}

fn load_minimal_bundle_manifest(
    bundle_root: &Path,
    trust_store: &TrustStore,
) -> Result<MinimalBundleManifest, EngineError> {
    let manifest_path = bundle_root.join(crate::AI_DIR).join("manifest.yaml");
    let file_type = std::fs::symlink_metadata(&manifest_path)
        .map_err(|e| EngineError::QualifiedBinManifestInvalid {
            path: manifest_path.display().to_string(),
            reason: format!("stat failed: {e}"),
        })?
        .file_type();
    if file_type.is_symlink() || !file_type.is_file() {
        return Err(EngineError::QualifiedBinManifestInvalid {
            path: manifest_path.display().to_string(),
            reason: "manifest is not a regular file".into(),
        });
    }

    let raw = std::fs::read_to_string(&manifest_path).map_err(|e| {
        EngineError::QualifiedBinManifestInvalid {
            path: manifest_path.display().to_string(),
            reason: format!("read failed: {e}"),
        }
    })?;
    let envelope = SignatureEnvelope {
        prefix: "#".into(),
        suffix: None,
        after_shebang: false,
    };
    let sig_header =
        crate::item_resolution::parse_signature_header(&raw, &envelope).ok_or_else(|| {
            EngineError::QualifiedBinManifestInvalid {
                path: manifest_path.display().to_string(),
                reason: "missing or malformed signature header".into(),
            }
        })?;
    let (trust_class, _) =
        crate::trust::verify_item_signature(&raw, &sig_header, &envelope, trust_store).map_err(
            |e| EngineError::QualifiedBinManifestInvalid {
                path: manifest_path.display().to_string(),
                reason: format!("signature verification failed: {e}"),
            },
        )?;
    if trust_class != ContractTrustClass::Trusted {
        return Err(EngineError::QualifiedBinManifestInvalid {
            path: manifest_path.display().to_string(),
            reason: format!(
                "manifest signer {} is not trusted (trust_class: {:?})",
                sig_header.signer_fingerprint, trust_class
            ),
        });
    }

    let body = lillux::signature::strip_signature_lines(&raw);
    serde_yaml::from_str(&body).map_err(|e| EngineError::QualifiedBinManifestInvalid {
        path: manifest_path.display().to_string(),
        reason: format!("parse failed: {e}"),
    })
}

fn ensure_qualified_binary_dependency(
    source: &MinimalBundleManifest,
    target: &MinimalBundleManifest,
) -> Result<(), EngineError> {
    if source.name == target.name {
        return Ok(());
    }
    let source_needs = source.requires_kinds.iter().chain(source.uses_kinds.iter());
    if source_needs.clone().any(|kind| {
        target
            .provides_kinds
            .iter()
            .any(|provided| provided == kind)
    }) {
        return Ok(());
    }

    Err(EngineError::QualifiedBinDependencyMissing {
        source_bundle: source.name.clone(),
        target_bundle: target.name.clone(),
    })
}

fn validate_bundle_name(name: &str, raw_ref: &str) -> Result<(), EngineError> {
    if name.is_empty() {
        return Err(EngineError::InvalidBinPrefix {
            raw: raw_ref.to_string(),
            detail: "no bundle name in qualified binary ref".into(),
        });
    }
    if name.contains('/') || name.contains("..") || name.starts_with('.') || name.contains(' ') {
        return Err(EngineError::InvalidBinPrefix {
            raw: raw_ref.to_string(),
            detail: "bundle name must be a single non-hidden identifier without spaces, slashes, or `..`".into(),
        });
    }
    if name.chars().any(|c| c.is_control() || c == '\0') {
        return Err(EngineError::InvalidBinPrefix {
            raw: raw_ref.to_string(),
            detail: "bundle name must not contain control characters".into(),
        });
    }
    Ok(())
}

fn verify_item_source_sidecar(
    bin_name: &str,
    bin_path: &Path,
    expected_item_ref: &str,
    item_source: &serde_json::Value,
    trusted_verifying_key: &impl Fn(&str) -> Option<VerifyingKey>,
) -> Result<String, EngineError> {
    let sidecar_path = bin_path.with_file_name(format!("{bin_name}.item_source.json"));
    let sidecar_metadata =
        std::fs::symlink_metadata(&sidecar_path).map_err(|e| EngineError::BinSidecarInvalid {
            bin: bin_name.to_string(),
            reason: format!("stat {}: {e}", sidecar_path.display()),
        })?;
    if sidecar_metadata.file_type().is_symlink() || !sidecar_metadata.file_type().is_file() {
        return Err(EngineError::BinSidecarInvalid {
            bin: bin_name.to_string(),
            reason: format!("{} must be a regular file", sidecar_path.display()),
        });
    }
    let signed =
        std::fs::read_to_string(&sidecar_path).map_err(|e| EngineError::BinSidecarInvalid {
            bin: bin_name.to_string(),
            reason: format!("read {}: {e}", sidecar_path.display()),
        })?;

    let (signature_line, body) =
        signed
            .split_once('\n')
            .ok_or_else(|| EngineError::BinSidecarInvalid {
                bin: bin_name.to_string(),
                reason: format!(
                    "{} must contain one signature header and a canonical JSON body",
                    sidecar_path.display()
                ),
            })?;
    if !signature_line.starts_with("# ryeos:signed:") || signature_line.trim_end() != signature_line
    {
        return Err(EngineError::BinSidecarInvalid {
            bin: bin_name.to_string(),
            reason: "signature header is not in canonical `# ryeos:signed:` form".to_string(),
        });
    }
    let header =
        lillux::signature::parse_signature_line(signature_line, "#", None).ok_or_else(|| {
            EngineError::BinSidecarInvalid {
                bin: bin_name.to_string(),
                reason: format!(
                    "missing or malformed signature line in {}",
                    sidecar_path.display()
                ),
            }
        })?;
    if !is_lower_sha256(&header.signer_fingerprint)
        || !is_lower_sha256(&header.content_hash)
        || !crate::executor_resolution::has_canonical_signature_timestamp_shape(&header.timestamp)
    {
        return Err(EngineError::BinSidecarInvalid {
            bin: bin_name.to_string(),
            reason: "signature header fields are not canonical".to_string(),
        });
    }
    let signature_bytes = base64::engine::general_purpose::STANDARD
        .decode(&header.signature_b64)
        .map_err(|_| EngineError::BinSidecarInvalid {
            bin: bin_name.to_string(),
            reason: "signature must use canonical standard base64".to_string(),
        })?;
    if signature_bytes.len() != 64
        || base64::engine::general_purpose::STANDARD.encode(&signature_bytes)
            != header.signature_b64
    {
        return Err(EngineError::BinSidecarInvalid {
            bin: bin_name.to_string(),
            reason: "signature must be one canonical Ed25519 signature".to_string(),
        });
    }

    let Some(verifying_key) = trusted_verifying_key(&header.signer_fingerprint) else {
        return Err(EngineError::BinUntrusted {
            bin: bin_name.to_string(),
            fingerprint: header.signer_fingerprint,
        });
    };
    let actual_fingerprint = lillux::signature::compute_fingerprint(&verifying_key);
    if actual_fingerprint != header.signer_fingerprint {
        return Err(EngineError::BinSidecarInvalid {
            bin: bin_name.to_string(),
            reason: "trusted-key lookup returned a key with a different fingerprint".to_string(),
        });
    }

    if !lillux::signature::is_valid_signature_for(
        &header.content_hash,
        &header.signature_b64,
        &header.signer_fingerprint,
        body,
        &verifying_key,
        &actual_fingerprint,
    ) {
        return Err(EngineError::BinSidecarInvalid {
            bin: bin_name.to_string(),
            reason: "signature verification failed".to_string(),
        });
    }

    let canonical_item_source = lillux::cas::canonical_json(item_source).map_err(|error| {
        EngineError::BinSidecarInvalid {
            bin: bin_name.to_string(),
            reason: format!("item_source cannot be canonicalized: {error}"),
        }
    })?;
    if body != canonical_item_source {
        return Err(EngineError::BinSidecarInvalid {
            bin: bin_name.to_string(),
            reason: "signed sidecar body does not match CAS item_source object".to_string(),
        });
    }

    let actual_item_ref = item_source
        .get("item_ref")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if actual_item_ref != expected_item_ref {
        return Err(EngineError::BinSidecarInvalid {
            bin: bin_name.to_string(),
            reason: format!(
                "item_source item_ref `{actual_item_ref}` does not match resolved binary ref `{expected_item_ref}`"
            ),
        });
    }

    Ok(actual_fingerprint)
}

/// Validate a binary name for both `bin:<name>` and `bin/<triple>/<name>`.
///
/// Rejects:
///   - empty
///   - any `/`
///   - any `..` segment
///   - leading `.` (hidden file)
///   - any control char or NUL
///   - spaces
fn validate_bin_name(name: &str, raw_ref: &str) -> Result<(), EngineError> {
    if name.is_empty() {
        return Err(EngineError::InvalidBinPrefix {
            raw: raw_ref.to_string(),
            detail: "no binary name after prefix".into(),
        });
    }
    if name.contains('/') {
        return Err(EngineError::InvalidBinPrefix {
            raw: raw_ref.to_string(),
            detail: "binary name must not contain slashes".into(),
        });
    }
    if name.contains("..") {
        return Err(EngineError::InvalidBinPrefix {
            raw: raw_ref.to_string(),
            detail: "binary name must not contain path traversal (`..`)".into(),
        });
    }
    if name.starts_with('.') {
        return Err(EngineError::InvalidBinPrefix {
            raw: raw_ref.to_string(),
            detail: "binary name must not start with `.` (hidden file)".into(),
        });
    }
    if name.contains(' ') {
        return Err(EngineError::InvalidBinPrefix {
            raw: raw_ref.to_string(),
            detail: "binary name must not contain spaces — put subcommand args in the YAML's `args` list".into(),
        });
    }
    if name.chars().any(|c| c.is_control() || c == '\0') {
        return Err(EngineError::InvalidBinPrefix {
            raw: raw_ref.to_string(),
            detail: "binary name must not contain control characters".into(),
        });
    }
    Ok(())
}

/// Decide whether the signature-verified manifest ref's effective trust class
/// is high enough to dispatch the binary.
///
/// Both `TrustedBundle` and `TrustedProject` are dispatchable. The
/// effective tier is already the `min` of trusted bundle-publisher
/// authorization and the descriptor's `root_trust_class`.
/// A `TrustedProject` here therefore means a system-signed binary
/// reached through a user/project-tier descriptor — safe to run.
/// Anything weaker (`UntrustedProject`, `Unsigned`) must be refused.
fn is_dispatchable_trust_class(tc: TrustClass) -> bool {
    matches!(tc, TrustClass::TrustedBundle | TrustClass::TrustedProject)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_resolution::{ResolutionRoot, ResolutionRoots};
    use crate::trust::{TrustStore, TrustedSigner};
    use lillux::crypto::SigningKey;

    #[test]
    fn only_trusted_effective_classes_are_dispatchable() {
        assert!(is_dispatchable_trust_class(TrustClass::TrustedBundle));
        assert!(is_dispatchable_trust_class(TrustClass::TrustedProject));
        assert!(!is_dispatchable_trust_class(TrustClass::UntrustedProject));
        assert!(!is_dispatchable_trust_class(TrustClass::Unsigned));
    }

    /// Build a minimally valid bundle in `bundle_root` containing a
    /// single binary named `bin_name`, its CAS-stored item_source/manifest,
    /// and the signed `refs/bundles/manifest` pointer. Returns the signer
    /// fingerprint and signing key used for both trust anchors.
    fn write_resolver_fixture(bundle_root: &Path, bin_name: &str) -> (String, SigningKey) {
        write_resolver_fixture_for_triple(bundle_root, bin_name, env!("RYEOS_ENGINE_HOST_TRIPLE"))
    }

    fn write_resolver_fixture_for_triple(
        bundle_root: &Path,
        bin_name: &str,
        triple: &str,
    ) -> (String, SigningKey) {
        let ai = bundle_root.join(crate::AI_DIR);
        let bin_dir = ai.join("bin").join(triple);
        std::fs::create_dir_all(&bin_dir).unwrap();
        let bin_path = bin_dir.join(bin_name);
        let bin_bytes = b"placeholder-binary\n";
        std::fs::write(&bin_path, bin_bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let cas = lillux::cas::CasStore::new(ai.join("objects"));
        let content_blob_hash = cas.store_blob(bin_bytes).unwrap();
        let signing_key = SigningKey::from_bytes(&[31u8; 32]);
        let fingerprint = lillux::signature::compute_fingerprint(&signing_key.verifying_key());
        let item_ref = format!("bin/{triple}/{bin_name}");
        let item_source = serde_json::json!({
            "kind": "item_source",
            "item_ref": item_ref,
            "content_blob_hash": content_blob_hash,
            "integrity": format!("sha256:{content_blob_hash}"),
            "signature_info": null,
            "mode": 0o755,
        });
        let item_source_hash = cas.store_object(&item_source).unwrap();
        let sidecar_body = lillux::cas::canonical_json(&item_source).unwrap();
        let signed_sidecar =
            lillux::signature::sign_content(&sidecar_body, &signing_key, "#", None);
        std::fs::write(
            bin_path.with_file_name(format!("{bin_name}.item_source.json")),
            signed_sidecar,
        )
        .unwrap();
        let manifest = serde_json::json!({
            "kind": "source_manifest",
            "item_source_hashes": {
                item_ref: item_source_hash
            }
        });
        let manifest_hash = cas.store_object(&manifest).unwrap();

        let ref_path = ai.join("refs").join("bundles").join("manifest");
        std::fs::create_dir_all(ref_path.parent().unwrap()).unwrap();
        let signed_ref = lillux::signature::sign_content(
            &format!(
                "{}\n{manifest_hash}\n",
                crate::executor_resolution::EXECUTOR_MANIFEST_REF_DOMAIN
            ),
            &signing_key,
            "#",
            None,
        );
        std::fs::write(ref_path, signed_ref).unwrap();

        (fingerprint, signing_key)
    }

    fn trusted_key_for<'a>(
        expected_fp: &'a str,
        key: &'a SigningKey,
    ) -> impl Fn(&str) -> Option<lillux::crypto::VerifyingKey> + 'a {
        move |fp| (fp == expected_fp).then(|| key.verifying_key())
    }

    fn trust_store_for(fp: &str, key: &SigningKey) -> TrustStore {
        TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: fp.to_string(),
            verifying_key: key.verifying_key(),
            label: None,
        }])
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn captured_executable_is_sealed_against_installed_path_mutation() {
        use std::io::{Read as _, Seek as _};
        use std::os::fd::AsRawFd as _;
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let (fingerprint, key) = write_resolver_fixture(&bundle, "demo");
        let captured =
            capture_bundle_executable("demo", &bundle, &trust_store_for(&fingerprint, &key))
                .expect("signed executable should be captured");

        std::fs::write(&captured.identity.absolute_path, b"mutated-after-capture\n").unwrap();
        let mut retained = captured.handle.try_clone().unwrap();
        retained.rewind().unwrap();
        let mut bytes = Vec::new();
        retained.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"placeholder-binary\n");

        let seals = unsafe { libc::fcntl(retained.as_raw_fd(), libc::F_GET_SEALS) };
        let required =
            libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
        assert_eq!(seals & required, required);
        assert_ne!(retained.metadata().unwrap().permissions().mode() & 0o111, 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn captured_executable_rejects_set_id_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("set-id");
        std::fs::write(&path, b"binary").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o4755)).unwrap();
        let handle = std::fs::File::open(&path).unwrap();
        let error = validate_captured_executable(&handle, &path).unwrap_err();
        assert!(error.to_string().contains("setuid and setgid"));
    }

    #[test]
    fn full_bundle_executor_manifest_verifies_one_to_one_installed_set() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let (fingerprint, key) = write_resolver_fixture(&bundle, "demo");

        verify_bundle_executor_manifest(&bundle, &trust_store_for(&fingerprint, &key))
            .expect("complete signed executor chain should verify");

        let triple = env!("RYEOS_ENGINE_HOST_TRIPLE");
        std::fs::write(
            bundle
                .join(crate::AI_DIR)
                .join("bin")
                .join(triple)
                .join("extra"),
            b"not authorized",
        )
        .unwrap();
        let error = verify_bundle_executor_manifest(&bundle, &trust_store_for(&fingerprint, &key))
            .expect_err("an extra on-disk executable must be refused");
        assert!(error.to_string().contains("not authorized"));
    }

    #[test]
    fn admitted_bundle_set_rejects_duplicate_native_executor_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        let collision_triple = "future-vendor-platform-abi";
        let (fingerprint, key) =
            write_resolver_fixture_for_triple(&first, "shared-executor", collision_triple);
        write_resolver_fixture_for_triple(&second, "shared-executor", collision_triple);

        let error = verify_bundle_executor_manifests(
            &[first.clone(), second.clone()],
            &trust_store_for(&fingerprint, &key),
        )
        .expect_err("two owners of one target-triple/name identity must be refused");
        let message = error.to_string();
        assert!(message.contains(&format!("bin/{collision_triple}/shared-executor")));
        assert!(message.contains(&first.display().to_string()));
        assert!(message.contains(&second.display().to_string()));
    }

    #[test]
    fn admitted_bundle_set_allows_same_bare_name_for_different_triples() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        let (fingerprint, key) =
            write_resolver_fixture_for_triple(&first, "portable-executor", "target-one");
        write_resolver_fixture_for_triple(&second, "portable-executor", "target-two");

        verify_bundle_executor_manifests(&[first, second], &trust_store_for(&fingerprint, &key))
            .expect("target triple is part of the native executor identity");
    }

    #[cfg(unix)]
    #[test]
    fn full_bundle_executor_manifest_requires_exact_signed_mode() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let (fingerprint, key) = write_resolver_fixture(&bundle, "demo");
        let bin_path = bundle
            .join(crate::AI_DIR)
            .join("bin")
            .join(env!("RYEOS_ENGINE_HOST_TRIPLE"))
            .join("demo");
        std::fs::set_permissions(&bin_path, std::fs::Permissions::from_mode(0o750)).unwrap();

        let error = verify_bundle_executor_manifest(&bundle, &trust_store_for(&fingerprint, &key))
            .expect_err("installed mode drift must be refused");
        assert!(error.to_string().contains("does not match signed mode"));
    }

    #[cfg(unix)]
    #[test]
    fn full_bundle_executor_manifest_rejects_ai_root_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let real_bundle = tmp.path().join("real-bundle");
        let (fingerprint, key) = write_resolver_fixture(&real_bundle, "demo");
        let bundle = tmp.path().join("bundle");
        std::fs::create_dir(&bundle).unwrap();
        symlink(real_bundle.join(crate::AI_DIR), bundle.join(crate::AI_DIR)).unwrap();

        let error = verify_bundle_executor_manifest(&bundle, &trust_store_for(&fingerprint, &key))
            .expect_err("an ancestor .ai symlink must be refused");
        assert!(error.to_string().contains("control tree contains symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn full_bundle_executor_manifest_rejects_cas_ancestor_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let (fingerprint, key) = write_resolver_fixture(&bundle, "demo");
        let objects = bundle.join(crate::AI_DIR).join("objects");
        let external_objects = tmp.path().join("external-objects");
        std::fs::rename(&objects, &external_objects).unwrap();
        symlink(&external_objects, &objects).unwrap();

        let error = verify_bundle_executor_manifest(&bundle, &trust_store_for(&fingerprint, &key))
            .expect_err("a CAS ancestor symlink must be refused");
        assert!(error.to_string().contains("control tree contains symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn full_bundle_executor_manifest_rejects_special_ai_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let (fingerprint, key) = write_resolver_fixture(&bundle, "demo");
        let socket_path = bundle.join(crate::AI_DIR).join("unexpected.sock");
        let _socket = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();

        let error = verify_bundle_executor_manifest(&bundle, &trust_store_for(&fingerprint, &key))
            .expect_err("special entries in .ai must be refused");
        assert!(error.to_string().contains("non-regular entry"));
    }

    #[test]
    fn resolver_rejects_noncanonical_sidecar_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let (fingerprint, key) = write_resolver_fixture(&bundle, "demo");
        let sidecar_path = bundle
            .join(crate::AI_DIR)
            .join("bin")
            .join(env!("RYEOS_ENGINE_HOST_TRIPLE"))
            .join("demo.item_source.json");
        let signed = std::fs::read_to_string(&sidecar_path).unwrap();
        let (signature_line, body) = signed.split_once('\n').unwrap();
        let header = lillux::signature::parse_signature_line(signature_line, "#", None).unwrap();
        let noncanonical = format!(
            "# ryeos:signed:2026-07-14T12:34:56+00:00:{}:{}:{}\n{body}",
            header.content_hash, header.signature_b64, header.signer_fingerprint
        );
        std::fs::write(sidecar_path, noncanonical).unwrap();

        let error = resolve_bundle_binary_ref(
            "bin:demo",
            &bundle,
            trusted_key_for(&fingerprint, &key),
            TrustClass::TrustedBundle,
        )
        .expect_err("alternate timestamp encodings must be refused");
        assert!(matches!(
            error,
            EngineError::BinSidecarInvalid { ref reason, .. }
                if reason.contains("not canonical")
        ));
    }

    fn write_signed_bundle_manifest(
        bundle_root: &Path,
        name: &str,
        provides_kinds: &[&str],
        requires_kinds: &[&str],
        uses_kinds: &[&str],
        key: &SigningKey,
    ) {
        let ai = bundle_root.join(crate::AI_DIR);
        std::fs::create_dir_all(&ai).unwrap();
        let yaml = format!(
            "name: {name}\nversion: \"0.1.0\"\nprovides_kinds:\n{}requires_kinds:\n{}uses_kinds:\n{}\n",
            yaml_list(provides_kinds),
            yaml_list(requires_kinds),
            yaml_list(uses_kinds),
        );
        let signed = lillux::signature::sign_content(&yaml, key, "#", None);
        std::fs::write(ai.join("manifest.yaml"), signed).unwrap();
    }

    fn yaml_list(values: &[&str]) -> String {
        if values.is_empty() {
            "  []\n".to_string()
        } else {
            values
                .iter()
                .map(|value| format!("  - {value}\n"))
                .collect()
        }
    }

    fn roots_for(bundle_roots: &[&Path]) -> ResolutionRoots {
        ResolutionRoots {
            ordered: bundle_roots
                .iter()
                .enumerate()
                .map(|(i, root)| ResolutionRoot {
                    space: ItemSpace::Bundle,
                    label: format!("bundle:{i}"),
                    ai_root: root.join(crate::AI_DIR),
                })
                .collect(),
        }
    }

    /// `bin/{triple}/<name>` resolves identically to the canonical
    /// `bin/<host-triple>/<name>` shape, including manifest lookup.
    #[test]
    fn placeholder_triple_resolves_against_host_triple() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let (fp, key) = write_resolver_fixture(&bundle, "demo");

        let resolved = resolve_bundle_binary_ref(
            "bin/{triple}/demo",
            &bundle,
            trusted_key_for(&fp, &key),
            TrustClass::TrustedBundle,
        )
        .expect("placeholder triple ref must resolve");

        let triple = env!("RYEOS_ENGINE_HOST_TRIPLE");
        assert!(resolved
            .absolute_path
            .ends_with(format!("{}/bin/{triple}/demo", crate::AI_DIR)));
        assert_eq!(resolved.signer_fingerprint, fp);
    }

    /// `bin:<name>` and `bin/{triple}/<name>` must agree on the
    /// resolved path so descriptors can pick whichever shape reads
    /// best without changing the verified target.
    #[test]
    fn short_form_and_placeholder_form_resolve_to_same_path() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let (fp, key) = write_resolver_fixture(&bundle, "demo");

        let short = resolve_bundle_binary_ref(
            "bin:demo",
            &bundle,
            trusted_key_for(&fp, &key),
            TrustClass::TrustedBundle,
        )
        .expect("short form must resolve");
        let placeholder = resolve_bundle_binary_ref(
            "bin/{triple}/demo",
            &bundle,
            trusted_key_for(&fp, &key),
            TrustClass::TrustedBundle,
        )
        .expect("placeholder form must resolve");

        assert_eq!(short.absolute_path, placeholder.absolute_path);
        assert_eq!(short.manifest_hash, placeholder.manifest_hash);
    }

    #[test]
    fn qualified_runtime_ref_resolves_target_bundle_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("authoring");
        let target = tmp.path().join("core");
        let (fp, key) = write_resolver_fixture(&target, "ryeos-core-tools");
        write_signed_bundle_manifest(&source, "authoring", &[], &["tool"], &[], &key);
        write_signed_bundle_manifest(&target, "core", &["tool"], &[], &[], &key);
        let roots = roots_for(&[&source, &target]);
        let wrapper = source
            .join(crate::AI_DIR)
            .join("tools/authoring/author-item.yaml");
        std::fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
        std::fs::write(&wrapper, "version: 0.1.0\n").unwrap();

        let resolved = resolve_runtime_binary_command_ref(
            "bin:core/ryeos-core-tools",
            &wrapper,
            &roots,
            &trust_store_for(&fp, &key),
            TrustClass::TrustedBundle,
        )
        .expect("qualified core binary ref should resolve");

        assert!(resolved
            .absolute_path
            .starts_with(target.join(crate::AI_DIR).join("bin")));
        assert_eq!(resolved.signer_fingerprint, fp);
    }

    #[test]
    fn qualified_runtime_ref_requires_source_dependency_on_target() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("authoring");
        let target = tmp.path().join("core");
        let (fp, key) = write_resolver_fixture(&target, "ryeos-core-tools");
        write_signed_bundle_manifest(&source, "authoring", &[], &["knowledge"], &[], &key);
        write_signed_bundle_manifest(&target, "core", &["tool"], &[], &[], &key);
        let roots = roots_for(&[&source, &target]);
        let wrapper = source
            .join(crate::AI_DIR)
            .join("tools/authoring/author-item.yaml");

        let err = resolve_runtime_binary_command_ref(
            "bin:core/ryeos-core-tools",
            &wrapper,
            &roots,
            &trust_store_for(&fp, &key),
            TrustClass::TrustedBundle,
        )
        .unwrap_err();

        assert!(matches!(
            err,
            EngineError::QualifiedBinDependencyMissing {
                source_bundle,
                target_bundle,
            } if source_bundle == "authoring" && target_bundle == "core"
        ));
    }

    #[test]
    fn qualified_runtime_ref_rejects_slashy_bundle_or_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("authoring");
        let (fp, key) = write_resolver_fixture(&source, "local");
        write_signed_bundle_manifest(&source, "authoring", &["tool"], &[], &[], &key);
        let roots = roots_for(&[&source]);
        let wrapper = source
            .join(crate::AI_DIR)
            .join("tools/authoring/author-item.yaml");

        let err = resolve_runtime_binary_command_ref(
            "bin:ryeos/core/ryeos-core-tools",
            &wrapper,
            &roots,
            &trust_store_for(&fp, &key),
            TrustClass::TrustedBundle,
        )
        .unwrap_err();

        assert!(
            matches!(err, EngineError::InvalidBinPrefix { ref detail, .. }
                if detail.contains("bin:<bundle>/<binary>")),
            "expected clear qualified ref parse error, got: {err:?}"
        );
    }

    #[test]
    fn qualified_runtime_ref_rejects_duplicate_target_bundle_names() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("authoring");
        let target_a = tmp.path().join("core-a");
        let target_b = tmp.path().join("core-b");
        let (fp, key) = write_resolver_fixture(&target_a, "ryeos-core-tools");
        write_resolver_fixture(&target_b, "ryeos-core-tools");
        write_signed_bundle_manifest(&source, "authoring", &[], &["tool"], &[], &key);
        write_signed_bundle_manifest(&target_a, "core", &["tool"], &[], &[], &key);
        write_signed_bundle_manifest(&target_b, "core", &["tool"], &[], &[], &key);
        let roots = roots_for(&[&source, &target_a, &target_b]);
        let wrapper = source
            .join(crate::AI_DIR)
            .join("tools/authoring/author-item.yaml");

        let err = resolve_runtime_binary_command_ref(
            "bin:core/ryeos-core-tools",
            &wrapper,
            &roots,
            &trust_store_for(&fp, &key),
            TrustClass::TrustedBundle,
        )
        .unwrap_err();

        assert!(matches!(
            err,
            EngineError::QualifiedBinBundleAmbiguous { bundle, roots }
                if bundle == "core" && roots.len() == 2
        ));
    }

    #[test]
    fn unqualified_runtime_ref_resolves_wrapper_local() {
        // Regression guard for the changed hot path: an unqualified `bin:<name>`
        // still resolves in the wrapper item's own bundle.
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("authoring");
        let (fp, key) = write_resolver_fixture(&source, "local-tool");
        write_signed_bundle_manifest(&source, "authoring", &[], &[], &[], &key);
        let roots = roots_for(&[&source]);
        let wrapper = source.join(crate::AI_DIR).join("tools/authoring/w.yaml");
        std::fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
        std::fs::write(&wrapper, "version: 0.1.0\n").unwrap();

        let resolved = resolve_runtime_binary_command_ref(
            "bin:local-tool",
            &wrapper,
            &roots,
            &trust_store_for(&fp, &key),
            TrustClass::TrustedBundle,
        )
        .expect("unqualified ref resolves in the wrapper's own bundle");
        assert!(resolved
            .absolute_path
            .starts_with(source.join(crate::AI_DIR).join("bin")));
    }

    #[test]
    fn qualified_runtime_ref_rejects_untrusted_target_manifest() {
        // The target bundle's manifest is signed by a key absent from the trust
        // store, so it is not a trusted candidate — no trusted `core` is found.
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("authoring");
        let target = tmp.path().join("core");
        let (fp, key) = write_resolver_fixture(&target, "ryeos-core-tools");
        write_signed_bundle_manifest(&source, "authoring", &[], &["tool"], &[], &key);
        let rogue = SigningKey::from_bytes(&[7u8; 32]);
        write_signed_bundle_manifest(&target, "core", &["tool"], &[], &[], &rogue);
        let roots = roots_for(&[&source, &target]);
        let wrapper = source
            .join(crate::AI_DIR)
            .join("tools/authoring/author-item.yaml");

        let err = resolve_runtime_binary_command_ref(
            "bin:core/ryeos-core-tools",
            &wrapper,
            &roots,
            &trust_store_for(&fp, &key),
            TrustClass::TrustedBundle,
        )
        .unwrap_err();
        // Not a trusted candidate → not found, and the broken registration is
        // surfaced in the diagnostic rather than looking like a genuine absence.
        match err {
            EngineError::QualifiedBinBundleNotFound {
                bundle, skipped, ..
            } => {
                assert_eq!(bundle, "core");
                assert!(
                    skipped.iter().any(|s| s.contains("invalid")),
                    "expected the skipped invalid manifest to be reported, got: {skipped:?}"
                );
            }
            other => panic!("expected QualifiedBinBundleNotFound, got: {other:?}"),
        }
    }

    #[test]
    fn qualified_ref_tolerates_full_signed_manifest_fields() {
        // Guards the intentional lenient projection: a real generated manifest
        // carries `description` and a `runtime_authority` block (and may gain
        // more fields). The minimal projection must ignore them, not reject the
        // manifest — so qualified resolution keeps working against real bundles.
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("authoring");
        let target = tmp.path().join("core");
        let (fp, key) = write_resolver_fixture(&target, "ryeos-core-tools");
        write_full_signed_manifest(
            &source,
            "authoring",
            &[],
            &["tool"],
            &key,
            "runtime_authority:\n  item_authoring:\n    - kind: knowledge\n      namespace: runtime-authored/*\n",
        );
        write_full_signed_manifest(
            &target,
            "core",
            &["tool"],
            &[],
            &key,
            "runtime_authority: {}\n",
        );
        let roots = roots_for(&[&source, &target]);
        let wrapper = source
            .join(crate::AI_DIR)
            .join("tools/authoring/author-item.yaml");

        let resolved = resolve_runtime_binary_command_ref(
            "bin:core/ryeos-core-tools",
            &wrapper,
            &roots,
            &trust_store_for(&fp, &key),
            TrustClass::TrustedBundle,
        )
        .expect("full-shape signed manifests must still resolve");
        assert!(resolved
            .absolute_path
            .starts_with(target.join(crate::AI_DIR).join("bin")));
    }

    /// Like `write_signed_bundle_manifest` but includes `description` and a
    /// caller-supplied trailing block (e.g. `runtime_authority`), mirroring a
    /// generated `.ai/manifest.yaml`.
    fn write_full_signed_manifest(
        bundle_root: &Path,
        name: &str,
        provides_kinds: &[&str],
        requires_kinds: &[&str],
        key: &SigningKey,
        extra_yaml: &str,
    ) {
        let ai = bundle_root.join(crate::AI_DIR);
        std::fs::create_dir_all(&ai).unwrap();
        let yaml = format!(
            "name: {name}\nversion: \"0.1.0\"\ndescription: \"full manifest\"\nprovides_kinds:\n{}requires_kinds:\n{}uses_kinds:\n{}{extra_yaml}",
            yaml_list(provides_kinds),
            yaml_list(requires_kinds),
            yaml_list(&[]),
        );
        let signed = lillux::signature::sign_content(&yaml, key, "#", None);
        std::fs::write(ai.join("manifest.yaml"), signed).unwrap();
    }

    #[test]
    fn unsigned_forged_manifest_with_claimed_trusted_fingerprint_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let (fp, key) = write_resolver_fixture(&bundle, "demo");
        let triple = env!("RYEOS_ENGINE_HOST_TRIPLE");
        let ai = bundle.join(crate::AI_DIR);
        let bin_path = ai.join("bin").join(triple).join("demo");

        let forged_bytes = b"forged-binary\n";
        std::fs::write(&bin_path, forged_bytes).unwrap();
        let forged_hash = lillux::sha256_hex(forged_bytes);
        let forged_item_source = serde_json::json!({
            "kind": "item_source",
            "item_ref": format!("bin/{triple}/demo"),
            "content_blob_hash": forged_hash,
            "integrity": format!("sha256:{forged_hash}"),
            "signature_info": { "fingerprint": fp },
            "mode": 0o755,
        });
        let cas = lillux::cas::CasStore::new(ai.join("objects"));
        let forged_item_source_hash = cas.store_object(&forged_item_source).unwrap();
        let forged_manifest = serde_json::json!({
            "kind": "source_manifest",
            "item_source_hashes": {
                format!("bin/{triple}/demo"): forged_item_source_hash
            }
        });
        let forged_manifest_hash = cas.store_object(&forged_manifest).unwrap();
        std::fs::write(
            ai.join("refs").join("bundles").join("manifest"),
            forged_manifest_hash,
        )
        .unwrap();

        let err = resolve_bundle_binary_ref(
            "bin:demo",
            &bundle,
            trusted_key_for(&fp, &key),
            TrustClass::TrustedBundle,
        )
        .unwrap_err();

        assert!(
            matches!(err, EngineError::BinManifestInvalid { ref reason, .. }
                if reason.contains("signature")),
            "expected signed-manifest-ref rejection, got: {err:?}"
        );
    }

    // ── Phase 1A new tests ─────────────────────────────────────────

    #[test]
    fn short_form_rejects_traversal() {
        let err = resolve_bundle_binary_ref(
            "bin:../demo",
            Path::new("/tmp/bundle"),
            |_| None,
            TrustClass::TrustedBundle,
        )
        .unwrap_err();
        // `..` contains no `/` in the `bin:../demo` name (`../demo` does
        // contain `/` though), so the rejection depends on order: the
        // slash check fires first. Either slash or path traversal is fine.
        assert!(
            matches!(err, EngineError::InvalidBinPrefix { ref detail, .. }
                if detail.contains("path traversal") || detail.contains("slashes")),
            "expected path traversal or slash rejection, got: {err:?}"
        );
    }

    #[test]
    fn short_form_rejects_slash() {
        let err = resolve_bundle_binary_ref(
            "bin:subdir/demo",
            Path::new("/tmp/bundle"),
            |_| None,
            TrustClass::TrustedBundle,
        )
        .unwrap_err();
        assert!(
            matches!(err, EngineError::InvalidBinPrefix { ref detail, .. } if detail.contains("slashes")),
            "expected slash rejection, got: {err:?}"
        );
    }

    #[test]
    fn path_form_rejects_empty_name() {
        let triple = env!("RYEOS_ENGINE_HOST_TRIPLE");
        let err = resolve_bundle_binary_ref(
            &format!("bin/{triple}/"),
            Path::new("/tmp/bundle"),
            |_| None,
            TrustClass::TrustedBundle,
        )
        .unwrap_err();
        // splitn(4, '/') on "bin/x86_64-unknown-linux-gnu/" gives
        // ["bin", "x86_64...", ""], so name is empty.
        assert!(
            matches!(err, EngineError::InvalidBinPrefix { ref detail, .. } if detail.contains("no binary name")),
            "expected empty name rejection, got: {err:?}"
        );
    }

    #[test]
    fn symlink_escaping_bundle_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let triple = env!("RYEOS_ENGINE_HOST_TRIPLE");
        let bin_dir = bundle.join(crate::AI_DIR).join("bin").join(triple);
        std::fs::create_dir_all(&bin_dir).unwrap();

        // Place the real binary outside the bundle.
        let outside = tmp.path().join("outside-binary");
        std::fs::write(&outside, b"evil").unwrap();

        // Create a symlink inside the bin dir pointing outside.
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&outside, bin_dir.join("escaped")).unwrap();
        }

        let err =
            resolve_bundle_binary_ref("bin:escaped", &bundle, |_| None, TrustClass::TrustedBundle)
                .unwrap_err();

        // The symlink resolves outside the bundle bin dir.
        // Either BinOutsideBundle or BinNotRegularFile is acceptable
        // depending on whether the outside target is a regular file.
        // Since the outside file IS a regular file, we expect
        // BinOutsideBundle.
        assert!(
            matches!(err, EngineError::BinOutsideBundle { .. }),
            "expected BinOutsideBundle for symlink escaping bundle, got: {err:?}"
        );
    }

    #[test]
    fn non_regular_file_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let triple = env!("RYEOS_ENGINE_HOST_TRIPLE");
        let bin_dir = bundle.join(crate::AI_DIR).join("bin").join(triple);
        std::fs::create_dir_all(&bin_dir).unwrap();

        // Create a directory where a binary would go.
        let dir_path = bin_dir.join("not-a-file");
        std::fs::create_dir(&dir_path).unwrap();

        let err = resolve_bundle_binary_ref(
            "bin:not-a-file",
            &bundle,
            |_| None,
            TrustClass::TrustedBundle,
        )
        .unwrap_err();

        // A directory is not a regular file.
        assert!(
            matches!(err, EngineError::BinNotRegularFile { .. })
                || matches!(err, EngineError::BinNotFound { .. }),
            "expected BinNotRegularFile or BinNotFound for directory, got: {err:?}"
        );
    }
}
