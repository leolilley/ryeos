//! Cross-crate test helpers for code paths that exercise the live
//! parser/composer dispatcher. The dispatcher resolves handler refs
//! through `HandlerRegistry`, which means tests cannot use
//! `HandlerRegistry::empty()` and still expect parsing to succeed —
//! they need the real handler binaries shipped in `ryeos-bundles/core/`
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

use lillux::crypto::{DecodePublicKey, VerifyingKey};
use base64::Engine as _;

use crate::handlers::HandlerRegistry;
use crate::parsers::descriptor::ParserDescriptor;
use crate::parsers::dispatcher::ParserDispatcher;
use crate::parsers::ParserRegistry;
use crate::resolution::TrustClass;
use crate::trust::{TrustStore, TrustedSigner};

/// The repo workspace root (parent of `ryeos-engine/`).
pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("ryeos-engine has parent dir")
        .to_path_buf()
}

/// Absolute path to the canonical core bundle shipped in this repo.
pub fn core_bundle_root() -> PathBuf {
    workspace_root().join("ryeos-bundles/core")
}

/// Absolute path to the canonical standard bundle shipped in this repo.
pub fn standard_bundle_root() -> PathBuf {
    workspace_root().join("ryeos-bundles/standard")
}

/// Platform-author verifying key (`09674c8...`) that signs every
/// artifact shipped in `ryeos-bundles/core/` and `ryeos-bundles/standard/`
/// in the dev tree — kind schemas, handler descriptor YAMLs, and the
/// binary `item_source.json` sidecars all anchor to this single key.
/// Public-key bytes are pinned here so the engine doesn't have to read
/// `~/.ai/config/keys/signing/private_key.pem` from operator state.
///
/// PEM source: `ryeosd/tests/fixtures/trusted_signers/<fp>.toml`.
pub fn platform_author_verifying_key() -> VerifyingKey {
    const PEM: &str = "-----BEGIN PUBLIC KEY-----\n\
                       MCowBQYDK2VwAyEAARaVpY8d1iAKhKuLuDbEPZIpdRmb10H6QkuuXqNpZA4=\n\
                       -----END PUBLIC KEY-----\n";
    VerifyingKey::from_public_key_pem(PEM)
        .expect("platform-author public key PEM must decode")
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

/// Load the live `HandlerRegistry` from both `ryeos-bundles/core/` and
/// `ryeos-bundles/standard/` using the standard test trust store.
/// Requires that the handler binaries have been built and the bundle
/// manifest signed (e.g. via `populate-bundles.sh`).
///
/// Panics on failure so test bodies stay terse — the registry MUST
/// load for tests that drive the parser/composer dispatcher.
pub fn load_live_handler_registry() -> Arc<HandlerRegistry> {
    let core_root = core_bundle_root();
    let std_root = standard_bundle_root();
    let trust_store = live_trust_store();
    let tagged_roots: Vec<(PathBuf, TrustClass)> = vec![
        (core_root, TrustClass::TrustedSystem),
        (std_root, TrustClass::TrustedSystem),
    ];
    let registry = HandlerRegistry::load_base(&tagged_roots, &trust_store)
        .expect("live HandlerRegistry must load from ryeos-bundles/{core,standard}/");
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
