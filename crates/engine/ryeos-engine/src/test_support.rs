//! Cross-crate test helpers for code paths that exercise the live
//! parser/composer dispatcher. The dispatcher resolves handler refs
//! through `HandlerRegistry`, which means tests cannot use
//! `HandlerRegistry::empty()` and still expect parsing to succeed —
//! they need the real handler binaries shipped in `bundles/core/`
//! plus a trust store containing the platform-author key, which signs
//! both the descriptor YAML and the binary's item_source.json sidecars
//! in the dev tree.
//!
//! These helpers centralize that wiring so engine, daemon, and tools
//! tests don't keep re-deriving it inline. Build them by enabling the
//! `test-support` feature in `[dev-dependencies]`:
//!
//! ```toml
//! ryeos-engine = { path = "...", features = ["test-support"] }
//! ```
//!
//! Inside the engine crate the module is also visible under `#[cfg(test)]`
//! so unit tests can use it without the feature flag.

use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use lillux::crypto::{DecodePublicKey, VerifyingKey};

use crate::handlers::HandlerRegistry;
use crate::kind_registry::KindRegistry;
use crate::parsers::ParserRegistry;
use crate::parsers::descriptor::ParserDescriptor;
use crate::parsers::dispatcher::ParserDispatcher;
use crate::resolution::TrustClass;
use crate::trust::{TrustStore, TrustedSigner};

/// The repo workspace root (parent of `crates/engine/ryeos-engine/`).
pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("bundles").is_dir())
        .expect("workspace root with bundles/ directory")
        .to_path_buf()
}

/// Absolute path to the canonical core bundle shipped in this repo.
pub fn core_bundle_root() -> PathBuf {
    workspace_root().join("bundles/core")
}

/// Absolute path to the knowledge bundle shipped in this repo.
pub fn knowledge_bundle_root() -> PathBuf {
    workspace_root().join("bundles/knowledge")
}

/// Absolute path to the graph bundle shipped in this repo.
pub fn graph_bundle_root() -> PathBuf {
    workspace_root().join("bundles/graph")
}

/// Absolute path to the directive bundle shipped in this repo.
pub fn directive_bundle_root() -> PathBuf {
    workspace_root().join("bundles/directive")
}

/// Absolute path to the ryeos-docs bundle shipped in this repo.
pub fn docs_bundle_root() -> PathBuf {
    workspace_root().join("bundles/ryeos-docs")
}

/// The workflow bundle roots (knowledge, directive, graph) that sit on
/// top of core, in install order.
pub fn workflow_bundle_roots() -> [PathBuf; 3] {
    [
        knowledge_bundle_root(),
        directive_bundle_root(),
        graph_bundle_root(),
    ]
}

/// Platform-author verifying key (`09674c8...`) that signs every
/// artifact shipped in `bundles/core/` and the workflow bundles
/// in the dev tree — kind schemas, handler descriptor YAMLs, and the
/// binary `item_source.json` sidecars all anchor to this single key.
/// Public-key bytes are pinned here so the engine doesn't have to read
/// `<app_root>/.ai/config/keys/signing/private_key.pem` from operator state.
///
/// PEM source: `crates/bin/daemon/tests/fixtures/trusted_signers/<fp>.toml`.
pub fn platform_author_verifying_key() -> VerifyingKey {
    const PEM: &str = "-----BEGIN PUBLIC KEY-----\n\
                       MCowBQYDK2VwAyEAARaVpY8d1iAKhKuLuDbEPZIpdRmb10H6QkuuXqNpZA4=\n\
                       -----END PUBLIC KEY-----\n";
    VerifyingKey::from_public_key_pem(PEM).expect("platform-author public key PEM must decode")
}

/// Dev publisher verifying key (`741a8bc6...`) — the key used by
/// `populate-bundles.sh` (via `.dev-keys/PUBLISHER_DEV.pem`) to sign
/// dev-tree bundles. Tests must trust this key because CI and local dev
/// workflows publish with it.
pub fn dev_publisher_verifying_key() -> VerifyingKey {
    // ed25519:sDKyQ9rFxIduNjGtXq6aTrLlAg39177NzCT1+YYqpRk=
    const B64: &str = "sDKyQ9rFxIduNjGtXq6aTrLlAg39177NzCT1+YYqpRk=";
    let bytes: [u8; 32] = base64::engine::general_purpose::STANDARD
        .decode(B64)
        .expect("dev key base64 decode")
        .try_into()
        .expect("dev key must be 32 bytes");
    VerifyingKey::from_bytes(&bytes).expect("dev key must be valid Ed25519")
}

/// Trust store for verifying live core-bundle artifacts. Includes both
/// the platform-author key (`09674c8...`) and the dev publisher key
/// (`741a8bc6...`) so tests work with both official and dev-signed bundles.
pub fn live_trust_store() -> TrustStore {
    let platform_vk = platform_author_verifying_key();
    let platform_fp = lillux::signature::compute_fingerprint(&platform_vk);

    let dev_vk = dev_publisher_verifying_key();
    let dev_fp = lillux::signature::compute_fingerprint(&dev_vk);

    TrustStore::from_signers(vec![
        TrustedSigner {
            fingerprint: platform_fp,
            verifying_key: platform_vk,
            label: Some("test-support: platform author".into()),
        },
        TrustedSigner {
            fingerprint: dev_fp,
            verifying_key: dev_vk,
            label: Some("test-support: dev publisher".into()),
        },
    ])
}

/// Load the signed kind schemas from the live core, workflow and ryeos-ui bundles.
pub fn load_live_kind_registry() -> KindRegistry {
    let trust_store = live_trust_store();
    KindRegistry::load_base(
        &[
            core_bundle_root()
                .join(crate::AI_DIR)
                .join(crate::KIND_SCHEMAS_DIR),
            knowledge_bundle_root()
                .join(crate::AI_DIR)
                .join(crate::KIND_SCHEMAS_DIR),
            directive_bundle_root()
                .join(crate::AI_DIR)
                .join(crate::KIND_SCHEMAS_DIR),
            graph_bundle_root()
                .join(crate::AI_DIR)
                .join(crate::KIND_SCHEMAS_DIR),
            workspace_root()
                .join("bundles/ryeos-ui")
                .join(crate::AI_DIR)
                .join(crate::KIND_SCHEMAS_DIR),
        ],
        &trust_store,
    )
    .expect("live kind registry must load from bundles/{core,knowledge,directive,graph,ryeos-ui}")
}

/// Load the live `HandlerRegistry` from `bundles/core/`,
/// `bundles/graph/` and `bundles/directive/` using the standard test trust store.
/// Requires that the handler binaries have been built and the bundle
/// manifest signed (e.g. via `populate-bundles.sh`).
///
/// Panics on failure so test bodies stay terse — the registry MUST
/// load for tests that drive the parser/composer dispatcher.
pub fn load_live_handler_registry() -> Arc<HandlerRegistry> {
    let core_root = core_bundle_root();
    let trust_store = live_trust_store();
    let tagged_roots: Vec<(PathBuf, TrustClass)> = vec![
        (core_root, TrustClass::TrustedBundle),
        (graph_bundle_root(), TrustClass::TrustedBundle),
        (directive_bundle_root(), TrustClass::TrustedBundle),
    ];
    let registry = HandlerRegistry::load_base_for_authoring(&tagged_roots, &trust_store)
        .unwrap_or_else(|err| {
        let detail = format!("{err:?}");
        if detail.contains("not in trust store") {
            panic!(
                "PRECONDITION FAILED: bundle bin trees are signed with a key this test \
                 harness does not trust.\n\n  {detail}\n\n\
                 The harness trusts the platform-author and dev-publisher (741a8bc6…) keys \
                 only. This usually means bundles/{{core,graph,directive}}/.ai/bin was last signed \
                 with a different (e.g. operator) key. Re-sign with the dev key:\n\n  \
                 ./scripts/populate-bundles.sh --key .dev-keys/PUBLISHER_DEV.pem --owner \"RyeOS Development\"\n\n\
                 then re-run. Every live-registry test fails with this same message until then."
            );
        }
        panic!("live HandlerRegistry must load from bundles/{{core,graph,directive}}/: {detail}");
        });
    Arc::new(registry)
}

/// Build a `ParserDispatcher` whose handler resolutions go through the
/// live registry while the parser-descriptor table is supplied by the
/// caller. Lets tests assemble bespoke parser tables without
/// re-deriving the trust + registry plumbing.
pub fn build_parser_dispatcher_from_roots<I>(parser_descriptors: I) -> ParserDispatcher
where
    I: IntoIterator<Item = (String, ParserDescriptor)>,
{
    ParserDispatcher::new(
        ParserRegistry::from_entries(parser_descriptors),
        load_live_handler_registry(),
    )
}

// ── Signer registry fixtures ────────────────────────────────────────

/// Pin `verifying_key` in the node signer registry under `app_root`, the
/// way the trust operations do, so fixtures exercise the real loader.
///
/// The node key is read from `<app_root>/.ai/node/identity/private_key.pem`;
/// when the node has no identity yet, one is created along
/// with a minimal public identity document. Returns the pinned fingerprint.
pub fn pin_signer(
    app_root: &std::path::Path,
    verifying_key: &VerifyingKey,
    label: &str,
    role: crate::trust::SignerRole,
    scope: crate::trust::SignerScope,
) -> String {
    let identity_dir = app_root.join(crate::AI_DIR).join("node").join("identity");
    std::fs::create_dir_all(&identity_dir).expect("create fixture node identity dir");
    let key_path = identity_dir.join("private_key.pem");
    let node_key = if key_path.exists() {
        lillux::crypto::load_signing_key(&key_path).expect("load fixture node key")
    } else {
        lillux::crypto::create_signing_key(&key_path).expect("create fixture node key")
    };
    let public_path = crate::trust::node_public_identity_path(app_root);
    if !public_path.exists() {
        let node_vk = node_key.verifying_key();
        let doc = serde_json::json!({
            "kind": "identity/v1",
            "principal_id": format!("fp:{}", crate::trust::compute_fingerprint(&node_vk)),
            "signing_key": format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(node_vk.as_bytes())
            ),
        });
        std::fs::write(&public_path, doc.to_string()).expect("write fixture public identity");
    }
    pin_signer_with_node_key(app_root, &node_key, verifying_key, label, role, scope)
}

/// Like [`pin_signer`] for fixtures that already own the node signing key.
pub fn pin_signer_with_node_key(
    app_root: &std::path::Path,
    node_key: &lillux::crypto::SigningKey,
    verifying_key: &VerifyingKey,
    label: &str,
    role: crate::trust::SignerRole,
    scope: crate::trust::SignerScope,
) -> String {
    let mut record = crate::trust::SignerRecord::new(
        verifying_key,
        label,
        role,
        scope,
        crate::trust::SignerStatus::Active,
    );
    record.event = lillux::cas::sha256_hex(format!("fixture:{}", record.fingerprint).as_bytes());
    let dir = crate::trust::signers_dir(app_root);
    std::fs::create_dir_all(&dir).expect("create fixture signer registry dir");
    std::fs::write(
        dir.join(record.file_name()),
        record
            .render_signed(node_key)
            .expect("render fixture signer record"),
    )
    .expect("write fixture signer record");
    record.fingerprint
}
