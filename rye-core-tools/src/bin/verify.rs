//! rye-verify — verify CAS chain integrity

use anyhow::{Context, Result};
use clap::Parser;
use rye_core_tools::get_state_root;
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
}

#[derive(Debug, Serialize)]
struct ChainSummary {
    chain_id: String,
    valid: bool,
    issues_count: usize,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

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

fn verify_all(state_root: &PathBuf) -> Result<AllVerifyReport> {
    let cas_root = state_root.join("objects");
    let refs_root = state_root.join("refs");

    // Collect reachable for summary
    let reachable = rye_state::reachability::collect_reachable(&cas_root, &refs_root)?;

    let mut report = AllVerifyReport {
        chains_verified: reachable.chain_root_ids.len(),
        projects_verified: reachable.project_hashes.len(),
        reachable_objects: reachable.object_hashes.len(),
        reachable_blobs: reachable.blob_hashes.len(),
        total_issues: 0,
        chain_reports: Vec::new(),
    };

    // Verify each chain's hash integrity
    for chain_id in &reachable.chain_root_ids {
        let chain_report = verify_chain_inner(&cas_root, &refs_root, chain_id);
        let issues_count = chain_report.issues.len();
        report.total_issues += issues_count;
        report.chain_reports.push(ChainSummary {
            chain_id: chain_id.clone(),
            valid: issues_count == 0,
            issues_count,
        });
    }

    Ok(report)
}

fn verify_chain(state_root: &PathBuf, chain_id: &str) -> Result<VerifyReport> {
    let cas_root = state_root.join("objects");
    let refs_root = state_root.join("refs");
    Ok(verify_chain_inner(&cas_root, &refs_root, chain_id))
}

fn verify_chain_inner(cas_root: &PathBuf, refs_root: &PathBuf, chain_id: &str) -> VerifyReport {
    let ref_path = refs_root
        .join("refs/generic/chains")
        .join(chain_id)
        .join("head");

    let signed_ref = match rye_state::refs::read_signed_ref(&ref_path) {
        Ok(sr) => sr,
        Err(e) => {
            return VerifyReport {
                chain_id: Some(chain_id.to_string()),
                valid: false,
                events_count: 0,
                last_verified_hash: None,
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
                issues: vec![format!("chain state not found in CAS: {}", chain_state_path.display())],
            };
        }
    };

    let chain_state: rye_state::ChainState = match serde_json::from_str(&chain_state_json) {
        Ok(cs) => cs,
        Err(e) => {
            return VerifyReport {
                chain_id: Some(chain_id.to_string()),
                valid: false,
                events_count: 0,
                last_verified_hash: None,
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
                        match serde_json::from_str::<rye_state::ChainState>(&prev_json) {
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

    VerifyReport {
        chain_id: Some(chain_id.to_string()),
        valid: issues.is_empty(),
        events_count: verified_count,
        last_verified_hash: Some(current_hash),
        issues,
    }
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
        println!("  Issues:");
        for issue in &report.issues {
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

    for chain in &report.chain_reports {
        let s = if chain.valid { "✓" } else { "✗" };
        println!("  {} {} ({} issues)", s, chain.chain_id, chain.issues_count);
    }
}
