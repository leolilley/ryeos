//! Hardcoded CLI verbs that run LOCALLY without dispatching to the daemon.
//!
//! These verbs operate on operator state that exists before the daemon is
//! up (`ryeos init`), or that is operator-tier filesystem state the daemon
//! never owns (`ryeos trust pin`), or that is publisher-tier authoring state
//! disjoint from any daemon (`ryeos publish`). They are matched by the
//! dispatcher BEFORE the verb table lookup so they always work even on
//! a fresh checkout with no `core` bundle present.
//!
//! Each verb here parses its own argv slice with clap, runs shared local
//! lifecycle/tool actions, and prints a report on success.
//!
//! No daemon round-trip. No verb-table dependency. No trust-store load
//! before keys exist.

use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use clap::Parser;
use lillux::crypto::{DecodePrivateKey, SigningKey};

use ryeos_engine::roots;
use ryeos_node::{LifecycleController, LifecycleStatus, LocalLifecycleEnv, StopOptions};
use ryeos_tools::actions::authorize::{
    run_authorize_client as run_authorize_key, AuthorizeClientParams,
};
use ryeos_tools::actions::publish::{run_publish, PublishOptions};
use ryeos_tools::actions::trust::{run_pin, run_pin_from, PinFromOptions, PinOptions};
use ryeos_tools::actions::vault::{
    run_list as run_vault_list, run_put as run_vault_put, run_remove as run_vault_remove,
    run_rewrap as run_vault_rewrap, ListOptions, PutOptions, RemoveOptions, RewrapOptions,
};

use crate::error::CliError;

/// Returns `Ok(true)` if the argv was handled by a local verb, `Ok(false)`
/// if no match (caller should fall through to verb-table dispatch).
///
/// Errors from a matched local verb propagate as `CliError::Local`.
pub async fn try_dispatch(argv: &[String]) -> Result<bool, CliError> {
    if argv.is_empty() {
        return Ok(false);
    }
    match argv[0].as_str() {
        "init" => {
            run_init_verb(&argv[1..]).map_err(map_local_err)?;
            Ok(true)
        }
        "status" => {
            run_status_verb(&argv[1..]).await.map_err(map_local_err)?;
            Ok(true)
        }
        "start" => {
            run_start_verb(&argv[1..]).await.map_err(map_local_err)?;
            Ok(true)
        }
        "stop" => {
            run_stop_verb(&argv[1..]).await.map_err(map_local_err)?;
            Ok(true)
        }
        "authorize-key" => {
            run_authorize_key_verb(&argv[1..]).map_err(map_local_err)?;
            Ok(true)
        }
        "publish" => {
            run_publish_verb(&argv[1..]).map_err(map_local_err)?;
            Ok(true)
        }
        "trust" => {
            // Only `ryeos trust pin ...` is currently a local verb. If the
            // sub-token isn't `pin`, fall through to verb-table.
            if argv.len() < 2 || argv[1] != "pin" {
                return Ok(false);
            }
            run_trust_pin_verb(&argv[2..]).map_err(map_local_err)?;
            Ok(true)
        }
        "vault" => {
            // `ryeos vault {put,list,remove,rewrap}` are local verbs.
            // Vault state is daemon-side, but rotation must work even
            // when the daemon is down — these verbs read the on-disk
            // vault secret key directly. Anything else under `vault`
            // falls through to the verb table.
            if argv.len() < 2 {
                return Ok(false);
            }
            match argv[1].as_str() {
                "put" => run_vault_put_verb(&argv[2..]).map_err(map_local_err)?,
                "list" => run_vault_list_verb(&argv[2..]).map_err(map_local_err)?,
                "remove" => run_vault_remove_verb(&argv[2..]).map_err(map_local_err)?,
                "rewrap" => run_vault_rewrap_verb(&argv[2..]).map_err(map_local_err)?,
                _ => return Ok(false),
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn map_local_err(e: anyhow::Error) -> CliError {
    CliError::Local {
        detail: format!("{e:#}"),
    }
}

// ── ryeos init ──────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos init",
    about = "Bootstrap user + node keys, discover and install bundles, pin publisher keys",
    no_binary_name = true
)]
struct InitArgs {
    /// System space root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    system_space_dir: Option<PathBuf>,

    /// User space root (parent of `.ai/`). Defaults to the canonical user root.
    #[arg(long)]
    user_root: Option<PathBuf>,

    /// Source directory containing bundle subdirectories.
    /// Each immediate child with a `.ai/` subdirectory is installed as a bundle.
    /// Defaults to `/usr/share/ryeos` (packaged install).
    /// Override for dev (`bundles`), Docker (`/opt/ryeos`), etc.
    #[arg(long, default_value = "/usr/share/ryeos")]
    source: PathBuf,

    /// Additional publisher trust doc(s) to pin before verifying bundles.
    /// Each file should be a PUBLISHER_TRUST.toml with public_key and fingerprint.
    /// Repeatable: `--trust-file a.toml --trust-file b.toml`.
    #[arg(long = "trust-file", action = clap::ArgAction::Append)]
    trust_files: Vec<PathBuf>,
}

fn run_init_verb(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<InitArgs>(argv)?;
    let system_space_dir = args
        .system_space_dir
        .unwrap_or_else(default_system_space_dir);
    let user_root = args.user_root.unwrap_or_else(default_user_root);

    let opts = ryeos_node::InitOptions {
        system_space_dir,
        user_root,
        source_dir: args.source,
        trust_files: args.trust_files,
        skip_preflight: false,
    };
    let report = ryeos_node::run_init(&opts).context("ryeos init failed")?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

// ── ryeos {status,start,stop} ───────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos status",
    about = "Show local node lifecycle status",
    no_binary_name = true
)]
struct StatusArgs {
    /// System space root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    system_space_dir: Option<PathBuf>,

    /// Emit structured JSON instead of human-readable text.
    #[arg(long)]
    json: bool,
}

async fn run_status_verb(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<StatusArgs>(argv)?;
    let controller = LifecycleController::from_env(local_env(args.system_space_dir)?);
    let status = controller.status().await.context("ryeos status failed")?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print_lifecycle_status(&status);
    }
    Ok(())
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos start",
    about = "Bring the local node runtime online",
    no_binary_name = true
)]
struct StartArgs {
    /// System space root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    system_space_dir: Option<PathBuf>,
}

async fn run_start_verb(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<StartArgs>(argv)?;
    let controller = LifecycleController::from_env(local_env(args.system_space_dir)?);
    let report = controller.start().await.context("ryeos start failed")?;
    if report.already_running {
        println!("running");
    } else {
        println!("started");
    }
    print_lifecycle_status(&report.status);
    Ok(())
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos stop",
    about = "Gracefully stop the local node runtime",
    no_binary_name = true
)]
struct StopArgs {
    /// System space root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    system_space_dir: Option<PathBuf>,

    /// Fall back to signaling the confirmed live ryeosd process if graceful shutdown times out.
    #[arg(long)]
    force: bool,
}

async fn run_stop_verb(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<StopArgs>(argv)?;
    let controller = LifecycleController::from_env(local_env(args.system_space_dir)?);
    let report = controller
        .stop(StopOptions {
            force: args.force,
            ..StopOptions::default()
        })
        .await
        .context("ryeos stop failed")?;
    if report.already_stopped {
        println!("already stopped");
    } else {
        println!("stopped");
    }
    print_lifecycle_status(&report.status);
    Ok(())
}

fn local_env(system_space_dir: Option<PathBuf>) -> Result<LocalLifecycleEnv> {
    LocalLifecycleEnv::load(system_space_dir)
}

fn print_lifecycle_status(status: &LifecycleStatus) {
    match status {
        LifecycleStatus::NotInitialized { diagnostics } => {
            println!("not initialized — run: ryeos init");
            println!("detail: {}", diagnostics.message);
        }
        LifecycleStatus::Stopped { system_space_dir } => {
            println!("initialized, stopped — run: ryeos start");
            println!("system space: {}", system_space_dir.display());
        }
        LifecycleStatus::Running { metadata } => {
            println!("running");
            if let Some(pid) = metadata.pid {
                println!("pid: {pid}");
            }
            if let Some(bind) = &metadata.bind {
                println!("url: http://{bind}");
            }
            if let Some(socket) = &metadata.uds_path {
                println!("socket: {}", socket.display());
            }
        }
        LifecycleStatus::Stale { diagnostics, .. } => {
            println!("stale daemon metadata — {}", diagnostics.message);
        }
    }
}

// ── ryeos authorize-key ──────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos authorize-key",
    about = "Authorize an Ed25519 public key to call the daemon's authenticated endpoints",
    no_binary_name = true
)]
struct AuthorizeKeyArgs {
    /// Client public key in "ed25519:<base64>" format.
    #[arg(long)]
    public_key: String,

    /// Human-readable label for the authorized key.
    #[arg(long, default_value = "cli-authorized")]
    label: String,

    /// Comma-separated capabilities in canonical form
    /// `ryeos.<verb>.<kind>.<subject>`. Example:
    /// `--scopes ryeos.execute.service.remote.admin,ryeos.execute.service.bundle.install`.
    /// Short-form scopes like `bundle.install` are rejected because the
    /// authorizer does not auto-prefix. Use `--allow-wildcard` for `*`.
    #[arg(long)]
    scopes: String,

    /// Allow wildcard scope "*". Only use for operator bootstrap.
    #[arg(long)]
    allow_wildcard: bool,

    /// System space root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    system_space_dir: Option<PathBuf>,
}

fn run_authorize_key_verb(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<AuthorizeKeyArgs>(argv)?;
    let system_space_dir = args
        .system_space_dir
        .unwrap_or_else(default_system_space_dir);

    let pk_b64 = args
        .public_key
        .strip_prefix("ed25519:")
        .ok_or_else(|| anyhow!("public_key must be in 'ed25519:<base64>' format"))?;

    let pk_bytes = base64::engine::general_purpose::STANDARD
        .decode(pk_b64)
        .with_context(|| "invalid base64 in public_key")?;

    let verifying_key = lillux::crypto::VerifyingKey::from_bytes(
        pk_bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("public key must be 32 bytes (ed25519)"))?,
    )
    .map_err(|e| anyhow::anyhow!("invalid ed25519 public key: {e}"))?;

    let scopes: Vec<String> = args
        .scopes
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if scopes.is_empty() {
        bail!("--scopes must not be empty");
    }

    // Validate each scope is in canonical form. `*` is grammar-valid;
    // the writer enforces the wildcard policy via `--allow-wildcard`.
    for scope in &scopes {
        ryeos_runtime::authorizer::validate_scope_pattern(scope)
            .map_err(|e| anyhow!("invalid scope: {e}"))?;
    }

    let result = run_authorize_key(AuthorizeClientParams {
        system_space_dir,
        public_key: verifying_key,
        scopes,
        label: args.label,
        allow_wildcard: args.allow_wildcard,
    })
    .context("ryeos authorize-key failed")?;

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "fingerprint": result.fingerprint,
            "path": result.path.to_string_lossy(),
        }))?
    );
    Ok(())
}

// ── ryeos trust pin ──────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos trust pin",
    about = "Pin a publisher's Ed25519 public key into the operator trust store",
    no_binary_name = true
)]
struct PinArgs {
    /// Pin from a PUBLISHER_TRUST.toml file (canonical mode).
    /// Mutually exclusive with <fingerprint> + --pubkey-file.
    #[arg(long)]
    from: Option<PathBuf>,

    /// Expected fingerprint (SHA-256 hex of the raw 32-byte public key).
    /// Required in raw-key mode (when --from is not used).
    fingerprint: Option<String>,

    /// Path to a file containing the public key. Accepts PEM, `ed25519:<b64>`, or raw base64.
    /// Required in raw-key mode (when --from is not used).
    #[arg(long)]
    pubkey_file: Option<PathBuf>,

    /// Owner label (informational, raw-key mode only). Defaults to "third-party".
    #[arg(long, default_value = "third-party")]
    owner: String,

    /// User space root (parent of `.ai/`). Defaults to the canonical user root.
    #[arg(long)]
    user_root: Option<PathBuf>,
}

fn run_trust_pin_verb(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<PinArgs>(argv)?;
    let user_root = args.user_root.unwrap_or_else(default_user_root);

    if let Some(trust_file) = args.from {
        // Canonical mode: pin from PUBLISHER_TRUST.toml
        let report = run_pin_from(&PinFromOptions {
            user_root,
            trust_file,
        })
        .context("ryeos trust pin --from failed")?;
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        // Raw-key mode: <fingerprint> --pubkey-file <path>
        let fp = args
            .fingerprint
            .ok_or_else(|| anyhow!("positional <fingerprint> required when --from is not used"))?;
        let pkf = args
            .pubkey_file
            .ok_or_else(|| anyhow!("--pubkey-file required when --from is not used"))?;
        let report = run_pin(&PinOptions {
            user_root,
            expected_fingerprint: fp,
            pubkey_file: pkf,
            owner: args.owner,
        })
        .context("ryeos trust pin failed")?;
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    Ok(())
}

// ── ryeos vault {put,list,remove,rewrap} ───────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos vault put",
    about = "Add or overwrite a secret in the sealed secret store",
    no_binary_name = true
)]
struct VaultPutArgs {
    /// Name of the secret (e.g. `ZEN_API_KEY`).
    #[arg(long)]
    name: String,

    /// Read the secret value from stdin (default).
    /// Mutually exclusive with `--value-string`.
    #[arg(long, conflicts_with = "value_string")]
    value_stdin: bool,

    /// Pass the secret value directly on the command line.
    /// **Insecure** — leaks to shell history / argv / process listings.
    /// Use only in scripted contexts where stdin is unavailable.
    #[arg(long, conflicts_with = "value_stdin")]
    value_string: Option<String>,

    /// System space root. Defaults to XDG data dir / ryeos.
    #[arg(long)]
    system_space_dir: Option<PathBuf>,
}

fn run_vault_put_verb(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<VaultPutArgs>(argv)?;
    let _ = args.value_stdin;

    let value: String = if let Some(v) = args.value_string {
        v
    } else {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| anyhow::anyhow!("failed to read secret from stdin: {e}"))?;
        if buf.ends_with('\n') {
            buf.pop();
        }
        if buf.ends_with('\r') {
            buf.pop();
        }
        buf
    };

    let system_space_dir = args
        .system_space_dir
        .unwrap_or_else(default_system_space_dir);
    let report = run_vault_put(&PutOptions {
        system_space_dir,
        entries: vec![(args.name, value)],
    })
    .context("ryeos vault put failed")?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos vault list",
    about = "List the keys currently in the sealed secret store (values are NOT printed)",
    no_binary_name = true
)]
struct VaultListArgs {
    /// System space root. Defaults to XDG data dir / ryeos.
    #[arg(long)]
    system_space_dir: Option<PathBuf>,
}

fn run_vault_list_verb(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<VaultListArgs>(argv)?;
    let system_space_dir = args
        .system_space_dir
        .unwrap_or_else(default_system_space_dir);
    let report = run_vault_list(&ListOptions {
        system_space_dir: system_space_dir,
    })
    .context("ryeos vault list failed")?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos vault remove",
    about = "Remove KEYs from the sealed secret store (idempotent on missing keys)",
    no_binary_name = true
)]
struct VaultRemoveArgs {
    /// Keys to remove.
    #[arg(required = true)]
    keys: Vec<String>,

    /// System space root. Defaults to XDG data dir / ryeos.
    #[arg(long)]
    system_space_dir: Option<PathBuf>,
}

fn run_vault_remove_verb(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<VaultRemoveArgs>(argv)?;
    let system_space_dir = args
        .system_space_dir
        .unwrap_or_else(default_system_space_dir);
    let report = run_vault_remove(&RemoveOptions {
        system_space_dir: system_space_dir,
        keys: args.keys,
    })
    .context("ryeos vault remove failed")?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos vault rewrap",
    about = "Rotate the vault X25519 keypair and re-seal the store under the new identity",
    no_binary_name = true
)]
struct VaultRewrapArgs {
    /// System space root. Defaults to XDG data dir / ryeos.
    #[arg(long)]
    system_space_dir: Option<PathBuf>,
}

fn run_vault_rewrap_verb(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<VaultRewrapArgs>(argv)?;
    let system_space_dir = args
        .system_space_dir
        .unwrap_or_else(default_system_space_dir);
    let report = run_vault_rewrap(&RewrapOptions {
        system_space_dir: system_space_dir,
    })
    .context("ryeos vault rewrap failed")?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

// ── ryeos publish ────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos publish",
    about = "Sign all bundle items, rebuild CAS manifest, emit publisher trust doc",
    no_binary_name = true
)]
struct PublishArgs {
    /// Bundle source root (the directory containing `.ai/`).
    bundle_source: PathBuf,

    /// Registry root supplying kind schemas + parsers. Defaults to `bundle_source`
    /// (suitable when publishing `core` itself).
    #[arg(long)]
    registry_root: Option<PathBuf>,

    /// Path to a PEM-encoded Ed25519 signing key. Required.
    #[arg(long)]
    key: PathBuf,

    /// Owner label in PUBLISHER_TRUST.toml (e.g. "ryeos-official", "ryeos-dev").
    #[arg(long)]
    owner: String,

    /// Suppress emitting `<bundle_source>/PUBLISHER_TRUST.toml`.
    #[arg(long)]
    no_trust_doc: bool,
}

fn run_publish_verb(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<PublishArgs>(argv)?;
    let signing_key = load_signing_key(&args.key)?;
    let registry_root = args
        .registry_root
        .unwrap_or_else(|| args.bundle_source.clone());
    let report = run_publish(&PublishOptions {
        bundle_source: args.bundle_source,
        registry_root,
        signing_key,
        owner: args.owner,
        emit_trust_doc: !args.no_trust_doc,
    })
    .context("ryeos publish failed")?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// Parse argv with clap, but treat `--help` / `--version` as a successful
/// exit (print to stdout, exit 0) rather than an error. Other parse
/// failures are mapped to anyhow errors that propagate as `CliError::Local`.
fn parse_or_handle_help<P: Parser>(argv: &[String]) -> Result<P> {
    use clap::error::ErrorKind;
    match P::try_parse_from(argv) {
        Ok(p) => Ok(p),
        Err(e) => match e.kind() {
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                // Clap formatted the help/version text into the error;
                // print it cleanly and exit 0.
                let s = e.render().to_string();
                print!("{s}");
                std::process::exit(0);
            }
            _ => Err(anyhow!("{e}")),
        },
    }
}

fn load_signing_key(path: &std::path::Path) -> Result<SigningKey> {
    let pem = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    SigningKey::from_pkcs8_pem(&pem).with_context(|| format!("decode {}", path.display()))
}

// ── Defaults ────────────────────────────────────────────────────────

fn default_system_space_dir() -> PathBuf {
    if let Ok(p) = std::env::var("RYEOS_SYSTEM_SPACE_DIR") {
        return PathBuf::from(p);
    }
    dirs::data_dir()
        .map(|d| d.join("ryeos"))
        .expect("could not determine XDG data directory")
}

fn default_user_root() -> PathBuf {
    roots::user_root()
        .ok()
        .unwrap_or_else(|| PathBuf::from("."))
}
