//! Scheduler selection from a node's installed, trusted composition.
//!
//! The build only makes the scheduler available; the signed service items a
//! node installs decide whether that node runs it. The daemon's startup and
//! the stopped-node maintenance commands decide the same way through this one
//! function.

use anyhow::{Context, Result};

/// Signed service items whose compiled handlers operate the scheduler
/// participant. A node's composition selects the scheduler exactly when it
/// installs at least one of them. The refs are literal so the selection can be
/// read by a build that does not compile the handlers.
const SCHEDULER_SERVICE_REFS: [&str; 7] = [
    "service:scheduler/register",
    "service:scheduler/deregister",
    "service:scheduler/explain",
    "service:scheduler/list",
    "service:scheduler/show_fires",
    "service:scheduler/pause",
    "service:scheduler/resume",
];

/// Combine the composition's selection with what this build can run. A build
/// without the scheduler refuses a composition that selects it instead of
/// silently running scheduler-free; it proceeds scheduler-free only when the
/// composition does not select the scheduler.
pub fn scheduler_selection_for_build(
    composition_selects: bool,
    build_has_scheduler: bool,
) -> Result<bool> {
    anyhow::ensure!(
        build_has_scheduler || !composition_selects,
        "this build lacks the scheduler but the installed composition selects it; rebuild with the `scheduling` feature, or switch the node to a scheduler-free profile with `ryeos node reset policy-generation`"
    );
    Ok(composition_selects)
}

/// The scheduler selection for this node: the installed composition decides,
/// and a build that cannot run a selected scheduler refuses. Callers pass
/// `cfg!(feature = "scheduling")` for `build_has_scheduler`.
pub fn resolve_scheduler_selection(
    engine: &ryeos_engine::engine::Engine,
    build_has_scheduler: bool,
) -> Result<bool> {
    scheduler_selection_for_build(
        scheduler_selected_by_composition(engine)?,
        build_has_scheduler,
    )
}

/// Whether the node's installed, trusted composition selects the scheduler.
/// The build only makes the scheduler available; the installed service items
/// decide whether this node runs it. An installed scheduler service item that
/// does not resolve or is not trusted refuses rather than silently leaving the
/// scheduler unselected.
pub fn scheduler_selected_by_composition(engine: &ryeos_engine::engine::Engine) -> Result<bool> {
    engine.with_checked_bundle_generation(|generation| -> Result<bool> {
        let roots = engine.resolution_roots(None);
        let scheduler_refs = ryeos_engine::item_resolution::enumerate_in_process_registry_refs(
            &roots,
            &engine.kinds,
            ryeos_engine::kind_registry::InProcessRegistryKind::Services,
        )
        .context("enumerate installed Services-registry items")?
        .into_iter()
        .filter(|canonical| SCHEDULER_SERVICE_REFS.contains(&canonical.to_string().as_str()))
        .collect::<Vec<_>>();
        let requests = scheduler_refs
            .iter()
            .map(|canonical| ryeos_engine::engine::EffectiveItemRequest {
                item_ref: canonical.clone(),
                expected_kind: Some(canonical.kind.clone()),
                project_root: None,
                subject_resolution_authority:
                    ryeos_engine::contracts::SubjectResolutionAuthority::Projectless,
            })
            .collect::<Vec<_>>();
        for (canonical, effective) in scheduler_refs
            .iter()
            .zip(generation.effective_items(&requests))
        {
            let effective = effective
                .with_context(|| format!("resolve installed scheduler service {canonical}"))?;
            anyhow::ensure!(
                effective.trusted,
                "installed scheduler service {canonical} is not trusted"
            );
        }
        Ok(!scheduler_refs.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use super::{
        SCHEDULER_SERVICE_REFS, scheduler_selected_by_composition, scheduler_selection_for_build,
    };

    #[test]
    fn a_build_without_the_scheduler_refuses_a_composition_that_selects_it() {
        let error = scheduler_selection_for_build(true, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("this build lacks the scheduler"), "{error}");
        assert!(
            error.contains("ryeos node reset policy-generation"),
            "{error}"
        );
    }

    #[test]
    fn a_build_without_the_scheduler_proceeds_when_the_composition_does_not_select_it() {
        assert!(!scheduler_selection_for_build(false, false).unwrap());
        assert!(!scheduler_selection_for_build(false, true).unwrap());
        assert!(scheduler_selection_for_build(true, true).unwrap());
    }

    #[test]
    fn the_service_ref_list_matches_the_scheduling_bundle_service_items() {
        let dir = scheduling_bundle_root().join(".ai/services/scheduler");
        let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
            .expect("read scheduling bundle scheduler services")
            .map(|entry| {
                let path = entry.expect("directory entry").path();
                assert_eq!(
                    path.extension().and_then(|e| e.to_str()),
                    Some("yaml"),
                    "{path:?}"
                );
                format!(
                    "service:scheduler/{}",
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .expect("utf-8 file stem")
                )
            })
            .collect();
        on_disk.sort();
        let mut listed: Vec<String> = SCHEDULER_SERVICE_REFS
            .iter()
            .map(|r| (*r).to_owned())
            .collect();
        listed.sort();
        assert_eq!(on_disk, listed);
    }

    fn scheduling_bundle_root() -> PathBuf {
        ryeos_engine::test_support::workspace_root().join("bundles/scheduling")
    }

    /// An engine over the live core bundle plus `extra_roots`, trusting the
    /// live publisher keys only.
    fn engine_over(extra_roots: &[&Path]) -> ryeos_engine::engine::Engine {
        let trust_store = ryeos_engine::test_support::live_trust_store();
        let core_bundle = ryeos_engine::test_support::core_bundle_root();
        let kinds = ryeos_engine::kind_registry::KindRegistry::load_base(
            &[core_bundle.join(".ai/node/engine/kinds")],
            &trust_store,
        )
        .expect("load core kind registry");
        let mut bundle_roots = vec![core_bundle];
        bundle_roots.extend(extra_roots.iter().map(|root| root.to_path_buf()));
        let (parser_tools, _) =
            ryeos_engine::parsers::ParserRegistry::load_base(&bundle_roots, &trust_store, &kinds)
                .expect("load core parser tools");
        let native_handlers = ryeos_engine::test_support::load_live_handler_registry();
        let parser_dispatcher = ryeos_engine::parsers::ParserDispatcher::new(
            parser_tools,
            Arc::clone(&native_handlers),
        );
        let composers =
            ryeos_engine::composers::ComposerRegistry::from_kinds(&kinds, &native_handlers)
                .expect("derive core composers");
        ryeos_engine::engine::Engine::new(kinds, parser_dispatcher, bundle_roots)
            .with_trust_store(trust_store.clone())
            .with_node_trust_store(trust_store)
            .with_composers(composers)
    }

    /// The live scheduler list service body without its signature line.
    fn scheduler_list_body() -> String {
        let source = std::fs::read_to_string(
            scheduling_bundle_root().join(".ai/services/scheduler/list.yaml"),
        )
        .expect("read live scheduler list service");
        source
            .lines()
            .filter(|line| !line.contains(lillux::signature::SIGNATURE_PREFIX))
            .map(|line| format!("{line}\n"))
            .collect()
    }

    /// A bundle root installing only `services/scheduler/list.yaml`.
    fn root_with_scheduler_list(content: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("temp bundle root");
        let services = root.path().join(".ai/services/scheduler");
        std::fs::create_dir_all(&services).expect("create services directory");
        std::fs::write(services.join("list.yaml"), content).expect("write scheduler service");
        root
    }

    fn refusal(engine: &ryeos_engine::engine::Engine) -> String {
        let error = scheduler_selected_by_composition(engine)
            .expect_err("an installed scheduler service that cannot be admitted must refuse");
        format!("{error:#}")
    }

    #[test]
    fn a_composition_without_scheduler_services_does_not_select_the_scheduler() {
        let engine = engine_over(&[]);
        assert!(!scheduler_selected_by_composition(&engine).unwrap());
    }

    #[test]
    fn installed_trusted_scheduler_services_select_the_scheduler() {
        let scheduling = scheduling_bundle_root();
        let engine = engine_over(&[scheduling.as_path()]);
        assert!(scheduler_selected_by_composition(&engine).unwrap());
    }

    #[test]
    fn an_unsigned_scheduler_service_refuses_instead_of_deselecting() {
        let root = root_with_scheduler_list(&scheduler_list_body());
        let engine = engine_over(&[root.path()]);
        let refusal = refusal(&engine);
        assert!(refusal.contains("service:scheduler/list"), "{refusal}");
    }

    #[test]
    fn a_scheduler_service_signed_by_an_unknown_key_refuses_instead_of_deselecting() {
        let unknown = lillux::crypto::SigningKey::from_bytes(&[0x5a; 32]);
        let signed = lillux::signature::sign_content(&scheduler_list_body(), &unknown, "#", None);
        let root = root_with_scheduler_list(&signed);
        let engine = engine_over(&[root.path()]);
        let refusal = refusal(&engine);
        assert!(refusal.contains("service:scheduler/list"), "{refusal}");
    }

    #[test]
    fn an_unresolvable_scheduler_service_refuses_instead_of_deselecting() {
        let root = root_with_scheduler_list("category: [unterminated\n");
        let engine = engine_over(&[root.path()]);
        let refusal = refusal(&engine);
        assert!(refusal.contains("service:scheduler/list"), "{refusal}");
    }
}
