//! End-to-end gate tests for V5.2 service dispatch.
//!
//! Validates the full chain:
//!   1. Every installed item admitted to the Services registry resolves
//!   2. Every resolved service verifies (trust chain)
//!   3. Every verified item has an exact compiled descriptor and endpoint
//!   4. Every verified service's `required_caps` is consistent with expectations
//!   5. Capability enforcement rejects callers without required caps
//!
//! These are the same checks the daemon self-check performs at startup,
//! duplicated here as regression guards that run in CI without a live daemon.

use std::path::PathBuf;

use ryeos_api::{ServiceDescriptor, handlers as service_handlers};
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, Principal};
use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::trust::TrustStore;

/// Iterate the canonical descriptor table.
fn descriptors() -> &'static [ServiceDescriptor] {
    static DESCRIPTORS: once_cell::sync::Lazy<Vec<ServiceDescriptor>> =
        once_cell::sync::Lazy::new(|| {
            service_handlers::ALL
                .iter()
                .chain(ryeos_ui::handlers::ALL.iter())
                .copied()
                .collect()
        });
    &DESCRIPTORS
}

/// Enumerate the installed operational corpus from signed kind execution
/// contracts. Compiled descriptors are implementation capability, not the
/// authority deciding which services this bundle set installed.
fn service_refs(engine: &ryeos_engine::engine::Engine) -> Vec<CanonicalRef> {
    let refs = ryeos_engine::item_resolution::enumerate_in_process_registry_refs(
        &engine.resolution_roots(None),
        &engine.kinds,
        ryeos_engine::kind_registry::InProcessRegistryKind::Services,
    )
    .expect("enumerate installed Services-registry items");
    refs.into_iter()
        .filter(|canonical| {
            let effective = engine
                .effective_item(ryeos_engine::engine::EffectiveItemRequest {
                    item_ref: canonical.clone(),
                    expected_kind: Some(canonical.kind.clone()),
                    project_root: None,
                    subject_resolution_authority:
                        ryeos_engine::contracts::SubjectResolutionAuthority::Projectless,
                })
                .unwrap_or_else(|error| {
                    panic!("compose installed Services-registry item `{canonical}`: {error}")
                });
            ryeos_app::service_registry::requires_compiled_service_handler(
                &effective.composed_value,
            )
            .unwrap_or_else(|error| {
                panic!("classify installed Services-registry item `{canonical}`: {error}")
            })
        })
        .collect()
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("bundles").is_dir())
        .expect("workspace root with bundles/ directory")
        .to_path_buf()
}

/// Build an engine-like fixture using the live bundle + trusted signers.
/// Mirrors `engine_init::build_engine` but uses the test fixture trust store
/// instead of the daemon's three-tier loader. Loads both core and standard
/// bundles so all services in the descriptor table can resolve.
fn build_test_engine() -> ryeos_engine::engine::Engine {
    let trusted_dir = manifest_dir().join("tests/fixtures/trusted_signers");
    let trust_store = TrustStore::load_from_dir(&trusted_dir).expect("load trust store");

    let workspace = workspace_root();
    let core_bundle = workspace.join("bundles/core");
    let std_bundle = workspace.join("bundles/standard");

    // Kind schemas from both bundles (core has service/tool/parser/etc,
    // standard has directive/graph/knowledge).
    let kinds_dirs = vec![
        core_bundle.join(".ai/node/engine/kinds"),
        std_bundle.join(".ai/node/engine/kinds"),
    ];
    let kinds = KindRegistry::load_base(&kinds_dirs, &trust_store).expect("load kind registry");

    // Parser tools from both bundles.
    let bundle_roots: Vec<PathBuf> = vec![core_bundle.clone(), std_bundle];
    let (parser_tools, _) =
        ryeos_engine::parsers::ParserRegistry::load_base(&bundle_roots, &trust_store, &kinds)
            .expect("load parser tools");

    let native_handlers = ryeos_engine::test_support::load_live_handler_registry();
    let parser_dispatcher = ryeos_engine::parsers::ParserDispatcher::new(
        parser_tools,
        std::sync::Arc::clone(&native_handlers),
    );

    let composers = ryeos_engine::composers::ComposerRegistry::from_kinds(&kinds, &native_handlers)
        .expect("derive composers");

    ryeos_engine::engine::Engine::new(kinds, parser_dispatcher, bundle_roots)
        .with_trust_store(trust_store.clone())
        .with_node_trust_store(trust_store)
        .with_composers(composers)
}

fn local_plan_ctx() -> PlanContext {
    PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: "fp:test-gate".into(),
            scopes: vec![],
        }),
        project_context: ryeos_engine::contracts::ProjectContext::None,
        subject_resolution_authority:
            ryeos_engine::contracts::SubjectResolutionAuthority::Projectless,
        current_site_id: "site:local".into(),
        origin_site_id: "site:local".into(),
        execution_hints: ryeos_engine::contracts::ExecutionHints::default(),
        validate_only: true,
    }
}

/// Gate 1: Every installed Services-registry item resolves through the engine.
#[test]
fn gate_all_services_resolve() {
    let engine = build_test_engine();
    let ctx = local_plan_ctx();
    let services = service_refs(&engine);

    let mut missing = Vec::new();
    for canonical in &services {
        if engine.resolve(&ctx, canonical).is_err() {
            missing.push(canonical.to_string());
        }
    }

    assert!(
        missing.is_empty(),
        "operational services failed to resolve: {missing:?}"
    );
}

/// Gate 2: Every resolved service passes trust verification.
#[test]
fn gate_all_services_verify() {
    let engine = build_test_engine();
    let ctx = local_plan_ctx();
    let services = service_refs(&engine);

    let mut failed = Vec::new();
    for canonical in &services {
        let service_ref = canonical.to_string();
        let resolved = engine.resolve(&ctx, canonical).unwrap_or_else(|e| {
            panic!(
                "service `{service_ref}` should resolve (gate_all_services_resolve covers this): {e}"
            )
        });
        if let Err(e) = engine.verify(&ctx, resolved) {
            failed.push((service_ref, format!("{e}")));
        }
    }

    assert!(
        failed.is_empty(),
        "operational services failed verification: {failed:?}"
    );
}

/// Gate 3: Every verified service's `endpoint` matches the registered
/// handler endpoint declared in the descriptor table.
#[test]
fn gate_all_services_have_registered_handler() {
    let engine = build_test_engine();
    let ctx = local_plan_ctx();

    let mut unregistered = Vec::new();
    for canonical in service_refs(&engine) {
        let service_ref = canonical.to_string();
        let descriptor = descriptors()
            .iter()
            .find(|candidate| candidate.service_ref == service_ref)
            .unwrap_or_else(|| panic!("installed item `{service_ref}` has no compiled descriptor"));
        let resolved = engine.resolve(&ctx, &canonical).unwrap();
        let verified = engine.verify(&ctx, resolved).unwrap();
        let extra = &verified.resolved.metadata.extra;

        let endpoint = extra
            .get("endpoint")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        match endpoint {
            Some(ep) if ep == descriptor.endpoint => {}
            Some(ep) => unregistered.push((
                service_ref,
                format!(
                    "bundle endpoint `{ep}` != descriptor endpoint `{}`",
                    descriptor.endpoint
                ),
            )),
            None => unregistered.push((service_ref, "<no endpoint field>".into())),
        }

        let mut signed_caps = ryeos_app::service_registry::extract_required_caps(extra);
        signed_caps.sort();
        signed_caps.dedup();
        let mut compiled_caps = descriptor
            .required_caps
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>();
        compiled_caps.sort();
        compiled_caps.dedup();
        if signed_caps != compiled_caps {
            unregistered.push((
                descriptor.service_ref.to_owned(),
                format!(
                    "signed required_caps {signed_caps:?} != descriptor assertion {compiled_caps:?}"
                ),
            ));
        }
    }

    assert!(
        unregistered.is_empty(),
        "operational services with no registered handler: {unregistered:?}"
    );
}

/// Gate 4: Cap-sensitive services require non-empty caps, public services don't.
#[test]
fn gate_cap_consistency() {
    let engine = build_test_engine();
    let ctx = local_plan_ctx();

    // Services that MUST require caps
    let cap_required = [
        "service:commands/submit",
        "service:bundle/install",
        "service:bundle/remove",
        "service:maintenance/gc",
    ];

    // Services that MUST have empty caps (public)
    let cap_free = [
        "service:node/status",
        "service:threads/list",
        "service:threads/get",
        "service:bundle/list",
    ];

    for svc_ref in &cap_required {
        let canonical = CanonicalRef::parse(svc_ref).unwrap();
        let resolved = engine.resolve(&ctx, &canonical).unwrap();
        let verified = engine.verify(&ctx, resolved).unwrap();
        let extra = &verified.resolved.metadata.extra;

        let caps: Vec<String> = extra
            .get("required_caps")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        assert!(
            !caps.is_empty(),
            "cap-sensitive service `{svc_ref}` must declare non-empty required_caps; got: {caps:?}"
        );
    }

    for svc_ref in &cap_free {
        let canonical = CanonicalRef::parse(svc_ref).unwrap();
        let resolved = engine.resolve(&ctx, &canonical).unwrap();
        let verified = engine.verify(&ctx, resolved).unwrap();
        let extra = &verified.resolved.metadata.extra;

        let caps: Vec<String> = extra
            .get("required_caps")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        assert!(
            caps.is_empty(),
            "public service `{svc_ref}` should have empty required_caps; got: {caps:?}"
        );
    }
}

/// Gate 5: Cap enforcement logic — intersection of caller scopes ∩ required_caps.
/// All required caps must be present in caller scopes (AND semantics).
#[test]
fn gate_cap_enforcement_logic() {
    // Simulate the cap enforcement from execute.rs
    fn enforce(caller_scopes: &[&str], required_caps: &[&str]) -> (bool, Vec<String>) {
        let caller_set: std::collections::HashSet<&str> = caller_scopes.iter().copied().collect();
        let effective: Vec<String> = required_caps
            .iter()
            .filter(|cap| caller_set.contains(**cap))
            .map(|s| s.to_string())
            .collect();

        // Allowed only if: no caps required, OR all required caps are satisfied
        let allowed = required_caps.is_empty() || effective.len() == required_caps.len();
        (allowed, effective)
    }

    // Public service (empty required_caps) always passes
    let (ok, eff) = enforce(&["read"], &[]);
    assert!(ok);
    assert!(eff.is_empty());

    // Cap-sensitive with matching scope passes
    let (ok, eff) = enforce(&["commands.submit", "read"], &["commands.submit"]);
    assert!(ok);
    assert_eq!(eff, vec!["commands.submit"]);

    // Cap-sensitive with no matching scope fails
    let (ok, eff) = enforce(&["read"], &["commands.submit"]);
    assert!(!ok);
    assert!(eff.is_empty());

    // Multiple required caps, partial match fails
    let (ok, _) = enforce(
        &["commands.submit"],
        &["commands.submit", "node.maintenance"],
    );
    assert!(!ok);

    // Multiple required caps, full match passes
    let (ok, eff) = enforce(
        &["commands.submit", "node.maintenance"],
        &["commands.submit", "node.maintenance"],
    );
    assert!(ok);
    assert_eq!(eff.len(), 2);
}

/// Gate 6: the live bundle contains a signed kind selecting the Services
/// in-process registry. The concrete kind name is deliberately irrelevant.
#[test]
fn gate_services_registry_has_an_admitted_kind() {
    let trusted_dir = manifest_dir().join("tests/fixtures/trusted_signers");
    let trust_store = TrustStore::load_from_dir(&trusted_dir).expect("load trust store");

    let kinds_dir = workspace_root().join("bundles/core/.ai/node/engine/kinds");
    let kinds = KindRegistry::load_base(&[kinds_dir], &trust_store).expect("load kinds");

    let selected = kinds
        .kinds_for_in_process_registry(ryeos_engine::kind_registry::InProcessRegistryKind::Services)
        .map(|(kind, _)| kind)
        .collect::<Vec<_>>();
    assert!(
        !selected.is_empty(),
        "live bundle must admit at least one kind to the Services registry"
    );
}

/// Gate 7: compiled descriptor identity is unambiguous. Adding or removing a
/// handler does not require editing a count in Rust.
#[test]
fn gate_compiled_service_descriptors_are_unique() {
    let mut refs = std::collections::BTreeSet::new();
    let mut endpoints = std::collections::BTreeSet::new();
    for descriptor in descriptors() {
        assert!(
            refs.insert(descriptor.service_ref),
            "duplicate compiled service ref `{}`",
            descriptor.service_ref
        );
        assert!(
            endpoints.insert(descriptor.endpoint),
            "duplicate compiled service endpoint `{}`",
            descriptor.endpoint
        );
    }
}

/// Gate 8: Rust descriptors and bundle service YAMLs agree on
/// `required_caps`, every capability is canonical, and service-envelope caps
/// preserve `/` in the subject. A service may additionally require a
/// cross-cutting policy capability such as `ryeos.write.project.live`.
/// Catches drift like a YAML left empty while the Rust descriptor requires a
/// cap, or a dot-form service cap sneaking back in.
#[test]
fn gate_yaml_caps_match_descriptor_caps() {
    let engine = build_test_engine();
    let ctx = local_plan_ctx();

    let mut mismatched = Vec::new();
    let mut malformed = Vec::new();

    for canonical in service_refs(&engine) {
        let service_ref = canonical.to_string();
        let desc = descriptors()
            .iter()
            .find(|candidate| candidate.service_ref == service_ref)
            .unwrap_or_else(|| panic!("installed item `{service_ref}` has no descriptor"));
        let resolved = engine.resolve(&ctx, &canonical).unwrap();
        let verified = engine.verify(&ctx, resolved).unwrap();
        let extra = &verified.resolved.metadata.extra;

        let yaml_caps: Vec<String> = extra
            .get("required_caps")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let rust_caps: Vec<String> = desc.required_caps.iter().map(|s| s.to_string()).collect();

        if yaml_caps != rust_caps {
            mismatched.push((desc.service_ref, yaml_caps.clone(), rust_caps.clone()));
        }

        for cap in yaml_caps.iter().chain(rust_caps.iter()) {
            if let Err(error) = ryeos_runtime::authorizer::validate_scope_pattern(cap) {
                malformed.push((desc.service_ref, cap.clone(), error));
            } else if cap
                .strip_prefix("ryeos.execute.service.")
                .is_some_and(|subject| subject.contains('.'))
            {
                malformed.push((
                    desc.service_ref,
                    cap.clone(),
                    "dot in subject — service cap subjects are slash-form bare ids".to_string(),
                ));
            }
        }
    }

    assert!(
        mismatched.is_empty(),
        "service YAML required_caps must equal Rust descriptor required_caps; \
         drifted: {mismatched:#?}"
    );
    assert!(
        malformed.is_empty(),
        "required capabilities must be canonical and service-envelope subjects must be slash-form; violations: {malformed:#?}"
    );
}
