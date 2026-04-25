//! Boot-time cross-registry validation.
//!
//! After kind schemas, parser descriptors, native handlers, and
//! composers have all been loaded, this pass walks every kind schema's
//! extensions and verifies that its declared parser ref:
//!
//!   * resolves to a parser descriptor we know about,
//!   * targets a native handler we know about,
//!   * has a `parser_config` the handler accepts,
//!
//! …and that any kind requiring composition has a registered composer.
//! (Parser kind identity is implicit from location — descriptors live
//! at `.ai/<parser-kind-directory>/**/*.yaml` (typically
//! `.ai/parsers/rye/core/...`) and are addressed by canonical refs
//! like `parser:rye/core/yaml/yaml`. Parsers are their own kind; there
//! is no discriminator field on the descriptor to verify.)
//!
//! Every issue is collected — the validator does not short-circuit on
//! the first failure. Callers that want hard-fail boot semantics should
//! treat *any* returned `Vec<BootIssue>` as fatal.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::composers::{ComposerRegistry, NativeComposerHandlerRegistry};
use crate::contracts::ContractViolation;
use crate::kind_registry::KindRegistry;
use crate::parsers::{DuplicateRef, NativeParserHandlerRegistry, ParserRegistry};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootIssue {
    /// A kind extension's parser ref doesn't resolve to any known
    /// parser tool descriptor.
    DanglingParserRef {
        kind: String,
        ext: String,
        parser_ref: String,
    },
    /// A parser descriptor's `executor_id` references an unknown
    /// `native:` handler.
    UnknownNativeHandler {
        parser_ref: String,
        handler: String,
    },
    /// A parser descriptor's `parser_config` failed handler validation.
    InvalidParserConfig {
        parser_ref: String,
        reason: String,
    },
    /// A composer is registered for a kind that doesn't exist in the
    /// `KindRegistry`. Composer registration is explicit, so this is
    /// always a programmer error.
    ComposerForUnknownKind {
        kind: String,
    },
    /// A kind schema's `composer` field names a handler ID that is
    /// not registered in `NativeComposerHandlerRegistry`. The
    /// kind→handler mapping is data-driven; an unknown handler ID
    /// is the composer-side equivalent of `DanglingParserRef`.
    UnknownComposerHandler {
        kind: String,
        handler_id: String,
    },
    /// A kind schema's `composer_config` failed handler validation.
    /// Composer-side equivalent of `InvalidParserConfig`.
    InvalidComposerConfig {
        kind: String,
        handler_id: String,
        reason: String,
    },
    /// Two parser tool YAMLs collapsed onto the same canonical ref
    /// across distinct loader roots. Loader keeps first-found-wins
    /// for the data path; this variant exists so the boot validator
    /// fails loud rather than silently shadowing a descriptor.
    DuplicateParserRef {
        parser_ref: String,
        paths: Vec<PathBuf>,
    },
    /// The parser's declared `output_schema` does not satisfy the
    /// kind's `composed_value_contract`. One variant per individual
    /// `ContractViolation` so callers see every problem.
    ParserComposerContractViolation {
        kind: String,
        ext: String,
        parser_ref: String,
        violation: ContractViolation,
    },
}

/// Run the cross-registry validation. Returns `Ok(())` if no issues
/// were found, otherwise `Err(Vec<BootIssue>)` with **every** problem.
///
/// `dup_refs` is the duplicate list returned by
/// `ParserRegistry::load_base`; pass an empty slice when no loader was
/// involved (tests that build a registry from in-memory entries).
pub fn validate_boot(
    kinds: &KindRegistry,
    parser_tools: &ParserRegistry,
    native_handlers: &NativeParserHandlerRegistry,
    native_composers: &NativeComposerHandlerRegistry,
    composers: &ComposerRegistry,
    dup_refs: &[DuplicateRef],
) -> Result<(), Vec<BootIssue>> {
    let mut issues: Vec<BootIssue> = Vec::new();

    // Loader-detected canonical-ref collisions are always fatal: two
    // different parser YAMLs trying to occupy the same slot is an
    // ambiguous configuration even when first-found-wins picks a
    // deterministic winner.
    for dup in dup_refs {
        issues.push(BootIssue::DuplicateParserRef {
            parser_ref: dup.canonical_ref.clone(),
            paths: dup.paths.clone(),
        });
    }

    // Cache parser config validation results so we don't re-run the
    // handler check once per kind that uses the same parser ref.
    let mut config_checked: HashMap<String, ()> = HashMap::new();

    for kind in kinds.kinds().map(|s| s.to_string()).collect::<Vec<_>>() {
        let schema = match kinds.get(&kind) {
            Some(s) => s,
            None => continue,
        };

        for ext in &schema.extensions {
            let parser_ref = &ext.parser;

            let descriptor = match parser_tools.get(parser_ref) {
                Some(d) => d,
                None => {
                    issues.push(BootIssue::DanglingParserRef {
                        kind: kind.clone(),
                        ext: ext.ext.clone(),
                        parser_ref: parser_ref.clone(),
                    });
                    continue;
                }
            };

            let handler_name = match descriptor.executor_id.strip_prefix("native:") {
                Some(h) => h,
                None => {
                    issues.push(BootIssue::UnknownNativeHandler {
                        parser_ref: parser_ref.clone(),
                        handler: descriptor.executor_id.clone(),
                    });
                    continue;
                }
            };

            let handler = match native_handlers.get(handler_name) {
                Some(h) => h,
                None => {
                    issues.push(BootIssue::UnknownNativeHandler {
                        parser_ref: parser_ref.clone(),
                        handler: handler_name.to_string(),
                    });
                    continue;
                }
            };

            if !config_checked.contains_key(parser_ref) {
                if let Err(reason) = handler.validate_config(&descriptor.parser_config) {
                    issues.push(BootIssue::InvalidParserConfig {
                        parser_ref: parser_ref.clone(),
                        reason,
                    });
                }
                config_checked.insert(parser_ref.clone(), ());
            }

            // Parser → composer wiring contract.
            // ALWAYS enforced. Both fields are mandatory at the
            // schema/descriptor layer (serde-required + manual
            // YAML parser require), so there's no opt-in skip.
            // Aggregates all violations.
            let _ = handler; // contract check uses descriptor only
            for violation in schema
                .composed_value_contract
                .is_satisfied_by(&descriptor.output_schema)
            {
                issues.push(BootIssue::ParserComposerContractViolation {
                    kind: kind.clone(),
                    ext: ext.ext.clone(),
                    parser_ref: parser_ref.clone(),
                    violation,
                });
            }
        }
    }

    // Each kind's declared composer handler ID must resolve to a
    // registered native composer handler. Aggregate every offender.
    let mut sorted_kinds: Vec<&str> = kinds.kinds().collect();
    sorted_kinds.sort();
    for kind in sorted_kinds {
        let schema = match kinds.get(kind) {
            Some(s) => s,
            None => continue,
        };
        if !native_composers.contains(&schema.composer) {
            issues.push(BootIssue::UnknownComposerHandler {
                kind: kind.to_string(),
                handler_id: schema.composer.clone(),
            });
        } else if let Some(handler) = native_composers.get(&schema.composer) {
            if let Err(reason) = handler.validate_config(&schema.composer_config) {
                issues.push(BootIssue::InvalidComposerConfig {
                    kind: kind.to_string(),
                    handler_id: schema.composer.clone(),
                    reason,
                });
            }
        }
    }

    // Composer registry must not list kinds the KindRegistry doesn't know
    // about — registration is explicit, so a stale registration is a bug.
    for composer_kind in composers.kinds() {
        if kinds.get(composer_kind).is_none() {
            issues.push(BootIssue::ComposerForUnknownKind {
                kind: composer_kind.to_string(),
            });
        }
    }

    // Self-hosting parser check: the `parser` kind's declared parser
    // (the one used to parse parser-tool YAMLs themselves) is already
    // walked by the per-kind extension loop above, so any dangling
    // ref here is reported as `DanglingParserRef { kind: "parser", … }`.
    // This comment documents the invariant; see
    // `parser_kind_self_hosting_parser_must_resolve` test below.

    if issues.is_empty() {
        Ok(())
    } else {
        Err(issues)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composers::{
        ComposerRegistry, IdentityComposer, NativeComposerHandlerRegistry,
    };
    use crate::kind_registry::KindRegistry;
    use crate::parsers::descriptor::ParserDescriptor;
    use crate::parsers::handlers::{NativeParserHandler, ParseInput};
    use crate::parsers::{NativeParserHandlerRegistry, ParserRegistry};
    use crate::trust::{compute_fingerprint, TrustStore, TrustedSigner};
    use lillux::crypto::SigningKey;
    use serde_json::Value;
    use std::fs;
    use std::sync::Arc;

    fn native_composers() -> NativeComposerHandlerRegistry {
        NativeComposerHandlerRegistry::with_builtins()
    }

    /// Build the canonical composer registry from `kinds`, using the
    /// built-in native composer handlers. Replaces the old
    /// `ComposerRegistry::with_defaults()` shortcut at every test
    /// callsite — composer wiring is now data-driven.
    fn composers_from(kinds: &KindRegistry) -> ComposerRegistry {
        ComposerRegistry::from_kinds(kinds, &native_composers()).unwrap()
    }

    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rye_boot_val_{}_{}",
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
        SigningKey::from_bytes(&[11u8; 32])
    }

    fn trust_store(sk: &SigningKey) -> TrustStore {
        let vk = sk.verifying_key();
        TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: compute_fingerprint(&vk),
            verifying_key: vk,
            label: None,
        }])
    }

    /// Write a minimal directive kind schema referencing the supplied
    /// parser canonical ref. The schema is the smallest thing
    /// `KindRegistry::load_base` will accept that has one extension
    /// + one parser ref to validate against.
    fn write_directive_kind(root: &std::path::Path, parser_ref: &str, sk: &SigningKey) {
        // Every kind schema now MUST declare composed_value_contract
        // and composer.
        let yaml = format!(
            "\
location:
  directory: directives
formats:
  - extensions: [\".md\"]
    parser: {parser_ref}
    signature:
      prefix: \"<!--\"
      suffix: \"-->\"
composer: rye/core/identity
composed_value_contract:
  root_type: mapping
  required: {{}}
"
        );
        let dir = root.join("directive");
        fs::create_dir_all(&dir).unwrap();
        let signed = lillux::signature::sign_content(&yaml, sk, "#", None);
        fs::write(dir.join("directive.kind-schema.yaml"), signed).unwrap();
    }

    fn parser_descriptor(executor_id: &str, parser_config: Value) -> ParserDescriptor {
        ParserDescriptor {
            version: "1.0.0".into(),
            category: None,
            description: None,
            executor_id: executor_id.into(),
            parser_api_version: 1,
            parser_config,
            output_schema: crate::contracts::ValueShape::any_mapping(),
        }
    }

    /// Same as `write_directive_kind` but appends a
    /// `composed_value_contract` block so the boot validator runs the
    /// parser→composer wiring check.
    fn write_directive_kind_with_contract(
        root: &std::path::Path,
        parser_ref: &str,
        contract_yaml_indented: &str,
        sk: &SigningKey,
    ) {
        let yaml = format!(
            "\
location:
  directory: directives
formats:
  - extensions: [\".md\"]
    parser: {parser_ref}
    signature:
      prefix: \"<!--\"
      suffix: \"-->\"
composer: rye/core/identity
composed_value_contract:
{contract_yaml_indented}
"
        );
        let dir = root.join("directive");
        fs::create_dir_all(&dir).unwrap();
        let signed = lillux::signature::sign_content(&yaml, sk, "#", None);
        fs::write(dir.join("directive.kind-schema.yaml"), signed).unwrap();
    }

    fn kinds_with_directive_contract(
        parser_ref: &str,
        contract_yaml_indented: &str,
    ) -> KindRegistry {
        let root = tempdir();
        let sk = signing_key();
        let ts = trust_store(&sk);
        write_directive_kind_with_contract(&root, parser_ref, contract_yaml_indented, &sk);
        KindRegistry::load_base(&[root], &ts).unwrap()
    }

    fn parser_descriptor_with_schema(
        executor_id: &str,
        parser_config: Value,
        output_schema: crate::contracts::ValueShape,
    ) -> ParserDescriptor {
        ParserDescriptor {
            version: "1.0.0".into(),
            category: None,
            description: None,
            executor_id: executor_id.into(),
            parser_api_version: 1,
            parser_config,
            output_schema,
        }
    }

    fn kinds_with_directive(parser_ref: &str) -> KindRegistry {
        let root = tempdir();
        let sk = signing_key();
        let ts = trust_store(&sk);
        write_directive_kind(&root, parser_ref, &sk);
        KindRegistry::load_base(&[root], &ts).unwrap()
    }

    /// Happy path: kind references a parser tool that exists, executor
    /// is a known native handler, parser_config validates, and the
    /// composer registered for `directive` matches a known kind.
    #[test]
    fn validate_boot_happy_path() {
        let parser_ref = "parser:rye/core/markdown/frontmatter";
        let kinds = kinds_with_directive(parser_ref);

        let parsers = ParserRegistry::from_entries(vec![(
            parser_ref.to_string(),
            parser_descriptor(
                "native:parser_yaml_header_document",
                serde_json::json!({
                    "require_header": true,
                    "body_field": "body",
                    "forms": [
                        { "kind": "frontmatter", "delimiter": "---" }
                    ]
                }),
            ),
        )]);
        let handlers = NativeParserHandlerRegistry::with_builtins();
        let composers = composers_from(&kinds);

        validate_boot(&kinds, &parsers, &handlers, &native_composers(), &composers, &[]).expect("happy path Ok");
    }

    #[test]
    fn dangling_parser_ref_emitted() {
        let parser_ref = "tool:does/not/exist";
        let kinds = kinds_with_directive(parser_ref);
        let parsers = ParserRegistry::empty();
        let handlers = NativeParserHandlerRegistry::with_builtins();
        let composers = composers_from(&kinds);

        let issues = validate_boot(&kinds, &parsers, &handlers, &native_composers(), &composers, &[]).unwrap_err();
        assert!(issues.iter().any(|i| matches!(
            i,
            BootIssue::DanglingParserRef { parser_ref: pr, kind, .. }
                if pr == parser_ref && kind == "directive"
        )));
    }

    #[test]
    fn unknown_native_handler_emitted() {
        let parser_ref = "parser:rye/core/x/x";
        let kinds = kinds_with_directive(parser_ref);

        let parsers = ParserRegistry::from_entries(vec![(
            parser_ref.to_string(),
            // executor_id with `native:` prefix but unknown handler suffix
            parser_descriptor("native:totally_made_up_handler", serde_json::json!({})),
        )]);
        let handlers = NativeParserHandlerRegistry::with_builtins();
        let composers = composers_from(&kinds);

        let issues = validate_boot(&kinds, &parsers, &handlers, &native_composers(), &composers, &[]).unwrap_err();
        assert!(issues.iter().any(|i| matches!(
            i,
            BootIssue::UnknownNativeHandler { handler, .. }
                if handler == "totally_made_up_handler"
        )));
    }

    #[test]
    fn unknown_native_handler_no_native_prefix() {
        // `executor_id` without a `native:` prefix is also reported via
        // UnknownNativeHandler — the handler name in the issue is the
        // raw executor_id.
        let parser_ref = "parser:rye/core/x/x";
        let kinds = kinds_with_directive(parser_ref);

        let parsers = ParserRegistry::from_entries(vec![(
            parser_ref.to_string(),
            parser_descriptor("subprocess:python", serde_json::json!({})),
        )]);
        let handlers = NativeParserHandlerRegistry::with_builtins();
        let composers = composers_from(&kinds);

        let issues = validate_boot(&kinds, &parsers, &handlers, &native_composers(), &composers, &[]).unwrap_err();
        assert!(issues.iter().any(|i| matches!(
            i,
            BootIssue::UnknownNativeHandler { handler, .. } if handler == "subprocess:python"
        )));
    }

    #[test]
    fn invalid_parser_config_emitted() {
        let parser_ref = "parser:rye/core/yaml/yaml";
        let kinds = kinds_with_directive(parser_ref);

        let parsers = ParserRegistry::from_entries(vec![(
            parser_ref.to_string(),
            // require_mapping must be a bool — pass a string to fail
            // YamlDocumentHandler::validate_config.
            parser_descriptor(
                "native:parser_yaml_document",
                serde_json::json!({ "require_mapping": "yes please" }),
            ),
        )]);
        let handlers = NativeParserHandlerRegistry::with_builtins();
        let composers = composers_from(&kinds);

        let issues = validate_boot(&kinds, &parsers, &handlers, &native_composers(), &composers, &[]).unwrap_err();
        assert!(issues.iter().any(|i| matches!(
            i,
            BootIssue::InvalidParserConfig { parser_ref: pr, .. } if pr == parser_ref
        )));
    }

    #[test]
    fn composer_for_unknown_kind_emitted() {
        // No directive kind in the registry, but a composer IS
        // registered for `directive` — should report.
        let kinds = KindRegistry::empty();
        let parsers = ParserRegistry::empty();
        let handlers = NativeParserHandlerRegistry::with_builtins();
        let mut composers = ComposerRegistry::new();
        composers.register("directive", Arc::new(IdentityComposer), Value::Null);
        composers.register("graph", Arc::new(IdentityComposer), Value::Null);

        let issues = validate_boot(&kinds, &parsers, &handlers, &native_composers(), &composers, &[]).unwrap_err();
        let unknown_kinds: Vec<&str> = issues
            .iter()
            .filter_map(|i| match i {
                BootIssue::ComposerForUnknownKind { kind } => Some(kind.as_str()),
                _ => None,
            })
            .collect();
        assert!(unknown_kinds.contains(&"directive"));
        assert!(unknown_kinds.contains(&"graph"));
    }

    /// Aggregation: multiple issues across kinds returned together
    /// (validator does not short-circuit).
    #[test]
    fn aggregates_multiple_issues() {
        let parser_ref = "parser:rye/core/yaml/yaml";
        let kinds = kinds_with_directive(parser_ref);

        // Two faults at once:
        // 1. invalid parser_config → InvalidParserConfig
        // 2. ComposerForUnknownKind for a fake kind we register a composer for
        let parsers = ParserRegistry::from_entries(vec![(
            parser_ref.to_string(),
            parser_descriptor(
                "native:parser_yaml_document",
                serde_json::json!({ "require_mapping": "not a bool" }),
            ),
        )]);
        let handlers = NativeParserHandlerRegistry::with_builtins();
        let mut composers = composers_from(&kinds);
        composers.register("ghost_kind", Arc::new(IdentityComposer), Value::Null);

        let issues = validate_boot(&kinds, &parsers, &handlers, &native_composers(), &composers, &[]).unwrap_err();
        let has_cfg = issues
            .iter()
            .any(|i| matches!(i, BootIssue::InvalidParserConfig { .. }));
        let has_ghost = issues.iter().any(|i| matches!(
            i,
            BootIssue::ComposerForUnknownKind { kind } if kind == "ghost_kind"
        ));
        assert!(
            has_cfg && has_ghost,
            "expected both faults reported, got: {issues:?}"
        );
        assert!(issues.len() >= 2);
    }


    /// Sanity: a noop handler whose validate_config always succeeds
    /// can be plugged in via `NativeParserHandlerRegistry::register`,
    /// proving the validator consults the registry rather than a
    /// hardcoded handler set.
    #[test]
    fn custom_handler_resolves_via_registry() {
        struct Noop;
        impl NativeParserHandler for Noop {
            fn validate_config(&self, _: &Value) -> Result<(), String> {
                Ok(())
            }
            fn parse(&self, _: &Value, _: ParseInput<'_>) -> Result<Value, crate::error::EngineError> {
                Ok(Value::Null)
            }
        }

        let parser_ref = "parser:rye/core/custom/x";
        let kinds = kinds_with_directive(parser_ref);
        let parsers = ParserRegistry::from_entries(vec![(
            parser_ref.to_string(),
            parser_descriptor("native:custom_noop", serde_json::json!({})),
        )]);
        let mut handlers = NativeParserHandlerRegistry::new();
        handlers.register("custom_noop", Box::new(Noop));
        let composers = composers_from(&kinds);
        validate_boot(&kinds, &parsers, &handlers, &native_composers(), &composers, &[]).expect("custom handler accepted");
    }

    /// `dup_refs` from the loader is surfaced as a structured
    /// `BootIssue::DuplicateParserRef` — the validator does not silently
    /// ignore loader collisions even when first-found-wins picked a
    /// deterministic winner.
    #[test]
    fn duplicate_parser_ref_emitted() {
        // Use a setup that is otherwise valid so the duplicate is the
        // ONLY issue we expect.
        let parser_ref = "parser:rye/core/markdown/frontmatter";
        let kinds = kinds_with_directive(parser_ref);
        let parsers = ParserRegistry::from_entries(vec![(
            parser_ref.to_string(),
            parser_descriptor(
                "native:parser_yaml_header_document",
                serde_json::json!({
                    "require_header": true,
                    "body_field": "body",
                    "forms": [
                        { "kind": "frontmatter", "delimiter": "---" }
                    ]
                }),
            ),
        )]);
        let handlers = NativeParserHandlerRegistry::with_builtins();
        let composers = composers_from(&kinds);

        let dup_refs = vec![DuplicateRef {
            canonical_ref: "parser:rye/core/yaml/yaml".to_string(),
            paths: vec![
                std::path::PathBuf::from("/system/.ai/parsers/rye/core/yaml/yaml.yaml"),
                std::path::PathBuf::from("/user/.ai/parsers/rye/core/yaml/yaml.yaml"),
            ],
        }];

        let issues = validate_boot(&kinds, &parsers, &handlers, &native_composers(), &composers, &dup_refs).unwrap_err();
        assert!(issues.iter().any(|i| matches!(
            i,
            BootIssue::DuplicateParserRef { parser_ref: pr, paths }
                if pr == "parser:rye/core/yaml/yaml" && paths.len() == 2
        )), "expected DuplicateParserRef in {issues:?}");
    }

    /// Build a kind schema for the `parser` kind itself referencing a
    /// configurable canonical ref, mirroring the bundle's
    /// `parser.kind-schema.yaml`. Used to exercise the self-hosting
    /// invariant.
    fn write_parser_kind(root: &std::path::Path, parser_ref: &str, sk: &SigningKey) {
        // Includes the now-mandatory composed_value_contract + composer.
        let yaml = format!(
            "\
location:
  directory: parsers
formats:
  - extensions: [\".yaml\"]
    parser: {parser_ref}
    signature:
      prefix: \"#\"
composer: rye/core/identity
composed_value_contract:
  root_type: mapping
  required: {{}}
"
        );
        let dir = root.join("parser");
        fs::create_dir_all(&dir).unwrap();
        let signed = lillux::signature::sign_content(&yaml, sk, "#", None);
        fs::write(dir.join("parser.kind-schema.yaml"), signed).unwrap();
    }

    /// Self-hosting invariant: the `parser` kind's own declared parser
    /// must be present in the loaded `ParserRegistry`. If the YAML
    /// parser used to parse parser-tool descriptors is missing, boot
    /// must fail loud (caught by the per-kind extension walk).
    #[test]
    fn parser_kind_self_hosting_parser_must_resolve() {
        let root = tempdir();
        let sk = signing_key();
        let ts = trust_store(&sk);
        write_parser_kind(&root, "parser:rye/core/yaml/yaml", &sk);
        let kinds = KindRegistry::load_base(&[root], &ts).unwrap();

        // Empty ParserRegistry → the self-hosting parser ref dangles.
        let parsers = ParserRegistry::empty();
        let handlers = NativeParserHandlerRegistry::with_builtins();
        let composers = composers_from(&kinds);

        let issues = validate_boot(&kinds, &parsers, &handlers, &native_composers(), &composers, &[]).unwrap_err();
        assert!(issues.iter().any(|i| matches!(
            i,
            BootIssue::DanglingParserRef { kind, parser_ref, .. }
                if kind == "parser" && parser_ref == "parser:rye/core/yaml/yaml"
        )), "expected DanglingParserRef for parser kind in {issues:?}");
    }

    /// Companion happy-path: when the self-hosting parser IS present
    /// in the registry, no DanglingParserRef is emitted for the
    /// `parser` kind.
    #[test]
    fn parser_kind_self_hosting_parser_present_passes() {
        let root = tempdir();
        let sk = signing_key();
        let ts = trust_store(&sk);
        let self_ref = "parser:rye/core/yaml/yaml";
        write_parser_kind(&root, self_ref, &sk);
        let kinds = KindRegistry::load_base(&[root], &ts).unwrap();

        let parsers = ParserRegistry::from_entries(vec![(
            self_ref.to_string(),
            parser_descriptor(
                "native:parser_yaml_document",
                serde_json::json!({ "require_mapping": true }),
            ),
        )]);
        let handlers = NativeParserHandlerRegistry::with_builtins();
        // No `parser` composer is registered (parsers don't compose),
        // so use new() to avoid spurious ComposerForUnknownKind for
        // `directive`.
        let composers = ComposerRegistry::new();

        validate_boot(&kinds, &parsers, &handlers, &native_composers(), &composers, &[])
            .expect("self-hosting parser resolves cleanly");
    }

    // ── Parser → composer wiring contract tests ──────────────────────

    use crate::contracts::{FieldType, PrimType, ShapeType, ValueShape};
    use std::collections::BTreeMap;

    fn shape_with_required_body() -> ValueShape {
        let mut required = BTreeMap::new();
        required.insert("body".to_string(), FieldType::Single(PrimType::String));
        ValueShape {
            root_type: ShapeType::Mapping,
            required,
            optional: BTreeMap::new(),
        }
    }

    /// Kind contract is satisfied by parser output_schema → no
    /// `ParserComposerContractViolation`.
    #[test]
    fn contract_satisfied_no_error() {
        let parser_ref = "parser:rye/core/markdown/directive";
        let kinds = kinds_with_directive_contract(
            parser_ref,
            "  root_type: mapping\n  required:\n    body: string\n",
        );
        let parsers = ParserRegistry::from_entries(vec![(
            parser_ref.to_string(),
            parser_descriptor_with_schema(
                "native:parser_yaml_header_document",
                serde_json::json!({
                    "require_header": true,
                    "body_field": "body",
                    "forms": [{ "kind": "frontmatter", "delimiter": "---" }]
                }),
                shape_with_required_body(),
            ),
        )]);
        let handlers = NativeParserHandlerRegistry::with_builtins();
        let composers = composers_from(&kinds);
        validate_boot(&kinds, &parsers, &handlers, &native_composers(), &composers, &[])
            .expect("contract satisfied path");
    }

    /// Kind schema YAML missing `composed_value_contract` is rejected
    /// at load time — there is no longer an opt-in path. Replaces the
    /// old `missing_output_schema_emitted` test (impossible state now
    /// that both fields are mandatory).
    #[test]
    fn kind_schema_missing_contract_rejected_at_load() {
        let root = tempdir();
        let sk = signing_key();
        let ts = trust_store(&sk);
        // Hand-write a directive kind schema WITHOUT
        // composed_value_contract — must be rejected by the loader.
        let yaml = "\
location:
  directory: directives
formats:
  - extensions: [\".md\"]
    parser: parser:rye/core/markdown/directive
    signature:
      prefix: \"<!--\"
      suffix: \"-->\"
";
        let dir = root.join("directive");
        fs::create_dir_all(&dir).unwrap();
        let signed = lillux::signature::sign_content(yaml, &sk, "#", None);
        fs::write(dir.join("directive.kind-schema.yaml"), signed).unwrap();

        let err = KindRegistry::load_base(&[root], &ts).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("composed_value_contract") && msg.contains("missing required field"),
            "expected missing-required-field error for composed_value_contract, got: {err:?}"
        );
    }

    /// Kind schema YAML missing `composer` is rejected at load time.
    /// Mirrors `kind_schema_missing_contract_rejected_at_load` for the
    /// new required field — every kind must explicitly name its
    /// composer handler ID.
    #[test]
    fn kind_schema_missing_composer_rejected_at_load() {
        let root = tempdir();
        let sk = signing_key();
        let ts = trust_store(&sk);
        let yaml = "\
location:
  directory: directives
formats:
  - extensions: [\".md\"]
    parser: parser:rye/core/markdown/directive
    signature:
      prefix: \"<!--\"
      suffix: \"-->\"
composed_value_contract:
  root_type: mapping
  required: {}
";
        let dir = root.join("directive");
        fs::create_dir_all(&dir).unwrap();
        let signed = lillux::signature::sign_content(yaml, &sk, "#", None);
        fs::write(dir.join("directive.kind-schema.yaml"), signed).unwrap();

        let err = KindRegistry::load_base(&[root], &ts).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("composer") && msg.contains("missing required field"),
            "expected missing-required-field error for composer, got: {err:?}"
        );
    }

    /// A kind schema's `composer` field that names an unregistered
    /// native composer handler must be flagged by the boot validator.
    /// Composer-side equivalent of `dangling_parser_ref_emitted`.
    #[test]
    fn unknown_composer_handler_emitted() {
        // Hand-build a kind schema whose composer ID is not in the
        // native registry. We bypass `kinds_with_directive` so we can
        // set `composer:` to something other than `rye/core/directive`.
        let root = tempdir();
        let sk = signing_key();
        let ts = trust_store(&sk);
        let yaml = "\
location:
  directory: directives
formats:
  - extensions: [\".md\"]
    parser: parser:rye/core/markdown/directive
    signature:
      prefix: \"<!--\"
      suffix: \"-->\"
composer: totally/made/up
composed_value_contract:
  root_type: mapping
  required: {}
";
        let dir = root.join("directive");
        fs::create_dir_all(&dir).unwrap();
        let signed = lillux::signature::sign_content(yaml, &sk, "#", None);
        fs::write(dir.join("directive.kind-schema.yaml"), signed).unwrap();
        let kinds = KindRegistry::load_base(&[root], &ts).unwrap();

        // Provide a parser to keep parser-side checks clean.
        let parsers = ParserRegistry::from_entries(vec![(
            "parser:rye/core/markdown/directive".to_string(),
            parser_descriptor(
                "native:parser_yaml_header_document",
                serde_json::json!({
                    "require_header": true,
                    "body_field": "body",
                    "forms": [{ "kind": "frontmatter", "delimiter": "---" }]
                }),
            ),
        )]);
        let handlers = NativeParserHandlerRegistry::with_builtins();
        // Empty composer registry — we're testing the per-kind
        // handler-ID resolution check, not the ComposerForUnknownKind
        // check.
        let composers = ComposerRegistry::new();
        let issues =
            validate_boot(&kinds, &parsers, &handlers, &native_composers(), &composers, &[])
                .unwrap_err();
        assert!(
            issues.iter().any(|i| matches!(
                i,
                BootIssue::UnknownComposerHandler { kind, handler_id }
                    if kind == "directive" && handler_id == "totally/made/up"
            )),
            "expected UnknownComposerHandler for directive→totally/made/up, got: {issues:?}"
        );
    }

    /// All wiring violations aggregated at once: root_type mismatch,
    /// missing required field, AND type mismatch — no short-circuit.
    #[test]
    fn aggregates_all_contract_violations() {
        let parser_ref = "parser:rye/core/markdown/directive";
        // Kind needs mapping with body:string + name:string.
        let mut required = BTreeMap::new();
        required.insert("body".to_string(), FieldType::Single(PrimType::String));
        required.insert("name".to_string(), FieldType::Single(PrimType::String));
        let _kind_shape = ValueShape {
            root_type: ShapeType::Mapping,
            required,
            optional: BTreeMap::new(),
        };
        let kinds = kinds_with_directive_contract(
            parser_ref,
            "  root_type: mapping\n  required:\n    body: string\n    name: string\n",
        );
        // Parser declares: sequence root, body=integer, no name.
        let mut p_required = BTreeMap::new();
        p_required.insert("body".to_string(), FieldType::Single(PrimType::Integer));
        let bad_producer = ValueShape {
            root_type: ShapeType::Sequence,
            required: p_required,
            optional: BTreeMap::new(),
        };
        let parsers = ParserRegistry::from_entries(vec![(
            parser_ref.to_string(),
            parser_descriptor_with_schema(
                "native:parser_yaml_header_document",
                serde_json::json!({
                    "require_header": true,
                    "body_field": "body",
                    "forms": [{ "kind": "frontmatter", "delimiter": "---" }]
                }),
                bad_producer,
            ),
        )]);
        let handlers = NativeParserHandlerRegistry::with_builtins();
        let composers = composers_from(&kinds);
        let issues = validate_boot(&kinds, &parsers, &handlers, &native_composers(), &composers, &[]).unwrap_err();

        let viols: Vec<&ContractViolation> = issues
            .iter()
            .filter_map(|i| match i {
                BootIssue::ParserComposerContractViolation { violation, .. } => Some(violation),
                _ => None,
            })
            .collect();

        let has_root = viols
            .iter()
            .any(|v| matches!(v, ContractViolation::RootTypeMismatch { .. }));
        let has_missing = viols.iter().any(|v| matches!(
            v,
            ContractViolation::MissingRequiredField { name, .. } if name == "name"
        ));
        let has_type_mismatch = viols.iter().any(|v| matches!(
            v,
            ContractViolation::FieldTypeMismatch { name, .. } if name == "body"
        ));
        assert!(
            has_root && has_missing && has_type_mismatch,
            "expected root + missing + type-mismatch all aggregated, got: {issues:?}"
        );
    }

    /// Two kinds with bad `composer_config` produce two
    /// `InvalidComposerConfig` violations in one boot report —
    /// validator does not short-circuit on the first.
    #[test]
    fn invalid_composer_config_aggregated() {
        let root = tempdir();
        let sk = signing_key();
        let ts = trust_store(&sk);
        // Two kinds — both bind identity composer but pass non-empty
        // configs, which identity rejects. Use distinct kinds so we
        // can assert two distinct kind names appear.
        for (kind, junk) in [("alpha", "  unused: 1"), ("beta", "  also_unused: 2")] {
            let yaml = format!(
                "\
location:
  directory: {kind}s
formats:
  - extensions: [\".md\"]
    parser: parser:rye/core/markdown/x
    signature:
      prefix: \"<!--\"
      suffix: \"-->\"
composer: rye/core/identity
composer_config:
{junk}
composed_value_contract:
  root_type: mapping
  required: {{}}
"
            );
            let dir = root.join(kind);
            fs::create_dir_all(&dir).unwrap();
            let signed = lillux::signature::sign_content(&yaml, &sk, "#", None);
            fs::write(dir.join(format!("{kind}.kind-schema.yaml")), signed).unwrap();
        }
        let kinds = KindRegistry::load_base(&[root], &ts).unwrap();

        let parsers = ParserRegistry::from_entries(vec![(
            "parser:rye/core/markdown/x".to_string(),
            parser_descriptor(
                "native:parser_yaml_header_document",
                serde_json::json!({
                    "require_header": true,
                    "body_field": "body",
                    "forms": [{ "kind": "frontmatter", "delimiter": "---" }]
                }),
            ),
        )]);
        let handlers = NativeParserHandlerRegistry::with_builtins();
        // Empty composer registry — we're testing the per-kind
        // composer-config check, not registry-side bookkeeping.
        let composers = ComposerRegistry::new();
        let issues =
            validate_boot(&kinds, &parsers, &handlers, &native_composers(), &composers, &[])
                .unwrap_err();

        let bad: Vec<&str> = issues
            .iter()
            .filter_map(|i| match i {
                BootIssue::InvalidComposerConfig { kind, .. } => Some(kind.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            bad.contains(&"alpha") && bad.contains(&"beta"),
            "expected InvalidComposerConfig for both kinds, got: {issues:?}"
        );
    }
}
