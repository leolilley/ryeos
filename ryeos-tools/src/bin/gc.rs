//! rye-gc — garbage collect unused CAS objects
//!
//! Routes GC requests through the daemon when one is running (default).
//! Falls back to direct GC when no daemon is detected.
//!
//! Flags:
//!   --direct   Force direct mode (fail if daemon is running)
//!   --daemon   Force daemon-routed mode (fail if daemon is not running)

use anyhow::{Context, Result};
use clap::Parser;
use ryeos_tools::get_state_root;
use serde::Serialize;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "rye-gc")]
#[command(about = "Garbage collect CAS objects")]
struct Args {
    /// RYE_STATE directory
    #[arg(long)]
    state_dir: Option<PathBuf>,

    /// Dry run (don't delete)
    #[arg(long)]
    dry_run: bool,

    /// Compact project snapshot history before sweep
    #[arg(long)]
    compact: bool,

    /// Path to signing key PEM (required for --compact)
    #[arg(long)]
    key: Option<PathBuf>,

    /// Max manual push snapshots to keep per project
    #[arg(long, default_value = "10")]
    manual_pushes: usize,

    /// Max auto snapshots to keep per project
    #[arg(long, default_value = "30")]
    auto_snapshots: usize,

    /// Output format
    #[arg(short, long, default_value = "text")]
    format: String,

    /// Force direct GC (fail if daemon is running)
    #[arg(long, conflicts_with = "daemon")]
    direct: bool,

    /// Force daemon-routed GC (fail if daemon is not running)
    #[arg(long, conflicts_with = "direct")]
    daemon: bool,
}

#[derive(Debug, Serialize)]
struct GcReport {
    dry_run: bool,
    compact: bool,
    roots_walked: usize,
    reachable_objects: usize,
    reachable_blobs: usize,
    deleted_objects: usize,
    deleted_blobs: usize,
    freed_bytes: u64,
    snapshots_compacted: usize,
    duration_ms: u64,
}

fn main() -> Result<()> {
    ryeos_tracing::init_subscriber(ryeos_tracing::SubscriberConfig::for_cli_tool());

    let args = Args::parse();
    let state_root = args.state_dir.clone().or_else(|| get_state_root().ok())
        .context("RYE_STATE not set and --state-dir not provided")?;

    let daemon_socket = detect_daemon(&state_root);

    // Build params (needed for both paths)
    let policy = ryeos_state::gc::RetentionPolicy {
        manual_pushes: args.manual_pushes,
        auto_snapshots: args.auto_snapshots,
    };
    let params = ryeos_state::gc::GcParams {
        dry_run: args.dry_run,
        compact: args.compact,
        policy: Some(policy),
    };

    let result = match (daemon_socket, args.direct, args.daemon) {
        // --direct: force direct, fail if daemon running
        (Some(socket), true, false) => {
            anyhow::bail!(
                "daemon is running (socket: {}). \
                 Cannot use --direct while daemon is active. \
                 Either stop the daemon or remove --direct.",
                socket.display()
            );
        }
        // --daemon: force daemon, fail if not running
        (None, false, true) => {
            anyhow::bail!(
                "daemon is not running. \
                 Cannot use --daemon without a running daemon."
            );
        }
        // Auto-detect or --daemon with daemon running: route through daemon
        (Some(socket), false, _) => {
            if !args.daemon {
                eprintln!(
                    "Daemon detected, routing GC through daemon (use --direct to override)."
                );
            }
            run_gc_via_daemon(&socket, &params)?
        }
        // No daemon, direct or auto-detect: run directly
        (None, _, false) => {
            run_gc_direct(&state_root, &args, &params)?
        }
        // Unreachable due to clap conflicts_with
        _ => unreachable!("clap conflicts_with prevents this"),
    };

    // Report
    let report = GcReport {
        dry_run: args.dry_run,
        compact: args.compact,
        roots_walked: result.roots_walked,
        reachable_objects: result.reachable_objects,
        reachable_blobs: result.reachable_blobs,
        deleted_objects: result.deleted_objects,
        deleted_blobs: result.deleted_blobs,
        freed_bytes: result.freed_bytes,
        snapshots_compacted: result
            .compaction
            .as_ref()
            .map(|c| c.snapshots_removed)
            .unwrap_or(0),
        duration_ms: result.duration_ms,
    };

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

    Ok(())
}

/// Run GC directly (daemon not running path).
#[tracing::instrument(name = "tool:gc", skip(state_root, args, params), fields(dry_run = args.dry_run))]
fn run_gc_direct(
    state_root: &std::path::Path,
    args: &Args,
    params: &ryeos_state::gc::GcParams,
) -> Result<ryeos_state::gc::GcResult> {
    let cas_root = state_root.join("objects");
    let refs_root = state_root.join("refs");

    if args.dry_run {
        eprintln!("Dry run mode -- no files will be deleted.");
    }

    // 1. Acquire GC lock
    let node_id = hostname();
    let _lock = ryeos_state::gc::GcLock::acquire(state_root, &node_id)?;

    // 2. Load signer if compaction requested
    let signer: Option<Box<dyn ryeos_state::Signer>> = if args.compact {
        let key_path = args.key.as_ref()
            .context("--compact requires --key <path> to provide the signing key")?;
        let secret_key = lillux::crypto::load_signing_key(key_path)?;

        let fp = lillux::crypto::fingerprint(&secret_key.verifying_key());
        Some(Box::new(KeySigner { secret_key, fingerprint: fp }))
    } else {
        None
    };

    // 3. Run GC
    _lock.update_phase("gc")?;
    let result = ryeos_state::gc::run_gc(
        &cas_root,
        &refs_root,
        signer.as_ref().map(|s| s.as_ref()),
        params,
    )?;

    // 4. Log event
    let event = ryeos_state::gc::GcEvent {
        timestamp: lillux::time::iso8601_now(),
        dry_run: args.dry_run,
        compact: args.compact,
        roots_walked: result.roots_walked,
        reachable_objects: result.reachable_objects,
        reachable_blobs: result.reachable_blobs,
        deleted_objects: result.deleted_objects,
        deleted_blobs: result.deleted_blobs,
        freed_bytes: result.freed_bytes,
        snapshots_compacted: result
            .compaction
            .as_ref()
            .map(|c| c.snapshots_removed)
            .unwrap_or(0),
        duration_ms: result.duration_ms,
    };

    if let Err(e) = ryeos_state::gc::event_log::append_event(state_root, &event) {
        tracing::warn!(error = %e, "failed to write GC event log");
    }

    Ok(result)
}

/// Probe for a running daemon by reading daemon.json or checking env var.
/// Returns the socket path if daemon is alive, None otherwise.
fn detect_daemon(state_root: &std::path::Path) -> Option<PathBuf> {
    // Check daemon.json written by ryeosd on startup
    let candidates: Vec<PathBuf> = [
        Some(state_root.join("daemon.json")),
        state_root.parent().map(|p| p.join("daemon.json")),
    ]
    .into_iter()
    .flatten()
    .collect();

    for candidate in candidates {
        if let Ok(content) = std::fs::read_to_string(&candidate) {
            if let Ok(info) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(socket) = info.get("socket").and_then(|v| v.as_str()) {
                    let socket_path = PathBuf::from(socket);
                    // Verify the daemon is actually alive
                    if UnixStream::connect(&socket_path).is_ok() {
                        return Some(socket_path);
                    }
                }
            }
        }
    }

    None
}

/// Send GC request to daemon via UDS and wait for result.
///
/// Uses the daemon's msgpack RPC protocol (4-byte length-prefixed frames).
fn run_gc_via_daemon(
    socket_path: &std::path::Path,
    params: &ryeos_state::gc::GcParams,
) -> Result<ryeos_state::gc::GcResult> {
    let mut stream = UnixStream::connect(socket_path)
        .with_context(|| format!("failed to connect to daemon at {}", socket_path.display()))?;

    // Build the RPC request in the daemon's expected format
    let request = serde_json::json!({
        "request_id": 1,
        "method": "maintenance.gc",
        "params": {
            "dry_run": params.dry_run,
            "compact": params.compact,
            "policy": params.policy,
        }
    });

    // Encode as msgpack and frame it (4-byte BE length prefix)
    let request_bytes = rmp_serde::to_vec_named(&request)
        .context("failed to encode GC request as msgpack")?;
    let len_bytes = (request_bytes.len() as u32).to_be_bytes();
    stream.write_all(&len_bytes)
        .context("failed to write request length")?;
    stream.write_all(&request_bytes)
        .context("failed to write request body")?;

    // Read the response frame
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)
        .context("failed to read response length")?;
    let frame_len = u32::from_be_bytes(len_buf) as usize;
    if frame_len > 10 * 1024 * 1024 {
        anyhow::bail!("daemon response too large: {} bytes", frame_len);
    }
    let mut response_buf = vec![0u8; frame_len];
    stream.read_exact(&mut response_buf)
        .context("failed to read response body")?;

    // Decode msgpack response
    let response: serde_json::Value = rmp_serde::from_slice(&response_buf)
        .context("failed to decode daemon GC response")?;

    // Check for error
    if let Some(error) = response.get("error") {
        let message = error.get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        anyhow::bail!("daemon GC failed: {}", message);
    }

    // Extract result
    let result_value = response.get("result")
        .ok_or_else(|| anyhow::anyhow!("daemon returned no result"))?;
    let result: ryeos_state::gc::GcResult = serde_json::from_value(result_value.clone())
        .context("failed to parse GC result from daemon")?;

    Ok(result)
}

fn print_text_report(report: &GcReport) {
    let mode = if report.dry_run { " (DRY RUN)" } else { "" };
    println!("GC{} complete in {}ms", mode, report.duration_ms);

    if report.compact {
        println!("  Compaction:");
        println!("    Snapshots compacted: {}", report.snapshots_compacted);
    }

    println!("  Reachable:");
    println!("    Roots walked: {}", report.roots_walked);
    println!("    Objects: {}", report.reachable_objects);
    println!("    Blobs: {}", report.reachable_blobs);

    println!("  Freed:");
    println!("    Objects deleted: {}", report.deleted_objects);
    println!("    Blobs deleted: {}", report.deleted_blobs);
    println!("    Bytes freed: {}", report.freed_bytes);
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Wrapper to adapt lillux::crypto::SigningKey to ryeos_state::Signer trait.
struct KeySigner {
    secret_key: lillux::crypto::SigningKey,
    fingerprint: String,
}

impl ryeos_state::Signer for KeySigner {
    fn sign(&self, data: &[u8]) -> Vec<u8> {
        use lillux::crypto::Signer as _;
        self.secret_key.sign(data).to_bytes().to_vec()
    }

    fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}
