//! rye-verify — verify CAS chain integrity

use anyhow::{Context, Result};
use clap::Parser;
use ryeos_tools::get_state_root;
use serde::Serialize;
use std::path::PathBuf;
use lillux;

#[derive(Parser)]
#[command(name = "rye-verify")]
#[command(about = "Verify CAS chain integrity")]
struct Args {
    /// Chain root ID to verify (omit for --all)
    chain_id: Option<String>,

    /// Verify all chains + project heads
    #[arg(long)]
    all: bool,

    /// RYE_STATE directory
    #[arg(long)]
    state_dir: Option<PathBuf>,

    /// Output format (json or text)
    #[arg(short, long, default_value = "text")]
    format: String,
}

#[derive(Debug, Serialize)]
struct VerifyReport {
    chain_id: Option<String>,
    valid: bool,
    events_count: u64,
    last_verified_hash: Option<String>,
    project_issues: Vec<String>,
    issues: Vec<String>,
}

#[derive(Debug, Serialize)]
struct AllVerifyReport {
    chains_verified: usize,
    projects_verified: usize,
    reachable_objects: usize,
    reachable_blobs: usize,
    total_issues: usize,
    chain_reports: Vec<ChainSummary>,
    project_reports: Vec<ProjectSummary>,
}

#[derive(Debug, Serialize)]
struct ChainSummary {
    chain_id: String,
    valid: bool,
    issues_count: usize,
}

#[derive(Debug, Serialize)]
struct ProjectSummary {
    project_hash: String,
    valid: bool,
    issues_count: usize,
}

fn main() -> Result<()> {
    ryeos_tracing::init_subscriber(ryeos_tracing::SubscriberConfig::for_cli_tool());

    let args = Args::parse();
    let state_root = args.state_dir.or_else(|| get_state_root().ok())
        .context("RYE_STATE not set and --state-dir not provided")?;

    if args.all {
        let report = verify_all(&state_root)?;

        match args.format.as_str() {
            "json" => {
                let json = serde_json::to_string_pretty(&report)?;
                println!("{}", json);
            }
            "text" => {
                print_all_text_report(&report);
            }
            _ => anyhow::bail!("unknown format: {}", args.format),
        }

        if report.total_issues > 0 {
            std::process::exit(1);
        }
    } else if let Some(chain_id) = args.chain_id {
        let report = verify_chain(&state_root, &chain_id)?;

        match args.format.as_str() {
            "json" => {
                let json = serde_json::to_string_pretty(&report)?;
                println!("{}", json);
            }
            "text" => {
                print_text_report(&report);
            }
            _ => anyhow::bail!("unknown format: {}", args.format),
        }

        if !report.valid {
            std::process::exit(1);
        }
    } else {
        anyhow::bail!("provide a chain_id or use --all");
    }

    Ok(())
}

#[tracing::instrument(name = "tool:verify", skip(state_root))]
fn verify_all(state_root: &PathBuf) -> Result<AllVerifyReport> {
    let cas_root = state_root.join("objects");
    let refs_root = state_root.join("refs");

    // Collect reachable for summary
    let reachable = ryeos_state::reachability::collect_reachable(&cas_root, &refs_root)?;

    let mut report = AllVerifyReport {
        chains_verified: reachable.chain_root_ids.len(),
        projects_verified: reachable.project_hashes.len(),
        reachable_objects: reachable.object_hashes.len(),
        reachable_blobs: reachable.blob_hashes.len(),
        total_issues: 0,
        chain_reports: Vec::new(),
        project_reports: Vec::new(),
    };

    // Verify each chain's hash integrity
    for chain_id in &reachable.chain_root_ids {
        let chain_report = verify_chain_inner(&cas_root, &refs_root, chain_id, &reachable);
        let issues_count = chain_report.issues.len() + chain_report.project_issues.len();
        report.total_issues += issues_count;
        report.chain_reports.push(ChainSummary {
            chain_id: chain_id.clone(),
            valid: issues_count == 0,
            issues_count,
        });
    }

    // Verify each project's source integrity
    for project_hash in &reachable.project_hashes {
        let proj_issues = verify_project_source(&cas_root, &refs_root, project_hash, &reachable);
        let issues_count = proj_issues.len();
        report.total_issues += issues_count;
        report.project_reports.push(ProjectSummary {
            project_hash: project_hash.clone(),
            valid: issues_count == 0,
            issues_count,
        });
    }

    Ok(report)
}

fn verify_chain(state_root: &PathBuf, chain_id: &str) -> Result<VerifyReport> {
    let cas_root = state_root.join("objects");
    let refs_root = state_root.join("refs");

    // Collect reachable for project source verification
    let reachable = ryeos_state::reachability::collect_reachable(&cas_root, &refs_root)?;

    Ok(verify_chain_inner(&cas_root, &refs_root, chain_id, &reachable))
}

fn verify_chain_inner(
    cas_root: &PathBuf,
    refs_root: &PathBuf,
    chain_id: &str,
    reachable: &ryeos_state::reachability::ReachableSet,
) -> VerifyReport {
    let ref_path = refs_root
        .join("generic/chains")
        .join(chain_id)
        .join("head");

    let signed_ref = match ryeos_state::refs::read_signed_ref(&ref_path) {
        Ok(sr) => sr,
        Err(e) => {
            return VerifyReport {
                chain_id: Some(chain_id.to_string()),
                valid: false,
                events_count: 0,
                last_verified_hash: None,
                project_issues: vec![],
                issues: vec![format!("failed to read signed ref: {}", e)],
            };
        }
    };

    let mut issues = vec![];

    if let Err(e) = signed_ref.validate() {
        issues.push(format!("signed ref validation failed: {}", e));
    }

    let chain_state_path = lillux::shard_path(cas_root, "objects", &signed_ref.target_hash, ".json");
    let chain_state_json = match std::fs::read_to_string(&chain_state_path) {
        Ok(content) => content,
        Err(_) => {
            return VerifyReport {
                chain_id: Some(chain_id.to_string()),
                valid: false,
                events_count: 0,
                last_verified_hash: None,
                project_issues: vec![],
                issues: vec![format!("chain state not found in CAS: {}", chain_state_path.display())],
            };
        }
    };

    let chain_state: ryeos_state::ChainState = match serde_json::from_str(&chain_state_json) {
        Ok(cs) => cs,
        Err(e) => {
            return VerifyReport {
                chain_id: Some(chain_id.to_string()),
                valid: false,
                events_count: 0,
                last_verified_hash: None,
                project_issues: vec![],
                issues: vec![format!("failed to parse chain state: {}", e)],
            };
        }
    };

    if let Err(e) = chain_state.validate() {
        issues.push(format!("chain state validation failed: {}", e));
    }

    let mut current_hash = signed_ref.target_hash.clone();
    let mut current_state = chain_state;
    let mut verified_count = 0u64;
    let mut prev_hash: Option<String> = None;

    loop {
        let current_json = serde_json::to_value(&current_state).unwrap_or_default();
        let canonical = lillux::canonical_json(&current_json);
        let computed_hash = lillux::sha256_hex(canonical.as_bytes());

        if computed_hash != current_hash {
            issues.push(format!(
                "hash mismatch at chain seq {}: expected {}, computed {}",
                current_state.last_chain_seq, current_hash, computed_hash
            ));
            break;
        }

        verified_count = current_state.last_chain_seq;

        if let Some(expected_prev) = &prev_hash {
            if current_state.prev_chain_state_hash.as_ref() != Some(expected_prev) {
                issues.push(format!(
                    "prev_chain_state_hash mismatch at seq {}: expected {}, got {:?}",
                    current_state.last_chain_seq,
                    expected_prev,
                    current_state.prev_chain_state_hash
                ));
                break;
            }
        } else if current_state.prev_chain_state_hash.is_some() {
            issues.push(format!(
                "first chain state should have no previous, but has: {:?}",
                current_state.prev_chain_state_hash
            ));
            break;
        }

        match &current_state.prev_chain_state_hash {
            Some(prev_hash_str) => {
                let prev_path = lillux::shard_path(cas_root, "objects", prev_hash_str, ".json");
                match std::fs::read_to_string(&prev_path) {
                    Ok(prev_json) => {
                        match serde_json::from_str::<ryeos_state::ChainState>(&prev_json) {
                            Ok(prev_state) => {
                                if let Err(e) = prev_state.validate() {
                                    issues.push(format!(
                                        "previous chain state validation failed at seq {}: {}",
                                        prev_state.last_chain_seq, e
                                    ));
                                    break;
                                }
                                prev_hash = Some(current_hash.clone());
                                current_hash = prev_hash_str.clone();
                                current_state = prev_state;
                            }
                            Err(e) => {
                                issues.push(format!(
                                    "failed to parse previous chain state at {}: {}",
                                    prev_hash_str, e
                                ));
                                break;
                            }
                        }
                    }
                    Err(_) => {
                        issues.push(format!("previous chain state not found in CAS: {}", prev_hash_str));
                        break;
                    }
                }
            }
            None => break,
        }
    }

    // Verify project source links for each thread snapshot in this chain
    let project_issues = verify_chain_project_links(cas_root, &signed_ref.target_hash, reachable);

    VerifyReport {
        chain_id: Some(chain_id.to_string()),
        valid: issues.is_empty() && project_issues.is_empty(),
        events_count: verified_count,
        last_verified_hash: Some(current_hash),
        project_issues,
        issues,
    }
}

/// Verify project source links for all thread snapshots in a chain.
///
/// For each thread_snapshot, follows `base_project_snapshot_hash` and
/// `result_project_snapshot_hash` into the project source graph and
/// verifies all linked objects exist and hash correctly.
fn verify_chain_project_links(
    cas_root: &PathBuf,
    head_hash: &str,
    reachable: &ryeos_state::reachability::ReachableSet,
) -> Vec<String> {
    let mut issues = Vec::new();
    let mut visited_chain_states: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut current_hash = head_hash.to_string();

    // Walk chain states head → oldest, collecting thread snapshot hashes
    let mut snapshot_hashes = Vec::new();
    while !visited_chain_states.contains(&current_hash) && !current_hash.is_empty() {
        visited_chain_states.insert(current_hash.clone());

        let cs_path = lillux::shard_path(cas_root, "objects", &current_hash, ".json");
        let cs_json = match std::fs::read_to_string(&cs_path) {
            Ok(j) => j,
            Err(_) => break,
        };
        let cs_value: serde_json::Value = match serde_json::from_str(&cs_json) {
            Ok(v) => v,
            Err(_) => break,
        };

        if let Some(threads) = cs_value.get("threads").and_then(|v| v.as_object()) {
            for (thread_id, entry) in threads {
                if let Some(snap_hash) = entry.get("snapshot_hash").and_then(|v| v.as_str()) {
                    snapshot_hashes.push((thread_id.clone(), snap_hash.to_string()));
                }
            }
        }

        current_hash = cs_value
            .get("prev_chain_state_hash")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
    }

    // For each snapshot, verify project hash links
    for (thread_id, snap_hash) in &snapshot_hashes {
        let snap_path = lillux::shard_path(cas_root, "objects", snap_hash, ".json");
        let snap_json = match std::fs::read_to_string(&snap_path) {
            Ok(j) => j,
            Err(_) => {
                issues.push(format!(
                    "thread {} snapshot {} not found in CAS",
                    thread_id, snap_hash
                ));
                continue;
            }
        };
        let snap_value: serde_json::Value = match serde_json::from_str(&snap_json) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Check base_project_snapshot_hash
        if let Some(base_hash) = snap_value.get("base_project_snapshot_hash")
            .and_then(|v| v.as_str())
            .filter(|h| !h.is_empty())
        {
            if !reachable.object_hashes.contains(base_hash) {
                let base_path = lillux::shard_path(cas_root, "objects", base_hash, ".json");
                if !base_path.exists() {
                    issues.push(format!(
                        "thread {} base_project_snapshot {} missing from CAS",
                        thread_id, base_hash
                    ));
                }
            }
            // Verify hash integrity of the project snapshot
            if let Err(e) = verify_object_hash(cas_root, base_hash) {
                issues.push(format!(
                    "thread {} base_project_snapshot {} hash mismatch: {}",
                    thread_id, base_hash, e
                ));
            }
        }

        // Check result_project_snapshot_hash
        if let Some(result_hash) = snap_value.get("result_project_snapshot_hash")
            .and_then(|v| v.as_str())
            .filter(|h| !h.is_empty())
        {
            if !reachable.object_hashes.contains(result_hash) {
                let result_path = lillux::shard_path(cas_root, "objects", result_hash, ".json");
                if !result_path.exists() {
                    issues.push(format!(
                        "thread {} result_project_snapshot {} missing from CAS",
                        thread_id, result_hash
                    ));
                }
            }
            if let Err(e) = verify_object_hash(cas_root, result_hash) {
                issues.push(format!(
                    "thread {} result_project_snapshot {} hash mismatch: {}",
                    thread_id, result_hash, e
                ));
            }
        }
    }

    issues
}

/// Verify a project source tree: follow project_snapshot → source_manifest → item_sources → blobs.
fn verify_project_source(
    cas_root: &PathBuf,
    refs_root: &PathBuf,
    project_hash: &str,
    _reachable: &ryeos_state::reachability::ReachableSet,
) -> Vec<String> {
    let mut issues = Vec::new();

    let head_path = refs_root.join("projects").join(project_hash).join("head");
    let signed_ref = match ryeos_state::refs::read_signed_ref(&head_path) {
        Ok(sr) => sr,
        Err(e) => {
            return vec![format!("failed to read project head ref: {}", e)];
        }
    };

    // Verify the project snapshot object exists and hashes correctly
    if let Err(e) = verify_object_hash(cas_root, &signed_ref.target_hash) {
        issues.push(format!("project snapshot hash mismatch: {}", e));
    }

    // Verify all reachable objects for this project exist
    let project_snap_path = lillux::shard_path(cas_root, "objects", &signed_ref.target_hash, ".json");
    let project_snap_json = match std::fs::read_to_string(&project_snap_path) {
        Ok(j) => j,
        Err(_) => {
            issues.push(format!("project snapshot {} not found in CAS", signed_ref.target_hash));
            return issues;
        }
    };
    let project_snap: serde_json::Value = match serde_json::from_str(&project_snap_json) {
        Ok(v) => v,
        Err(_) => return issues,
    };

    // Verify project manifest exists
    if let Some(manifest_hash) = project_snap.get("project_manifest_hash").and_then(|v| v.as_str()) {
        if !manifest_hash.is_empty() {
            if let Err(e) = verify_object_hash(cas_root, manifest_hash) {
                issues.push(format!("project manifest {} hash mismatch: {}", manifest_hash, e));
            }

            // Walk source manifest → item sources → blobs
            let manifest_path = lillux::shard_path(cas_root, "objects", manifest_hash, ".json");
            if let Ok(manifest_json) = std::fs::read_to_string(&manifest_path) {
                if let Ok(manifest_val) = serde_json::from_str::<serde_json::Value>(&manifest_json) {
                    if let Some(item_hashes) = manifest_val.get("item_source_hashes").and_then(|v| v.as_object()) {
                        for (item_ref, hash_val) in item_hashes {
                            if let Some(item_hash) = hash_val.as_str() {
                                if !item_hash.is_empty() {
                                    if let Err(e) = verify_object_hash(cas_root, item_hash) {
                                        issues.push(format!("item_source {} for {} hash mismatch: {}", item_hash, item_ref, e));
                                    }

                                    // Check blob hash
                                    let item_path = lillux::shard_path(cas_root, "objects", item_hash, ".json");
                                    if let Ok(item_json) = std::fs::read_to_string(&item_path) {
                                        if let Ok(item_val) = serde_json::from_str::<serde_json::Value>(&item_json) {
                                            if let Some(blob_hash) = item_val.get("content_blob_hash").and_then(|v| v.as_str()) {
                                                if !blob_hash.is_empty() {
                                                    let blob_path = lillux::shard_path(cas_root, "blobs", blob_hash, "");
                                                    if !blob_path.exists() {
                                                        issues.push(format!("blob {} for item {} missing from CAS", blob_hash, item_ref));
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Verify user manifest if present
    if let Some(user_manifest_hash) = project_snap.get("user_manifest_hash").and_then(|v| v.as_str()) {
        if !user_manifest_hash.is_empty() {
            if let Err(e) = verify_object_hash(cas_root, user_manifest_hash) {
                issues.push(format!("user manifest {} hash mismatch: {}", user_manifest_hash, e));
            }
        }
    }

    issues
}

/// Verify that an object's content hashes to its filename hash.
fn verify_object_hash(cas_root: &PathBuf, hash: &str) -> Result<()> {
    let path = lillux::shard_path(cas_root, "objects", hash, ".json");
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read object {}", hash))?;

    // Parse as JSON to get canonical form
    let value: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse object {}", hash))?;

    let canonical = lillux::canonical_json(&value);
    let computed = lillux::sha256_hex(canonical.as_bytes());

    if computed != hash {
        anyhow::bail!("expected {}, computed {}", hash, computed);
    }

    Ok(())
}

fn print_text_report(report: &VerifyReport) {
    let status = if report.valid { "✓ VALID" } else { "✗ CORRUPTED" };
    let chain_id = report.chain_id.as_deref().unwrap_or("unknown");
    println!("{} Chain: {}", status, chain_id);
    println!("  Events: {}", report.events_count);
    if let Some(hash) = &report.last_verified_hash {
        println!("  Last Hash: {}...", &hash[..16.min(hash.len())]);
    }

    if !report.issues.is_empty() {
        println!("  Chain issues:");
        for issue in &report.issues {
            println!("    - {}", issue);
        }
    }

    if !report.project_issues.is_empty() {
        println!("  Project source issues:");
        for issue in &report.project_issues {
            println!("    - {}", issue);
        }
    }
}

fn print_all_text_report(report: &AllVerifyReport) {
    let status = if report.total_issues == 0 { "✓ ALL VALID" } else { "✗ ISSUES FOUND" };
    println!("{}", status);
    println!("  Chains verified: {}", report.chains_verified);
    println!("  Projects verified: {}", report.projects_verified);
    println!("  Reachable objects: {}", report.reachable_objects);
    println!("  Reachable blobs: {}", report.reachable_blobs);
    println!("  Total issues: {}", report.total_issues);

    if !report.chain_reports.is_empty() {
        println!("  Chains:");
        for chain in &report.chain_reports {
            let s = if chain.valid { "✓" } else { "✗" };
            println!("    {} {} ({} issues)", s, chain.chain_id, chain.issues_count);
        }
    }

    if !report.project_reports.is_empty() {
        println!("  Projects:");
        for project in &report.project_reports {
            let s = if project.valid { "✓" } else { "✗" };
            println!("    {} {} ({} issues)", s, project.project_hash, project.issues_count);
        }
    }
}
