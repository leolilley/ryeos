//! `ryeos-core-tools` — unified core tools binary.
//!
//! Subcommands: sign, fetch, verify, snapshot, identity, authorize-client,
//! admission-token, remote-descriptor.
//!
//! Multi-tool binary for signing and inspecting RyeOS items.
//! Invoked by tool YAMLs via `bin:ryeos-core-tools <subcommand>`.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context;
use base64::Engine;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "ryeos-core-tools",
    about = "Unified core tools binary (sign, fetch, verify, snapshot, identity, authorize-client, admission-token, remote-descriptor)",
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

    /// Build (re-publish) a bundle from source using the user signing key.
    ///
    /// Runs the full publish pipeline: clean derived artifacts, bootstrap-sign
    /// kind schemas and parsers, rebuild the CAS manifest (binary hashes),
    /// sign all items, and generate the bundle manifest. The signing key is
    /// auto-resolved from the user root — no `--key` flag needed.
    Build {
        /// Bundle source root (directory containing `.ai/`).
        bundle_source: Option<PathBuf>,

        /// Registry/dependency root supplying kind schemas + parsers.
        /// Defaults to `bundle_source` (suitable when building `core` itself).
        /// May be repeated for bundles that depend on multiple bundle roots.
        #[arg(long = "registry-root")]
        registry_roots: Vec<PathBuf>,

        /// Owner label for the trust doc. Defaults to "local-dev".
        #[arg(long, default_value = "local-dev")]
        owner: String,

        /// Suppress emitting `<bundle_source>/PUBLISHER_TRUST.toml`.
        #[arg(long)]
        no_trust_doc: bool,
    },

    /// Verify a bundle source tree without rewriting files.
    BundleVerify {
        /// Bundle source root (directory containing `.ai/`).
        source: Option<PathBuf>,

        /// Registry/dependency root to include while validating. May be repeated.
        #[arg(long = "registry-root")]
        registry_roots: Vec<PathBuf>,
    },

    /// Sign all signable items in a bundle source tree.
    BundleSign {
        /// Bundle source root (directory containing `.ai/`).
        source: Option<PathBuf>,

        /// Registry/dependency root supplying kind schemas, parsers, and handlers.
        /// Defaults to installed bundle roots. May be repeated.
        #[arg(long = "registry-root")]
        registry_roots: Vec<PathBuf>,
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

    /// Inspect and create local project snapshots.
    Snapshot {
        #[command(subcommand)]
        cmd: SnapshotCmd,
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

        /// Comma-separated scopes to grant (required).
        #[arg(long)]
        scopes: Option<String>,

        /// Human-readable label for the authorized key.
        #[arg(long, default_value = "cli-authorized")]
        label: String,
    },

    /// Mint a one-time node-local admission token for remote bootstrap.
    AdmissionToken {
        /// System space directory for the target node.
        #[arg(long)]
        system_space_dir: Option<String>,

        /// Comma-separated scopes this token may grant.
        #[arg(long)]
        scopes: Option<String>,

        /// Optional default label for the authorized key created by claim.
        #[arg(long)]
        label: Option<String>,

        /// Token lifetime in seconds.
        #[arg(long, default_value_t = 600)]
        ttl_secs: u64,
    },

    /// Export a remote descriptor trust pin for this node.
    RemoteDescriptor {
        /// System space directory for the node being described.
        #[arg(long)]
        system_space_dir: Option<String>,

        /// Name callers should use for the remote.
        #[arg(long)]
        name: Option<String>,

        /// Public URL callers should use to reach the node.
        #[arg(long)]
        url: Option<String>,

        /// Comma-separated informational capability labels.
        #[arg(long)]
        capabilities: Option<String>,

        /// Admission mode label to advertise. Defaults to hosted policy or one_time_token.
        #[arg(long)]
        admission_mode: Option<String>,

        /// Optional provider/operator label.
        #[arg(long)]
        provider_name: Option<String>,

        /// Optional output path for the descriptor YAML.
        #[arg(long)]
        output: Option<PathBuf>,
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

#[derive(Subcommand, Debug)]
enum SnapshotCmd {
    /// Compare the worktree with the principal's project head snapshot.
    Status {
        /// Project root path.
        #[arg(long)]
        project_path: Option<PathBuf>,

        /// Include unchanged files in the changes list.
        #[arg(long)]
        include_unchanged: bool,
    },

    /// Show recent snapshots from the principal's project head.
    Log {
        /// Project root path.
        #[arg(long)]
        project_path: Option<PathBuf>,

        /// Maximum snapshots to return.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },

    /// Create a new local project snapshot from the current worktree.
    Create {
        /// Project root path.
        #[arg(long)]
        project_path: Option<PathBuf>,

        /// Snapshot message.
        #[arg(long)]
        message: Option<String>,

        /// Create even when the manifest matches the current head.
        #[arg(long)]
        allow_empty: bool,
    },

    /// Show metadata for a snapshot object.
    Show {
        /// Snapshot hash to inspect.
        snapshot_hash: Option<String>,

        /// Project root path, used to include head/deployed relation flags.
        #[arg(long)]
        project_path: Option<PathBuf>,
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
        Cmd::Build {
            bundle_source,
            registry_roots,
            owner,
            no_trust_doc,
        } => run_build(
            bundle_source,
            registry_roots,
            owner,
            no_trust_doc,
            cli.stdin_json,
        ),
        Cmd::BundleVerify {
            source,
            registry_roots,
        } => run_bundle_verify(source, registry_roots, cli.stdin_json),
        Cmd::BundleSign {
            source,
            registry_roots,
        } => run_bundle_sign(source, registry_roots, cli.stdin_json),
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
        Cmd::Snapshot { cmd } => run_snapshot(cmd, cli.stdin_json),
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
        } => {
            let scopes = scopes.ok_or_else(|| anyhow::anyhow!(
                "--scopes required, comma-separated, in canonical form. \
                 Example: --scopes ryeos.execute.service.remote.admin,ryeos.execute.service.bundle.install"
            ))?;
            run_authorize_client(system_space_dir, public_key, scopes, label, cli.stdin_json)
        }
        Cmd::AdmissionToken {
            system_space_dir,
            scopes,
            label,
            ttl_secs,
        } => run_admission_token(system_space_dir, scopes, label, ttl_secs, cli.stdin_json),
        Cmd::RemoteDescriptor {
            system_space_dir,
            name,
            url,
            capabilities,
            admission_mode,
            provider_name,
            output,
        } => run_remote_descriptor(
            system_space_dir,
            name,
            url,
            capabilities,
            admission_mode,
            provider_name,
            output,
            cli.stdin_json,
        ),
        Cmd::Vault { cmd } => run_vault(cmd),
    }
}

fn run_snapshot(cmd: SnapshotCmd, stdin_json: bool) -> anyhow::Result<()> {
    use ryeos_tools::actions::snapshot::{
        run_create, run_log, run_show, run_status, SnapshotCreateParams, SnapshotLogParams,
        SnapshotShowParams, SnapshotStatusParams,
    };

    match cmd {
        SnapshotCmd::Status {
            project_path,
            include_unchanged,
        } => {
            let params = if stdin_json {
                serde_json::from_value(read_stdin_json()?)?
            } else {
                SnapshotStatusParams {
                    project_path: project_path
                        .or_else(|| std::env::current_dir().ok())
                        .ok_or_else(|| anyhow::anyhow!("--project-path required"))?,
                    include_unchanged,
                }
            };
            println!("{}", serde_json::to_string_pretty(&run_status(params)?)?);
            Ok(())
        }
        SnapshotCmd::Log {
            project_path,
            limit,
        } => {
            let params = if stdin_json {
                serde_json::from_value(read_stdin_json()?)?
            } else {
                SnapshotLogParams {
                    project_path: project_path
                        .or_else(|| std::env::current_dir().ok())
                        .ok_or_else(|| anyhow::anyhow!("--project-path required"))?,
                    limit,
                }
            };
            println!("{}", serde_json::to_string_pretty(&run_log(params)?)?);
            Ok(())
        }
        SnapshotCmd::Create {
            project_path,
            message,
            allow_empty,
        } => {
            let params = if stdin_json {
                serde_json::from_value(read_stdin_json()?)?
            } else {
                SnapshotCreateParams {
                    project_path: project_path
                        .or_else(|| std::env::current_dir().ok())
                        .ok_or_else(|| anyhow::anyhow!("--project-path required"))?,
                    message,
                    allow_empty,
                }
            };
            println!("{}", serde_json::to_string_pretty(&run_create(params)?)?);
            Ok(())
        }
        SnapshotCmd::Show {
            snapshot_hash,
            project_path,
        } => {
            let params = if stdin_json {
                serde_json::from_value(read_stdin_json()?)?
            } else {
                SnapshotShowParams {
                    snapshot_hash: snapshot_hash
                        .ok_or_else(|| anyhow::anyhow!("SNAPSHOT_HASH required"))?,
                    project_path,
                }
            };
            println!("{}", serde_json::to_string_pretty(&run_show(params)?)?);
            Ok(())
        }
    }
}

fn run_build(
    bundle_source: Option<PathBuf>,
    registry_roots: Vec<PathBuf>,
    owner: String,
    no_trust_doc: bool,
    stdin_json: bool,
) -> anyhow::Result<()> {
    use ryeos_engine::roots;
    use ryeos_tools::actions::publish::{run_publish, PublishOptions};

    let (bundle_source, mut registry_roots, owner, no_trust_doc) = if stdin_json {
        if bundle_source.is_some() {
            anyhow::bail!("--stdin-json is mutually exclusive with positional BUNDLE_SOURCE");
        }
        let params: BundlePublishParams = serde_json::from_value(read_stdin_json()?)?;
        let registry_roots = params.registry_roots();
        (
            params.source,
            registry_roots,
            params.owner.unwrap_or_else(|| "local-dev".to_string()),
            params.no_trust_doc,
        )
    } else {
        let source = bundle_source
            .ok_or_else(|| anyhow::anyhow!("BUNDLE_SOURCE required (or pass --stdin-json)"))?;
        (source, registry_roots, owner, no_trust_doc)
    };

    let user_root = roots::user_root().map_err(|_| anyhow::anyhow!("cannot resolve user root"))?;
    let key_path = user_root
        .join(ryeos_engine::AI_DIR)
        .join("config")
        .join("keys")
        .join("signing")
        .join("private_key.pem");

    if !key_path.exists() {
        anyhow::bail!(
            "user signing key not found at {} — run `ryeos init` first",
            key_path.display()
        );
    }

    let signing_key = ryeos_tools::actions::build_bundle::load_signing_key(&key_path)
        .with_context(|| format!("load signing key from {}", key_path.display()))?;

    if registry_roots.is_empty() {
        registry_roots.push(bundle_source.clone());
    }

    let report = run_publish(&PublishOptions {
        bundle_source,
        registry_roots,
        signing_key,
        owner,
        emit_trust_doc: !no_trust_doc,
    })?;

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[derive(serde::Deserialize)]
struct BundlePublishParams {
    source: PathBuf,
    #[serde(default)]
    registry_root: Option<PathBuf>,
    #[serde(default)]
    registry_roots: Vec<PathBuf>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    no_trust_doc: bool,
}

impl BundlePublishParams {
    fn registry_roots(&self) -> Vec<PathBuf> {
        if self.registry_roots.is_empty() {
            self.registry_root.iter().cloned().collect()
        } else {
            self.registry_roots.clone()
        }
    }
}

fn run_bundle_verify(
    source: Option<PathBuf>,
    registry_roots: Vec<PathBuf>,
    stdin_json: bool,
) -> anyhow::Result<()> {
    let (source, registry_roots) = if stdin_json {
        if source.is_some() {
            anyhow::bail!("--stdin-json is mutually exclusive with positional SOURCE");
        }
        let params: BundleVerifyParams = serde_json::from_value(read_stdin_json()?)?;
        let registry_roots = params.registry_roots();
        (params.source, registry_roots)
    } else {
        let source =
            source.ok_or_else(|| anyhow::anyhow!("SOURCE required (or pass --stdin-json)"))?;
        (source, registry_roots)
    };

    let source_path = std::fs::canonicalize(&source)
        .with_context(|| format!("resolve bundle source path {}", source.display()))?;
    if !source_path.is_dir() {
        anyhow::bail!("--source is not a directory: {}", source_path.display());
    }

    let system_space_dir = std::env::var("RYEOS_SYSTEM_SPACE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::data_dir()
                .map(|d| d.join("ryeos"))
                .expect("could not determine XDG data directory")
        });
    let user_root = ryeos_engine::roots::user_root().ok();
    let dependency_roots = bundle_verify_dependency_roots(
        &source_path,
        registry_roots,
        &system_space_dir,
        user_root.as_deref(),
    )?;

    let preflight_report = ryeos_bundle::preflight::preflight_verify_bundle_report_in_context(
        &source_path,
        &dependency_roots,
        user_root.as_deref(),
    )
    .context("bundle verify failed")?;

    let warnings: Vec<serde_json::Value> = preflight_report
        .warnings
        .iter()
        .map(|warning| {
            serde_json::json!({
                "item_path": warning.item_path,
                "severity": "warning",
                "code": warning.code.to_string(),
                "path": warning.path,
                "expected": warning.expected,
                "found": warning.found,
            })
        })
        .collect();

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "source": source_path,
            "status": "verified",
            "detail": "all items pass signature, metadata, and applicable contract validation",
            "warnings": warnings,
        }))?
    );
    Ok(())
}

fn bundle_verify_dependency_roots(
    source_path: &Path,
    registry_roots: Vec<PathBuf>,
    system_space_dir: &Path,
    user_root: Option<&Path>,
) -> anyhow::Result<Vec<PathBuf>> {
    let mut dependency_roots: Vec<PathBuf> = Vec::new();
    if !registry_roots.is_empty() {
        for root in registry_roots {
            let root = std::fs::canonicalize(&root)
                .with_context(|| format!("resolve registry root {}", root.display()))?;
            if root != source_path && !dependency_roots.iter().any(|seen| seen == &root) {
                dependency_roots.push(root);
            }
        }
    } else {
        let installed_roots =
            ryeos_bundle::installed::load_installed_bundle_records(system_space_dir, user_root)
                .context("bundle verify: load installed bundle registrations")?
                .into_iter()
                .filter(|r| r.bundle_root != source_path)
                .map(|r| r.bundle_root);
        dependency_roots.extend(installed_roots);
    }
    Ok(dependency_roots)
}

#[derive(serde::Deserialize)]
struct BundleVerifyParams {
    source: PathBuf,
    #[serde(default)]
    registry_root: Option<PathBuf>,
    #[serde(default)]
    registry_roots: Vec<PathBuf>,
}

impl BundleVerifyParams {
    fn registry_roots(&self) -> Vec<PathBuf> {
        if self.registry_roots.is_empty() {
            self.registry_root.iter().cloned().collect()
        } else {
            self.registry_roots.clone()
        }
    }
}

fn run_bundle_sign(
    source: Option<PathBuf>,
    registry_roots: Vec<PathBuf>,
    stdin_json: bool,
) -> anyhow::Result<()> {
    let (source, registry_roots) = if stdin_json {
        if source.is_some() {
            anyhow::bail!("--stdin-json is mutually exclusive with positional SOURCE");
        }
        let params: BundleSignParams = serde_json::from_value(read_stdin_json()?)?;
        let registry_roots = params.registry_roots();
        (params.source, registry_roots)
    } else {
        let source =
            source.ok_or_else(|| anyhow::anyhow!("SOURCE required (or pass --stdin-json)"))?;
        (source, registry_roots)
    };

    let source_path = canonical_bundle_source(&source)?;
    let system_space_dir = std::env::var("RYEOS_SYSTEM_SPACE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::data_dir()
                .map(|d| d.join("ryeos"))
                .expect("could not determine XDG data directory")
        });
    let user_root = ryeos_engine::roots::user_root()
        .map_err(|_| anyhow::anyhow!("cannot resolve user root"))?;
    let dependency_roots = bundle_verify_dependency_roots(
        &source_path,
        registry_roots,
        &system_space_dir,
        Some(&user_root),
    )?;
    let signing_key = load_user_signing_key(&user_root)?;
    let trust_store =
        ryeos_engine::trust::TrustStore::load_three_tier(None, Some(&user_root), &dependency_roots)
            .context("load trust store for registry roots")?;

    if source_path.join(ryeos_engine::AI_DIR).join("bin").is_dir() {
        ryeos_tools::actions::build_bundle::rebuild_bundle_manifest(&source_path, &signing_key)
            .context("rebuild source bundle binary manifest")?;
    }

    let report = ryeos_tools::actions::sign_bundle::sign_bundle_items_with_trust(
        &source_path,
        &dependency_roots,
        &signing_key,
        Some(&trust_store),
    )
    .context("bundle sign failed")?;

    println!("{}", serde_json::to_string_pretty(&report)?);
    if !report.is_total_success() {
        anyhow::bail!(
            "bundle sign failed for {} of {} item(s)",
            report.failed.len(),
            report.total()
        );
    }
    Ok(())
}

#[derive(serde::Deserialize)]
struct BundleSignParams {
    source: PathBuf,
    #[serde(default)]
    registry_root: Option<PathBuf>,
    #[serde(default)]
    registry_roots: Vec<PathBuf>,
}

impl BundleSignParams {
    fn registry_roots(&self) -> Vec<PathBuf> {
        if self.registry_roots.is_empty() {
            self.registry_root.iter().cloned().collect()
        } else {
            self.registry_roots.clone()
        }
    }
}

fn canonical_bundle_source(root: &Path) -> anyhow::Result<PathBuf> {
    let canonical = std::fs::canonicalize(root)
        .with_context(|| format!("resolve bundle source path {}", root.display()))?;
    let ai_dir = canonical.join(ryeos_engine::AI_DIR);
    if !ai_dir.is_dir() {
        anyhow::bail!(
            "bundle source {} has no {} directory",
            canonical.display(),
            ryeos_engine::AI_DIR
        );
    }
    Ok(canonical)
}

fn load_user_signing_key(user_root: &Path) -> anyhow::Result<lillux::crypto::SigningKey> {
    let key_path = user_root
        .join(ryeos_engine::AI_DIR)
        .join("config")
        .join("keys")
        .join("signing")
        .join("private_key.pem");

    if !key_path.exists() {
        anyhow::bail!(
            "user signing key not found at {} — run `ryeos init` first",
            key_path.display()
        );
    }

    ryeos_tools::actions::build_bundle::load_signing_key(&key_path)
        .with_context(|| format!("load signing key from {}", key_path.display()))
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
        let ir =
            item_ref.ok_or_else(|| anyhow::anyhow!("ITEM_REF required (or pass --stdin-json)"))?;
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
        .or_else(|| {
            std::env::var("RYEOS_SYSTEM_SPACE_DIR")
                .ok()
                .map(std::path::PathBuf::from)
        })
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
                if buf.ends_with('\n') {
                    buf.pop();
                }
                if buf.ends_with('\r') {
                    buf.pop();
                }
                buf
            };

            let report =
                ryeos_tools::actions::vault::run_put(&ryeos_tools::actions::vault::PutOptions {
                    system_space_dir: ssd,
                    entries: vec![(name, value)],
                })?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        VaultCmd::List { system_space_dir } => {
            let ssd = resolve_system_space_dir(system_space_dir)?;
            let report =
                ryeos_tools::actions::vault::run_list(&ryeos_tools::actions::vault::ListOptions {
                    system_space_dir: ssd,
                })?;
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
    use lillux::crypto::VerifyingKey;
    use ryeos_tools::actions::authorize::{run_authorize_client as run, AuthorizeClientParams};

    let params = if stdin_json {
        let val = read_stdin_json()?;
        let ssd = val["system_space_dir"].as_str().map(String::from);
        let pk = val["public_key"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("public_key required"))?
            .to_string();
        let sc = val["scopes"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("scopes required in stdin JSON"))?
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
        pk_bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("public key must be 32 bytes (ed25519)"))?,
    )
    .map_err(|e| anyhow::anyhow!("invalid ed25519 public key: {e}"))?;

    let scopes: Vec<String> = scopes_str
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if scopes.is_empty() {
        anyhow::bail!("--scopes must not be empty");
    }

    // Validate each scope is in canonical form. core-tools is not the
    // bootstrap path, so wildcard '*' is rejected at the writer below.
    for scope in &scopes {
        ryeos_runtime::authorizer::validate_scope_pattern(scope)
            .map_err(|e| anyhow::anyhow!("invalid scope: {e}"))?;
    }

    let result = run(AuthorizeClientParams {
        system_space_dir,
        public_key: verifying_key,
        scopes,
        label,
        allow_wildcard: false, // core-tools is not the bootstrap path
    })?;

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "fingerprint": result.fingerprint,
            "path": result.path.to_string_lossy(),
        }))?
    );

    Ok(())
}

fn run_admission_token(
    system_space_dir: Option<String>,
    scopes: Option<String>,
    label: Option<String>,
    ttl_secs: u64,
    stdin_json: bool,
) -> anyhow::Result<()> {
    use ryeos_tools::actions::authorize::{run_mint_admission_token, MintAdmissionTokenParams};

    let (system_space_dir, scopes, label, ttl_secs) = if stdin_json {
        let val = read_stdin_json()?;
        let ssd = val["system_space_dir"].as_str().map(String::from);
        let scopes = val["scopes"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("scopes required in stdin JSON"))?
            .to_string();
        let label = val["label"].as_str().map(String::from);
        let ttl_secs = val["ttl_secs"].as_u64().unwrap_or(600);
        (ssd, scopes, label, ttl_secs)
    } else {
        let scopes = scopes.ok_or_else(|| anyhow::anyhow!("--scopes required"))?;
        (system_space_dir, scopes, label, ttl_secs)
    };

    let system_space_dir = resolve_system_space_dir(system_space_dir)?;
    let scopes: Vec<String> = scopes
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let report = run_mint_admission_token(MintAdmissionTokenParams {
        system_space_dir,
        scopes,
        label,
        ttl_secs,
    })?;

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_remote_descriptor(
    system_space_dir: Option<String>,
    name: Option<String>,
    url: Option<String>,
    capabilities: Option<String>,
    admission_mode: Option<String>,
    provider_name: Option<String>,
    output: Option<PathBuf>,
    stdin_json: bool,
) -> anyhow::Result<()> {
    use ryeos_tools::actions::remote_descriptor::{
        run_export_remote_descriptor, ExportRemoteDescriptorParams,
    };

    let params = if stdin_json {
        serde_json::from_value(read_stdin_json()?)?
    } else {
        let name = name.ok_or_else(|| anyhow::anyhow!("--name required"))?;
        let url = url.ok_or_else(|| anyhow::anyhow!("--url required"))?;
        let capabilities = capabilities
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        ExportRemoteDescriptorParams {
            system_space_dir,
            name,
            url,
            capabilities,
            admission_mode,
            provider_name,
            output,
        }
    };

    let report = run_export_remote_descriptor(params)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn read_stdin_json() -> anyhow::Result<serde_json::Value> {
    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf)?;
    serde_json::from_str(&buf).map_err(|e| anyhow::anyhow!("parse stdin JSON: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::SigningKey;
    use rand::rngs::OsRng;

    struct InstalledFixture {
        _tmp: tempfile::TempDir,
        system: PathBuf,
        user: PathBuf,
        key: SigningKey,
    }

    impl InstalledFixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let system = tmp.path().join("system");
            let user = tmp.path().join("user");
            let trust_dir = user
                .join(ryeos_engine::AI_DIR)
                .join("config")
                .join("keys")
                .join("trusted");
            std::fs::create_dir_all(&trust_dir).unwrap();
            let key = SigningKey::generate(&mut OsRng);
            ryeos_engine::trust::pin_key(&key.verifying_key(), "test", &trust_dir, None).unwrap();
            Self {
                _tmp: tmp,
                system,
                user,
                key,
            }
        }

        fn write_signed(&self, path: &Path, body: &str) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let signed = lillux::signature::sign_content(body, &self.key, "#", None);
            std::fs::write(path, signed).unwrap();
        }

        fn write_broken_installed_registration(&self, name: &str) -> PathBuf {
            let bundle = self
                .system
                .join(ryeos_engine::AI_DIR)
                .join("bundles")
                .join(name);
            std::fs::create_dir_all(bundle.join(ryeos_engine::AI_DIR)).unwrap();
            let registration = self
                .system
                .join(ryeos_engine::AI_DIR)
                .join("node")
                .join("bundles")
                .join(format!("{name}.yaml"));
            self.write_signed(
                &registration,
                &format!(
                    "kind: node\nsection: bundles\nid: {name}\npath: {}\n",
                    bundle.display()
                ),
            );
            bundle
        }
    }

    #[test]
    fn bundle_verify_explicit_registry_root_does_not_load_installed_bundles() {
        let fixture = InstalledFixture::new();
        let source = fixture.system.join("source");
        let registry = fixture.system.join("registry");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&registry).unwrap();
        fixture.write_broken_installed_registration("broken");

        let roots = bundle_verify_dependency_roots(
            &source.canonicalize().unwrap(),
            vec![registry.clone()],
            &fixture.system,
            Some(&fixture.user),
        )
        .unwrap();

        assert_eq!(roots, vec![registry.canonicalize().unwrap()]);
    }

    #[test]
    fn bundle_verify_without_registry_root_fails_on_broken_installed_bundle() {
        let fixture = InstalledFixture::new();
        let source = fixture.system.join("source");
        std::fs::create_dir_all(&source).unwrap();
        fixture.write_broken_installed_registration("broken");

        let err = bundle_verify_dependency_roots(
            &source.canonicalize().unwrap(),
            Vec::new(),
            &fixture.system,
            Some(&fixture.user),
        )
        .unwrap_err();

        let msg = format!("{err:#}");
        assert!(
            msg.contains("bundle verify: load installed bundle registrations")
                && msg.contains("manifest"),
            "unexpected error: {msg}"
        );
    }
}
