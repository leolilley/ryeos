use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::error::CliError;

const ARTIFACT_EXPORT_CHUNK_BYTES: usize = 512 * 1024;

pub(crate) async fn export_artifact(
    app_root: &Path,
    parameters: &Value,
) -> Result<Value, CliError> {
    let blob_hash = parameters
        .get("blob_hash")
        .and_then(Value::as_str)
        .ok_or_else(|| local("artifact export requires blob_hash"))?;
    validate_blob_hash(blob_hash)?;
    let destination = parameters
        .get("destination")
        .and_then(Value::as_str)
        .ok_or_else(|| local("artifact export requires destination"))?;
    let destination = absolute_destination(destination)?;
    let target_name = destination
        .file_name()
        .ok_or_else(|| local("artifact export destination has no file name"))?;
    let parent_path = destination
        .parent()
        .ok_or_else(|| local("artifact export destination has no parent"))?;
    let parent = lillux::PinnedDirectory::open(parent_path)
        .map_err(local_error)?
        .ok_or_else(|| local("artifact export destination parent does not exist"))?;
    parent.ensure_path_binding().map_err(local_error)?;
    if parent
        .entry_no_follow(target_name)
        .map_err(local_error)?
        .is_some()
    {
        return Err(local("artifact export destination already exists"));
    }

    crate::daemon_preflight::lifecycle_preflight(app_root).await?;
    let daemon_url = crate::transport::http::resolve_daemon_url(app_root).await?;
    let signer = crate::transport::signing::Signer::resolve(app_root)?;
    let discovered = crate::transport::discovery::discover_audience(&daemon_url).await?;

    let temp_name = OsString::from(format!(
        ".ryeos-artifact-export.{}.{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let mut staging = parent
        .open_regular_create(&temp_name, true, true, 0o600)
        .map_err(local_error)?;
    let transfer = async {
        let mut offset = 0_u64;
        let mut declared_total = None;
        let mut digest = Sha256::new();
        loop {
            let body = json!({
                "object_hashes": [],
                "blob_hashes": [],
                "blob_chunk": {
                    "hash": blob_hash,
                    "offset": offset,
                    "length": ARTIFACT_EXPORT_CHUNK_BYTES,
                },
            });
            let body_bytes = serde_json::to_vec(&body).expect("JSON value serialization");
            let headers = signer.sign(
                "POST",
                "/objects/get",
                &body_bytes,
                &discovered.principal_id,
            )?;
            let url = format!(
                "{}/objects/get",
                discovered.effective_base_url.trim_end_matches('/')
            );
            let response = crate::transport::http::post_json(&url, &headers, &body_bytes).await?;
            let chunk = response
                .get("blob_chunk")
                .and_then(Value::as_object)
                .ok_or_else(|| local("objects/get omitted the artifact blob chunk"))?;
            let kind = chunk.get("kind").and_then(Value::as_str).unwrap_or("");
            if kind == "missing" {
                return Err(local("artifact blob is not present on this node"));
            }
            if kind != "blob_chunk"
                || chunk.get("hash").and_then(Value::as_str) != Some(blob_hash)
                || chunk.get("offset").and_then(Value::as_u64) != Some(offset)
            {
                return Err(local("objects/get returned a contradictory artifact chunk"));
            }
            let total = chunk
                .get("total_size")
                .and_then(Value::as_u64)
                .ok_or_else(|| local("objects/get artifact chunk omitted total_size"))?;
            if declared_total
                .replace(total)
                .is_some_and(|prior| prior != total)
            {
                return Err(local("artifact size changed during export"));
            }
            let encoded = chunk
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| local("objects/get artifact chunk omitted data"))?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|error| local(format!("decode artifact chunk: {error}")))?;
            if bytes.len() > ARTIFACT_EXPORT_CHUNK_BYTES {
                return Err(local(
                    "objects/get artifact chunk exceeded its requested bound",
                ));
            }
            let end = offset
                .checked_add(u64::try_from(bytes.len()).map_err(local_error)?)
                .ok_or_else(|| local("artifact export byte count overflow"))?;
            let eof = chunk.get("eof").and_then(Value::as_bool) == Some(true);
            if end > total || eof != (end == total) || (bytes.is_empty() && !eof) {
                return Err(local("objects/get returned an invalid artifact byte range"));
            }
            staging.write_all(&bytes).map_err(local_error)?;
            digest.update(&bytes);
            offset = end;
            if eof {
                break;
            }
        }
        let observed = format!("{:x}", digest.finalize());
        if observed != blob_hash {
            return Err(local(
                "exported artifact bytes do not match their durable identity",
            ));
        }
        staging.sync_all().map_err(local_error)?;
        lillux::set_open_regular_file_mode(&staging, 0o644).map_err(local_error)?;
        parent.ensure_path_binding().map_err(local_error)?;
        match parent.rename_regular_child_noreplace_atomic(&temp_name, target_name, &staging) {
            Ok(()) => {}
            Err(error) if error.namespace_committed() => {
                if let Err(sync_error) = parent.sync() {
                    return Err(local(format!(
                        "artifact export committed at '{}' but directory durability is uncertain; do not retry to another destination: {error}; durability retry failed: {sync_error}",
                        destination.display()
                    )));
                }
            }
            Err(error) => return Err(local(format!("publish artifact export: {error}"))),
        }
        Ok::<u64, CliError>(offset)
    }
    .await;

    let bytes = match transfer {
        Ok(bytes) => bytes,
        Err(error) => {
            let _ = parent.remove_if_same(&temp_name, &staging);
            return Err(error);
        }
    };
    Ok(json!({
        "status": "exported",
        "blob_hash": blob_hash,
        "bytes": bytes,
        "destination": destination,
    }))
}

fn absolute_destination(raw: &str) -> Result<PathBuf, CliError> {
    if raw.trim() != raw || raw.is_empty() || raw.chars().any(char::is_control) {
        return Err(local("artifact export destination is invalid"));
    }
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        Ok(path)
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(local_error)
    }
}

fn validate_blob_hash(value: &str) -> Result<(), CliError> {
    if value.len() == 64
        && value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(local(
            "artifact export requires a canonical SHA-256 blob hash",
        ))
    }
}

fn local(detail: impl Into<String>) -> CliError {
    CliError::Local {
        detail: detail.into(),
    }
}

fn local_error(error: impl std::fmt::Display) -> CliError {
    local(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_hash_requires_lowercase_sha256() {
        validate_blob_hash(&"a".repeat(64)).unwrap();
        assert!(validate_blob_hash(&"A".repeat(64)).is_err());
        assert!(validate_blob_hash("abc").is_err());
        assert!(validate_blob_hash(&"g".repeat(64)).is_err());
    }

    #[test]
    fn artifact_destination_rejects_ambiguous_text() {
        assert!(absolute_destination("").is_err());
        assert!(absolute_destination(" result.bin").is_err());
        assert!(absolute_destination("result\nbin").is_err());
        assert!(absolute_destination("result.bin").unwrap().is_absolute());
    }
}
