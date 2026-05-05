//! Verb loading and table construction.
//!
//! Verbs are loaded from `.ai/config/cli/*.yaml` in the three-tier hierarchy
//! (system → user → project). Each verb YAML is a simple routing definition:
//!
//!   kind: config
//!   category: cli
//!   id: status
//!   tokens: [status]
//!   description: "Show daemon status"
//!   execute: service:system/status
//!
//! The CLI does not declare params — those come from the engine item that
//! `execute` points to. The CLI just passes all remaining argv as a JSON
//! object to the daemon's `/execute` endpoint.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use ryeos_engine::roots;
use ryeos_runtime::authorizer::canonical_cap;

use crate::error::CliConfigError;

/// A parsed verb entry ready for matching and dispatch.
pub struct VerbEntry {
    pub verb_tokens: Vec<String>,
    pub description: String,
    pub execute: String,
    /// The canonical cap required to execute this verb's target, derived
    /// from the `execute` ref: `service:bundle/install` →
    /// `rye.execute.service.bundle/install`. Used for client-side
    /// pre-check and help display.
    pub required_cap: String,
    pub source_file: PathBuf,
    pub signer_fingerprint: String,
}

/// The compiled verb table.
pub struct VerbTable {
    /// Entries sorted by token length descending (longest-prefix match).
    sorted: Vec<VerbEntry>,
}

impl VerbTable {
    pub fn match_argv<'a>(&self, argv: &'a [String]) -> Option<(&VerbEntry, &'a [String])> {
        for entry in &self.sorted {
            if argv.len() >= entry.verb_tokens.len()
                && argv[..entry.verb_tokens.len()] == entry.verb_tokens[..]
            {
                return Some((entry, &argv[entry.verb_tokens.len()..]));
            }
        }
        None
    }

    pub fn all(&self) -> &[VerbEntry] {
        &self.sorted
    }
}

/// Load all verb YAMLs from the three-tier hierarchy and build the table.
///
/// Roots passed throughout this function are "bare" (no `.ai/` suffix).
/// `TrustStore::load_three_tier` joins `.ai/config/keys/trusted/` itself,
/// and we explicitly join `.ai/config/cli/` for verb-YAML discovery.
pub fn load_verbs(project_root: &Path) -> Result<VerbTable, crate::error::CliError> {
    let bundle_roots = discover_bundle_roots();
    let system_roots = roots::system_roots(&bundle_roots);
    let user_root = roots::user_root().ok();

    let trust_store = ryeos_engine::trust::TrustStore::load_three_tier(
        Some(project_root),
        user_root.as_deref(),
        &system_roots,
    )
    .map_err(|e| CliConfigError::TrustStoreLoad {
        detail: e.to_string(),
    })?;

    // Collect verb YAMLs from all tiers. Each root is bare; the verb dir
    // lives at `<root>/.ai/config/cli/`.
    let mut raw_entries: Vec<(PathBuf, String)> = Vec::new();

    // System roots first (authoritative)
    for root in &system_roots {
        load_cli_dir(root.join(".ai/config/cli"), &trust_store, &mut raw_entries);
    }
    // User
    if let Some(ref ur) = user_root {
        load_cli_dir(ur.join(".ai/config/cli"), &trust_store, &mut raw_entries);
    }
    // Project (highest priority for clash)
    load_cli_dir(
        project_root.join(".ai/config/cli"),
        &trust_store,
        &mut raw_entries,
    );

    // Parse into VerbEntries
    let mut by_tokens: HashMap<Vec<String>, VerbEntry> = HashMap::new();
    for (path, content) in &raw_entries {
        let entry = parse_verb_yaml(path, content)?;
        if let Some(existing) = by_tokens.insert(entry.verb_tokens.clone(), entry) {
            return Err(CliConfigError::DuplicateVerbTokens {
                tokens: existing.verb_tokens.clone(),
                paths: vec![existing.source_file, path.clone()],
            }
            .into());
        }
    }

    // Build sorted table
    let mut sorted: Vec<_> = by_tokens.into_values().collect();
    sorted.sort_by_key(|b| std::cmp::Reverse(b.verb_tokens.len()));

    Ok(VerbTable { sorted })
}

fn load_cli_dir(
    dir: PathBuf,
    trust_store: &ryeos_engine::trust::TrustStore,
    out: &mut Vec<(PathBuf, String)>,
) {
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        match path.extension().and_then(|e| e.to_str()) {
            Some("yaml") | Some("yml") => {}
            _ => continue,
        }

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Verify signature using trust store
        match verify_verb_content(&content, path.as_path(), trust_store) {
            Ok(_fp) => {
                // Stash fingerprint for later use
                // (we store it alongside the content)
                out.push((path, content));
            }
            Err(e) => {
                eprintln!("rye: warning: skipping {}: {e}", path.display());
            }
        }
    }
}

fn verify_verb_content(
    content: &str,
    _path: &Path,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<String, String> {
    let header = ryeos_engine::item_resolution::parse_signature_header(
        content,
        &ryeos_engine::contracts::SignatureEnvelope {
            prefix: "#".to_owned(),
            suffix: None,
            after_shebang: false,
        },
    )
    .ok_or_else(|| "no signature header".to_string())?;

    // `content_hash_after_signature` already returns the sha256 hex of the
    // body bytes after the signature line — do NOT hash it again.
    let body_hash = lillux::signature::content_hash_after_signature(content, "#", None, false)
        .ok_or_else(|| "no body after signature line".to_string())?;

    if body_hash != header.content_hash {
        return Err(format!(
            "content hash mismatch: header says {}, body hashes to {}",
            header.content_hash, body_hash
        ));
    }

    if !trust_store.is_trusted(&header.signer_fingerprint) {
        return Err(format!("untrusted signer: {}", header.signer_fingerprint));
    }

    Ok(header.signer_fingerprint)
}

fn parse_verb_yaml(path: &Path, content: &str) -> Result<VerbEntry, crate::error::CliError> {
    let value: Value = serde_yaml::from_str(content).map_err(|e| CliConfigError::SchemaError {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })?;

    let kind = value
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if kind != "config" {
        return Err(CliConfigError::WrongKind {
            path: path.to_path_buf(),
            got: kind.to_string(),
        }
        .into());
    }

    let category = value
        .get("category")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if category != "cli" {
        return Err(CliConfigError::WrongCategory {
            path: path.to_path_buf(),
            got: category.to_string(),
        }
        .into());
    }

    let tokens: Vec<String> = value
        .get("tokens")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .ok_or_else(|| CliConfigError::SchemaError {
            path: path.to_path_buf(),
            detail: "missing or invalid `tokens` array".into(),
        })?;

    if tokens.is_empty() {
        return Err(CliConfigError::EmptyVerbTokens {
            path: path.display().to_string(),
        }
        .into());
    }

    for token in &tokens {
        if token.is_empty() {
            return Err(CliConfigError::SchemaError {
                path: path.to_path_buf(),
                detail: "empty verb token".into(),
            }
            .into());
        }
        if token.starts_with('-') {
            return Err(CliConfigError::SchemaError {
                path: path.to_path_buf(),
                detail: format!("dash-prefixed verb token \"{token}\""),
            }
            .into());
        }
        if token == "help" {
            return Err(CliConfigError::SchemaError {
                path: path.to_path_buf(),
                detail: "reserved verb token \"help\"".into(),
            }
            .into());
        }
    }

    let description = value
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let execute = value
        .get("execute")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CliConfigError::MissingExecute {
            path: path.display().to_string(),
        })?
        .to_string();

    // Validate the execute ref parses as a canonical ref
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(&execute).map_err(|e| {
        CliConfigError::InvalidExecuteRef {
            path: path.display().to_string(),
            item_ref: execute.clone(),
            detail: e.to_string(),
        }
    })?;

    // Derive the canonical cap: service:bundle/install → rye.execute.service.bundle/install
    let required_cap = canonical_cap(&canonical.kind, &canonical.bare_id, "execute");

    // Extract signer fingerprint from signature header
    let signer_fingerprint =
        ryeos_engine::item_resolution::parse_signature_header(
            content,
            &ryeos_engine::contracts::SignatureEnvelope {
                prefix: "#".to_owned(),
                suffix: None,
                after_shebang: false,
            },
        )
        .map(|h| h.signer_fingerprint)
        .unwrap_or_default();

    Ok(VerbEntry {
        verb_tokens: tokens,
        description,
        execute,
        required_cap,
        source_file: path.to_path_buf(),
        signer_fingerprint,
    })
}

/// Discover installed bundle roots from the daemon state directory.
fn discover_bundle_roots() -> Vec<PathBuf> {
    let state_dir = match std::env::var("RYEOS_STATE_DIR") {
        Ok(p) => PathBuf::from(p),
        Err(_) => dirs::state_dir()
            .map(|d| d.join("ryeosd"))
            .unwrap_or_else(|| PathBuf::from(".ryeosd")),
    };
    let bundles_dir = state_dir.join(".ai").join("bundles");
    let mut roots = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&bundles_dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                roots.push(entry.path());
            }
        }
    }
    roots
}
