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
        /// Canonical refs or `.ai/...` paths of the items to sign.
        #[arg(value_name = "ITEM_REF", num_args = 0..)]
        item_refs: Vec<String>,

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

        /// Owner label for the trust doc. Defaults to an existing pinned label for
        /// the signing key, or "local-dev" when the key is not pinned.
        #[arg(long)]
        owner: Option<String>,

        /// Effective bundle id the generated manifest must carry — the first
        /// bare-id segment of the bundle's item refs. Defaults to the source
        /// directory's basename. Runtime authority requires this to equal the
        /// bundle id used in item refs.
        #[arg(long)]
        name: Option<String>,

        /// Report and skip items that fail to sign instead of aborting the
        /// publish. The manifest is still generated; the result is a PARTIAL
        /// publish (`partial: true`, `skipped_unsignable: [...]`) and the trust
        /// doc is suppressed. Default: fail fast on the first unsignable item.
        #[arg(long)]
        skip_unsignable: bool,

        /// Publish even when an item's effective bundle id diverges from a
        /// runtime-authority manifest's name. Default: fail (the daemon would
        /// reject runtime-cap minting, making the manifest unusable).
        #[arg(long)]
        allow_namespace_mismatch: bool,

        /// Suppress emitting `<bundle_source>/PUBLISHER_TRUST.toml`.
        #[arg(long)]
        no_trust_doc: bool,
    },

    /// Generate + sign `.ai/manifest.yaml` from `.ai/manifest.source.yaml`
    /// without running the full publish pipeline.
    ///
    /// Touches only the manifest — no CAS clean, no item signing, no trust
    /// doc. The signing key is auto-resolved from the user root. Use when
    /// iterating on manifest declarations (under `runtime_authority:`).
    ManifestSign {
        /// Bundle source root (directory containing `.ai/`).
        bundle_source: Option<PathBuf>,

        /// Effective bundle id the manifest must carry — the first bare-id
        /// segment of the bundle's item refs. Defaults to the source
        /// directory's basename.
        #[arg(long)]
        name: Option<String>,
    },

    /// Report the runtime-authority delta between two bundle manifests.
    ///
    /// Parses both manifests and prints ONLY the granted-authority change (event
    /// kinds, vault namespaces/verbs, item-authoring patterns, provides/requires/
    /// uses kinds), ordered by risk — a new wildcard authoring pattern first,
    /// removed grants last — so a re-sign campaign reviews as authority deltas
    /// rather than YAML diffs. Each path may be a generated `manifest.yaml` or a
    /// `manifest.source.yaml`.
    ManifestAudit {
        /// Old (baseline) manifest file.
        old: PathBuf,

        /// New (proposed) manifest file.
        new: PathBuf,
    },

    /// Tail the local node's trace events and startup stderr from the app root.
    ///
    /// Reads files directly, so it works offline — when the daemon failed to
    /// start or crashed and no live handler can answer.
    Logs {
        /// App root (parent of `.ai/`). Defaults to RYEOS_APP_ROOT or the XDG
        /// data dir.
        #[arg(long)]
        app_root: Option<PathBuf>,

        /// Number of trailing lines to show from each log.
        #[arg(long, default_value_t = 200)]
        lines: usize,
    },

    /// Offline preflight checklist for a project/bundle source.
    ///
    /// Runs deterministic checks with no daemon: manifest present +
    /// name-consistent, bundle verify (headers/parsers/signatures), and a
    /// python-tool import dry-run; advisory checks report `unknown`.
    Doctor {
        /// Project/bundle source root (directory containing `.ai/`).
        source: Option<PathBuf>,

        /// Registry/dependency root supplying kind schemas + runtimes.
        /// Defaults to installed bundle roots. May be repeated.
        #[arg(long = "registry-root")]
        registry_roots: Vec<PathBuf>,

        /// Exit nonzero when any deterministic check fails (for CI/preflight
        /// gating). Default: always exit 0 and report; the JSON `ok` field
        /// carries the verdict.
        #[arg(long)]
        strict: bool,
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

        /// Union the given scopes with any already on the existing
        /// authorized-key file instead of replacing them. Without this,
        /// existing scopes not re-listed are dropped (and a warning is
        /// printed).
        #[arg(long)]
        merge_scopes: bool,
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

    /// Author (create or upsert) a signed project item through the daemon
    /// `runtime.author_item` callback.
    ///
    /// Only meaningful when dispatched inside a running thread: the daemon
    /// injects the callback + thread-auth tokens and the thread id via env, then
    /// authorizes `ryeos.author.<kind>.<bare_id>`, derives the path from the kind
    /// schema, injects provenance, signs with its own identity, and writes. The
    /// item body is proposed unsigned; the daemon owns the signature.
    ///
    /// Params usually arrive as `--stdin-json` (`{item_ref, content, mode,
    /// format_ext}`); the flags are for manual invocation.
    AuthorItem {
        /// Canonical target ref `kind:bare_id` (no ref suffix).
        #[arg(long)]
        item_ref: Option<String>,

        /// Unsigned item body. Read from stdin when omitted (and not
        /// `--stdin-json`).
        #[arg(long)]
        content: Option<String>,

        /// `create` (default; fails if the item exists) or `upsert` (replaces).
        #[arg(long, default_value = "create")]
        mode: String,

        /// File extension including the leading dot (e.g. `.md`); required when
        /// creating a new item.
        #[arg(long)]
        format_ext: Option<String>,

        /// Compare-and-swap guard for `--mode upsert`: the incumbent's authored
        /// `content_digest` must equal this or the upsert fails with a conflict.
        #[arg(long)]
        expected_digest: Option<String>,
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

        /// Worktree scan budget in milliseconds; 0 disables the cap.
        #[arg(long, default_value_t = 5_000)]
        time_budget_ms: u64,
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
            item_refs,
            project,
            source,
        } => run_sign(item_refs, project, source, cli.stdin_json),
        Cmd::Build {
            bundle_source,
            registry_roots,
            owner,
            name,
            skip_unsignable,
            allow_namespace_mismatch,
            no_trust_doc,
        } => run_build(
            bundle_source,
            registry_roots,
            owner,
            name,
            skip_unsignable,
            allow_namespace_mismatch,
            no_trust_doc,
            cli.stdin_json,
        ),
        Cmd::ManifestSign {
            bundle_source,
            name,
        } => run_manifest_sign(bundle_source, name, cli.stdin_json),
        Cmd::ManifestAudit { old, new } => run_manifest_audit(old, new),
        Cmd::Logs { app_root, lines } => run_logs(app_root, lines, cli.stdin_json),
        Cmd::Doctor {
            source,
            registry_roots,
            strict,
        } => run_doctor(source, registry_roots, strict, cli.stdin_json),
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
            let params: ryeos_core_tools::actions::inspect::fetch::FetchParams =
                serde_json::from_value(params)?;
            let engine = ryeos_core_tools::actions::inspect::boot(
                params.project_path.as_deref().map(std::path::Path::new),
            )?;
            let report = ryeos_core_tools::actions::inspect::fetch::run_fetch(params, &engine)?;
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
            let params: ryeos_core_tools::actions::inspect::verify::VerifyParams =
                serde_json::from_value(params)?;
            let engine = ryeos_core_tools::actions::inspect::boot(
                params.project_path.as_deref().map(std::path::Path::new),
            )?;
            let report = ryeos_core_tools::actions::inspect::verify::run_verify(params, &engine)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        Cmd::Snapshot { cmd } => run_snapshot(cmd, cli.stdin_json),
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
            let params: ryeos_core_tools::actions::inspect::identity::IdentityParams =
                serde_json::from_value(params)?;
            let report = ryeos_core_tools::actions::inspect::identity::run_identity(params)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        Cmd::AuthorizeClient {
            app_root,
            public_key,
            scopes,
            label,
            merge_scopes,
        } => {
            let scopes = scopes.ok_or_else(|| anyhow::anyhow!(
                "--scopes required, comma-separated, in canonical form. \
                 Example: --scopes ryeos.execute.service.remote/admin,ryeos.execute.service.bundle/install"
            ))?;
            run_authorize_client(
                app_root,
                public_key,
                scopes,
                label,
                merge_scopes,
                cli.stdin_json,
            )
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
        Cmd::AuthorItem {
            item_ref,
            content,
            mode,
            format_ext,
            expected_digest,
        } => run_author_item(
            item_ref,
            content,
            mode,
            format_ext,
            expected_digest,
            cli.stdin_json,
        ),
        Cmd::Vault { cmd } => run_vault(cmd),
    }
}

/// Params for the `author-item` subcommand when invoked with `--stdin-json`
/// (the dispatched-tool path; `thread_id` comes from the env, not the params).
///
/// `deny_unknown_fields` is intentionally NOT set: the runtime compiler injects
/// extra context (e.g. `project_path`) into the params before expanding
/// `{params_json}` onto stdin. Unknown keys are ignored, and the daemon request
/// is rebuilt from only the known fields below — so nothing extra is forwarded.
#[derive(serde::Deserialize)]
struct AuthorItemParams {
    item_ref: String,
    content: String,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    format_ext: Option<String>,
    #[serde(default)]
    expected_digest: Option<String>,
}

/// Propose a project item to the daemon `runtime.author_item` callback. This is
/// a capability-bounded runtime callback client — it never touches project state
/// directly. The two proofs (callback token + thread-auth token) and the socket
/// path come from the env the daemon set on this dispatched tool; the daemon
/// authorizes `ryeos.author.<kind>.<bare_id>`, signs, and writes.
fn run_author_item(
    item_ref: Option<String>,
    content: Option<String>,
    mode: String,
    format_ext: Option<String>,
    expected_digest: Option<String>,
    stdin_json: bool,
) -> anyhow::Result<()> {
    let request = if stdin_json {
        let params: AuthorItemParams = serde_json::from_value(read_stdin_json()?)
            .context("invalid author-item params JSON on stdin")?;
        let mut request = serde_json::json!({
            "item_ref": params.item_ref,
            "content": params.content,
        });
        if let Some(mode) = params.mode {
            request["mode"] = serde_json::json!(mode);
        }
        if let Some(ext) = params.format_ext {
            request["format_ext"] = serde_json::json!(ext);
        }
        if let Some(digest) = params.expected_digest {
            request["expected_digest"] = serde_json::json!(digest);
        }
        request
    } else {
        let item_ref = item_ref.context("--item-ref is required (or pass --stdin-json)")?;
        let content = match content {
            Some(content) => content,
            None => {
                let mut buf = String::new();
                io::stdin()
                    .read_to_string(&mut buf)
                    .context("read item body from stdin")?;
                buf
            }
        };
        let mut request = serde_json::json!({
            "item_ref": item_ref,
            "content": content,
            "mode": mode,
        });
        if let Some(ext) = format_ext {
            request["format_ext"] = serde_json::json!(ext);
        }
        if let Some(digest) = expected_digest {
            request["expected_digest"] = serde_json::json!(digest);
        }
        request
    };

    // The daemon stamps the running thread id into the tool's env; the callback
    // client keys authoring to this exact thread. The request is built above from
    // only the known fields, so no caller-supplied thread_id or context leaks in.
    let thread_id = std::env::var("RYEOSD_THREAD_ID").context(
        "RYEOSD_THREAD_ID is not set — author-item runs only inside a thread the daemon dispatched",
    )?;

    let client = ryeos_runtime::callback_uds::UdsRuntimeClient::from_env()
        .map_err(|e| anyhow::anyhow!("cannot build runtime callback client: {e}"))?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build callback runtime")?;
    let response = {
        use ryeos_runtime::callback::RuntimeCallbackAPI;
        runtime
            .block_on(client.author_item(&thread_id, request))
            .map_err(|e| anyhow::anyhow!("runtime.author_item failed: {e}"))?
    };

    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

fn run_snapshot(cmd: SnapshotCmd, stdin_json: bool) -> anyhow::Result<()> {
    use ryeos_core_tools::actions::snapshot::{
        run_create, run_log, run_show, run_status, SnapshotCreateParams, SnapshotLogParams,
        SnapshotShowParams, SnapshotStatusParams,
    };

    match cmd {
        SnapshotCmd::Status {
            project_path,
            include_unchanged,
            time_budget_ms,
        } => {
            let params = if stdin_json {
                serde_json::from_value(read_stdin_json()?)?
            } else {
                SnapshotStatusParams {
                    project_path: project_path
                        .or_else(|| std::env::current_dir().ok())
                        .ok_or_else(|| anyhow::anyhow!("--project-path required"))?,
                    include_unchanged,
                    time_budget_ms,
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

#[allow(clippy::too_many_arguments)]
fn run_build(
    bundle_source: Option<PathBuf>,
    registry_roots: Vec<PathBuf>,
    owner: Option<String>,
    name: Option<String>,
    skip_unsignable: bool,
    allow_namespace_mismatch: bool,
    no_trust_doc: bool,
    stdin_json: bool,
) -> anyhow::Result<()> {
    use ryeos_core_tools::actions::publish::{run_publish, PublishOptions};
    use ryeos_engine::roots;

    let (
        bundle_source,
        registry_roots,
        owner,
        name,
        skip_unsignable,
        allow_namespace_mismatch,
        no_trust_doc,
    ) = if stdin_json {
        if bundle_source.is_some() {
            anyhow::bail!("--stdin-json is mutually exclusive with positional BUNDLE_SOURCE");
        }
        let params: BundlePublishParams = serde_json::from_value(read_stdin_json()?)?;
        let registry_roots = params.registry_roots();
        (
            params.source,
            registry_roots,
            params.owner,
            params.name,
            params.skip_unsignable,
            params.allow_namespace_mismatch,
            params.no_trust_doc,
        )
    } else {
        let source = bundle_source
            .ok_or_else(|| anyhow::anyhow!("BUNDLE_SOURCE required (or pass --stdin-json)"))?;
        (
            source,
            registry_roots,
            owner,
            name,
            skip_unsignable,
            allow_namespace_mismatch,
            no_trust_doc,
        )
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

    let signing_key = ryeos_core_tools::actions::build_bundle::load_signing_key(&key_path)
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
    let owner = resolve_publish_owner(owner, &signing_key, &base_trust_store);

    let report = run_publish(&PublishOptions {
        bundle_source: source_path,
        registry_roots,
        signing_key,
        base_trust_store: Some(base_trust_store),
        owner,
        name,
        skip_unsignable,
        allow_namespace_mismatch,
        emit_trust_doc: !no_trust_doc,
    })?;

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn run_manifest_sign(
    bundle_source: Option<PathBuf>,
    name: Option<String>,
    stdin_json: bool,
) -> anyhow::Result<()> {
    use ryeos_engine::roots;

    let (bundle_source, name) = if stdin_json {
        if bundle_source.is_some() {
            anyhow::bail!("--stdin-json is mutually exclusive with positional BUNDLE_SOURCE");
        }
        let params: BundleManifestSignParams = serde_json::from_value(read_stdin_json()?)?;
        (params.source, params.name)
    } else {
        let source = bundle_source
            .ok_or_else(|| anyhow::anyhow!("BUNDLE_SOURCE required (or pass --stdin-json)"))?;
        (source, name)
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
    let signing_key = ryeos_core_tools::actions::build_bundle::load_signing_key(&key_path)
        .with_context(|| format!("load signing key from {}", key_path.display()))?;

    let source_path = canonical_bundle_source(&bundle_source)?;
    let report = ryeos_core_tools::actions::manifest_sign::manifest_sign(
        &source_path,
        name.as_deref(),
        &signing_key,
    )?;

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[derive(serde::Deserialize)]
struct BundleManifestSignParams {
    source: PathBuf,
    #[serde(default)]
    name: Option<String>,
}

fn run_manifest_audit(old: PathBuf, new: PathBuf) -> anyhow::Result<()> {
    let audit = ryeos_core_tools::actions::manifest_audit::run_manifest_audit(&old, &new)?;
    print!("{}", audit.render());
    Ok(())
}

fn run_logs(app_root: Option<PathBuf>, lines: usize, stdin_json: bool) -> anyhow::Result<()> {
    let (app_root, lines) = if stdin_json {
        let params: NodeLogsParams = serde_json::from_value(read_stdin_json()?)?;
        (params.app_root, params.lines.unwrap_or(200))
    } else {
        (app_root, lines)
    };

    let app_root = match app_root {
        Some(p) => p,
        None => std::env::var("RYEOS_APP_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs::data_dir()
                    .map(|d| d.join("ryeos"))
                    .expect("could not determine XDG data directory")
            }),
    };

    let report = ryeos_core_tools::actions::node_logs::read_node_logs(&app_root, lines);
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[derive(serde::Deserialize)]
struct NodeLogsParams {
    #[serde(default)]
    app_root: Option<PathBuf>,
    // CLI flags arrive as strings through command-arg binding, so accept either
    // a JSON number or a numeric string (e.g. `--lines 3` → "3").
    #[serde(default, deserialize_with = "de_opt_usize")]
    lines: Option<usize>,
}

/// Deserialize an optional `usize` from either a JSON number or a numeric
/// string — CLI args bind as strings, daemon/stdin callers may send numbers.
fn de_opt_usize<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum NumOrStr {
        Num(usize),
        Str(String),
    }
    match Option::<NumOrStr>::deserialize(deserializer)? {
        None => Ok(None),
        Some(NumOrStr::Num(n)) => Ok(Some(n)),
        Some(NumOrStr::Str(s)) => s
            .parse()
            .map(Some)
            .map_err(|e| serde::de::Error::custom(format!("invalid `lines` value '{s}': {e}"))),
    }
}

fn run_doctor(
    source: Option<PathBuf>,
    registry_roots: Vec<PathBuf>,
    strict: bool,
    stdin_json: bool,
) -> anyhow::Result<()> {
    let (source, registry_roots, strict) = if stdin_json {
        if source.is_some() {
            anyhow::bail!("--stdin-json is mutually exclusive with positional SOURCE");
        }
        let params: DoctorParams = serde_json::from_value(read_stdin_json()?)?;
        let registry_roots = params.registry_roots();
        (params.source, registry_roots, params.strict)
    } else {
        let source =
            source.ok_or_else(|| anyhow::anyhow!("SOURCE required (or pass --stdin-json)"))?;
        (source, registry_roots, strict)
    };

    let source_path = std::fs::canonicalize(&source)
        .with_context(|| format!("resolve source path {}", source.display()))?;
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
    let operator_config_root = ryeos_engine::roots::RuntimeRoot::new(app_root.clone()).config();

    let config = ryeos_app::config::Config::load(&ryeos_app::config::ConfigSources {
        app_root: Some(app_root.clone()),
        ..Default::default()
    })
    .context("load config for offline doctor engine")?;
    // A failed engine build is non-fatal: the static checks still run and the
    // import dry-run reports `unavailable` (e.g. when doctoring a bundle that
    // provides its own parsers, where the offline engine can't bootstrap them).
    let engine = ryeos_app::engine_init::build_engine_for_roots(
        &config,
        &dependency_roots,
        Some(&source_path),
        None,
    );
    let engine_err = engine
        .as_ref()
        .err()
        .map(|e| format!("{e:#}"))
        .unwrap_or_default();

    let report = ryeos_core_tools::actions::doctor::run_doctor(
        engine.as_ref().map_err(|_| engine_err.as_str()),
        &source_path,
        &dependency_roots,
        &operator_config_root,
    );

    let ok = report.ok;
    println!("{}", serde_json::to_string_pretty(&report)?);
    // Default: always exit 0 and let the `ok` field carry the verdict (so an
    // offline-dispatched `ryeos doctor` isn't reported as a failed tool). With
    // --strict, exit nonzero on any deterministic failure for CI/preflight
    // gating (advisory `unknown` checks never fail `ok`).
    if strict && !ok {
        std::process::exit(1);
    }
    Ok(())
}

#[derive(serde::Deserialize)]
struct DoctorParams {
    source: PathBuf,
    #[serde(default)]
    registry_root: Option<PathBuf>,
    #[serde(default)]
    registry_roots: Vec<PathBuf>,
    #[serde(default)]
    strict: bool,
}

impl DoctorParams {
    fn registry_roots(&self) -> Vec<PathBuf> {
        if self.registry_roots.is_empty() {
            self.registry_root.iter().cloned().collect()
        } else {
            self.registry_roots.clone()
        }
    }
}

fn resolve_publish_owner(
    explicit_owner: Option<String>,
    signing_key: &lillux::crypto::SigningKey,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> String {
    explicit_owner.unwrap_or_else(|| {
        let fp = ryeos_engine::trust::compute_fingerprint(&signing_key.verifying_key());
        trust_store
            .get(&fp)
            .and_then(|signer| signer.label.clone())
            .unwrap_or_else(|| "local-dev".to_string())
    })
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BundlePublishParams {
    source: PathBuf,
    #[serde(default)]
    registry_root: Option<PathBuf>,
    #[serde(default)]
    registry_roots: Vec<PathBuf>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    skip_unsignable: bool,
    #[serde(default)]
    allow_namespace_mismatch: bool,
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
        ryeos_core_tools::actions::build_bundle::rebuild_bundle_manifest(
            &source_path,
            &signing_key,
        )
        .context("rebuild source bundle binary manifest")?;
    }

    let report = ryeos_core_tools::actions::sign_bundle::sign_bundle_items_with_trust(
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

    ryeos_core_tools::actions::build_bundle::load_signing_key(&key_path)
        .with_context(|| format!("load signing key from {}", key_path.display()))
}

fn run_sign(
    item_refs: Vec<String>,
    project: Option<PathBuf>,
    source: String,
    stdin_json: bool,
) -> anyhow::Result<()> {
    use ryeos_core_tools::actions::sign::{run_sign, BatchReport, ItemOutcome, SignSource};

    let (item_refs, project_arg, source_str) = if stdin_json {
        if !item_refs.is_empty() {
            anyhow::bail!("--stdin-json is mutually exclusive with positional ITEM_REF values");
        }
        let parsed: StdinSignParams = serde_json::from_value(read_stdin_json()?)?;
        parsed.into_parts()
    } else {
        if item_refs.is_empty() {
            anyhow::bail!("ITEM_REF required (or pass --stdin-json)");
        }
        (item_refs, project, source)
    };

    if item_refs.is_empty() {
        anyhow::bail!("ITEM_REF required (or pass item_ref/item_refs in stdin JSON)");
    }

    let source = SignSource::parse(&source_str)?;
    let project = project_arg.or_else(|| std::env::current_dir().ok());

    let mut batch = BatchReport::default();
    let batch_mode = item_refs.len() > 1;
    for item_ref in item_refs {
        match run_sign(&item_ref, project.as_deref(), source) {
            Ok(report) => batch.extend(report),
            Err(e) if batch_mode => batch.failed.push(ItemOutcome {
                item_ref,
                signature: None,
                error: Some(format!("{e:#}")),
                warnings: Vec::new(),
            }),
            Err(e) => return Err(e),
        }
    }
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
    #[serde(
        default,
        deserialize_with = "ryeos_runtime::scalar_or_vec::deserialize"
    )]
    item_refs: Vec<String>,
    #[serde(default)]
    item_ref: Option<String>,
    #[serde(default)]
    project_path: Option<PathBuf>,
    #[serde(default = "default_source")]
    source: String,
}

impl StdinSignParams {
    fn into_parts(self) -> (Vec<String>, Option<PathBuf>, String) {
        let item_refs = if !self.item_refs.is_empty() {
            self.item_refs
        } else {
            self.item_ref.into_iter().collect()
        };
        (item_refs, self.project_path, self.source)
    }
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

            let report = ryeos_core_tools::actions::vault::run_put(
                &ryeos_core_tools::actions::vault::PutOptions {
                    app_root: ssd,
                    entries: vec![(name, value)],
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        VaultCmd::List { app_root } => {
            let ssd = resolve_app_root(app_root)?;
            let report = ryeos_core_tools::actions::vault::run_list(
                &ryeos_core_tools::actions::vault::ListOptions { app_root: ssd },
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        VaultCmd::Rm { keys, app_root } => {
            let ssd = resolve_app_root(app_root)?;
            let report = ryeos_core_tools::actions::vault::run_remove(
                &ryeos_core_tools::actions::vault::RemoveOptions {
                    app_root: ssd,
                    keys,
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        VaultCmd::Rewrap { app_root } => {
            let ssd = resolve_app_root(app_root)?;
            let outcome = ryeos_core_tools::actions::vault::run_rewrap(
                &ryeos_core_tools::actions::vault::RewrapOptions { app_root: ssd },
            )?;
            let failure_status = match &outcome {
                ryeos_core_tools::actions::vault::RewrapOutcome::CommittedDurable {
                    warning: Some(warning),
                    ..
                } => {
                    eprintln!("vault rewrap committed durably; cleanup is deferred: {warning}");
                    None
                }
                ryeos_core_tools::actions::vault::RewrapOutcome::CommittedDurable {
                    warning: None,
                    ..
                } => None,
                ryeos_core_tools::actions::vault::RewrapOutcome::RestoredPrevious {
                    reason,
                    ..
                } => {
                    eprintln!(
                        "vault rewrap did not commit; the previous generation was durably restored: {reason}"
                    );
                    Some("restored_previous")
                }
                ryeos_core_tools::actions::vault::RewrapOutcome::CommitDurabilityUncertain {
                    reason,
                    ..
                } => {
                    eprintln!(
                        "vault rewrap changed the live namespace, but crash durability is uncertain; recovery evidence was preserved: {reason}"
                    );
                    Some("commit_durability_uncertain")
                }
            };
            println!("{}", serde_json::to_string_pretty(&outcome)?);
            match failure_status {
                Some(status) => Err(anyhow::anyhow!(
                    "vault rewrap completed with non-success status `{status}`; inspect the structured outcome"
                )),
                None => Ok(()),
            }
        }
    }
}

fn run_authorize_client(
    app_root: Option<String>,
    public_key: Option<String>,
    scopes: String,
    label: String,
    merge_scopes: bool,
    stdin_json: bool,
) -> anyhow::Result<()> {
    use lillux::crypto::VerifyingKey;
    use ryeos_core_tools::actions::authorize::{
        run_authorize_client as run, AuthorizeClientParams,
    };

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
        merge: merge_scopes,
    })?;

    if !result.dropped_scopes.is_empty() {
        eprintln!(
            "warning: replaced the authorized-key scopes for {fp}; {n} existing scope(s) \
             were dropped: {dropped}. Re-run with --merge-scopes to keep them.",
            fp = result.fingerprint,
            n = result.dropped_scopes.len(),
            dropped = result.dropped_scopes.join(", "),
        );
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "fingerprint": result.fingerprint,
            "path": result.path.to_string_lossy(),
            "merged": result.merged,
            "dropped_scopes": result.dropped_scopes,
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
    use ryeos_core_tools::actions::authorize::{
        run_mint_admission_token, MintAdmissionTokenParams,
    };

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
    use ryeos_core_tools::actions::remote_descriptor::{
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
            let system_trust_dir = system
                .join(ryeos_engine::AI_DIR)
                .join("config")
                .join("keys")
                .join("trusted");
            std::fs::create_dir_all(&system_trust_dir).unwrap();
            ryeos_engine::trust::pin_key(&key.verifying_key(), "test", &system_trust_dir, None)
                .unwrap();
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
                &format!("kind: node\npath: {}\n", bundle.display()),
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
    fn bundle_publish_owner_defaults_to_pinned_signer_label() {
        let key = SigningKey::generate(&mut OsRng);
        let fp = ryeos_engine::trust::compute_fingerprint(&key.verifying_key());
        let trust_store = ryeos_engine::trust::TrustStore::from_signers(vec![
            ryeos_engine::trust::TrustedSigner {
                fingerprint: fp,
                verifying_key: key.verifying_key(),
                label: Some("user".to_string()),
            },
        ]);

        assert_eq!(resolve_publish_owner(None, &key, &trust_store), "user");
    }

    #[test]
    fn bundle_publish_owner_defaults_to_local_dev_when_unpinned() {
        let key = SigningKey::generate(&mut OsRng);
        let trust_store = ryeos_engine::trust::TrustStore::empty();

        assert_eq!(resolve_publish_owner(None, &key, &trust_store), "local-dev");
    }

    #[test]
    fn bundle_publish_owner_explicit_value_wins() {
        let key = SigningKey::generate(&mut OsRng);
        let trust_store = ryeos_engine::trust::TrustStore::empty();

        assert_eq!(
            resolve_publish_owner(Some("alice".to_string()), &key, &trust_store),
            "alice"
        );
    }
}
