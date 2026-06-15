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

/// Strictness policy for config/item verification.
///
/// `Permissive` is the historical default — accepts unsigned files
/// and unknown-signer files with a warning. Suitable for development
/// where bundle-signing may lag.
///
/// `Required` rejects unsigned and unknown-signer files outright.
/// Used for security-sensitive configs where a wrong source means
/// vault secrets get redirected (provider configs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadStrictness {
    Permissive,
    Required,
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
    /// Load trust from the project's trusted-keys dir plus the
    /// operator's trusted-keys dir (`<app_root>/.ai/config/keys/trusted`,
    /// passed explicitly by the caller — the daemon for preflight, the
    /// launch envelope for runtimes). Bundle roots are NOT a trust
    /// authority: a bundle cannot ship keys that vouch for itself.
    pub fn load(project_root: &Path, operator_trusted_keys_dir: &Path) -> Self {
        let mut keys = HashMap::new();

        let project_trusted_dir = project_root.join(".ai/config/keys/trusted");
        for dir in [project_trusted_dir.as_path(), operator_trusted_keys_dir] {
            if !dir.is_dir() {
                continue;
            }
            if let Ok(entries) = fs::read_dir(dir) {
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
                        keys.entry(key.fingerprint.clone()).or_insert(key);
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
        // `crates/bin/daemon/tests/fixtures/trusted_signers/`). Either form is
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
                let val = val.trim_start_matches(['=', ' ']).trim().trim_matches('"');
                fingerprint = Some(val.to_string());
            }
            if let Some(val) = trimmed.strip_prefix("owner") {
                let val = val.trim_start_matches(['=', ' ']).trim().trim_matches('"');
                owner = val.to_string();
            }
            // Accept both `pem` and `public_key` as field names for the
            // inline ed25519 key.  `PUBLISHER_TRUST.toml` files use
            // `public_key`; the daemon's self-trust bootstrap uses `pem`.
            let maybe_key = trimmed
                .strip_prefix("pem")
                .or_else(|| trimmed.strip_prefix("public_key"));
            if let Some(val) = maybe_key {
                let val = val.trim_start_matches(['=', ' ']).trim().trim_matches('"');
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
    bundle_roots: Vec<PathBuf>,
    trust_store: TrustStore,
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
    /// `bundle_roots` are CONFIG search roots only (configs ship in
    /// bundles); trust comes exclusively from the project root and the
    /// explicit operator trusted-keys dir. No hidden env reads here —
    /// the caller owns the trust context.
    pub fn new(
        project_root: PathBuf,
        bundle_roots: Vec<PathBuf>,
        operator_trusted_keys_dir: &Path,
    ) -> Self {
        let trust_store = TrustStore::load(&project_root, operator_trusted_keys_dir);
        Self {
            project_root,
            bundle_roots,
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
            "config" => ".ai/config/",
            _ => ".ai/",
        }
    }

    /// Load and verify a file. Permissive mode — warns on unsigned/unknown-signer.
    pub fn load_verified(&self, kind: &str, path: &Path) -> Result<VerifiedContent> {
        self.load_verified_with_strictness(kind, path, LoadStrictness::Permissive)
    }

    /// Load and verify a file with configurable strictness.
    pub fn load_verified_with_strictness(
        &self,
        kind: &str,
        path: &Path,
        strictness: LoadStrictness,
    ) -> Result<VerifiedContent> {
        let raw =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

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
                match strictness {
                    LoadStrictness::Permissive => {
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
                    LoadStrictness::Required => {
                        bail!(
                            "REJECTED: {} is signed by unknown signer {} \
                             (not in trust store). Strict mode requires a \
                             trusted publisher signature for this config kind.",
                            path.display(),
                            sig_header.signer_fingerprint
                        );
                    }
                }
            }
        } else {
            match strictness {
                LoadStrictness::Permissive => VerifiedContent {
                    content,
                    hash,
                    path: path.to_path_buf(),
                },
                LoadStrictness::Required => {
                    bail!(
                        "REJECTED: {} is unsigned. Strict mode requires a \
                         valid publisher signature for this config kind. \
                         Re-sign with: ./scripts/populate-bundles.sh \
                         --key .dev-keys/PUBLISHER_DEV.pem --owner ryeos-dev",
                        path.display()
                    );
                }
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

        // Collect least-specific first (bundles) then project last, so the
        // deep merge below — where each later overlay wins — yields the
        // documented `project > bundle` precedence.
        let mut candidate_paths = Vec::new();

        for bundle_root in &self.bundle_roots {
            let p = bundle_root.join(&item_path);
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
            let verified =
                self.load_verified("config", path)
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
            let verified =
                self.load_verified("config", path)
                    .map_err(|e| ConfigLoadError::VerifyFailed {
                        path: path.clone(),
                        source: e,
                    })?;
            let value =
                serde_yaml::from_str::<serde_yaml::Value>(&verified.content).map_err(|e| {
                    ConfigLoadError::RawYamlParseFailed {
                        path: path.clone(),
                        source: e,
                    }
                })?;
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
        let value =
            serde_yaml::from_value::<T>(merged).map_err(|e| ConfigLoadError::TypedParseFailed {
                path: last_path,
                source: e,
            })?;
        Ok(Some(value))
    }

    /// Same as `load_config_strict` but also returns the set of source root
    /// labels (`"project"` / `"bundle"`) that contributed a file
    /// to the result, in resolution order (bundle → project; project wins).
    /// Used for trust decisions where ANY contribution from an untrusted
    /// root must be detectable.
    ///
    /// Returns `Ok(None)` if no root contributed a file.
    /// Load a config requiring a valid signature from a trusted publisher.
    /// Returns only the parsed value (no contributor labels).
    /// Rejects unsigned and unknown-signer files outright.
    pub fn load_config_strict_signed<T: DeserializeOwned>(
        &self,
        config_id: &str,
    ) -> std::result::Result<Option<T>, ConfigLoadError> {
        self.load_config_with_strictness(config_id, LoadStrictness::Required)
            .map(|opt| opt.map(|(v, _contribs)| v))
    }

    /// Permissive provenance loader — permissive.
    pub fn load_config_with_provenance<T: DeserializeOwned>(
        &self,
        config_id: &str,
    ) -> std::result::Result<Option<(T, Vec<String>)>, ConfigLoadError> {
        self.load_config_with_strictness(config_id, LoadStrictness::Permissive)
    }

    /// Core loader with configurable strictness. Returns the parsed value
    /// and the list of contributing root labels.
    pub fn load_config_with_strictness<T: DeserializeOwned>(
        &self,
        config_id: &str,
        strictness: LoadStrictness,
    ) -> std::result::Result<Option<(T, Vec<String>)>, ConfigLoadError> {
        let subdir = Self::kind_subdir("config");
        let item_path = PathBuf::from(format!("{subdir}{config_id}.yaml"));

        // Collect (path, root_label) pairs least-specific first (bundle →
        // project), so the deep merge below — where each later overlay wins —
        // yields the documented `project > bundle` precedence.
        let mut candidate_paths: Vec<(PathBuf, &'static str)> = Vec::new();

        for bundle_root in &self.bundle_roots {
            let p = bundle_root.join(&item_path);
            if p.exists() {
                candidate_paths.push((p, "bundle"));
            }
        }

        let p = self.project_root.join(&item_path);
        if p.exists() {
            candidate_paths.push((p, "project"));
        }

        if candidate_paths.is_empty() {
            return Ok(None);
        }

        let contributors: Vec<String> = candidate_paths
            .iter()
            .map(|(_, label)| label.to_string())
            .collect();

        if candidate_paths.len() == 1 {
            let (path, _) = &candidate_paths[0];
            let verified = self
                .load_verified_with_strictness("config", path, strictness)
                .map_err(|e| ConfigLoadError::VerifyFailed {
                    path: path.clone(),
                    source: e,
                })?;
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
            return Ok(Some((value, contributors)));
        }

        // Multi-root merge path — preserve existing merge logic but return
        // all contributors so callers can apply trust policy.
        let mut merged = serde_yaml::Value::Null;
        for (path, _) in &candidate_paths {
            let verified = self
                .load_verified_with_strictness("config", path, strictness)
                .map_err(|e| ConfigLoadError::VerifyFailed {
                    path: path.clone(),
                    source: e,
                })?;
            let value =
                serde_yaml::from_str::<serde_yaml::Value>(&verified.content).map_err(|e| {
                    ConfigLoadError::RawYamlParseFailed {
                        path: path.clone(),
                        source: e,
                    }
                })?;
            merged = deep_merge_yaml(merged, value);
        }

        let last_path = candidate_paths
            .last()
            .map(|(p, _)| p.clone())
            .unwrap_or_else(|| item_path.clone());
        let value =
            serde_yaml::from_value::<T>(merged).map_err(|e| ConfigLoadError::TypedParseFailed {
                path: last_path,
                source: e,
            })?;
        Ok(Some((value, contributors)))
    }

    pub fn scan_kind(&self, kind: &str) -> Result<Vec<ScannedItem>> {
        let subdir = Self::kind_subdir(kind);
        let mut seen_names: HashSet<String> = HashSet::new();
        let mut results = Vec::new();

        let roots_to_scan: Vec<(&Path, &str)> = {
            let mut v = Vec::new();
            v.push((self.project_root.as_path(), "project"));
            for sr in &self.bundle_roots {
                v.push((sr.as_path(), "bundle"));
            }
            v
        };

        for (root, _space) in &roots_to_scan {
            let dir = root.join(subdir);
            if !dir.is_dir() {
                continue;
            }

            let entries =
                fs::read_dir(&dir).with_context(|| format!("scanning {}", dir.display()))?;

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
        if let Some(d) = p.parent() {
            fs::create_dir_all(d).unwrap()
        }
        fs::write(&p, content).unwrap();
        p
    }

    /// Operator trusted-keys dir for tests that don't exercise operator
    /// trust. Nonexistent path — `TrustStore::load` skips non-dirs.
    fn no_operator_trust() -> PathBuf {
        PathBuf::from("/nonexistent-operator-trust")
    }

    fn create_trust_store(dir: &Path, signing_key: &SigningKey) {
        let fingerprint = lillux::signature::compute_fingerprint(&signing_key.verifying_key());
        let vk_bytes = signing_key.verifying_key().to_bytes();
        let pem_b64 = base64::engine::general_purpose::STANDARD.encode(
            [
                0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
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
    fn load_config_project_overrides_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let bundle = tmp.path().join("bundle");

        create_file(&bundle, ".ai/config/test.yaml", "name: bundle\n");
        create_file(&project, ".ai/config/test.yaml", "name: project\n");

        let loader = VerifiedLoader::new(project, vec![bundle], &no_operator_trust());
        let config: serde_yaml::Value = loader.load_config_strict("test").unwrap().unwrap();

        assert_eq!(config["name"], "project");
    }

    #[test]
    fn load_config_missing_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");

        let loader = VerifiedLoader::new(project, vec![], &no_operator_trust());
        let config = loader
            .load_config_strict::<serde_yaml::Value>("nonexistent")
            .unwrap();

        assert!(config.is_none());
    }

    #[test]
    fn load_config_bad_yaml_returns_err() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");

        create_file(&project, ".ai/config/bad.yaml", "not valid yaml: [");

        let loader = VerifiedLoader::new(project, vec![], &no_operator_trust());
        let result = loader.load_config_strict::<serde_yaml::Value>("bad");

        assert!(
            result.is_err(),
            "bad YAML should fail, not silently return None"
        );
    }

    #[test]
    fn load_config_bundle_only() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let bundle = tmp.path().join("bundle");

        create_file(&bundle, ".ai/config/defaults.yaml", "key: from_bundle\n");

        let loader = VerifiedLoader::new(project, vec![bundle], &no_operator_trust());
        let config: serde_yaml::Value = loader.load_config_strict("defaults").unwrap().unwrap();

        assert_eq!(config["key"], "from_bundle");
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

        let loader = VerifiedLoader::new(tmp.path().to_path_buf(), vec![], &no_operator_trust());
        let verified = loader.load_verified("directive", &path).unwrap();

        assert!(!verified.content.contains("ryeos:signed:"));
        assert!(verified.content.contains("# Hello"));
        assert_eq!(verified.hash.len(), 64);
    }

    #[test]
    fn load_verified_unsigned_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("plain.md");
        let content = "# Plain Directive\n\nSome content here.\n";
        fs::write(&path, content).unwrap();

        let loader = VerifiedLoader::new(tmp.path().to_path_buf(), vec![], &no_operator_trust());
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

        let loader = VerifiedLoader::new(tmp.path().to_path_buf(), vec![], &no_operator_trust());
        let result = loader.load_verified("directive", &path);

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("content hash mismatch"));
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

        let loader = VerifiedLoader::new(tmp.path().to_path_buf(), vec![], &no_operator_trust());
        let result = loader.load_verified("directive", &path);

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("signature verification failed"));
    }

    #[test]
    fn load_verified_accepts_unknown_signer() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        let body = "# Test\n";
        let signed = sign_md(body, &sk);
        let path = tmp.path().join("unknown_signer.md");
        fs::write(&path, &signed).unwrap();

        let loader = VerifiedLoader::new(tmp.path().to_path_buf(), vec![], &no_operator_trust());
        let verified = loader.load_verified("directive", &path).unwrap();

        assert!(verified.content.contains("# Test"));
    }

    #[test]
    fn trust_store_loads_operator_and_project_dirs() {
        let op_sk = SigningKey::from_bytes(&[42u8; 32]);
        let proj_sk = SigningKey::from_bytes(&[43u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        let operator_root = tmp.path().join("app-root");
        let project = tmp.path().join("project");
        create_trust_store(&operator_root, &op_sk);
        create_trust_store(&project, &proj_sk);

        let store = TrustStore::load(&project, &operator_root.join(".ai/config/keys/trusted"));

        assert_eq!(store.len(), 2);
        let op_fp = lillux::signature::compute_fingerprint(&op_sk.verifying_key());
        let proj_fp = lillux::signature::compute_fingerprint(&proj_sk.verifying_key());
        assert!(store.get(&op_fp).is_some());
        assert!(store.get(&proj_fp).is_some());
    }

    #[test]
    fn trust_store_ignores_bundle_roots() {
        // A bundle shipping its own trusted-keys dir must NOT become a
        // trust authority — only project + operator dirs are consulted.
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        create_trust_store(&bundle, &sk);

        let loader = VerifiedLoader::new(
            tmp.path().join("project"),
            vec![bundle],
            &no_operator_trust(),
        );

        assert!(
            loader.trust_store().is_empty(),
            "bundle-shipped trust dirs must be ignored"
        );
    }

    #[test]
    fn trust_store_empty_when_no_dirs() {
        let store = TrustStore::load(Path::new("/nonexistent"), Path::new("/also-nonexistent"));
        assert!(store.is_empty());
    }

    #[test]
    fn load_verified_hash_is_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("det.md");
        let content = "deterministic content";
        fs::write(&path, content).unwrap();

        let loader = VerifiedLoader::new(tmp.path().to_path_buf(), vec![], &no_operator_trust());
        let v1 = loader.load_verified("directive", &path).unwrap();
        let v2 = loader.load_verified("directive", &path).unwrap();

        assert_eq!(v1.hash, v2.hash);
    }

    #[test]
    fn scan_kind_finds_across_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let bundle = tmp.path().join("bundle");

        create_file(&bundle, ".ai/tools/bundle_tool.md", "# Bundle Tool\n");
        create_file(&bundle, ".ai/tools/shared.md", "# Bundle Shared\n");
        create_file(&project, ".ai/tools/proj_tool.md", "# Project Tool\n");
        create_file(&project, ".ai/tools/shared.md", "# Project Shared\n");

        let project_clone = project.clone();
        let loader = VerifiedLoader::new(project, vec![bundle], &no_operator_trust());
        let items = loader.scan_kind("tool").unwrap();
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();

        assert!(names.contains(&"bundle_tool"));
        assert!(names.contains(&"proj_tool"));
        assert!(names.contains(&"shared"));

        // Project is scanned first, so a name present in both roots is
        // attributed to the project root (first-found-wins for enumeration).
        let shared = items.iter().find(|i| i.name == "shared").unwrap();
        assert_eq!(shared.root, project_clone);
    }

    #[test]
    fn scan_kind_empty_when_no_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");

        let loader = VerifiedLoader::new(project, vec![], &no_operator_trust());
        let items = loader.scan_kind("directive").unwrap();

        assert!(items.is_empty());
    }

    // ── Strict mode tests ──────────────────────────────────────────────

    /// Helper: sign YAML with a test key and pin it into `trust_dir`
    /// (a trusted-keys dir, written as-is) so strict mode accepts it.
    fn sign_and_pin(yaml_body: &str, trust_dir: &Path) -> String {
        use base64::Engine;
        use ed25519_dalek::SigningKey;
        use lillux::signature::{compute_fingerprint, sign_content_at};

        let sk = SigningKey::from_bytes(&[99u8; 32]);
        let vk = sk.verifying_key();
        let fp = compute_fingerprint(&vk);
        let signed = sign_content_at(yaml_body, &sk, "#", None, "2026-01-01T00:00:00Z");

        std::fs::create_dir_all(trust_dir).unwrap();
        let vk_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
        let toml =
            format!("fingerprint = \"{fp}\"\npem = \"ed25519:{vk_b64}\"\nowner = \"test\"\n");
        std::fs::write(trust_dir.join("test.toml"), toml).unwrap();
        signed
    }

    #[test]
    fn strict_load_rejects_unsigned_config() {
        let tmp = tempfile::tempdir().unwrap();
        let system = tmp.path().join("system");
        let cfg_subpath = ".ai/config/ryeos-runtime/model-providers/test.yaml";
        std::fs::create_dir_all(system.join(cfg_subpath).parent().unwrap()).unwrap();
        // NO signature header.
        std::fs::write(
            system.join(cfg_subpath),
            "base_url: https://example.com/v1\n",
        )
        .unwrap();

        let loader = VerifiedLoader::new(
            tmp.path().join("project"),
            vec![system],
            &no_operator_trust(),
        );
        let res = loader
            .load_config_strict_signed::<serde_yaml::Value>("ryeos-runtime/model-providers/test");
        assert!(res.is_err(), "strict mode must reject unsigned config");
        let msg = format!("{:#}", res.unwrap_err());
        assert!(
            msg.contains("unsigned") || msg.contains("REJECTED"),
            "error must explain unsigned rejection: {msg}"
        );
    }

    #[test]
    fn strict_load_rejects_unknown_signer() {
        let tmp = tempfile::tempdir().unwrap();
        let system = tmp.path().join("system");
        let cfg_subpath = ".ai/config/ryeos-runtime/model-providers/test.yaml";
        std::fs::create_dir_all(system.join(cfg_subpath).parent().unwrap()).unwrap();

        // Sign with a throwaway key that is NOT in the trust store.
        let yaml_body = "base_url: https://example.com/v1\n";
        let sk = ed25519_dalek::SigningKey::from_bytes(&[77u8; 32]);
        let signed =
            lillux::signature::sign_content_at(yaml_body, &sk, "#", None, "2026-01-01T00:00:00Z");
        std::fs::write(system.join(cfg_subpath), signed).unwrap();

        let loader = VerifiedLoader::new(
            tmp.path().join("project"),
            vec![system],
            &no_operator_trust(),
        );
        let res = loader
            .load_config_strict_signed::<serde_yaml::Value>("ryeos-runtime/model-providers/test");
        assert!(res.is_err(), "strict mode must reject unknown signer");
        let msg = format!("{:#}", res.unwrap_err());
        assert!(
            msg.contains("unknown signer") || msg.contains("REJECTED"),
            "error must explain unknown-signer rejection: {msg}"
        );
    }

    #[test]
    fn strict_load_accepts_operator_trusted_signed_config() {
        let tmp = tempfile::tempdir().unwrap();
        let system = tmp.path().join("system");
        let operator_keys = tmp.path().join("app-root/.ai/config/keys/trusted");
        let cfg_subpath = ".ai/config/ryeos-runtime/model-providers/test.yaml";
        std::fs::create_dir_all(system.join(cfg_subpath).parent().unwrap()).unwrap();

        let yaml_body = "base_url: https://example.com/v1\n";
        let signed = sign_and_pin(yaml_body, &operator_keys);
        std::fs::write(system.join(cfg_subpath), signed).unwrap();

        let loader = VerifiedLoader::new(tmp.path().join("project"), vec![system], &operator_keys);
        let res = loader
            .load_config_strict_signed::<serde_yaml::Value>("ryeos-runtime/model-providers/test");
        assert!(
            res.is_ok(),
            "strict mode must accept config signed by an operator-trusted key"
        );
        let val = res.unwrap().expect("should have a value");
        assert_eq!(val["base_url"].as_str(), Some("https://example.com/v1"));
    }

    #[test]
    fn strict_load_rejects_bundle_pinned_signer() {
        // The signer is pinned ONLY inside the bundle's own
        // `.ai/config/keys/trusted` — the removed self-vouching path.
        // Strict verification must treat it as an unknown signer.
        let tmp = tempfile::tempdir().unwrap();
        let system = tmp.path().join("system");
        let cfg_subpath = ".ai/config/ryeos-runtime/model-providers/test.yaml";
        std::fs::create_dir_all(system.join(cfg_subpath).parent().unwrap()).unwrap();

        let yaml_body = "base_url: https://example.com/v1\n";
        let signed = sign_and_pin(yaml_body, &system.join(".ai/config/keys/trusted"));
        std::fs::write(system.join(cfg_subpath), signed).unwrap();

        let loader = VerifiedLoader::new(
            tmp.path().join("project"),
            vec![system],
            &no_operator_trust(),
        );
        let res = loader
            .load_config_strict_signed::<serde_yaml::Value>("ryeos-runtime/model-providers/test");
        assert!(
            res.is_err(),
            "bundle-pinned signer must be rejected as unknown"
        );
        let msg = format!("{:#}", res.unwrap_err());
        assert!(
            msg.contains("unknown signer") || msg.contains("REJECTED"),
            "error must explain unknown-signer rejection: {msg}"
        );
    }
}
