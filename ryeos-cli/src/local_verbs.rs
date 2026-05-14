//! Hardcoded CLI verbs that run LOCALLY without dispatching to the daemon.
//!
//! These verbs operate on operator state that exists before the daemon is
//! up (`ryeos init`), or that is operator-tier filesystem state the daemon
//! never owns (`ryeos trust pin`), or that is publisher-tier authoring state
//! disjoint from any daemon (`ryeos publish`). They are matched by the
//! dispatcher BEFORE the verb table lookup so they always work even on
//! a fresh checkout with no `core` bundle present.
//!
//! Each verb here parses its own argv slice with clap, runs an action in
//! `ryeos-tools`, and prints a JSON report on success.
//!
//! No daemon round-trip. No verb-table dependency. No trust-store load
//! before keys exist.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use lillux::crypto::{DecodePrivateKey, SigningKey};

use ryeos_engine::roots;
use ryeos_tools::actions::init::{run_init, InitOptions};
use ryeos_tools::actions::publish::{run_publish, PublishOptions};
use ryeos_tools::actions::trust::{run_pin, run_pin_from, PinFromOptions, PinOptions};
use ryeos_tools::actions::vault::{
    run_list as run_vault_list, run_put as run_vault_put,
    run_remove as run_vault_remove, run_rewrap as run_vault_rewrap, ListOptions,
    PutOptions, RemoveOptions, RewrapOptions,
};

use crate::error::CliError;

/// Returns `Ok(true)` if the argv was handled by a local verb, `Ok(false)`
/// if no match (caller should fall through to verb-table dispatch).
///
/// Errors from a matched local verb propagate as `CliError::Local`.
pub fn try_dispatch(argv: &[String]) -> Result<bool, CliError> {
    if argv.is_empty() {
        return Ok(false);
    }
    match argv[0].as_str() {
        "init" => {
            run_init_verb(&argv[1..]).map_err(map_local_err)?;
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

    /// User space root (parent of `~/.ai/`). Defaults to $HOME.
    #[arg(long)]
    user_root: Option<PathBuf>,

    /// Source directory containing bundle subdirectories.
    /// Each immediate child with a `.ai/` subdirectory is installed as a bundle.
    /// Examples: `/usr/share/ryeos` (packaged), `ryeos-bundles` (dev), `/opt/ryeos` (docker).
    #[arg(long)]
    source: PathBuf,

    /// Additional publisher trust doc(s) to pin before verifying bundles.
    /// Each file should be a PUBLISHER_TRUST.toml with public_key and fingerprint.
    /// Repeatable: `--trust-file a.toml --trust-file b.toml`.
    #[arg(long = "trust-file", action = clap::ArgAction::Append)]
    trust_files: Vec<PathBuf>,

    /// Force-regenerate the node signing key. Does NOT touch the user key.
    #[arg(long)]
    force_node_key: bool,
}

fn run_init_verb(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<InitArgs>(argv)?;
    let system_space_dir = args.system_space_dir.unwrap_or_else(default_system_space_dir);
    let user_root = args.user_root.unwrap_or_else(default_user_root);

    let opts = InitOptions {
        system_space_dir,
        user_root,
        source_dir: args.source,
        force_node_key: args.force_node_key,
        trust_files: args.trust_files,
        skip_preflight: false,
    };
    let report = run_init(&opts).context("ryeos init failed")?;
    println!("{}", serde_json::to_string_pretty(&report)?);
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

    /// User space root (parent of `~/.ai/`). Defaults to $HOME.
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
        if buf.ends_with('\n') { buf.pop(); }
        if buf.ends_with('\r') { buf.pop(); }
        buf
    };

    let system_space_dir = args.system_space_dir.unwrap_or_else(default_system_space_dir);
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
    let system_space_dir = args.system_space_dir.unwrap_or_else(default_system_space_dir);
    let report = run_vault_list(&ListOptions { system_space_dir: system_space_dir })
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
    let system_space_dir = args.system_space_dir.unwrap_or_else(default_system_space_dir);
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
    let system_space_dir = args.system_space_dir.unwrap_or_else(default_system_space_dir);
    let report = run_vault_rewrap(&RewrapOptions { system_space_dir: system_space_dir })
        .context("ryeos vault rewrap failed")?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

// ── ryos publish ─────────────────────────────────────────────────────

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
    let registry_root = args.registry_root.unwrap_or_else(|| args.bundle_source.clone());
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
    let pem = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    SigningKey::from_pkcs8_pem(&pem)
        .with_context(|| format!("decode {}", path.display()))
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
    roots::user_root().ok().unwrap_or_else(|| PathBuf::from("."))
}
