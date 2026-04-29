//! End-to-end gate tests for V5.2 service dispatch.
//!
//! Validates the full chain:
//!   1. Every service ref in the descriptor table resolves through the engine
//!   2. Every resolved service verifies (trust chain)
//!   3. Every verified service's `endpoint` field matches a registered handler
//!   4. Every verified service's `required_caps` is consistent with expectations
//!   5. Capability enforcement rejects callers without required caps
//!
//! These are the same checks the daemon self-check performs at startup,
//! duplicated here as regression guards that run in CI without a live daemon.

use std::path::PathBuf;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, Principal};
use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::trust::TrustStore;
use ryeosd::{service_handlers, ServiceDescriptor};

/// Iterate the canonical descriptor table.
fn descriptors() -> &'static [ServiceDescriptor] {
    service_handlers::ALL
}

/// Iterate every `service_ref` in the descriptor table.
fn service_refs() -> Vec<&'static str> {
    descriptors().iter().map(|d| d.service_ref).collect()
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workspace_root() -> PathBuf {
    manifest_dir().parent().expect("ryeosd has a parent dir").to_path_buf()
}

/// Build an engine-like fixture using the live bundle + trusted signers.
/// Mirrors `engine_init::build_engine` but uses the test fixture trust store
/// instead of the daemon's three-tier loader.
fn build_test_engine() -> ryeos_engine::engine::Engine {
    let trusted_dir = manifest_dir().join("tests/fixtures/trusted_signers");
    let trust_store = TrustStore::load_from_dir(&trusted_dir).expect("load trust store");

    let workspace = workspace_root();
    let kinds_dir = workspace.join("ryeos-bundles/core/.ai/node/engine/kinds");
    let kinds =
        KindRegistry::load_base(&[kinds_dir.clone()], &trust_store).expect("load kind registry");

    let bundle_root = workspace.join("ryeos-bundles/core");
    let (parser_tools, _) = ryeos_engine::parsers::ParserRegistry::load_base(
        &[bundle_root.clone()],
        &trust_store,
        &kinds,
    )
    .expect("load parser tools");

    let native_handlers = ryeos_engine::parsers::NativeParserHandlerRegistry::with_builtins();
    let parser_dispatcher =
        ryeos_engine::parsers::ParserDispatcher::new(parser_tools, native_handlers);

    let native_composers = ryeos_engine::composers::NativeComposerHandlerRegistry::with_builtins();
    let composers =
        ryeos_engine::composers::ComposerRegistry::from_kinds(&kinds, &native_composers)
            .expect("derive composers");

    ryeos_engine::engine::Engine::new(
        kinds,
        parser_dispatcher,
        None,
        vec![bundle_root],
    )
    .with_trust_store(trust_store)
    .with_composers(composers)
}

fn local_plan_ctx() -> PlanContext {
    PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: "fp:test-gate".into(),
            scopes: vec![],
        }),
        project_context: ryeos_engine::contracts::ProjectContext::None,
        current_site_id: "site:local".into(),
        origin_site_id: "site:local".into(),
        execution_hints: ryeos_engine::contracts::ExecutionHints::default(),
        validate_only: true,
    }
}

/// Gate 1: Every operational service ref resolves through the engine.
#[test]
fn gate_all_services_resolve() {
    let engine = build_test_engine();
    let ctx = local_plan_ctx();
    let services = service_refs();

    let mut missing = Vec::new();
    for svc_ref in &services {
        let canonical = CanonicalRef::parse(*svc_ref).unwrap_or_else(|e| {
            panic!("descriptor table contains unparseable ref `{svc_ref}`: {e}")
        });
        if engine.resolve(&ctx, &canonical).is_err() {
            missing.push(*svc_ref);
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
    let services = service_refs();

    let mut failed = Vec::new();
    for svc_ref in &services {
        let canonical = CanonicalRef::parse(*svc_ref).unwrap();
        let resolved = engine.resolve(&ctx, &canonical).unwrap_or_else(|e| {
            panic!("service `{svc_ref}` should resolve (gate_all_services_resolve covers this): {e}")
        });
        if let Err(e) = engine.verify(&ctx, resolved) {
            failed.push((*svc_ref, format!("{e}")));
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
    for desc in descriptors() {
        let canonical = CanonicalRef::parse(desc.service_ref).unwrap();
        let resolved = engine.resolve(&ctx, &canonical).unwrap();
        let verified = engine.verify(&ctx, resolved).unwrap();
        let extra = &verified.resolved.metadata.extra;

        let endpoint = extra
            .get("endpoint")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        match endpoint {
            Some(ep) if ep == desc.endpoint => {}
            Some(ep) => unregistered.push((
                desc.service_ref,
                format!("bundle endpoint `{ep}` != descriptor endpoint `{}`", desc.endpoint),
            )),
            None => unregistered.push((desc.service_ref, "<no endpoint field>".into())),
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
        "service:system/status",
        "service:identity/public_key",
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
        let allowed = required_caps.is_empty()
            || effective.len() == required_caps.len();
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
    let (ok, _) = enforce(&["commands.submit"], &["commands.submit", "node.maintenance"]);
    assert!(!ok);

    // Multiple required caps, full match passes
    let (ok, eff) = enforce(&["commands.submit", "node.maintenance"], &["commands.submit", "node.maintenance"]);
    assert!(ok);
    assert_eq!(eff.len(), 2);
}

/// Gate 6: `service` kind is present in the live bundle.
#[test]
fn gate_service_kind_in_bundle() {
    let trusted_dir = manifest_dir().join("tests/fixtures/trusted_signers");
    let trust_store = TrustStore::load_from_dir(&trusted_dir).expect("load trust store");

    let kinds_dir = workspace_root().join("ryeos-bundles/core/.ai/node/engine/kinds");
    let kinds = KindRegistry::load_base(&[kinds_dir], &trust_store).expect("load kinds");

    assert!(
        kinds.contains("service"),
        "live bundle must contain `service` kind; loaded kinds = {:?}",
        kinds.kinds().collect::<Vec<_>>()
    );

    let service_kind = kinds.get("service").expect("service kind");
    // V5.3 Task 0a.2: service kind is now schema-driven via the
    // `in_process_handler { services }` terminator. Backed by the same
    // ServiceDescriptor table; the schema just declares the dispatch
    // path. Wired by 0a.3.
    assert!(
        service_kind.is_executable(),
        "`service` kind must declare an execution block in V5.3 \
         (terminator: in_process_handler, registry: services)"
    );
}

/// Gate 7: Service descriptor table has exactly 17 entries.
#[test]
fn gate_service_count_matches_expected() {
    let services = service_refs();
    assert_eq!(
        services.len(),
        17,
        "service descriptor table count drifted from expected 17"
    );
}
