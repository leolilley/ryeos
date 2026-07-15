//! Bundle graph planner.
//!
//! Given candidate and/or installed bundles, this module builds a complete
//! dependency graph from signed manifests, detects invalid graph states, and
//! emits preflight verification jobs with exact transitive dependency roots.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};

use crate::manifest::{materialize_manifest_source, parse_manifest, BundleManifest};

pub type BundleName = String;
pub type KindName = String;

#[derive(Debug, Clone)]
pub struct BundlePlan {
    pub mode: BundlePlanMode,
    pub bundles: BTreeMap<BundleName, PlannedBundle>,
    pub provider_map: BTreeMap<KindName, BundleName>,
    pub install_order: Vec<BundleName>,
    pub verification_jobs: Vec<VerificationJob>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundlePlanMode {
    /// `ryeos init --source`: source bundles only, no installed state.
    InitSourceSet,
    /// `bundle install`: installed bundles plus one or more new candidates.
    Install,
    /// `bundle replace`: installed bundles with candidates replacing same-name bundles.
    Replace,
    /// `bundle update-set`: installed bundles with candidates replacing same-name bundles.
    UpdateSet,
    /// `bundle remove`: installed bundles with candidate names marked for removal.
    ///
    /// Candidate paths are not consumed; installed inputs remain the authority
    /// for every manifest in the exact post-removal graph.
    Remove,
    /// Verify all registered installed bundles without mutation.
    VerifyInstalled,
}

#[derive(Debug, Clone)]
pub struct PlannedBundle {
    pub name: BundleName,
    pub source: BundleSource,
    pub manifest: BundleManifest,
    pub action: BundleAction,
    /// Direct dependency bundle names (one hop).
    pub direct_dependencies: BTreeSet<BundleName>,
    /// Full transitive dependency closure.
    pub dependency_closure: BTreeSet<BundleName>,
}

#[derive(Debug, Clone)]
pub struct VerificationJob {
    pub subject: BundleName,
    pub subject_root: PathBuf,
    pub subject_kind: VerificationSubjectKind,
    /// Exact dependency roots for the subject's transitive dependency closure.
    pub dependency_roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationSubjectKind {
    CandidateSource,
    InstalledBundle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleAction {
    Keep,
    Install,
    Replace,
    Remove,
}

#[derive(Debug, Clone)]
pub enum BundleSource {
    /// Source directory that is not yet installed.
    SourceDir(PathBuf),
    /// Already installed bundle with a signed registration.
    Installed {
        registration_path: PathBuf,
        bundle_root: PathBuf,
    },
}

impl BundleSource {
    pub fn root_path(&self) -> &PathBuf {
        match self {
            BundleSource::SourceDir(path) => path,
            BundleSource::Installed { bundle_root, .. } => bundle_root,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlanInput {
    pub name: BundleName,
    pub source: BundleSource,
}

/// Build a fully resolved bundle plan.
pub fn build_plan(
    mode: BundlePlanMode,
    candidates: &[PlanInput],
    installed: &[PlanInput],
) -> Result<BundlePlan> {
    validate_input_names("candidate", candidates)?;
    validate_input_names("installed", installed)?;
    validate_mode_policy(mode, candidates, installed)?;

    let effective = build_effective_graph(mode, candidates, installed)?;

    let mut bundles: BTreeMap<BundleName, PlannedBundle> = BTreeMap::new();
    for input in &effective {
        if bundles.contains_key(&input.name) {
            bail!(
                "duplicate bundle '{}' in effective bundle graph",
                input.name
            );
        }

        let manifest = match (mode, &input.source) {
            // Init is the one development boundary allowed to materialize
            // publisher input explicitly when no generated manifest exists.
            // Runtime/install planning always consumes the generated manifest.
            (BundlePlanMode::InitSourceSet, BundleSource::SourceDir(_)) => {
                let generated = input
                    .source
                    .root_path()
                    .join(ryeos_engine::AI_DIR)
                    .join("manifest.yaml");
                if generated.try_exists().with_context(|| {
                    format!("inspect generated manifest for bundle '{}'", input.name)
                })? {
                    parse_manifest(input.source.root_path(), &input.name)
                } else {
                    materialize_manifest_source(input.source.root_path(), &input.name)
                }
            }
            _ => parse_manifest(input.source.root_path(), &input.name),
        }
        .with_context(|| format!("parse manifest for bundle '{}'", input.name))?;

        let action = determine_action(mode, &input.name, candidates, installed);
        bundles.insert(
            input.name.clone(),
            PlannedBundle {
                name: input.name.clone(),
                source: input.source.clone(),
                manifest,
                action,
                direct_dependencies: BTreeSet::new(),
                dependency_closure: BTreeSet::new(),
            },
        );
    }

    let provider_map = build_provider_map(&bundles)?;
    resolve_dependencies(&mut bundles, &provider_map)?;
    detect_cycles(&bundles)?;
    compute_dependency_closures(&mut bundles)?;
    let install_order = compute_install_order(&bundles)?;
    let verification_jobs = emit_verification_jobs(mode, &bundles, &install_order, candidates)?;

    Ok(BundlePlan {
        mode,
        bundles,
        provider_map,
        install_order,
        verification_jobs,
    })
}

fn validate_input_names(label: &str, inputs: &[PlanInput]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for input in inputs {
        if !seen.insert(input.name.as_str()) {
            bail!("duplicate {label} bundle name '{}'", input.name);
        }
    }
    Ok(())
}

fn validate_mode_policy(
    mode: BundlePlanMode,
    candidates: &[PlanInput],
    installed: &[PlanInput],
) -> Result<()> {
    let installed_names: BTreeSet<&str> =
        installed.iter().map(|input| input.name.as_str()).collect();

    match mode {
        BundlePlanMode::InitSourceSet => {
            if !installed.is_empty() {
                bail!("InitSourceSet planning must not include installed bundles");
            }
        }
        BundlePlanMode::Install => {
            for candidate in candidates {
                if installed_names.contains(candidate.name.as_str()) {
                    bail!(
                        "bundle '{}' is already installed; use replace/update instead of install",
                        candidate.name
                    );
                }
            }
        }
        BundlePlanMode::Replace | BundlePlanMode::UpdateSet => {
            for candidate in candidates {
                if !installed_names.contains(candidate.name.as_str()) {
                    bail!(
                        "bundle '{}' is not installed; replace/update requires an existing bundle",
                        candidate.name
                    );
                }
            }
        }
        BundlePlanMode::Remove => {
            if candidates.is_empty() {
                bail!("Remove planning requires at least one installed bundle name");
            }
            for candidate in candidates {
                if !installed_names.contains(candidate.name.as_str()) {
                    bail!(
                        "bundle '{}' is not installed; remove requires an existing bundle",
                        candidate.name
                    );
                }
            }
        }
        BundlePlanMode::VerifyInstalled => {
            if !candidates.is_empty() {
                bail!("VerifyInstalled planning must not include candidate bundles");
            }
        }
    }

    Ok(())
}

fn build_effective_graph(
    mode: BundlePlanMode,
    candidates: &[PlanInput],
    installed: &[PlanInput],
) -> Result<Vec<PlanInput>> {
    match mode {
        BundlePlanMode::InitSourceSet => Ok(candidates.to_vec()),
        BundlePlanMode::Install => {
            let mut graph = installed.to_vec();
            graph.extend_from_slice(candidates);
            Ok(graph)
        }
        BundlePlanMode::Replace | BundlePlanMode::UpdateSet => {
            let candidate_names: HashSet<&str> = candidates
                .iter()
                .map(|candidate| candidate.name.as_str())
                .collect();
            let mut graph: Vec<PlanInput> = installed
                .iter()
                .filter(|input| !candidate_names.contains(input.name.as_str()))
                .cloned()
                .collect();
            graph.extend_from_slice(candidates);
            Ok(graph)
        }
        BundlePlanMode::Remove => Ok(installed.to_vec()),
        BundlePlanMode::VerifyInstalled => Ok(installed.to_vec()),
    }
}

fn build_provider_map(
    bundles: &BTreeMap<BundleName, PlannedBundle>,
) -> Result<BTreeMap<KindName, BundleName>> {
    let mut provider_map = BTreeMap::new();
    for (name, bundle) in bundles {
        if bundle.action == BundleAction::Remove {
            continue;
        }
        for kind in &bundle.manifest.provides_kinds {
            if let Some(existing) = provider_map.get(kind) {
                bail!(
                    "duplicate provider for kind '{}': both '{}' and '{}' provide it",
                    kind,
                    existing,
                    name
                );
            }
            provider_map.insert(kind.clone(), name.clone());
        }
    }
    Ok(provider_map)
}

fn resolve_dependencies(
    bundles: &mut BTreeMap<BundleName, PlannedBundle>,
    provider_map: &BTreeMap<KindName, BundleName>,
) -> Result<()> {
    let external_kinds_by_bundle: Vec<(BundleName, Vec<KindName>)> = bundles
        .iter()
        .filter(|(_, bundle)| bundle.action != BundleAction::Remove)
        .map(|(name, bundle)| {
            let provided: HashSet<&str> = bundle
                .manifest
                .provides_kinds
                .iter()
                .map(|kind| kind.as_str())
                .collect();
            let external: BTreeSet<KindName> = bundle
                .manifest
                .requires_kinds
                .iter()
                .chain(bundle.manifest.uses_kinds.iter())
                .filter(|kind| !provided.contains(kind.as_str()))
                .cloned()
                .collect();
            (name.clone(), external.into_iter().collect())
        })
        .collect();

    for (name, external_kinds) in external_kinds_by_bundle {
        let mut deps = BTreeSet::new();
        for kind in external_kinds {
            let provider = provider_map.get(&kind).ok_or_else(|| {
                anyhow!(
                    "bundle '{}' requires/uses kind '{}' but no bundle in the effective graph provides it",
                    name,
                    kind
                )
            })?;
            deps.insert(provider.clone());
        }
        if let Some(bundle) = bundles.get_mut(&name) {
            bundle.direct_dependencies = deps;
        }
    }

    Ok(())
}

fn detect_cycles(bundles: &BTreeMap<BundleName, PlannedBundle>) -> Result<()> {
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    let mut path = Vec::new();

    for name in bundles.keys() {
        dfs_detect(name, bundles, &mut visiting, &mut visited, &mut path)?;
    }

    Ok(())
}

fn dfs_detect(
    name: &str,
    bundles: &BTreeMap<BundleName, PlannedBundle>,
    visiting: &mut HashSet<BundleName>,
    visited: &mut HashSet<BundleName>,
    path: &mut Vec<BundleName>,
) -> Result<()> {
    if visited.contains(name) {
        return Ok(());
    }
    if visiting.contains(name) {
        bail!(
            "bundle dependency cycle detected: {} -> {}",
            path.join(" -> "),
            name
        );
    }

    visiting.insert(name.to_string());
    path.push(name.to_string());

    if let Some(bundle) = bundles.get(name) {
        for dep in &bundle.direct_dependencies {
            dfs_detect(dep, bundles, visiting, visited, path)?;
        }
    }

    path.pop();
    visiting.remove(name);
    visited.insert(name.to_string());
    Ok(())
}

fn compute_dependency_closures(bundles: &mut BTreeMap<BundleName, PlannedBundle>) -> Result<()> {
    let order = compute_install_order(bundles)?;

    for name in order {
        let closure = {
            let bundle = bundles.get(&name).context(format!("bundle {name}"))?;
            let mut closure = BTreeSet::new();
            for dep in &bundle.direct_dependencies {
                closure.insert(dep.clone());
                if let Some(dep_bundle) = bundles.get(dep) {
                    closure.extend(dep_bundle.dependency_closure.iter().cloned());
                }
            }
            closure
        };
        if let Some(bundle) = bundles.get_mut(&name) {
            bundle.dependency_closure = closure;
        }
    }

    Ok(())
}

fn compute_install_order(bundles: &BTreeMap<BundleName, PlannedBundle>) -> Result<Vec<BundleName>> {
    let mut in_degree: BTreeMap<BundleName, usize> = bundles
        .iter()
        .filter(|(_, bundle)| bundle.action != BundleAction::Remove)
        .map(|(name, _)| (name.clone(), 0))
        .collect();
    let mut dependents: BTreeMap<BundleName, BTreeSet<BundleName>> = bundles
        .iter()
        .filter(|(_, bundle)| bundle.action != BundleAction::Remove)
        .map(|(name, _)| (name.clone(), BTreeSet::new()))
        .collect();

    for (name, bundle) in bundles {
        if bundle.action == BundleAction::Remove {
            continue;
        }
        for dep in &bundle.direct_dependencies {
            *in_degree
                .get_mut(name)
                .context(format!("missing in-degree entry for {name}"))? += 1;
            dependents
                .get_mut(dep)
                .context(format!("bundle {name} depends on unknown bundle {dep}"))?
                .insert(name.clone());
        }
    }

    let mut queue: BTreeSet<BundleName> = in_degree
        .iter()
        .filter_map(|(name, degree)| (*degree == 0).then_some(name.clone()))
        .collect();
    let mut sorted = Vec::new();

    while let Some(name) = queue.iter().next().cloned() {
        queue.remove(&name);
        sorted.push(name.clone());

        for dependent in dependents.get(&name).into_iter().flatten() {
            let degree = in_degree
                .get_mut(dependent)
                .context(format!("missing in-degree entry for {dependent}"))?;
            *degree -= 1;
            if *degree == 0 {
                queue.insert(dependent.clone());
            }
        }
    }

    if sorted.len() != in_degree.len() {
        bail!(
            "bundle dependency cycle detected (topological sort incomplete): sorted {} of {} bundles",
            sorted.len(),
            in_degree.len()
        );
    }

    Ok(sorted)
}

fn determine_action(
    mode: BundlePlanMode,
    name: &str,
    candidates: &[PlanInput],
    installed: &[PlanInput],
) -> BundleAction {
    let is_candidate = candidates.iter().any(|candidate| candidate.name == name);
    let is_installed = installed.iter().any(|input| input.name == name);

    match mode {
        BundlePlanMode::InitSourceSet => BundleAction::Install,
        BundlePlanMode::Install => {
            if is_candidate {
                BundleAction::Install
            } else {
                BundleAction::Keep
            }
        }
        BundlePlanMode::Replace | BundlePlanMode::UpdateSet => {
            if is_candidate && is_installed {
                BundleAction::Replace
            } else if is_candidate {
                BundleAction::Install
            } else {
                BundleAction::Keep
            }
        }
        BundlePlanMode::Remove => {
            if is_candidate {
                BundleAction::Remove
            } else {
                BundleAction::Keep
            }
        }
        BundlePlanMode::VerifyInstalled => BundleAction::Keep,
    }
}

fn emit_verification_jobs(
    mode: BundlePlanMode,
    bundles: &BTreeMap<BundleName, PlannedBundle>,
    install_order: &[BundleName],
    candidates: &[PlanInput],
) -> Result<Vec<VerificationJob>> {
    let candidate_names: BTreeSet<&str> =
        candidates.iter().map(|input| input.name.as_str()).collect();
    let mut jobs = Vec::new();

    for name in install_order {
        let bundle = bundles.get(name).context(format!("bundle {name}"))?;
        let should_verify = match mode {
            BundlePlanMode::InitSourceSet => true,
            BundlePlanMode::Install | BundlePlanMode::Replace | BundlePlanMode::UpdateSet => {
                candidate_names.contains(name.as_str())
            }
            BundlePlanMode::Remove => true,
            BundlePlanMode::VerifyInstalled => true,
        };
        if !should_verify {
            continue;
        }

        let dependency_roots = install_order
            .iter()
            .filter(|dep_name| bundle.dependency_closure.contains(dep_name.as_str()))
            .map(|dep_name| {
                bundles
                    .get(dep_name)
                    .map(|dep| dep.source.root_path().clone())
                    .context(format!("dependency bundle {dep_name}"))
            })
            .collect::<Result<Vec<_>>>()?;

        jobs.push(VerificationJob {
            subject: name.clone(),
            subject_root: bundle.source.root_path().clone(),
            subject_kind: match bundle.source {
                BundleSource::SourceDir(_) => VerificationSubjectKind::CandidateSource,
                BundleSource::Installed { .. } => VerificationSubjectKind::InstalledBundle,
            },
            dependency_roots,
        });
    }

    Ok(jobs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_bundle(
        root: &std::path::Path,
        name: &str,
        provides: &[&str],
        requires: &[&str],
        uses: &[&str],
    ) -> PathBuf {
        let bundle = root.join(name);
        fs::create_dir_all(bundle.join(".ai")).unwrap();
        let manifest = format!(
            "name: {name}\nversion: '1.0'\nprovides_kinds:\n{}requires_kinds:\n{}uses_kinds:\n{}",
            yaml_list(provides),
            yaml_list(requires),
            yaml_list(uses),
        );
        fs::write(bundle.join(".ai/manifest.yaml"), manifest).unwrap();
        bundle
    }

    fn yaml_list(values: &[&str]) -> String {
        if values.is_empty() {
            " []\n".to_string()
        } else {
            values
                .iter()
                .map(|value| format!("\n  - {value}"))
                .collect::<String>()
                + "\n"
        }
    }

    fn input(name: &str, root: PathBuf) -> PlanInput {
        PlanInput {
            name: name.to_string(),
            source: BundleSource::SourceDir(root),
        }
    }

    #[test]
    fn init_plan_orders_dependencies_before_dependents_and_emits_jobs_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let core = write_bundle(tmp.path(), "core", &["config", "knowledge"], &[], &[]);
        let standard = write_bundle(
            tmp.path(),
            "standard",
            &["directive"],
            &["config"],
            &["knowledge"],
        );

        let plan = build_plan(
            BundlePlanMode::InitSourceSet,
            &[
                input("standard", standard.clone()),
                input("core", core.clone()),
            ],
            &[],
        )
        .unwrap();

        assert_eq!(plan.install_order, vec!["core", "standard"]);
        assert_eq!(
            plan.verification_jobs
                .iter()
                .map(|job| job.subject.as_str())
                .collect::<Vec<_>>(),
            vec!["core", "standard"]
        );
        assert_eq!(plan.verification_jobs[1].dependency_roots, vec![core]);
        assert_eq!(
            plan.bundles["standard"].dependency_closure,
            BTreeSet::from(["core".to_string()])
        );
    }

    #[test]
    fn verification_jobs_use_exact_multihop_closure_in_install_order() {
        let tmp = tempfile::tempdir().unwrap();
        let a = write_bundle(tmp.path(), "a", &["a-kind"], &[], &[]);
        let b = write_bundle(tmp.path(), "b", &["b-kind"], &["a-kind"], &[]);
        let c = write_bundle(tmp.path(), "c", &["c-kind"], &["b-kind"], &[]);

        let plan = build_plan(
            BundlePlanMode::InitSourceSet,
            &[input("c", c), input("b", b.clone()), input("a", a.clone())],
            &[],
        )
        .unwrap();

        assert_eq!(plan.install_order, vec!["a", "b", "c"]);
        assert_eq!(
            plan.bundles["c"].dependency_closure,
            BTreeSet::from(["a".to_string(), "b".to_string()])
        );
        assert_eq!(plan.verification_jobs[2].subject, "c");
        assert_eq!(plan.verification_jobs[2].dependency_roots, vec![a, b]);
    }

    #[test]
    fn duplicate_providers_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let a = write_bundle(tmp.path(), "a", &["config"], &[], &[]);
        let b = write_bundle(tmp.path(), "b", &["config"], &[], &[]);

        let err = build_plan(
            BundlePlanMode::InitSourceSet,
            &[input("a", a), input("b", b)],
            &[],
        )
        .unwrap_err();
        assert!(err.to_string().contains("duplicate provider"));
    }

    #[test]
    fn install_rejects_existing_candidate_name() {
        let tmp = tempfile::tempdir().unwrap();
        let installed = write_bundle(tmp.path(), "core", &["config"], &[], &[]);
        let candidate = write_bundle(tmp.path(), "core-new", &["config"], &[], &[]);
        let candidate = PlanInput {
            name: "core".to_string(),
            source: BundleSource::SourceDir(candidate),
        };
        let installed = PlanInput {
            name: "core".to_string(),
            source: BundleSource::Installed {
                registration_path: tmp.path().join("core.yaml"),
                bundle_root: installed,
            },
        };

        let err = build_plan(BundlePlanMode::Install, &[candidate], &[installed]).unwrap_err();
        assert!(err.to_string().contains("already installed"));
    }
}
