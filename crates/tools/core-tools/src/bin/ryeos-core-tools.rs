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

        /// Where to look for the item: `project`.
        #[arg(long, default_value = "project")]
        source: String,
    },

    /// Build (re-publish) a bundle from source using the user signing key.
    ///
    /// Runs the full publish pipeline: clean derived artifacts, bootstrap-sign
    /// kind schemas and parsers, rebuild the CAS manifest when `.ai/bin`
    /// exists, sign all items, and generate the bundle manifest. The signing
    /// key is auto-resolved from the user root — no `--key` flag needed.
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

    /// Append/read/scan project-local bundle events.
    BundleEvents {
        #[command(subcommand)]
        cmd: BundleEventsCmd,
    },

    /// Return the node's public identity document.
    Identity {
        /// App root directory.
        #[arg(long)]
        app_root: Option<String>,
    },

    /// Authorize an HTTP client to call the daemon's authenticated endpoints.
    AuthorizeClient {
        /// App root directory (contains `.ai/node/identity/`).
        #[arg(long)]
        app_root: Option<String>,

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
        /// App root directory for the target node.
        #[arg(long)]
        app_root: Option<String>,

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
        /// App root directory for the node being described.
        #[arg(long)]
        app_root: Option<String>,

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

        /// App root directory.
        #[arg(long)]
        app_root: Option<String>,
    },

    /// List key names in the sealed vault store (values are not printed).
    List {
        /// App root directory.
        #[arg(long)]
        app_root: Option<String>,
    },

    /// Remove keys from the sealed vault store.
    Rm {
        /// Key names to remove.
        #[arg(required = true)]
        keys: Vec<String>,

        /// App root directory.
        #[arg(long)]
        app_root: Option<String>,
    },

    /// Re-encrypt every entry in the sealed vault store under a
    /// freshly-generated vault keypair.
    Rewrap {
        /// App root directory.
        #[arg(long)]
        app_root: Option<String>,
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

#[derive(Subcommand, Debug)]
enum BundleEventsCmd {
    /// Low-level/dev-only append to a bundle/event-kind chain.
    ///
    /// This CLI is not an authorization boundary: `effective_bundle_id` is
    /// caller supplied here. Production daemon-backed execution must derive
    /// bundle identity from verified tool context and enforce capabilities.
    Append {
        /// Runtime project root containing `.ai/state`.
        #[arg(long)]
        project_path: Option<PathBuf>,

        /// Effective verified bundle identity. In daemon-backed execution this
        /// should be derived by the runtime; this CLI requires it explicitly.
        #[arg(long)]
        effective_bundle_id: Option<String>,

        /// Optional requested bundle id. Must match `effective_bundle_id`.
        #[arg(long)]
        bundle_id: Option<String>,

        #[arg(long)]
        event_kind: Option<String>,

        #[arg(long)]
        chain_id: Option<String>,

        #[arg(long)]
        event_type: Option<String>,

        #[arg(long, default_value_t = 1)]
        schema_version: u32,

        /// JSON payload as an inline string. Defaults to `{}`.
        #[arg(long)]
        payload_json: Option<String>,

        #[arg(long)]
        expected_chain_head_hash: Option<String>,

        #[arg(long)]
        idempotency_key: Option<String>,

        #[arg(long)]
        correlation_id: Option<String>,

        #[arg(long)]
        causation_id: Option<String>,
    },

    /// Read one bundle event chain.
    ReadChain {
        #[arg(long)]
        project_path: Option<PathBuf>,
        #[arg(long)]
        bundle_id: Option<String>,
        #[arg(long)]
        event_kind: Option<String>,
        #[arg(long)]
        chain_id: Option<String>,
    },

    /// Scan bundle events for a bundle/event kind.
    Scan {
        #[arg(long)]
        project_path: Option<PathBuf>,
        #[arg(long)]
        bundle_id: Option<String>,
        #[arg(long)]
        event_kind: Option<String>,
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
        Cmd::BundleEvents { cmd } => run_bundle_events(cmd, cli.stdin_json),
        Cmd::Identity { app_root } => {
            let params = if cli.stdin_json {
                read_stdin_json()?
            } else {
                let mut obj = serde_json::json!({});
                if let Some(s) = app_root {
                    obj["app_root"] = serde_json::json!(s);
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
            app_root,
            public_key,
            scopes,
            label,
        } => {
            let scopes = scopes.ok_or_else(|| anyhow::anyhow!(
                "--scopes required, comma-separated, in canonical form. \
                 Example: --scopes ryeos.execute.service.remote.admin,ryeos.execute.service.bundle.install"
            ))?;
            run_authorize_client(app_root, public_key, scopes, label, cli.stdin_json)
        }
        Cmd::AdmissionToken {
            app_root,
            scopes,
            label,
            ttl_secs,
        } => run_admission_token(app_root, scopes, label, ttl_secs, cli.stdin_json),
        Cmd::RemoteDescriptor {
            app_root,
            name,
            url,
            capabilities,
            admission_mode,
            provider_name,
            output,
        } => run_remote_descriptor(
            app_root,
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

    let (bundle_source, registry_roots, owner, no_trust_doc) = if stdin_json {
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

    let key_path = roots::runtime_root()
        .map_err(|e| anyhow::anyhow!("cannot resolve app root: {e}"))?
        .operator_signing_key_path();

    if !key_path.exists() {
        anyhow::bail!(
            "operator signing key not found at {} — run `ryeos init` first",
            key_path.display()
        );
    }

    let signing_key = ryeos_tools::actions::build_bundle::load_signing_key(&key_path)
        .with_context(|| format!("load signing key from {}", key_path.display()))?;

    let source_path = canonical_bundle_source(&bundle_source)?;
    let app_root = std::env::var("RYEOS_APP_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::data_dir()
                .map(|d| d.join("ryeos"))
                .expect("could not determine XDG data directory")
        });
    let registry_roots = bundle_publish_dependency_roots(&source_path, registry_roots, &app_root)?;
    let operator_config_root = ryeos_engine::roots::RuntimeRoot::new(app_root.clone()).config();
    let base_trust_store = ryeos_engine::trust::TrustStore::load(None, &operator_config_root)
        .context("load trust store for registry roots")?;

    let report = run_publish(&PublishOptions {
        bundle_source: source_path,
        registry_roots,
        signing_key,
        base_trust_store: Some(base_trust_store),
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

fn bundle_publish_dependency_roots(
    source_path: &Path,
    registry_roots: Vec<PathBuf>,
    app_root: &Path,
) -> anyhow::Result<Vec<PathBuf>> {
    if !registry_roots.is_empty() {
        reject_unpublished_source_registry_roots(source_path, &registry_roots)?;
        return bundle_verify_dependency_roots(source_path, registry_roots, app_root);
    }

    if source_bundle_name(source_path)?.as_deref() == Some("core") {
        return Ok(vec![source_path.to_path_buf()]);
    }

    bundle_verify_dependency_roots(source_path, registry_roots, app_root)
}

fn reject_unpublished_source_registry_roots(
    source_path: &Path,
    registry_roots: &[PathBuf],
) -> anyhow::Result<()> {
    for root in registry_roots {
        let canonical_root = std::fs::canonicalize(root)
            .with_context(|| format!("resolve registry root {}", root.display()))?;
        if canonical_root == source_path {
            continue;
        }
        let ai_dir = root.join(ryeos_engine::AI_DIR);
        if ai_dir.join("manifest.source.yaml").exists()
            && !ai_dir
                .join("refs")
                .join("bundles")
                .join("manifest")
                .exists()
        {
            anyhow::bail!(
                "--registry-root {} looks like an unpublished source checkout, not a published dependency root. Omit --registry-root to use installed bundle dependencies, or publish/install that dependency first.",
                root.display()
            );
        }
    }
    Ok(())
}

fn source_bundle_name(source_path: &Path) -> anyhow::Result<Option<String>> {
    let source_manifest = source_path
        .join(ryeos_engine::AI_DIR)
        .join("manifest.source.yaml");
    if !source_manifest.exists() {
        return Ok(None);
    }

    let raw = std::fs::read_to_string(&source_manifest)
        .with_context(|| format!("read manifest source {}", source_manifest.display()))?;
    let source: ryeos_bundle::manifest::BundleManifestSource = serde_yaml::from_str(&raw)
        .with_context(|| format!("parse manifest source {}", source_manifest.display()))?;
    Ok(Some(source.name))
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

    let app_root = std::env::var("RYEOS_APP_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::data_dir()
                .map(|d| d.join("ryeos"))
                .expect("could not determine XDG data directory")
        });
    let dependency_roots = bundle_verify_dependency_roots(&source_path, registry_roots, &app_root)?;

    let preflight_report = ryeos_bundle::preflight::preflight_verify_bundle_report_in_context(
        &source_path,
        &dependency_roots,
        &ryeos_engine::roots::RuntimeRoot::new(app_root.clone()).config(),
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
    app_root: &Path,
) -> anyhow::Result<Vec<PathBuf>> {
    let mut dependency_roots: Vec<PathBuf> = Vec::new();
    let source_name = source_bundle_name(source_path)?;
    if !registry_roots.is_empty() {
        for root in registry_roots {
            let root = std::fs::canonicalize(&root)
                .with_context(|| format!("resolve registry root {}", root.display()))?;
            if root != source_path && !dependency_roots.iter().any(|seen| seen == &root) {
                dependency_roots.push(root);
            }
        }
    } else {
        let installed_roots = ryeos_bundle::installed::load_installed_bundle_records(app_root)
            .context("bundle verify: load installed bundle registrations")?
            .into_iter()
            .filter(|r| r.bundle_root != source_path && Some(&r.name) != source_name.as_ref())
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
    let app_root = std::env::var("RYEOS_APP_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::data_dir()
                .map(|d| d.join("ryeos"))
                .expect("could not determine XDG data directory")
        });
    let dependency_roots = bundle_verify_dependency_roots(&source_path, registry_roots, &app_root)?;
    let signing_key = load_operator_signing_key()?;
    let operator_config_root = ryeos_engine::roots::RuntimeRoot::new(app_root.clone()).config();
    let trust_store = ryeos_engine::trust::TrustStore::load(None, &operator_config_root)
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

fn load_operator_signing_key() -> anyhow::Result<lillux::crypto::SigningKey> {
    let key_path = ryeos_engine::roots::runtime_root()
        .map_err(|e| anyhow::anyhow!("cannot resolve app root: {e}"))?
        .operator_signing_key_path();

    if !key_path.exists() {
        anyhow::bail!(
            "operator signing key not found at {} — run `ryeos init` first",
            key_path.display()
        );
    }

    ryeos_tools::actions::build_bundle::load_signing_key(&key_path)
        .with_context(|| format!("load signing key from {}", key_path.display()))
}

struct CoreToolsStateSigner {
    signing_key: lillux::crypto::SigningKey,
    fingerprint: String,
}

impl CoreToolsStateSigner {
    fn new(signing_key: lillux::crypto::SigningKey) -> Self {
        let fingerprint = lillux::crypto::fingerprint(&signing_key.verifying_key());
        Self {
            signing_key,
            fingerprint,
        }
    }
}

impl ryeos_state::Signer for CoreToolsStateSigner {
    fn sign(&self, data: &[u8]) -> Vec<u8> {
        use lillux::crypto::Signer as _;
        self.signing_key.sign(data).to_bytes().to_vec()
    }

    fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

#[derive(serde::Deserialize)]
struct BundleEventAppendParams {
    project_path: Option<PathBuf>,
    effective_bundle_id: String,
    #[serde(default)]
    bundle_id: Option<String>,
    event_kind: String,
    chain_id: String,
    event_type: String,
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    #[serde(default)]
    payload: Option<serde_json::Value>,
    #[serde(default)]
    expected_chain_head_hash: Option<String>,
    #[serde(default)]
    idempotency_key: Option<String>,
    #[serde(default)]
    correlation_id: Option<String>,
    #[serde(default)]
    causation_id: Option<String>,
}

#[derive(serde::Deserialize)]
struct BundleEventReadChainParams {
    project_path: Option<PathBuf>,
    bundle_id: String,
    event_kind: String,
    chain_id: String,
}

#[derive(serde::Deserialize)]
struct BundleEventScanParams {
    project_path: Option<PathBuf>,
    bundle_id: String,
    event_kind: String,
}

fn default_schema_version() -> u32 {
    1
}

fn run_bundle_events(cmd: BundleEventsCmd, stdin_json: bool) -> anyhow::Result<()> {
    match cmd {
        BundleEventsCmd::Append {
            project_path,
            effective_bundle_id,
            bundle_id,
            event_kind,
            chain_id,
            event_type,
            schema_version,
            payload_json,
            expected_chain_head_hash,
            idempotency_key,
            correlation_id,
            causation_id,
        } => {
            let params = if stdin_json {
                serde_json::from_value::<BundleEventAppendParams>(read_stdin_json()?)?
            } else {
                BundleEventAppendParams {
                    project_path,
                    effective_bundle_id: effective_bundle_id
                        .ok_or_else(|| anyhow::anyhow!("--effective-bundle-id required"))?,
                    bundle_id,
                    event_kind: event_kind
                        .ok_or_else(|| anyhow::anyhow!("--event-kind required"))?,
                    chain_id: chain_id.ok_or_else(|| anyhow::anyhow!("--chain-id required"))?,
                    event_type: event_type
                        .ok_or_else(|| anyhow::anyhow!("--event-type required"))?,
                    schema_version,
                    payload: Some(match payload_json {
                        Some(json) => {
                            serde_json::from_str(&json).context("parse --payload-json")?
                        }
                        None => serde_json::json!({}),
                    }),
                    expected_chain_head_hash,
                    idempotency_key,
                    correlation_id,
                    causation_id,
                }
            };
            let db = open_bundle_event_state(params.project_path.as_deref())?;
            let signer = CoreToolsStateSigner::new(load_operator_signing_key()?);
            let result = db.append_bundle_event(
                ryeos_state::BundleEventAppendRequest {
                    effective_bundle_id: params.effective_bundle_id,
                    bundle_id: params.bundle_id,
                    event_kind: params.event_kind,
                    chain_id: params.chain_id,
                    event_type: params.event_type,
                    schema_version: params.schema_version,
                    payload: params.payload.unwrap_or_else(|| serde_json::json!({})),
                    expected_chain_head_hash: params.expected_chain_head_hash,
                    idempotency_key: params.idempotency_key,
                    correlation_id: params.correlation_id,
                    causation_id: params.causation_id,
                    attribution: ryeos_state::BundleEventAttribution::default(),
                },
                &signer,
            )?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "event_hash": result.event_hash,
                    "chain_head_hash": result.chain_head_hash,
                    "idempotent": result.idempotent,
                    "event": result.event,
                }))?
            );
            Ok(())
        }
        BundleEventsCmd::ReadChain {
            project_path,
            bundle_id,
            event_kind,
            chain_id,
        } => {
            let params = if stdin_json {
                serde_json::from_value::<BundleEventReadChainParams>(read_stdin_json()?)?
            } else {
                BundleEventReadChainParams {
                    project_path,
                    bundle_id: bundle_id.ok_or_else(|| anyhow::anyhow!("--bundle-id required"))?,
                    event_kind: event_kind
                        .ok_or_else(|| anyhow::anyhow!("--event-kind required"))?,
                    chain_id: chain_id.ok_or_else(|| anyhow::anyhow!("--chain-id required"))?,
                }
            };
            let db = open_bundle_event_state(params.project_path.as_deref())?;
            let records = db.read_bundle_event_chain(
                &params.bundle_id,
                &params.event_kind,
                &params.chain_id,
            )?;
            println!(
                "{}",
                serde_json::to_string_pretty(&records_to_json(records))?
            );
            Ok(())
        }
        BundleEventsCmd::Scan {
            project_path,
            bundle_id,
            event_kind,
        } => {
            let params = if stdin_json {
                serde_json::from_value::<BundleEventScanParams>(read_stdin_json()?)?
            } else {
                BundleEventScanParams {
                    project_path,
                    bundle_id: bundle_id.ok_or_else(|| anyhow::anyhow!("--bundle-id required"))?,
                    event_kind: event_kind
                        .ok_or_else(|| anyhow::anyhow!("--event-kind required"))?,
                }
            };
            let db = open_bundle_event_state(params.project_path.as_deref())?;
            let records = db.scan_bundle_events(&params.bundle_id, &params.event_kind)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&records_to_json(records))?
            );
            Ok(())
        }
    }
}

fn open_bundle_event_state(project_path: Option<&Path>) -> anyhow::Result<ryeos_state::StateDb> {
    let project_path = match project_path {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().context("resolve current directory")?,
    };
    ryeos_state::StateDb::open(&project_path.join(ryeos_engine::AI_DIR).join("state"))
}

fn records_to_json(records: Vec<ryeos_state::BundleEventRecord>) -> serde_json::Value {
    serde_json::Value::Array(
        records
            .into_iter()
            .map(|record| {
                serde_json::json!({
                    "event_hash": record.event_hash,
                    "event": record.event,
                })
            })
            .collect(),
    )
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
        anyhow::bail!(
            "{}/{} items failed validation or signing",
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

fn resolve_app_root(opt: Option<String>) -> anyhow::Result<std::path::PathBuf> {
    opt.map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("RYEOS_APP_ROOT")
                .ok()
                .map(std::path::PathBuf::from)
        })
        .ok_or_else(|| anyhow::anyhow!("--app-root or RYEOS_APP_ROOT required"))
}

fn run_vault(cmd: VaultCmd) -> anyhow::Result<()> {
    match cmd {
        VaultCmd::Put {
            name,
            value_stdin,
            value_string,
            app_root,
        } => {
            let ssd = resolve_app_root(app_root)?;
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
                    app_root: ssd,
                    entries: vec![(name, value)],
                })?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        VaultCmd::List { app_root } => {
            let ssd = resolve_app_root(app_root)?;
            let report =
                ryeos_tools::actions::vault::run_list(&ryeos_tools::actions::vault::ListOptions {
                    app_root: ssd,
                })?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        VaultCmd::Rm { keys, app_root } => {
            let ssd = resolve_app_root(app_root)?;
            let report = ryeos_tools::actions::vault::run_remove(
                &ryeos_tools::actions::vault::RemoveOptions {
                    app_root: ssd,
                    keys,
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        VaultCmd::Rewrap { app_root } => {
            let ssd = resolve_app_root(app_root)?;
            let report = ryeos_tools::actions::vault::run_rewrap(
                &ryeos_tools::actions::vault::RewrapOptions { app_root: ssd },
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
    }
}

fn run_authorize_client(
    app_root: Option<String>,
    public_key: Option<String>,
    scopes: String,
    label: String,
    stdin_json: bool,
) -> anyhow::Result<()> {
    use lillux::crypto::VerifyingKey;
    use ryeos_tools::actions::authorize::{run_authorize_client as run, AuthorizeClientParams};

    let params = if stdin_json {
        let val = read_stdin_json()?;
        let ssd = val["app_root"].as_str().map(String::from);
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
        (app_root, pk, scopes, label)
    };

    let (ssd, pk_b64, scopes_str, label) = params;

    let app_root = resolve_app_root(ssd)?;

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
        app_root,
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
    app_root: Option<String>,
    scopes: Option<String>,
    label: Option<String>,
    ttl_secs: u64,
    stdin_json: bool,
) -> anyhow::Result<()> {
    use ryeos_tools::actions::authorize::{run_mint_admission_token, MintAdmissionTokenParams};

    let (app_root, scopes, label, ttl_secs) = if stdin_json {
        let val = read_stdin_json()?;
        let ssd = val["app_root"].as_str().map(String::from);
        let scopes = val["scopes"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("scopes required in stdin JSON"))?
            .to_string();
        let label = val["label"].as_str().map(String::from);
        let ttl_secs = val["ttl_secs"].as_u64().unwrap_or(600);
        (ssd, scopes, label, ttl_secs)
    } else {
        let scopes = scopes.ok_or_else(|| anyhow::anyhow!("--scopes required"))?;
        (app_root, scopes, label, ttl_secs)
    };

    let app_root = resolve_app_root(app_root)?;
    let scopes: Vec<String> = scopes
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let report = run_mint_admission_token(MintAdmissionTokenParams {
        app_root,
        scopes,
        label,
        ttl_secs,
    })?;

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_remote_descriptor(
    app_root: Option<String>,
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
            app_root,
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
    use lillux::crypto::{EncodePrivateKey, SigningKey};
    use rand::rngs::OsRng;
    use std::sync::Mutex;

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    struct InstalledFixture {
        _tmp: tempfile::TempDir,
        system: PathBuf,
        _user: PathBuf,
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
                _user: user,
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
        )
        .unwrap_err();

        let msg = format!("{err:#}");
        assert!(
            msg.contains("bundle verify: load installed bundle registrations")
                && msg.contains("manifest"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn bundle_publish_rejects_unpublished_source_checkout_as_registry_root() {
        let fixture = InstalledFixture::new();
        let source = fixture.system.join("source");
        let registry = fixture.system.join("registry-source");
        std::fs::create_dir_all(source.join(ryeos_engine::AI_DIR)).unwrap();
        std::fs::create_dir_all(registry.join(ryeos_engine::AI_DIR)).unwrap();
        std::fs::write(
            registry
                .join(ryeos_engine::AI_DIR)
                .join("manifest.source.yaml"),
            "name: core\nversion: 0.0.0\nitems: []\n",
        )
        .unwrap();

        let err = bundle_publish_dependency_roots(
            &source.canonicalize().unwrap(),
            vec![registry.clone()],
            &fixture.system,
        )
        .unwrap_err();

        let msg = format!("{err:#}");
        assert!(
            msg.contains("unpublished source checkout"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains("Omit --registry-root"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn bundle_publish_without_registry_root_uses_installed_dependency_loader() {
        let fixture = InstalledFixture::new();
        let source = fixture.system.join("source");
        std::fs::create_dir_all(source.join(ryeos_engine::AI_DIR)).unwrap();

        let roots = bundle_publish_dependency_roots(
            &source.canonicalize().unwrap(),
            Vec::new(),
            &fixture.system,
        )
        .unwrap();

        assert!(roots.is_empty());
    }

    #[test]
    fn bundle_events_append_and_read_chain_via_handler() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        let previous_app_root = std::env::var_os("RYEOS_APP_ROOT");
        let tmp = tempfile::tempdir().unwrap();
        let user = tmp.path().join("user");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(project.join(ryeos_engine::AI_DIR)).unwrap();
        let key_dir = user
            .join(ryeos_engine::AI_DIR)
            .join("config")
            .join("keys")
            .join("signing");
        std::fs::create_dir_all(&key_dir).unwrap();
        let key = SigningKey::generate(&mut OsRng);
        std::fs::write(
            key_dir.join("private_key.pem"),
            key.to_pkcs8_pem(Default::default()).unwrap().as_bytes(),
        )
        .unwrap();
        std::env::set_var("RYEOS_APP_ROOT", &user);

        let append = BundleEventsCmd::Append {
            project_path: Some(project.clone()),
            effective_bundle_id: Some("ryeos-email".to_string()),
            bundle_id: Some("ryeos-email".to_string()),
            event_kind: Some("email_event".to_string()),
            chain_id: Some("email_1".to_string()),
            event_type: Some("email_planned".to_string()),
            schema_version: 1,
            payload_json: Some("{\"email_id\":\"email_1\"}".to_string()),
            expected_chain_head_hash: None,
            idempotency_key: Some("plan:email_1".to_string()),
            correlation_id: None,
            causation_id: None,
        };
        run_bundle_events(append, false).unwrap();

        let db =
            ryeos_state::StateDb::open(&project.join(ryeos_engine::AI_DIR).join("state")).unwrap();
        let chain = db
            .read_bundle_event_chain("ryeos-email", "email_event", "email_1")
            .unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].event.event_type, "email_planned");

        if let Some(value) = previous_app_root {
            std::env::set_var("RYEOS_APP_ROOT", value);
        } else {
            std::env::remove_var("RYEOS_APP_ROOT");
        }
    }
}
