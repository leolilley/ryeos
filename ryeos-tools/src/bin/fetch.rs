//! rye-fetch — fetch items through rye-engine with resolution and verification
//!
//! Fetches items by:
//! 1. Parsing the item_ref (e.g., tool:foo/bar, directive:x/y)
//! 2. Resolving via rye-engine (system-first search)
//! 3. Verifying the resolved item
//! 4. Extracting and outputting source/binary content
//! 5. Reporting metadata and fetch status

use anyhow::{Context, Result};
use clap::Parser;
use ryeos_engine::{
    canonical_ref::CanonicalRef,
    contracts::{EffectivePrincipal, PlanContext, Principal, ProjectContext},
    engine::Engine,
    executor_registry::ExecutorRegistry,
    kind_registry::KindRegistry,
    metadata::MetadataParserRegistry,
    trust::TrustStore,
    AI_DIR, KIND_SCHEMAS_DIR,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "rye-fetch")]
#[command(about = "Fetch items through rye-engine with resolution and verification")]
struct Args {
    /// Item reference (e.g., tool:foo/bar, directive:x/y)
    item_ref: String,

    /// Project root path (sets project context for resolution)
    #[arg(long)]
    project: Option<PathBuf>,

    /// User root path (overrides default ~/.ai)
    #[arg(long)]
    user: Option<PathBuf>,

    /// System bundle roots (can be repeated)
    #[arg(long)]
    system: Vec<PathBuf>,

    /// Output format (json or text)
    #[arg(short, long, default_value = "text")]
    format: String,

    /// Write fetched content to file instead of stdout
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Include fetched content in output (JSON only)
    #[arg(long)]
    with_content: bool,

    /// Verify signature (requires trust store)
    #[arg(long)]
    verify: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct FetchReport {
    item_ref: String,
    kind: String,
    resolved_path: String,
    resolved_from: String,
    space: String,
    content_hash: String,
    signature_status: Option<String>,
    shadowed_count: usize,
    fetch_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    // Parse the canonical ref
    let canonical_ref = CanonicalRef::parse(&args.item_ref)
        .context(format!("failed to parse item ref: {}", args.item_ref))?;

    tracing::debug!(item_ref = %args.item_ref, "parsed canonical ref");

    // Set up project context
    let project_context = match &args.project {
        Some(path) => {
            // Ensure it's an absolute path
            let abs_path = if path.is_absolute() {
                path.clone()
            } else {
                std::env::current_dir()?.join(path)
            };
            ProjectContext::LocalPath { path: abs_path }
        }
        None => ProjectContext::None,
    };

    // Use provided system roots, or discover from environment
    let mut system_roots: Vec<PathBuf> = args.system.clone();

    if system_roots.is_empty() {
        // Try RYE_NODE_ROOT first (single system .ai/ root)
        if let Ok(node_root) = std::env::var("RYE_NODE_ROOT") {
            system_roots.push(PathBuf::from(node_root));
        }
        // Then RYE_SYSTEM_ROOTS (colon-separated bundle roots)
        if let Ok(roots) = std::env::var("RYE_SYSTEM_ROOTS") {
            for root in roots.split(':') {
                if !root.is_empty() {
                    system_roots.push(PathBuf::from(root));
                }
            }
        }
    }

    // Resolve user root from CLI or environment
    let user_root = args.user.clone().or_else(|| {
        std::env::var("RYE_USER_ROOT").ok().map(PathBuf::from)
    }).or_else(|| {
        dirs::home_dir().map(|h| h.join(".ai"))
    });

    // Build the engine
    let engine = build_engine(user_root, system_roots)?;

    // Create plan context for resolution
    let plan_context = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: "fetch-tool".to_string(),
            scopes: vec!["rye.fetch".to_string()],
        }),
        project_context,
        current_site_id: "localhost".to_string(),
        origin_site_id: "localhost".to_string(),
        execution_hints: Default::default(),
        validate_only: false,
    };

    // Attempt to resolve the item
    let resolved = match engine.resolve(&plan_context, &canonical_ref) {
        Ok(item) => item,
        Err(e) => {
            let report = FetchReport {
                item_ref: args.item_ref.clone(),
                kind: canonical_ref.kind.clone(),
                resolved_path: String::new(),
                resolved_from: String::new(),
                space: String::new(),
                content_hash: String::new(),
                signature_status: None,
                shadowed_count: 0,
                fetch_status: "FAILED".to_string(),
                content: None,
                error: Some(format!("{}", e)),
            };
            output_report(&args.format, &report)?;
            std::process::exit(1);
        }
    };

    tracing::debug!(
        item_ref = %args.item_ref,
        path = %resolved.source_path.display(),
        space = %resolved.source_space.as_str(),
        "resolved item successfully"
    );

    // Optionally verify the item
    let signature_status = if args.verify {
        match engine.verify(&plan_context, resolved.clone()) {
            Ok(verified) => {
                let trust_str = match verified.trust_class {
                    ryeos_engine::contracts::TrustClass::Trusted => "TRUSTED",
                    ryeos_engine::contracts::TrustClass::Untrusted => "UNTRUSTED",
                    ryeos_engine::contracts::TrustClass::Unsigned => "UNSIGNED",
                };
                tracing::debug!(item_ref = %args.item_ref, status = %trust_str, "item verified");
                Some(trust_str.to_string())
            }
            Err(e) => {
                tracing::warn!(item_ref = %args.item_ref, error = %e, "verification failed");
                Some(format!("VERIFICATION_FAILED: {}", e))
            }
        }
    } else {
        None
    };

    // Read the content
    let content = std::fs::read_to_string(&resolved.source_path)
        .context(format!("failed to read item content from {:?}", resolved.source_path))?;

    // Write to output file if requested
    if let Some(output_path) = &args.output {
        std::fs::write(output_path, &content)
            .context(format!("failed to write content to {:?}", output_path))?;
        tracing::info!(path = %output_path.display(), "wrote fetched content to file");
    }

    // Prepare report
    let report = FetchReport {
        item_ref: args.item_ref.clone(),
        kind: resolved.kind.clone(),
        resolved_path: resolved.source_path.display().to_string(),
        resolved_from: resolved.resolved_from.clone(),
        space: resolved.source_space.as_str().to_string(),
        content_hash: resolved.content_hash.clone(),
        signature_status,
        shadowed_count: resolved.shadowed.len(),
        fetch_status: "SUCCESS".to_string(),
        content: if args.with_content && args.format == "json" {
            Some(content.clone())
        } else {
            None
        },
        error: None,
    };

    // Output report
    output_report(&args.format, &report)?;

    // If text format and not writing to file, optionally show content hint
    if args.format == "text" && args.output.is_none() && content.len() < 5000 {
        println!("\n{}", "─".repeat(60));
        println!("Content preview ({} bytes):", content.len());
        println!("{}", "─".repeat(60));
        println!("{}", content);
    }

    Ok(())
}

/// Build the engine with real kind schemas from disk
fn build_engine(user_root: Option<PathBuf>, system_roots: Vec<PathBuf>) -> Result<Engine> {
    // Collect kind schema search roots from all system roots + user space
    let mut schema_roots = Vec::new();

    for root in &system_roots {
        let kinds_dir = root.join(AI_DIR).join(KIND_SCHEMAS_DIR);
        if kinds_dir.is_dir() {
            schema_roots.push(kinds_dir);
        }
    }

    if let Some(ref ur) = user_root {
        let user_kinds = ur.join(AI_DIR).join(KIND_SCHEMAS_DIR);
        if user_kinds.is_dir() {
            schema_roots.push(user_kinds);
        }
    }

    // Load trust store (project root unknown at fetch time, use None)
    let trust_store = TrustStore::load_three_tier(None, user_root.as_deref(), &system_roots)
        .context("failed to load trust store")?;

    // Load kind registry from filesystem
    let kinds = if schema_roots.is_empty() {
        anyhow::bail!(
            "no kind schema roots found. Set RYE_NODE_ROOT or RYE_SYSTEM_ROOTS, \
             or pass --system <path>"
        );
    } else {
        KindRegistry::load_base(&schema_roots, &trust_store)
            .context("failed to load kind schemas")?
    };

    // Build executor registry and parser registry
    let executors = ExecutorRegistry::new();
    let parsers = MetadataParserRegistry::with_builtins();

    // Construct engine with trust store
    let engine = Engine::new(kinds, executors, parsers, user_root, system_roots)
        .with_trust_store(trust_store);

    Ok(engine)
}

/// Output the fetch report in the specified format
fn output_report(format: &str, report: &FetchReport) -> Result<()> {
    match format {
        "json" => {
            let json = serde_json::to_string_pretty(report)?;
            println!("{}", json);
        }
        "text" => {
            print_text_report(report);
        }
        _ => anyhow::bail!("unknown format: {}", format),
    }
    Ok(())
}

/// Print a human-readable text report
fn print_text_report(report: &FetchReport) {
    let status_icon = if report.fetch_status == "SUCCESS" {
        "✓"
    } else {
        "✗"
    };

    println!("{} Fetch {} [{}]", status_icon, report.item_ref, report.fetch_status);

    if report.fetch_status == "SUCCESS" {
        println!("  Kind:            {}", report.kind);
        println!("  Resolved from:   {}", report.resolved_from);
        println!("  Space:           {}", report.space);
        println!("  Path:            {}", report.resolved_path);
        println!("  Content hash:    {}", &report.content_hash[..16.min(report.content_hash.len())]);

        if let Some(sig_status) = &report.signature_status {
            println!("  Signature:       {}", sig_status);
        }

        if report.shadowed_count > 0 {
            println!("  Shadowed items:  {}", report.shadowed_count);
        }
    } else if let Some(error) = &report.error {
        println!("  Error: {}", error);
    }
}

/// Helper module for home directory detection
mod dirs {
    use std::path::PathBuf;
    use std::ffi::OsString;

    pub fn home_dir() -> Option<PathBuf> {
        std::env::var_os("HOME")
            .and_then(|h: OsString| {
                if h.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(h))
                }
            })
    }
}
