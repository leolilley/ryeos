use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use lillux::crypto::DecodePrivateKey;
use zeroize::Zeroizing;

const MANAGED_MARKER: &str = "# ryeos:onboarding-managed-model-route:v1";
const MAX_MODEL_ROUTE_BYTES: u64 = 64 * 1024;
const MAX_OPERATOR_KEY_BYTES: u64 = 32 * 1024;

#[derive(Debug, Clone)]
pub struct PersistModelRouteOptions {
    pub app_root: PathBuf,
    pub provider_id: String,
    pub model_name: String,
    pub context_window: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PersistModelRouteReport {
    pub path: PathBuf,
    pub provider_id: String,
    pub model_name: String,
    pub context_window: u64,
    pub replaced_managed_route: bool,
}

pub fn persist_default_model_route(
    options: &PersistModelRouteOptions,
) -> Result<PersistModelRouteReport> {
    validate_identifier("provider_id", &options.provider_id, 128)?;
    validate_model_name(&options.model_name)?;
    if options.context_window == 0 {
        bail!("default model context_window must be greater than zero");
    }
    let path = options
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("config")
        .join("ryeos-runtime")
        .join("model_routing.yaml");
    let key_path = options
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("config/keys/signing/private_key.pem");
    let key_metadata = fs::symlink_metadata(&key_path)
        .with_context(|| format!("inspect operator key {}", key_path.display()))?;
    if key_metadata.file_type().is_symlink() || !key_metadata.is_file() {
        bail!("refusing unsafe operator key path {}", key_path.display());
    }
    let pem = Zeroizing::new(String::from_utf8(
        lillux::read_regular_file_bounded_no_follow(&key_path, MAX_OPERATOR_KEY_BYTES)
            .with_context(|| format!("read operator key {}", key_path.display()))?,
    )?);
    let signing_key = lillux::crypto::SigningKey::from_pkcs8_pem(pem.as_str())
        .with_context(|| format!("parse operator key {}", key_path.display()))?;
    let signer_fingerprint = lillux::crypto::fingerprint(&signing_key.verifying_key());
    let parent_path = path.parent().expect("model route has a parent");
    let parent = lillux::PinnedDirectory::open_or_create(parent_path)
        .with_context(|| format!("open model routing directory {}", parent_path.display()))?;
    let name = std::ffi::OsStr::new("model_routing.yaml");
    let _lock = lillux::ExclusiveFileLock::acquire_in(&parent, name)
        .with_context(|| format!("lock model routing {}", path.display()))?;
    let mut existing = parent.open_regular(name, false)?;
    let replaced_managed_route = match existing.as_mut() {
        Some(file) => {
            if file.metadata()?.len() > MAX_MODEL_ROUTE_BYTES {
                bail!("model routing exceeds {MAX_MODEL_ROUTE_BYTES} bytes");
            }
            let mut bytes = Vec::new();
            (&mut *file)
                .take(MAX_MODEL_ROUTE_BYTES.saturating_add(1))
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_MODEL_ROUTE_BYTES {
                bail!("model routing exceeds {MAX_MODEL_ROUTE_BYTES} bytes");
            }
            let source = String::from_utf8(bytes)?;
            verify_managed_route(&source, &signing_key.verifying_key(), &signer_fingerprint)
                .with_context(|| {
                    format!(
                        "refusing to replace unverified or operator-authored model routing at {}",
                        path.display()
                    )
                })?;
            true
        }
        None => false,
    };
    let body = format!(
        "{MANAGED_MARKER}\ncategory: ryeos-runtime\ntiers:\n  general:\n    provider: {}\n    model: {}\n    context_window: {}\n",
        yaml_scalar(&options.provider_id),
        yaml_scalar(&options.model_name),
        options.context_window
    );
    let signed = lillux::signature::sign_content(&body, &signing_key, "#", None);
    parent
        .atomic_write_if_same(name, existing.as_ref(), signed.as_bytes(), 0o600)
        .with_context(|| format!("write model routing {}", path.display()))?;
    Ok(PersistModelRouteReport {
        path,
        provider_id: options.provider_id.clone(),
        model_name: options.model_name.clone(),
        context_window: options.context_window,
        replaced_managed_route,
    })
}

fn verify_managed_route(
    source: &str,
    verifying_key: &lillux::crypto::VerifyingKey,
    expected_fingerprint: &str,
) -> Result<()> {
    let mut lines = source.lines();
    let signature_line = lines.next().unwrap_or_default();
    let header = lillux::signature::parse_signature_line(signature_line, "#", None)
        .ok_or_else(|| anyhow::anyhow!("managed route has no valid signature header"))?;
    let body = source
        .strip_prefix(signature_line)
        .and_then(|rest| rest.strip_prefix('\n'))
        .unwrap_or_default();
    if !body.lines().any(|line| line.trim() == MANAGED_MARKER) {
        bail!("managed route marker is absent");
    }
    if !lillux::signature::is_valid_signature_for(
        &header.content_hash,
        &header.signature_b64,
        &header.signer_fingerprint,
        body,
        verifying_key,
        expected_fingerprint,
    ) {
        bail!("managed route signature is invalid");
    }
    Ok(())
}

fn validate_identifier(label: &str, value: &str, max: usize) -> Result<()> {
    if value.is_empty()
        || value.len() > max
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("{label} contains invalid characters or exceeds {max} bytes");
    }
    Ok(())
}

fn validate_model_name(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 256
        || value
            .chars()
            .any(|character| character.is_control() || matches!(character, '\'' | '"' | '\\'))
    {
        bail!("model_name is empty, unsafe, or exceeds 256 bytes");
    }
    Ok(())
}

fn yaml_scalar(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[allow(dead_code)]
fn is_managed_route(path: &Path) -> bool {
    fs::read_to_string(path)
        .map(|source| source.lines().any(|line| line.trim() == MANAGED_MARKER))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::EncodePrivateKey;

    fn app_root() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().expect("temporary app root");
        let root = temp.path().to_path_buf();
        let key_path = root.join(".ai/config/keys/signing/private_key.pem");
        fs::create_dir_all(key_path.parent().expect("key parent")).expect("key directory");
        let key = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
        let pem = key.to_pkcs8_pem(Default::default()).expect("encode key");
        lillux::atomic_write_private(&key_path, pem.as_bytes()).expect("write key");
        (temp, root)
    }

    #[test]
    fn managed_route_is_signed_and_can_be_reselected() {
        let (_temp, root) = app_root();
        let first = persist_default_model_route(&PersistModelRouteOptions {
            app_root: root.clone(),
            provider_id: "verified-provider".to_string(),
            model_name: "model/one".to_string(),
            context_window: 128_000,
        })
        .expect("first route");
        assert!(!first.replaced_managed_route);
        let source = fs::read_to_string(&first.path).expect("route source");
        assert!(source.contains(MANAGED_MARKER));
        assert!(source.contains("# ryeos:signed:"));

        let second = persist_default_model_route(&PersistModelRouteOptions {
            app_root: root,
            provider_id: "verified-provider".to_string(),
            model_name: "model/two".to_string(),
            context_window: 200_000,
        })
        .expect("second route");
        assert!(second.replaced_managed_route);
    }

    #[test]
    fn operator_authored_route_is_never_overwritten() {
        let (_temp, root) = app_root();
        let path = root.join(".ai/config/ryeos-runtime/model_routing.yaml");
        fs::create_dir_all(path.parent().expect("route parent")).expect("route directory");
        fs::write(&path, "tiers: {}\n").expect("operator route");
        let error = persist_default_model_route(&PersistModelRouteOptions {
            app_root: root,
            provider_id: "verified-provider".to_string(),
            model_name: "model".to_string(),
            context_window: 128_000,
        })
        .expect_err("operator route must be preserved");
        assert!(error
            .to_string()
            .contains("refusing to replace unverified or operator-authored"));
        assert_eq!(
            fs::read_to_string(path).expect("preserved route"),
            "tiers: {}\n"
        );
    }

    #[test]
    fn forged_managed_marker_does_not_authorize_replacement() {
        let (_temp, root) = app_root();
        let path = root.join(".ai/config/ryeos-runtime/model_routing.yaml");
        fs::create_dir_all(path.parent().expect("route parent")).expect("route directory");
        fs::write(&path, format!("{MANAGED_MARKER}\ntiers: {{}}\n")).expect("forged route");
        let error = persist_default_model_route(&PersistModelRouteOptions {
            app_root: root,
            provider_id: "verified-provider".to_string(),
            model_name: "model".to_string(),
            context_window: 128_000,
        })
        .expect_err("unsigned marker must not be trusted");
        assert!(error
            .to_string()
            .contains("unverified or operator-authored"));
    }
}
