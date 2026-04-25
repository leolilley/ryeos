//! Daemon-side kind composers — produce `KindComposedView` from the
//! resolved root + extends chain so the envelope ships a single composed
//! view that both launcher (policy) and runtime (prompt) consume.
//!
//! Data-driven, mirroring `parsers::ParserDispatcher`:
//!
//!   * Native composer handlers register at boot under string IDs in
//!     `NativeComposerHandlerRegistry::with_builtins()`.
//!   * Kind schemas declare `composer: <handler-id>` (REQUIRED on
//!     every kind — there is no silent "no composer" path) and an
//!     optional `composer_config:` blob the handler validates and
//!     consumes.
//!   * `ComposerRegistry::from_kinds` walks loaded kind schemas and
//!     binds each kind name to its declared handler PLUS the
//!     handler-validated config blob.
//!
//! The engine code never names a kind in Rust string literals — the
//! kind→composer mapping is entirely data.

pub mod handlers;

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

pub use handlers::{
    ExtendsChainComposer, IdentityComposer, KindComposer, NativeComposerHandlerRegistry,
};

use crate::error::EngineError;
use crate::kind_registry::KindRegistry;

/// One bound composer entry: the handler plus the kind-schema-supplied
/// `composer_config` it was bound with at boot. The config has already
/// been run through `handler.validate_config` by `from_kinds` /
/// `boot_validation`.
struct BoundComposer {
    handler: Arc<dyn KindComposer>,
    config: Value,
}

/// Registry of kind composers, one per kind name.
///
/// Built data-drivenly via `from_kinds`: each kind schema declares a
/// composer handler ID + an optional `composer_config`, and we look
/// the ID up in the supplied `NativeComposerHandlerRegistry` to bind
/// kind→(handler, config). There is no `with_defaults()` constructor
/// — the kind schemas are the only source of truth for which composer
/// handles which kind.
pub struct ComposerRegistry {
    composers: HashMap<String, BoundComposer>,
}

impl ComposerRegistry {
    pub fn new() -> Self {
        Self {
            composers: HashMap::new(),
        }
    }

    /// Build by walking loaded kind schemas: for each kind, look up
    /// its declared composer handler in `native`, run the handler's
    /// `validate_config` against the kind's `composer_config`, and
    /// bind the kind name to (handler, config).
    ///
    /// Symmetric in shape to how `ParserDispatcher` is built from
    /// descriptors + native handlers. Fails loud if any kind
    /// references an unregistered handler OR supplies a config the
    /// handler rejects — `boot_validation` does both checks
    /// independently and aggregates issues into a `Vec<BootIssue>`,
    /// but `from_kinds` itself MUST refuse to construct an
    /// inconsistent registry so misuse (e.g. calling without running
    /// boot validation) still surfaces a structured error.
    pub fn from_kinds(
        kinds: &KindRegistry,
        native: &NativeComposerHandlerRegistry,
    ) -> Result<Self, EngineError> {
        let mut composers: HashMap<String, BoundComposer> = HashMap::new();
        let mut missing: Vec<(String, String)> = Vec::new();
        let mut bad_configs: Vec<(String, String, String)> = Vec::new();

        let mut kind_names: Vec<&str> = kinds.kinds().collect();
        kind_names.sort();
        for kind in kind_names {
            let schema = match kinds.get(kind) {
                Some(s) => s,
                None => continue,
            };
            match native.get(&schema.composer) {
                Some(handler) => match handler.validate_config(&schema.composer_config) {
                    Ok(()) => {
                        composers.insert(
                            kind.to_owned(),
                            BoundComposer {
                                handler,
                                config: schema.composer_config.clone(),
                            },
                        );
                    }
                    Err(reason) => {
                        bad_configs.push((kind.to_owned(), schema.composer.clone(), reason));
                    }
                },
                None => missing.push((kind.to_owned(), schema.composer.clone())),
            }
        }

        if !missing.is_empty() || !bad_configs.is_empty() {
            let mut detail = String::new();
            for (k, h) in &missing {
                detail.push_str(&format!(
                    "\n  - kind `{k}` declares composer `{h}` which is not registered in NativeComposerHandlerRegistry"
                ));
            }
            for (k, h, r) in &bad_configs {
                detail.push_str(&format!(
                    "\n  - kind `{k}` composer `{h}` rejected composer_config: {r}"
                ));
            }
            return Err(EngineError::SchemaLoaderError {
                reason: format!(
                    "ComposerRegistry::from_kinds: {} faulty kind binding(s):{detail}",
                    missing.len() + bad_configs.len()
                ),
            });
        }

        Ok(Self { composers })
    }

    /// Test/escape-hatch registration. Production code goes through
    /// `from_kinds`; this exists so test setups can install or
    /// override a handler for a synthetic kind. Caller is responsible
    /// for passing a config the handler accepts.
    pub fn register(&mut self, kind: &str, composer: Arc<dyn KindComposer>, config: Value) {
        self.composers.insert(
            kind.to_string(),
            BoundComposer {
                handler: composer,
                config,
            },
        );
    }

    /// Look up the bound (handler, config) pair for a kind.
    pub fn get(&self, kind: &str) -> Option<(&dyn KindComposer, &Value)> {
        self.composers
            .get(kind)
            .map(|b| (b.handler.as_ref(), &b.config))
    }

    /// Iterate over the kinds for which a composer is registered.
    pub fn kinds(&self) -> impl Iterator<Item = &str> {
        self.composers.keys().map(|s| s.as_str())
    }
}

impl std::fmt::Debug for ComposerRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComposerRegistry")
            .field("kinds", &self.composers.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kind_registry::KindRegistry;
    use crate::trust::{compute_fingerprint, TrustStore, TrustedSigner};
    use lillux::crypto::SigningKey;
    use std::fs;
    use std::path::PathBuf;

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rye_composer_reg_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[5u8; 32])
    }

    fn trust_store(sk: &SigningKey) -> TrustStore {
        let vk = sk.verifying_key();
        TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: compute_fingerprint(&vk),
            verifying_key: vk,
            label: None,
        }])
    }

    fn write_kind(
        root: &std::path::Path,
        kind: &str,
        composer: &str,
        composer_config_yaml: Option<&str>,
        sk: &SigningKey,
    ) {
        let cfg_block = composer_config_yaml
            .map(|c| format!("composer_config:\n{c}"))
            .unwrap_or_default();
        let yaml = format!(
            "\
location:
  directory: {kind}s
formats:
  - extensions: [\".md\"]
    parser: parser:rye/core/markdown/extends_chain
    signature:
      prefix: \"<!--\"
      suffix: \"-->\"
composer: {composer}
{cfg_block}composed_value_contract:
  root_type: mapping
  required: {{}}
"
        );
        let dir = root.join(kind);
        fs::create_dir_all(&dir).unwrap();
        let signed = lillux::signature::sign_content(&yaml, sk, "#", None);
        fs::write(dir.join(format!("{kind}.kind-schema.yaml")), signed).unwrap();
    }

    // Synthetic kind names — engine code under
    // `ryeos-engine/src/composers/` contains zero string literals
    // naming a real kind.

    #[test]
    fn from_kinds_binds_each_kind_to_its_declared_handler() {
        let root = tempdir();
        let sk = signing_key();
        let ts = trust_store(&sk);
        let cfg = "  extends_field: ext\n  fields: []\n";
        write_kind(&root, "alpha", "rye/core/extends_chain", Some(cfg), &sk);
        write_kind(&root, "beta", "rye/core/identity", None, &sk);
        let kinds = KindRegistry::load_base(&[root], &ts).unwrap();

        let native = NativeComposerHandlerRegistry::with_builtins();
        let reg = ComposerRegistry::from_kinds(&kinds, &native).unwrap();
        assert!(reg.get("alpha").is_some());
        assert!(reg.get("beta").is_some());
        let mut names: Vec<&str> = reg.kinds().collect();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn from_kinds_fails_loud_for_unregistered_handler() {
        let root = tempdir();
        let sk = signing_key();
        let ts = trust_store(&sk);
        write_kind(&root, "alpha", "totally/made/up", None, &sk);
        let kinds = KindRegistry::load_base(&[root], &ts).unwrap();

        let native = NativeComposerHandlerRegistry::with_builtins();
        let err = ComposerRegistry::from_kinds(&kinds, &native).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("totally/made/up") && msg.contains("alpha"),
            "expected unknown-handler error naming both kind and handler, got: {msg}"
        );
    }

    #[test]
    fn from_kinds_fails_loud_for_invalid_composer_config() {
        let root = tempdir();
        let sk = signing_key();
        let ts = trust_store(&sk);
        // identity composer rejects any non-empty config.
        let cfg = "  not_allowed: 1\n";
        write_kind(&root, "alpha", "rye/core/identity", Some(cfg), &sk);
        let kinds = KindRegistry::load_base(&[root], &ts).unwrap();

        let native = NativeComposerHandlerRegistry::with_builtins();
        let err = ComposerRegistry::from_kinds(&kinds, &native).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("alpha") && msg.contains("rejected composer_config"),
            "expected rejected-config error, got: {msg}"
        );
    }
}
