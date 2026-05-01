//! Daemon-side kind composers — produce `KindComposedView` from the
//! resolved root + extends chain so the envelope ships a single composed
//! view that both launcher (policy) and runtime (prompt) consume.
//!
//! Subprocess-based, mirroring `parsers::ParserDispatcher`:
//!
//!   * Composer handler binaries register at boot via the
//!     `HandlerRegistry` (see `handlers::registry`). Their canonical
//!     refs are `handler:rye/core/<name>` (e.g.
//!     `handler:rye/core/extends-chain`,
//!     `handler:rye/core/identity`, `handler:rye/core/graph-permissions`).
//!   * Kind schemas declare `composer: handler:rye/core/<name>`
//!     (REQUIRED on every kind — there is no silent "no composer"
//!     path) and an optional `composer_config:` blob the handler
//!     validates and consumes.
//!   * `ComposerRegistry::from_kinds` walks loaded kind schemas and
//!     binds each kind name to its declared handler PLUS the
//!     handler-validated config blob. The `compose()` call spawns
//!     the handler binary as a subprocess (env-cleared via
//!     `lillux::exec::lib_run`) and decodes the wire response into a
//!     `KindComposedView`.
//!
//! The engine code never names a kind in Rust string literals — the
//! kind→composer mapping is entirely data.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use ryeos_handler_protocol::{
    ComposeInput, ComposeItemContext, ComposeRequest, ComposeSuccess, HandlerRequest,
    HandlerResponse, ResolutionStepNameWire, TrustClassWire,
};
use serde_json::Value;

use crate::error::EngineError;
use crate::handlers::subprocess::run_handler_subprocess;
use crate::handlers::{HandlerRegistry, HandlerServes, VerifiedHandler};
use crate::kind_registry::KindRegistry;
use crate::resolution::{
    KindComposedView, ResolutionError, ResolutionStepName, ResolvedAncestor, TrustClass,
};

const COMPOSE_TIMEOUT: Duration = Duration::from_secs(30);

/// One bound composer entry: the verified handler plus the kind
/// schema's `composer_config` blob it was bound with at boot.
///
/// The handler is held by `Arc<VerifiedHandler>` so the dispatcher
/// can hand a cheap reference to long-lived contexts that walk many
/// kinds. Config validation is performed at boot time by
/// `boot_validation` via a subprocess call — `from_kinds` does not
/// validate the config itself (parser symmetry: the parser dispatcher
/// also defers config validation to boot).
#[derive(Debug, Clone)]
struct BoundComposer {
    handler: Arc<VerifiedHandler>,
    config: Value,
}

/// Registry of kind composers, one per kind name.
///
/// Built data-drivenly via `from_kinds`: each kind schema declares a
/// composer handler ref (`handler:rye/core/<name>`) + an optional
/// `composer_config`, and we look the ref up in the supplied
/// `HandlerRegistry` to bind kind→(handler, config).
///
/// Symmetric in shape to how `ParserDispatcher` is built from
/// descriptors + handlers. There is no `with_defaults()` constructor
/// — the kind schemas are the only source of truth for which composer
/// handles which kind.
#[derive(Debug, Clone)]
pub struct ComposerRegistry {
    composers: HashMap<String, BoundComposer>,
}

impl ComposerRegistry {
    /// Empty registry. Mostly useful in tests where no composer dispatch
    /// is exercised; production paths build via `from_kinds`.
    pub fn new() -> Self {
        Self {
            composers: HashMap::new(),
        }
    }

    /// Build by walking loaded kind schemas: for each kind, look up
    /// its declared composer handler ref in `handlers` (verifying
    /// `serves: composer`), and bind the kind name to (handler, config).
    ///
    /// Symmetric in shape to how `ParserDispatcher` is built from
    /// descriptors + the handler registry. Fails loud if any kind
    /// references an unknown handler ref or one that does not serve
    /// `composer`.
    ///
    /// `composer_config` validation happens at boot via subprocess
    /// (see `boot_validation::validate_boot`), not here — `from_kinds`
    /// only checks the structural binding so engine startup can
    /// distinguish wiring errors (this fn) from per-handler validation
    /// errors (boot validation aggregates those into `BootIssue`s).
    pub fn from_kinds(
        kinds: &KindRegistry,
        handlers: &Arc<HandlerRegistry>,
    ) -> Result<Self, EngineError> {
        let mut composers: HashMap<String, BoundComposer> = HashMap::new();
        let mut missing: Vec<(String, String)> = Vec::new();
        let mut wrong_serves: Vec<(String, String)> = Vec::new();

        let mut kind_names: Vec<&str> = kinds.kinds().collect();
        kind_names.sort();
        for kind in kind_names {
            let schema = match kinds.get(kind) {
                Some(s) => s,
                None => continue,
            };
            match handlers.ensure_serves(&schema.composer, HandlerServes::Composer) {
                Ok(handler) => {
                    composers.insert(
                        kind.to_owned(),
                        BoundComposer {
                            handler: Arc::new(handler.clone()),
                            config: schema.composer_config.clone(),
                        },
                    );
                }
                Err(crate::handlers::HandlerError::NotRegistered { .. }) => {
                    missing.push((kind.to_owned(), schema.composer.clone()));
                }
                Err(crate::handlers::HandlerError::ServesMismatch { .. }) => {
                    wrong_serves.push((kind.to_owned(), schema.composer.clone()));
                }
                Err(other) => {
                    return Err(EngineError::SchemaLoaderError {
                        reason: format!(
                            "ComposerRegistry::from_kinds: kind `{kind}` composer ref \
                             `{}` failed handler-registry lookup: {other}",
                            schema.composer
                        ),
                    });
                }
            }
        }

        if !missing.is_empty() || !wrong_serves.is_empty() {
            let mut detail = String::new();
            for (k, h) in &missing {
                detail.push_str(&format!(
                    "\n  - kind `{k}` declares composer `{h}` which is not registered in HandlerRegistry"
                ));
            }
            for (k, h) in &wrong_serves {
                detail.push_str(&format!(
                    "\n  - kind `{k}` composer `{h}` is registered but does not serve `composer`"
                ));
            }
            return Err(EngineError::SchemaLoaderError {
                reason: format!(
                    "ComposerRegistry::from_kinds: {} faulty kind binding(s):{detail}",
                    missing.len() + wrong_serves.len()
                ),
            });
        }

        Ok(Self { composers })
    }

    /// Test/escape-hatch registration. Production code goes through
    /// `from_kinds`; this exists so test setups can install or
    /// override a composer for a synthetic kind.
    pub fn register(&mut self, kind: &str, handler: Arc<VerifiedHandler>, config: Value) {
        self.composers.insert(
            kind.to_string(),
            BoundComposer { handler, config },
        );
    }

    /// True iff a composer is bound for `kind`.
    pub fn contains(&self, kind: &str) -> bool {
        self.composers.contains_key(kind)
    }

    /// Iterate over the kinds for which a composer is registered.
    pub fn kinds(&self) -> impl Iterator<Item = &str> {
        self.composers.keys().map(|s| s.as_str())
    }

    /// Look up the canonical handler ref + composer_config bound to
    /// `kind`. Used by boot validation to drive subprocess-based
    /// `composer_config` validation without exposing the
    /// `VerifiedHandler` itself.
    pub fn handler_ref_for(&self, kind: &str) -> Option<(&str, &Value)> {
        self.composers
            .get(kind)
            .map(|b| (b.handler.canonical_ref.as_str(), &b.config))
    }

    /// Run the composer for `kind`. Spawns the handler subprocess
    /// (env-cleared via `lillux::exec::lib_run`), serializes
    /// `(root, ancestors)` as a slim `ComposeRequest`, and decodes
    /// the response into a `KindComposedView`.
    ///
    /// Returns `ResolutionError::StepFailed` when the handler reports
    /// a `ComposeErr`, and bubbles transport / protocol failures via
    /// `EngineError` mapped through `ResolutionError::StepFailed`.
    pub fn compose(
        &self,
        kind: &str,
        root: &ResolvedAncestor,
        root_parsed: &Value,
        ancestors: &[ResolvedAncestor],
        ancestor_parsed: &[Value],
    ) -> Result<KindComposedView, ResolutionError> {
        let bound = self.composers.get(kind).ok_or_else(|| {
            ResolutionError::StepFailed {
                step: ResolutionStepName::PipelineInit,
                reason: format!(
                    "no composer bound for kind `{kind}` — production paths must \
                     bind every kind via ComposerRegistry::from_kinds"
                ),
            }
        })?;

        if ancestors.len() != ancestor_parsed.len() {
            return Err(ResolutionError::StepFailed {
                step: ResolutionStepName::PipelineInit,
                reason: format!(
                    "ancestors ({}) / ancestor_parsed ({}) length mismatch — \
                     caller must keep them parallel",
                    ancestors.len(),
                    ancestor_parsed.len()
                ),
            });
        }

        let request = HandlerRequest::Compose(ComposeRequest {
            composer_config: bound.config.clone(),
            root: to_compose_input(root, root_parsed.clone()),
            ancestors: ancestors
                .iter()
                .zip(ancestor_parsed.iter())
                .map(|(a, p)| to_compose_input(a, p.clone()))
                .collect(),
        });

        let resp = run_handler_subprocess(&bound.handler, &request, COMPOSE_TIMEOUT)
            .map_err(engine_to_resolution_error)?;

        match resp {
            HandlerResponse::ComposeOk(success) => Ok(success_to_view(success)),
            HandlerResponse::ComposeErr { step, reason } => Err(ResolutionError::StepFailed {
                step: wire_step_to_engine(step),
                reason,
            }),
            other => Err(ResolutionError::StepFailed {
                step: ResolutionStepName::PipelineInit,
                reason: format!(
                    "composer handler `{}` returned unexpected response: {other:?}",
                    bound.handler.canonical_ref
                ),
            }),
        }
    }
}

impl Default for ComposerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn to_compose_input(a: &ResolvedAncestor, parsed: Value) -> ComposeInput {
    ComposeInput {
        item: ComposeItemContext {
            requested_id: a.requested_id.clone(),
            resolved_ref: a.resolved_ref.clone(),
            trust_class: trust_class_to_wire(a.trust_class),
        },
        parsed,
    }
}

fn trust_class_to_wire(t: TrustClass) -> TrustClassWire {
    match t {
        TrustClass::TrustedSystem => TrustClassWire::TrustedSystem,
        TrustClass::TrustedUser => TrustClassWire::TrustedUser,
        TrustClass::UntrustedUserSpace => TrustClassWire::UntrustedUserSpace,
        TrustClass::Unsigned => TrustClassWire::Unsigned,
    }
}

fn wire_step_to_engine(s: ResolutionStepNameWire) -> ResolutionStepName {
    match s {
        ResolutionStepNameWire::PipelineInit => ResolutionStepName::PipelineInit,
        ResolutionStepNameWire::ResolveExtendsChain => ResolutionStepName::ResolveExtendsChain,
        ResolutionStepNameWire::ResolveReferences => ResolutionStepName::ResolveReferences,
    }
}

fn success_to_view(s: ComposeSuccess) -> KindComposedView {
    KindComposedView {
        composed: s.composed,
        derived: s.derived.into_iter().collect(),
        policy_facts: s.policy_facts.into_iter().collect(),
    }
}

fn engine_to_resolution_error(e: EngineError) -> ResolutionError {
    ResolutionError::StepFailed {
        step: ResolutionStepName::PipelineInit,
        reason: format!("composer handler subprocess failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kind_registry::KindRegistry;
    use crate::resolution::{ResolutionStepName, ResolvedAncestor, TrustClass};
    use crate::test_support::load_live_handler_registry;
    use lillux::crypto::SigningKey;
    use lillux::signature::compute_fingerprint;
    use crate::trust::{TrustStore, TrustedSigner};
    use serde_json::json;
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

    /// Trust store anchored to the platform-author key — needed so
    /// schemas signed in this test fixture verify under the same
    /// loader path the production engine uses.
    fn signing_key_with_trust() -> (SigningKey, TrustStore) {
        let sk = SigningKey::from_bytes(&[5u8; 32]);
        let vk = sk.verifying_key();
        let ts = TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: compute_fingerprint(&vk),
            verifying_key: vk,
            label: None,
        }]);
        (sk, ts)
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

    /// Synthetic kind names — engine code under
    /// `ryeos-engine/src/composers/` contains zero string literals
    /// naming a real kind. The composer refs are real handler refs
    /// that the live `HandlerRegistry` can resolve to a verified
    /// composer binary.
    #[test]
    fn from_kinds_binds_each_kind_to_its_declared_handler() {
        let root = tempdir();
        let (sk, ts) = signing_key_with_trust();
        let cfg = "  extends_field: ext\n  fields: []\n";
        write_kind(
            &root,
            "alpha",
            "handler:rye/core/extends-chain",
            Some(cfg),
            &sk,
        );
        write_kind(&root, "beta", "handler:rye/core/identity", None, &sk);
        write_kind(
            &root,
            "gamma",
            "handler:rye/core/graph-permissions",
            None,
            &sk,
        );
        let kinds = KindRegistry::load_base(&[root], &ts).unwrap();

        let handlers = load_live_handler_registry();
        let reg = ComposerRegistry::from_kinds(&kinds, &handlers).unwrap();
        assert!(reg.contains("alpha"));
        assert!(reg.contains("beta"));
        assert!(reg.contains("gamma"));
        let mut names: Vec<&str> = reg.kinds().collect();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
    }

    /// R-E guardrail: adding graph_permissions composer MUST NOT break
    /// the extends_chain composer's effective_caps policy-fact
    /// projection. This pins that directive-style kinds using
    /// extends_chain still produce effective_caps via policy_facts —
    /// after migration, the projection happens in the subprocess
    /// composer binary and is decoded back into the engine view.
    #[test]
    fn extends_chain_effective_caps_projection_unchanged_after_graph_permissions() {
        let root = tempdir();
        let (sk, ts) = signing_key_with_trust();
        let cfg = r#"
  extends_field: ext
  fields: []
  policy_facts:
    - name: effective_caps
      path: ["permissions", "execute"]
      expect: array_of_strings
"#;
        write_kind(
            &root,
            "directive",
            "handler:rye/core/extends-chain",
            Some(cfg),
            &sk,
        );
        let kinds = KindRegistry::load_base(std::slice::from_ref(&root), &ts).unwrap();

        let handlers = load_live_handler_registry();
        let reg = ComposerRegistry::from_kinds(&kinds, &handlers).unwrap();

        let parsed = json!({
            "permissions": {
                "execute": ["rye.execute.tool.echo", "rye.execute.tool.read"]
            }
        });
        let anc = ResolvedAncestor {
            requested_id: "directive:test".into(),
            resolved_ref: "directive:test".into(),
            source_path: root.join("directive/test.directive.md"),
            trust_class: TrustClass::TrustedSystem,
            alias_resolution: None,
            added_by: ResolutionStepName::PipelineInit,
            raw_content: String::new(),
            raw_content_digest: String::new(),
        };
        let view = reg.compose("directive", &anc, &parsed, &[], &[]).unwrap();
        let caps = view.policy_fact_string_seq("effective_caps");
        assert_eq!(
            caps,
            vec!["rye.execute.tool.echo", "rye.execute.tool.read"],
            "extends_chain effective_caps projection must work unchanged"
        );
    }

    #[test]
    fn from_kinds_fails_loud_for_unregistered_handler() {
        let root = tempdir();
        let (sk, ts) = signing_key_with_trust();
        write_kind(&root, "alpha", "handler:totally/made/up", None, &sk);
        let kinds = KindRegistry::load_base(&[root], &ts).unwrap();

        let handlers = load_live_handler_registry();
        let err = ComposerRegistry::from_kinds(&kinds, &handlers).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("handler:totally/made/up") && msg.contains("alpha"),
            "expected unknown-handler error naming both kind and handler, got: {msg}"
        );
    }
}
