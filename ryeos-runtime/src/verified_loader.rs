use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::Engine;
use lillux::crypto::VerifyingKey;
use serde::de::DeserializeOwned;
use thiserror::Error;

/// Strict, three-state error returned by [`VerifiedLoader::load_config_strict`].
///
/// Distinguishes "candidate file is absent" (`Ok(None)`) from "candidate
/// file exists but is broken" (`Err(_)`). Each variant carries the
/// offending file path so operators can act without re-running with
/// debug logs.
#[derive(Debug, Error)]
pub enum ConfigLoadError {
    #[error("config verify failed at {}: {source}", path.display())]
    VerifyFailed {
        path: PathBuf,
        #[source]
        source: anyhow::Error,
    },
    #[error("config raw-YAML parse failed at {}: {source}", path.display())]
    RawYamlParseFailed {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("config typed-parse failed at {}: {source}", path.display())]
    TypedParseFailed {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
}

impl ConfigLoadError {
    /// File path of the candidate that triggered the error. Useful for
    /// callers wrapping the error in higher-level diagnostics.
    pub fn path(&self) -> &Path {
        match self {
            Self::VerifyFailed { path, .. }
            | Self::RawYamlParseFailed { path, .. }
            | Self::TypedParseFailed { path, .. } => path,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TrustedKey {
    pub fingerprint: String,
    pub verifying_key: VerifyingKey,
    pub owner: String,
}

#[derive(Debug, Clone)]
pub struct TrustStore {
    keys: HashMap<String, TrustedKey>,
}

impl TrustStore {
    pub fn load_from_roots(
        project_root: &Path,
        user_root: Option<&Path>,
        system_roots: Vec<PathBuf>,
    ) -> Self {
        let mut keys = HashMap::new();

        let mut roots: Vec<&Path> = Vec::new();
        for sr in &system_roots {
            roots.push(sr);
        }
        if let Some(ur) = user_root {
            roots.push(ur);
        }
        roots.push(project_root);

        for root in &roots {
            let trusted_dir = root.join(".ai/config/keys/trusted");
            if !trusted_dir.is_dir() {
                continue;
            }
            if let Ok(entries) = fs::read_dir(&trusted_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                        continue;
                    }
                    if let Ok(key) = Self::parse_trusted_key_toml(&path) {
                        tracing::info!(
                            fingerprint = %key.fingerprint,
                            owner = %key.owner,
                            "loaded trusted key"
                        );
                        keys.entry(key.fingerprint.clone())
                            .or_insert(key);
                    }
                }
            }
        }

        if !keys.is_empty() {
            tracing::info!(count = keys.len(), "trust store loaded");
        }

        Self { keys }
    }

    fn parse_trusted_key_toml(path: &Path) -> Result<TrustedKey> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("reading trust store entry {}", path.display()))?;

        let mut fingerprint = None;
        let mut owner = String::new();
        let mut pem_lines: Vec<String> = Vec::new();
        let mut in_pem = false;
        // Single-line `pem = "ed25519:<b64>"` form, written by the
        // daemon's self-trust bootstrap and surfaced via the `ryeos-cli`
        // `identity-public-key` verb. The multi-line
        // `-----BEGIN PUBLIC KEY-----` PEM form is also supported (see
        // the daemon's trusted-signer fixture in
        // `ryeosd/tests/fixtures/trusted_signers/`). Either form is
        // accepted; if both appear the multi-line PEM wins (it is the
        // strictly typed format).
        let mut inline_key_b64: Option<String> = None;

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                continue;
            }
            if trimmed.starts_with("-----BEGIN PUBLIC KEY-----") {
                in_pem = true;
                continue;
            }
            if trimmed.starts_with("-----END PUBLIC KEY-----") {
                in_pem = false;
                continue;
            }
            if in_pem {
                pem_lines.push(trimmed.to_string());
                continue;
            }
            if let Some(val) = trimmed.strip_prefix("fingerprint") {
                let val = val.trim_start_matches(['=', ' '])
                    .trim()
                    .trim_matches('"');
                fingerprint = Some(val.to_string());
            }
            if let Some(val) = trimmed.strip_prefix("owner") {
                let val = val.trim_start_matches(['=', ' '])
                    .trim()
                    .trim_matches('"');
                owner = val.to_string();
            }
            if let Some(val) = trimmed.strip_prefix("pem") {
                let val = val
                    .trim_start_matches(['=', ' '])
                    .trim()
                    .trim_matches('"');
                if let Some(b64) = val.strip_prefix("ed25519:") {
                    inline_key_b64 = Some(b64.to_string());
                }
            }
        }

        let fingerprint = fingerprint.ok_or_else(|| anyhow::anyhow!("missing fingerprint"))?;
        let key_bytes: [u8; 32] = if !pem_lines.is_empty() {
            let pem_b64: String = pem_lines.join("");
            let pem_bytes = base64::engine::general_purpose::STANDARD
                .decode(&pem_b64)
                .context("invalid base64 in PEM")?;
            if pem_bytes.len() < 44 {
                bail!("PEM too short for Ed25519 public key");
            }
            pem_bytes[pem_bytes.len() - 32..]
                .try_into()
                .map_err(|_| anyhow::anyhow!("invalid key length"))?
        } else if let Some(b64) = inline_key_b64 {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(&b64)
                .context("invalid base64 in inline ed25519 key")?;
            if raw.len() != 32 {
                bail!(
                    "inline ed25519 key has wrong length: {} (expected 32 raw bytes)",
                    raw.len()
                );
            }
            raw.try_into()
                .map_err(|_| anyhow::anyhow!("invalid inline ed25519 key length"))?
        } else {
            bail!(
                "trust entry at {} has no public-key block: expected either a multi-line \
                 `-----BEGIN PUBLIC KEY-----` PEM or a single-line `pem = \"ed25519:<base64>\"`",
                path.display()
            );
        };
        let verifying_key = VerifyingKey::from_bytes(&key_bytes)
            .map_err(|_| anyhow::anyhow!("invalid Ed25519 public key"))?;

        Ok(TrustedKey {
            fingerprint,
            verifying_key,
            owner,
        })
    }

    pub fn get(&self, fingerprint: &str) -> Option<&TrustedKey> {
        self.keys.get(fingerprint)
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }
}

pub struct VerifiedLoader {
    project_root: PathBuf,
    user_root: Option<PathBuf>,
    system_roots: Vec<PathBuf>,
    trust_store: TrustStore,
}

#[derive(Debug)]
pub struct ResolvedPath {
    pub path: PathBuf,
    pub root: PathBuf,
    pub space: String,
}

#[derive(Debug)]
pub struct VerifiedContent {
    pub content: String,
    pub hash: String,
    pub path: PathBuf,
}

#[derive(Debug)]
pub struct ScannedItem {
    pub name: String,
    pub path: PathBuf,
    pub root: PathBuf,
}

impl VerifiedLoader {
    pub fn new(
        project_root: PathBuf,
        user_root: Option<PathBuf>,
        system_roots: Vec<PathBuf>,
    ) -> Self {
        let trust_store = TrustStore::load_from_roots(
            &project_root,
            user_root.as_deref(),
            system_roots.clone(),
        );
        Self {
            project_root,
            user_root,
            system_roots,
            trust_store,
        }
    }

    pub fn trust_store(&self) -> &TrustStore {
        &self.trust_store
    }

    fn kind_subdir(kind: &str) -> &'static str {
        match kind {
            "directive" => ".ai/directives/",
            "tool" => ".ai/tools/",
            "knowledge" => ".ai/knowledge/",
            "config" => ".ai/config/rye-runtime/",
            _ => ".ai/",
        }
    }

    fn strip_kind_prefix(item_id: &str) -> (&str, &str) {
        if let Some(rest) = item_id.split_once(':') {
            (rest.0, rest.1)
        } else {
            (item_id, item_id)
        }
    }

    pub fn resolve_item(&self, kind: &str, item_id: &str) -> Result<ResolvedPath> {
        let (effective_kind, bare_id) = Self::strip_kind_prefix(item_id);
        let kind = if effective_kind != bare_id { effective_kind } else { kind };
        let subdir = Self::kind_subdir(kind);

        let item_path = PathBuf::from(format!("{subdir}{bare_id}.md"));

        if self.project_root.join(&item_path).exists() {
            tracing::trace!(ref_path = %item_id, space = %"project", "resolved item location");
            return Ok(ResolvedPath {
                path: self.project_root.join(&item_path),
                root: self.project_root.clone(),
                space: "project".to_string(),
            });
        }

        if let Some(ref user_root) = self.user_root {
            if user_root.join(&item_path).exists() {
                tracing::trace!(ref_path = %item_id, space = %"user", "resolved item location");
                return Ok(ResolvedPath {
                    path: user_root.join(&item_path),
                    root: user_root.clone(),
                    space: "user".to_string(),
                });
            }
        }

        for system_root in &self.system_roots {
            if system_root.join(&item_path).exists() {
                tracing::trace!(ref_path = %item_id, space = %"system", "resolved item location");
                return Ok(ResolvedPath {
                    path: system_root.join(&item_path),
                    root: system_root.clone(),
                    space: "system".to_string(),
                });
            }
        }

        bail!("item not found: {kind}:{bare_id}");
    }

    pub fn load_verified(&self, kind: &str, path: &Path) -> Result<VerifiedContent> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;

        let content = lillux::signature::strip_signature_lines(&raw);

        let hash = lillux::sha256_hex(content.as_bytes());
        tracing::trace!(path = %path.display(), hash = %hash, "computed content hash for verification");

        let (prefix, suffix) = Self::signature_format_for(kind);
        let verified = if let Some(sig_header) = Self::parse_first_signature(&raw, prefix, suffix) {
            if sig_header.content_hash != hash {
                bail!(
                    "content hash mismatch in {}: signature says {}, computed {}",
                    path.display(),
                    sig_header.content_hash,
                    hash
                );
            }

            if let Some(trusted_key) = self.trust_store.get(&sig_header.signer_fingerprint) {
                if !lillux::signature::verify_signature(
                    &sig_header.content_hash,
                    &sig_header.signature_b64,
                    &trusted_key.verifying_key,
                ) {
                    bail!(
                        "signature verification failed for {} (signer: {})",
                        path.display(),
                        sig_header.signer_fingerprint
                    );
                }
                VerifiedContent {
                    content,
                    hash,
                    path: path.to_path_buf(),
                }
            } else {
                tracing::warn!(
                    "signed by unknown signer {} — not in trust store: {}",
                    sig_header.signer_fingerprint,
                    path.display()
                );
                VerifiedContent {
                    content,
                    hash,
                    path: path.to_path_buf(),
                }
            }
        } else {
            VerifiedContent {
                content,
                hash,
                path: path.to_path_buf(),
            }
        };

        Ok(verified)
    }

    fn signature_format_for(kind: &str) -> (&'static str, Option<&'static str>) {
        match kind {
            "directive" => ("<!--", Some("-->")),
            "knowledge" => ("<!--", Some("-->")),
            "tool" => ("#", None),
            "config" => ("#", None),
            _ => ("#", None),
        }
    }

    fn parse_first_signature(
        raw: &str,
        prefix: &str,
        suffix: Option<&str>,
    ) -> Option<lillux::signature::SignatureHeader> {
        for line in raw.lines().take(2) {
            if let Some(header) = lillux::signature::parse_signature_line(line, prefix, suffix) {
                return Some(header);
            }
            if prefix != "#" {
                if let Some(header) = lillux::signature::parse_signature_line(line, "#", None) {
                    return Some(header);
                }
            }
        }
        None
    }

    /// Strict, three-state config loader:
    ///
    /// - `Ok(None)`  — no candidate file exists at the expected path
    ///   under any space root. Truly absent.
    /// - `Ok(Some(_))` — a candidate exists, verified, and parsed
    ///   into the typed shape successfully.
    /// - `Err(_)` — a candidate file exists but verification or
    ///   parsing failed. The error names the file path and the
    ///   underlying cause so callers can surface a loud diagnostic.
    pub fn load_config_strict<T: DeserializeOwned>(
        &self,
        config_id: &str,
    ) -> std::result::Result<Option<T>, ConfigLoadError> {
        let subdir = Self::kind_subdir("config");
        let item_path = PathBuf::from(format!("{subdir}{config_id}.yaml"));

        let mut candidate_paths = Vec::new();

        for system_root in &self.system_roots {
            let p = system_root.join(&item_path);
            if p.exists() {
                candidate_paths.push(p);
            }
        }

        if let Some(ref user_root) = self.user_root {
            let p = user_root.join(&item_path);
            if p.exists() {
                candidate_paths.push(p);
            }
        }

        let p = self.project_root.join(&item_path);
        if p.exists() {
            candidate_paths.push(p);
        }

        if candidate_paths.is_empty() {
            return Ok(None);
        }

        if candidate_paths.len() == 1 {
            let path = &candidate_paths[0];
            let verified = self
                .load_verified("config", path)
                .map_err(|e| ConfigLoadError::VerifyFailed {
                    path: path.clone(),
                    source: e,
                })?;
            // Parse raw YAML first, then type-convert — same as the
            // merged path. This ensures YAML syntax errors always surface
            // as RawYamlParseFailed and type-shape errors as TypedParseFailed,
            // giving consistent enum semantics regardless of candidate count.
            let raw_value: serde_yaml::Value =
                serde_yaml::from_str(&verified.content).map_err(|e| {
                    ConfigLoadError::RawYamlParseFailed {
                        path: path.clone(),
                        source: e,
                    }
                })?;
            let value = serde_yaml::from_value(raw_value).map_err(|e| {
                ConfigLoadError::TypedParseFailed {
                    path: path.clone(),
                    source: e,
                }
            })?;
            return Ok(Some(value));
        }

        let mut merged = serde_yaml::Value::Null;
        for path in &candidate_paths {
            let verified = self
                .load_verified("config", path)
                .map_err(|e| ConfigLoadError::VerifyFailed {
                    path: path.clone(),
                    source: e,
                })?;
            let value = serde_yaml::from_str::<serde_yaml::Value>(&verified.content).map_err(
                |e| ConfigLoadError::RawYamlParseFailed {
                    path: path.clone(),
                    source: e,
                },
            )?;
            merged = deep_merge_yaml(merged, value);
        }

        // The merged value isn't tied to a single file; surface the
        // last contributing file in the typed-parse error path so
        // operators have *some* lead. This keeps the error variant
        // shape stable (file path + underlying error).
        let last_path = candidate_paths
            .last()
            .cloned()
            .unwrap_or_else(|| item_path.clone());
        let value = serde_yaml::from_value::<T>(merged).map_err(|e| {
            ConfigLoadError::TypedParseFailed {
                path: last_path,
                source: e,
            }
        })?;
        Ok(Some(value))
    }

    pub fn scan_kind(&self, kind: &str) -> Result<Vec<ScannedItem>> {
        let subdir = Self::kind_subdir(kind);
        let mut seen_names: HashSet<String> = HashSet::new();
        let mut results = Vec::new();

        let roots_to_scan: Vec<(&Path, &str)> = {
            let mut v = Vec::new();
            for sr in &self.system_roots {
                v.push((sr.as_path(), "system"));
            }
            if let Some(ref ur) = self.user_root {
                v.push((ur.as_path(), "user"));
            }
            v.push((self.project_root.as_path(), "project"));
            v
        };

        for (root, _space) in &roots_to_scan {
            let dir = root.join(subdir);
            if !dir.is_dir() {
                continue;
            }

            let entries = fs::read_dir(&dir)
                .with_context(|| format!("scanning {}", dir.display()))?;

            for entry in entries {
                let entry = entry?;
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }

                let name = match path.file_stem().and_then(|s| s.to_str()) {
                    Some(n) => n.to_string(),
                    None => continue,
                };

                if !seen_names.contains(&name) {
                    seen_names.insert(name.clone());
                    results.push(ScannedItem {
                        name,
                        path: path.clone(),
                        root: root.to_path_buf(),
                    });
                }
            }
        }

        Ok(results)
    }
}

fn deep_merge_yaml(base: serde_yaml::Value, overlay: serde_yaml::Value) -> serde_yaml::Value {
    use serde_yaml::Value as Yv;
    match (base, overlay) {
        (Yv::Mapping(mut base_map), Yv::Mapping(overlay_map)) => {
            for (key, value) in overlay_map {
                let merged = match base_map.remove(&key) {
                    Some(base_val) => deep_merge_yaml(base_val, value),
                    None => value,
                };
                base_map.insert(key, merged);
            }
            Yv::Mapping(base_map)
        }
        (_, overlay) => overlay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::SigningKey;
    use std::fs;

    fn create_file(dir: &Path, relative: &str, content: &str) -> PathBuf {
        let p = dir.join(relative);
        if let Some(d) = p.parent() { fs::create_dir_all(d).unwrap() }
        fs::write(&p, content).unwrap();
        p
    }

    fn create_trust_store(dir: &Path, signing_key: &SigningKey) {
        let fingerprint = lillux::signature::compute_fingerprint(&signing_key.verifying_key());
        let vk_bytes = signing_key.verifying_key().to_bytes();
        let pem_b64 = base64::engine::general_purpose::STANDARD.encode(
            [
                0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70,
                0x03, 0x21, 0x00,
            ]
            .as_slice()
            .iter()
            .chain(vk_bytes.iter())
            .copied()
            .collect::<Vec<u8>>(),
        );
        let toml_content = format!(
            r#"version = "1.0.0"
category = "keys/trusted"
fingerprint = "{fingerprint}"
owner = "test"

[public_key]
pem = """
-----BEGIN PUBLIC KEY-----
{pem_b64}
-----END PUBLIC KEY-----
"""
"#,
        );
        create_file(
            dir,
            &format!(".ai/config/keys/trusted/{fingerprint}.toml"),
            &toml_content,
        );
    }

    fn sign_md(body: &str, signing_key: &SigningKey) -> String {
        lillux::signature::sign_content(body, signing_key, "<!--", Some("-->"))
    }

    #[test]
    fn resolve_item_finds_in_project_first() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let system = tmp.path().join("system");

        create_file(
            &project,
            ".ai/directives/hello.md",
            "# Project Hello\n",
        );
        create_file(
            &system,
            ".ai/directives/hello.md",
            "# System Hello\n",
        );

        let loader = VerifiedLoader::new(project, None, vec![system]);
        let resolved = loader.resolve_item("directive", "hello").unwrap();

        assert_eq!(resolved.space, "project");
        assert!(resolved.path.to_string_lossy().contains("project"));
    }

    #[test]
    fn resolve_item_falls_back_to_user() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let user = tmp.path().join("user");
        let system = tmp.path().join("system");

        create_file(
            &user,
            ".ai/directives/shared.md",
            "# User Shared\n",
        );
        create_file(
            &system,
            ".ai/directives/shared.md",
            "# System Shared\n",
        );

        let loader = VerifiedLoader::new(project, Some(user), vec![system]);
        let resolved = loader.resolve_item("directive", "shared").unwrap();

        assert_eq!(resolved.space, "user");
    }

    #[test]
    fn resolve_item_falls_back_to_system() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let user = tmp.path().join("user");
        let system = tmp.path().join("system");

        create_file(
            &system,
            ".ai/tools/run.md",
            "# System Tool\n",
        );

        let loader = VerifiedLoader::new(project, Some(user), vec![system]);
        let resolved = loader.resolve_item("tool", "run").unwrap();

        assert_eq!(resolved.space, "system");
    }

    #[test]
    fn resolve_item_strips_kind_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");

        create_file(
            &project,
            ".ai/directives/agent.md",
            "# Agent Directive\n",
        );

        let loader = VerifiedLoader::new(project, None, vec![]);
        let resolved = loader.resolve_item("directive", "directive:agent").unwrap();

        assert_eq!(resolved.space, "project");
        assert!(resolved.path.to_string_lossy().ends_with("agent.md"));
    }

    #[test]
    fn resolve_item_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");

        let loader = VerifiedLoader::new(project, None, vec![]);
        let result = loader.resolve_item("directive", "nonexistent");

        assert!(result.is_err());
    }

    #[test]
    fn load_config_system_user_project_override() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let user = tmp.path().join("user");
        let system = tmp.path().join("system");

        create_file(
            &system,
            ".ai/config/rye-runtime/test.yaml",
            "name: system\n",
        );
        create_file(
            &user,
            ".ai/config/rye-runtime/test.yaml",
            "name: user\n",
        );
        create_file(
            &project,
            ".ai/config/rye-runtime/test.yaml",
            "name: project\n",
        );

        let loader = VerifiedLoader::new(project, Some(user), vec![system]);
        let config: serde_yaml::Value = loader.load_config_strict("test").unwrap().unwrap();

        assert_eq!(config["name"], "project");
    }

    #[test]
    fn load_config_missing_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");

        let loader = VerifiedLoader::new(project, None, vec![]);
        let config = loader.load_config_strict::<serde_yaml::Value>("nonexistent").unwrap();

        assert!(config.is_none());
    }

    #[test]
    fn load_config_bad_yaml_returns_err() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");

        create_file(
            &project,
            ".ai/config/rye-runtime/bad.yaml",
            "not valid yaml: [",
        );

        let loader = VerifiedLoader::new(project, None, vec![]);
        let result = loader.load_config_strict::<serde_yaml::Value>("bad");

        assert!(result.is_err(), "bad YAML should fail, not silently return None");
    }

    #[test]
    fn load_config_system_only() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let system = tmp.path().join("system");

        create_file(
            &system,
            ".ai/config/rye-runtime/defaults.yaml",
            "key: from_system\n",
        );

        let loader = VerifiedLoader::new(project, None, vec![system]);
        let config: serde_yaml::Value = loader.load_config_strict("defaults").unwrap().unwrap();

        assert_eq!(config["key"], "from_system");
    }

    #[test]
    fn load_verified_strips_signature_and_hashes() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        create_trust_store(tmp.path(), &sk);

        let body = "# Hello\n\nBody text.\n";
        let signed = sign_md(body, &sk);
        let path = tmp.path().join("test.md");
        fs::write(&path, &signed).unwrap();

        let loader = VerifiedLoader::new(tmp.path().to_path_buf(), None, vec![]);
        let verified = loader.load_verified("directive", &path).unwrap();

        assert!(!verified.content.contains("rye:signed:"));
        assert!(verified.content.contains("# Hello"));
        assert_eq!(verified.hash.len(), 64);
    }

    #[test]
    fn load_verified_unsigned_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("plain.md");
        let content = "# Plain Directive\n\nSome content here.\n";
        fs::write(&path, content).unwrap();

        let loader = VerifiedLoader::new(tmp.path().to_path_buf(), None, vec![]);
        let verified = loader.load_verified("directive", &path).unwrap();

        assert_eq!(verified.content, content);
        assert_eq!(verified.hash.len(), 64);
    }

    #[test]
    fn load_verified_rejects_tampered_content() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        create_trust_store(tmp.path(), &sk);

        let body = "# Original\n";
        let signed = sign_md(body, &sk);
        let tampered = signed.replace("# Original", "# Tampered");
        let path = tmp.path().join("tampered.md");
        fs::write(&path, &tampered).unwrap();

        let loader = VerifiedLoader::new(tmp.path().to_path_buf(), None, vec![]);
        let result = loader.load_verified("directive", &path);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("content hash mismatch"));
    }

    #[test]
    fn load_verified_rejects_bad_signature() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let other_sk = SigningKey::from_bytes(&[99u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        create_trust_store(tmp.path(), &other_sk);

        let body = "# Test\n";
        let signed = sign_md(body, &sk);
        let sk_fp = lillux::signature::compute_fingerprint(&sk.verifying_key());
        let other_fp = lillux::signature::compute_fingerprint(&other_sk.verifying_key());
        let forged = signed.replace(&sk_fp, &other_fp);
        let path = tmp.path().join("bad_sig.md");
        fs::write(&path, &forged).unwrap();

        let loader = VerifiedLoader::new(tmp.path().to_path_buf(), None, vec![]);
        let result = loader.load_verified("directive", &path);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("signature verification failed"));
    }

    #[test]
    fn load_verified_accepts_unknown_signer() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        let body = "# Test\n";
        let signed = sign_md(body, &sk);
        let path = tmp.path().join("unknown_signer.md");
        fs::write(&path, &signed).unwrap();

        let loader = VerifiedLoader::new(tmp.path().to_path_buf(), None, vec![]);
        let verified = loader.load_verified("directive", &path).unwrap();

        assert!(verified.content.contains("# Test"));
    }

    #[test]
    fn trust_store_loads_from_roots() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        let system = tmp.path().join("system");
        create_trust_store(&system, &sk);

        let store = TrustStore::load_from_roots(
            &tmp.path().join("project"),
            None,
            vec![system.clone()],
        );

        assert_eq!(store.len(), 1);
        let fp = lillux::signature::compute_fingerprint(&sk.verifying_key());
        assert!(store.get(&fp).is_some());
    }

    #[test]
    fn trust_store_empty_when_no_dirs() {
        let store = TrustStore::load_from_roots(
            Path::new("/nonexistent"),
            None,
            vec![],
        );
        assert!(store.is_empty());
    }

    #[test]
    fn load_verified_hash_is_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("det.md");
        let content = "deterministic content";
        fs::write(&path, content).unwrap();

        let loader = VerifiedLoader::new(tmp.path().to_path_buf(), None, vec![]);
        let v1 = loader.load_verified("directive", &path).unwrap();
        let v2 = loader.load_verified("directive", &path).unwrap();

        assert_eq!(v1.hash, v2.hash);
    }

    #[test]
    fn scan_kind_finds_across_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let user = tmp.path().join("user");
        let system = tmp.path().join("system");

        create_file(
            &system,
            ".ai/tools/sys_tool.md",
            "# System Tool\n",
        );
        create_file(
            &system,
            ".ai/tools/shared.md",
            "# System Shared\n",
        );
        create_file(
            &user,
            ".ai/tools/user_tool.md",
            "# User Tool\n",
        );
        create_file(
            &user,
            ".ai/tools/shared.md",
            "# User Shared\n",
        );
        create_file(
            &project,
            ".ai/tools/proj_tool.md",
            "# Project Tool\n",
        );

        let system_clone = system.clone();
        let loader = VerifiedLoader::new(project, Some(user), vec![system]);
        let items = loader.scan_kind("tool").unwrap();
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();

        assert!(names.contains(&"sys_tool"));
        assert!(names.contains(&"user_tool"));
        assert!(names.contains(&"proj_tool"));
        assert!(names.contains(&"shared"));

        let shared = items.iter().find(|i| i.name == "shared").unwrap();
        assert_eq!(shared.root, system_clone);
    }

    #[test]
    fn scan_kind_empty_when_no_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");

        let loader = VerifiedLoader::new(project, None, vec![]);
        let items = loader.scan_kind("directive").unwrap();

        assert!(items.is_empty());
    }
}
