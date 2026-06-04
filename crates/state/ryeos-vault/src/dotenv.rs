use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::policy::is_blocked_name;

pub fn read_dotenv_overlay(search_dirs: &[PathBuf]) -> Result<HashMap<String, String>> {
    let mut out: HashMap<String, String> = HashMap::new();
    for dir in search_dirs {
        let path = dir.join(".env");
        if !path.exists() {
            continue;
        }
        let content =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let parsed = parse_dotenv_text(&content, &path)?;
        for (k, v) in parsed {
            out.insert(k, v);
        }
    }
    Ok(out)
}

fn parse_dotenv_text(content: &str, path: &Path) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for (idx, raw) in content.lines().enumerate() {
        let lineno = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some(eq) = line.find('=') else {
            bail!(
                "vault dotenv: malformed line at {}:{lineno} (no `=`): {line:?}",
                path.display()
            );
        };
        let key = line[..eq].trim();
        if key.is_empty() {
            bail!("vault dotenv: empty key at {}:{lineno}", path.display());
        }
        if !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            bail!(
                "vault dotenv: invalid key `{key}` at {}:{lineno} \
                 (must match [A-Za-z0-9_]+)",
                path.display()
            );
        }
        if is_blocked_name(key) {
            bail!(
                "vault dotenv: key `{key}` at {}:{lineno} is on the \
                 OS-protected blocked list and would shadow inherited \
                 environment",
                path.display()
            );
        }
        let value = line[eq + 1..].trim();
        let value = strip_matching_quotes(value);
        out.insert(key.to_string(), value.to_string());
    }
    Ok(out)
}

fn strip_matching_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'"' || bytes[0] == b'\'')
        && bytes[0] == bytes[bytes.len() - 1]
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotenv_overlay_rejects_protected_application_env_names() {
        let tmp = tempfile::tempdir().unwrap();
        for key in [
            "LD_AUDIT",
            "DYLD_PRINT_LIBRARIES",
            "PYTHONHOME",
            "RYEOS_PROJECT_SECRET",
            "RYEOSD_THREAD_AUTH_TOKEN",
            "USER_SPACE",
            "RYEOS_SYSTEM_SPACE_DIR",
            "HTTP_PROXY",
            "SSL_CERT_FILE",
        ] {
            std::fs::write(tmp.path().join(".env"), format!("{key}=x\n")).unwrap();
            let err = read_dotenv_overlay(&[tmp.path().to_path_buf()]).unwrap_err();
            assert!(format!("{err:#}").contains(key), "got: {err:#}");
        }
    }
}
