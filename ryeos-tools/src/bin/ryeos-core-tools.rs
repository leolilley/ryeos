//! `ryeos-core-tools` — unified core tools binary.
//!
//! Subcommands: sign, fetch, verify, identity, authorize-client.
//!
//! Multi-tool binary for signing and inspecting RyeOS items.
//! Invoked by tool YAMLs via `bin:ryeos-core-tools <subcommand>`.
//!
//! Each subcommand supports two input modes:
//!  * argv (clap) — direct CLI invocation
//!  * `--stdin-json` — reads a JSON object from stdin (used by subprocess tools)

use std::io::{self, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use base64::Engine;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "ryeos-core-tools",
    about = "Unified core tools binary (sign, fetch, verify, identity, authorize-client)",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,

    /// Read params as JSON from stdin instead of using CLI flags.
    #[arg(long, global = true)]
    stdin_json: bool,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Sign a Rye item by canonical ref after path-anchoring validation.
    Sign {
        /// Canonical ref of the item to sign.
        #[arg(value_name = "ITEM_REF")]
        item_ref: Option<String>,

        /// Project root (parent of `.ai/`).
        #[arg(long)]
        project: Option<PathBuf>,

        /// Where to look for the item: `project` or `user`.
        #[arg(long, default_value = "project")]
        source: String,
    },

    /// Resolve, optionally verify, and read an item.
    Fetch {
        /// Canonical ref to fetch.
        #[arg(long)]
        item_ref: Option<String>,

        /// Include the full file content in the report.
        #[arg(long)]
        with_content: bool,

        /// Also verify trust status.
        #[arg(long)]
        verify: bool,

        /// Project root path.
        #[arg(long)]
        project_path: Option<String>,
    },

    /// Resolve and trust-verify an item.
    Verify {
        /// Canonical ref to verify.
        #[arg(long)]
        item_ref: Option<String>,

        /// Project root path.
        #[arg(long)]
        project_path: Option<String>,
    },

    /// Return the node's public identity document.
    Identity {
        /// System space directory.
        #[arg(long)]
        system_space_dir: Option<String>,
    },

    /// Authorize an HTTP client to call the daemon's authenticated endpoints.
    AuthorizeClient {
        /// System space directory (contains `.ai/node/identity/`).
        #[arg(long)]
        system_space_dir: Option<String>,

        /// Client public key as ed25519 base64.
        #[arg(long)]
        public_key: Option<String>,

        /// Comma-separated scopes to grant (default: "*").
        #[arg(long, default_value = "*")]
        scopes: String,

        /// Human-readable label for the authorized key.
        #[arg(long, default_value = "cli-authorized")]
        label: String,
    },

    /// Manage sealed secrets in the daemon vault.
    Vault {
        #[command(subcommand)]
        cmd: VaultCmd,
    },
}

#[derive(Subcommand, Debug)]
enum VaultCmd {
    /// Put a secret into the sealed vault store.
    ///
    /// By default the value is read from stdin (so it never touches argv,
    /// shell history, or process listings). For non-interactive scripts
    /// you may pass `--value-string`.
    Put {
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

        /// System space directory.
        #[arg(long)]
        system_space_dir: Option<String>,
    },

    /// List key names in the sealed vault store (values are not printed).
    List {
        /// System space directory.
        #[arg(long)]
        system_space_dir: Option<String>,
    },

    /// Remove keys from the sealed vault store.
    Rm {
        /// Key names to remove.
        #[arg(required = true)]
        keys: Vec<String>,

        /// System space directory.
        #[arg(long)]
        system_space_dir: Option<String>,
    },

    /// Re-encrypt every entry in the sealed vault store under a
    /// freshly-generated vault keypair.
    Rewrap {
        /// System space directory.
        #[arg(long)]
        system_space_dir: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ryeos-core-tools: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.cmd {
        Cmd::Sign {
            item_ref,
            project,
            source,
        } => run_sign(item_ref, project, source, cli.stdin_json),
        Cmd::Fetch {
            item_ref,
            with_content,
            verify,
            project_path,
        } => {
            let params = if cli.stdin_json {
                read_stdin_json()?
            } else {
                let ir = item_ref.ok_or_else(|| anyhow::anyhow!("--item-ref required"))?;
                let mut obj = serde_json::json!({
                    "item_ref": ir,
                    "with_content": with_content,
                    "verify": verify,
                });
                if let Some(p) = project_path {
                    obj["project_path"] = serde_json::json!(p);
                }
                obj
            };
            let params: ryeos_tools::actions::inspect::fetch::FetchParams =
                serde_json::from_value(params)?;
            let engine = ryeos_tools::actions::inspect::boot(
                params.project_path.as_deref().map(std::path::Path::new),
            )?;
            let report = ryeos_tools::actions::inspect::fetch::run_fetch(params, &engine)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        Cmd::Verify {
            item_ref,
            project_path,
        } => {
            let params = if cli.stdin_json {
                read_stdin_json()?
            } else {
                let ir = item_ref.ok_or_else(|| anyhow::anyhow!("--item-ref required"))?;
                let mut obj = serde_json::json!({ "item_ref": ir });
                if let Some(p) = project_path {
                    obj["project_path"] = serde_json::json!(p);
                }
                obj
            };
            let params: ryeos_tools::actions::inspect::verify::VerifyParams =
                serde_json::from_value(params)?;
            let engine = ryeos_tools::actions::inspect::boot(
                params.project_path.as_deref().map(std::path::Path::new),
            )?;
            let report = ryeos_tools::actions::inspect::verify::run_verify(params, &engine)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        Cmd::Identity { system_space_dir } => {
            let params = if cli.stdin_json {
                read_stdin_json()?
            } else {
                let mut obj = serde_json::json!({});
                if let Some(s) = system_space_dir {
                    obj["system_space_dir"] = serde_json::json!(s);
                }
                obj
            };
            let params: ryeos_tools::actions::inspect::identity::IdentityParams =
                serde_json::from_value(params)?;
            let report = ryeos_tools::actions::inspect::identity::run_identity(params)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        Cmd::AuthorizeClient {
            system_space_dir,
            public_key,
            scopes,
            label,
        } => run_authorize_client(system_space_dir, public_key, scopes, label, cli.stdin_json),
        Cmd::Vault { cmd } => run_vault(cmd),
    }
}

fn run_sign(
    item_ref: Option<String>,
    project: Option<PathBuf>,
    source: String,
    stdin_json: bool,
) -> anyhow::Result<()> {
    use ryeos_tools::actions::sign::{run_sign, SignSource};

    let (item_ref, project_arg, source_str) = if stdin_json {
        if item_ref.is_some() {
            anyhow::bail!("--stdin-json is mutually exclusive with positional ITEM_REF");
        }
        let parsed: StdinSignParams = serde_json::from_value(read_stdin_json()?)?;
        (parsed.item_ref, parsed.project_path, parsed.source)
    } else {
        let ir = item_ref.ok_or_else(|| anyhow::anyhow!("ITEM_REF required (or pass --stdin-json)"))?;
        (ir, project, source)
    };

    let source = SignSource::parse(&source_str)?;
    let project = project_arg.or_else(|| std::env::current_dir().ok());

    let batch = run_sign(&item_ref, project.as_deref(), source)?;
    println!("{}", serde_json::to_string_pretty(&batch)?);
    if !batch.is_total_success() {
        eprintln!(
            "✗ {}/{} items failed validation or signing",
            batch.failed.len(),
            batch.total()
        );
    }
    Ok(())
}

#[derive(serde::Deserialize)]
struct StdinSignParams {
    item_ref: String,
    #[serde(default)]
    project_path: Option<PathBuf>,
    #[serde(default = "default_source")]
    source: String,
}

fn default_source() -> String {
    "project".to_string()
}

fn resolve_system_space_dir(opt: Option<String>) -> anyhow::Result<std::path::PathBuf> {
    opt.map(std::path::PathBuf::from)
        .or_else(|| std::env::var("RYEOS_SYSTEM_SPACE_DIR").ok().map(std::path::PathBuf::from))
        .ok_or_else(|| anyhow::anyhow!("--system-space-dir or RYEOS_SYSTEM_SPACE_DIR required"))
}

fn run_vault(cmd: VaultCmd) -> anyhow::Result<()> {
    match cmd {
        VaultCmd::Put {
            name,
            value_stdin,
            value_string,
            system_space_dir,
        } => {
            let ssd = resolve_system_space_dir(system_space_dir)?;
            let _ = value_stdin;

            let value: String = if let Some(v) = value_string {
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

            let report = ryeos_tools::actions::vault::run_put(
                &ryeos_tools::actions::vault::PutOptions {
                    system_space_dir: ssd,
                    entries: vec![(name, value)],
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        VaultCmd::List { system_space_dir } => {
            let ssd = resolve_system_space_dir(system_space_dir)?;
            let report = ryeos_tools::actions::vault::run_list(
                &ryeos_tools::actions::vault::ListOptions {
                    system_space_dir: ssd,
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        VaultCmd::Rm {
            keys,
            system_space_dir,
        } => {
            let ssd = resolve_system_space_dir(system_space_dir)?;
            let report = ryeos_tools::actions::vault::run_remove(
                &ryeos_tools::actions::vault::RemoveOptions {
                    system_space_dir: ssd,
                    keys,
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        VaultCmd::Rewrap { system_space_dir } => {
            let ssd = resolve_system_space_dir(system_space_dir)?;
            let report = ryeos_tools::actions::vault::run_rewrap(
                &ryeos_tools::actions::vault::RewrapOptions {
                    system_space_dir: ssd,
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
    }
}

fn run_authorize_client(
    system_space_dir: Option<String>,
    public_key: Option<String>,
    scopes: String,
    label: String,
    stdin_json: bool,
) -> anyhow::Result<()> {
    use ryeos_tools::actions::authorize::{run_authorize_client as run, AuthorizeClientParams};
    use lillux::crypto::VerifyingKey;

    let params = if stdin_json {
        let val = read_stdin_json()?;
        let ssd = val["system_space_dir"]
            .as_str()
            .map(String::from);
        let pk = val["public_key"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("public_key required"))?
            .to_string();
        let sc = val["scopes"]
            .as_str()
            .unwrap_or("*")
            .to_string();
        let lb = val["label"]
            .as_str()
            .unwrap_or("cli-authorized")
            .to_string();
        (ssd, pk, sc, lb)
    } else {
        let pk = public_key.ok_or_else(|| anyhow::anyhow!("--public-key required"))?;
        (system_space_dir, pk, scopes, label)
    };

    let (ssd, pk_b64, scopes_str, label) = params;

    let system_space_dir = resolve_system_space_dir(ssd)?;

    let pk_bytes = base64::engine::general_purpose::STANDARD
        .decode(&pk_b64)
        .map_err(|e| anyhow::anyhow!("invalid base64 public key: {e}"))?;
    let verifying_key = VerifyingKey::from_bytes(
        pk_bytes.as_slice().try_into()
            .map_err(|_| anyhow::anyhow!("public key must be 32 bytes (ed25519)"))?,
    )
    .map_err(|e| anyhow::anyhow!("invalid ed25519 public key: {e}"))?;

    let scopes: Vec<String> = scopes_str.split(',').map(|s| s.trim().to_string()).collect();

    let result = run(AuthorizeClientParams {
        system_space_dir,
        public_key: verifying_key,
        scopes,
        label,
    })?;

    println!("{}", serde_json::to_string_pretty(&serde_json::json!({
        "fingerprint": result.fingerprint,
        "path": result.path.to_string_lossy(),
    }))?);

    Ok(())
}

fn read_stdin_json() -> anyhow::Result<serde_json::Value> {
    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf)?;
    serde_json::from_str(&buf).map_err(|e| anyhow::anyhow!("parse stdin JSON: {e}"))
}
