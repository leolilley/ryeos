use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::policy::is_blocked_name;

/// Read one project `.env` overlay through an already descriptor-pinned
/// root, returning ONLY the keys in `wanted`. `.env` must be a direct regular
/// child; links and special files fail closed.
///
/// Lines for keys the caller did not ask for are ignored entirely — including
/// blocked control names (`RYEOSD_URL`, `PATH`, …), invalid names, and
/// malformed lines. This is deliberate: a project `.env` legitimately mixes
/// tool secrets with unrelated app/client config, and an unrelated line must
/// never fail resolution of the secrets a tool actually declared.
///
/// Safety rests on the caller: `wanted` names are pre-validated
/// (`validate_spawn_secret_name` / `validate_secret_name`) to exclude every
/// blocked name, so a wanted key can never be a blocked name and no blocked key
/// can ever be returned. The blocked-name check below is unreachable
/// defense-in-depth. Empty `wanted` ⇒ the file is not read.
pub fn read_dotenv_overlay_pinned(
    root: &lillux::secure_fs::PinnedDirectory,
    wanted: &HashSet<String>,
) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    if wanted.is_empty() {
        return Ok(out);
    }
    let Some(mut file) = root.open_regular(std::ffi::OsStr::new(".env"), false)? else {
        return Ok(out);
    };
    let mut content = String::new();
    file.read_to_string(&mut content)
        .with_context(|| format!("read {}", root.path().join(".env").display()))?;
    parse_dotenv_text(&content, &root.path().join(".env"), wanted, &mut out)?;
    Ok(out)
}

fn parse_dotenv_text(
    content: &str,
    path: &Path,
    wanted: &HashSet<String>,
    out: &mut HashMap<String, String>,
) -> Result<()> {
    for (idx, raw) in content.lines().enumerate() {
        let lineno = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some(eq) = line.find('=') else {
            // A line with no `=` carries no value. Only worth failing if it is
            // exactly a wanted key (a likely `API_KEY` typo missing `=value`);
            // otherwise it is unrelated noise — skip it.
            if wanted.contains(line.trim()) {
                bail!(
                    "vault dotenv: malformed line at {}:{lineno} for wanted key (no `=`): {line:?}",
                    path.display()
                );
            }
            continue;
        };
        let key = line[..eq].trim();
        // Ignore everything the caller did not ask for: unrelated keys, blocked
        // control names, and invalid names all fall through here harmlessly.
        if !wanted.contains(key) {
            continue;
        }
        // Defense in depth — unreachable, since wanted names are pre-validated
        // to be valid and non-blocked at the call sites.
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
                 OS-protected blocked list and cannot be loaded as a secret",
                path.display()
            );
        }
        let value = line[eq + 1..].trim();
        let value = strip_matching_quotes(value);
        out.insert(key.to_string(), value.to_string());
    }
    Ok(())
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

    fn wanted(keys: &[&str]) -> HashSet<String> {
        keys.iter().map(|s| s.to_string()).collect()
    }

    fn read_overlay(dir: &Path, wanted: &HashSet<String>) -> Result<HashMap<String, String>> {
        let root = lillux::secure_fs::PinnedDirectory::open(dir)?.expect("overlay root present");
        read_dotenv_overlay_pinned(&root, wanted)
    }

    #[test]
    fn ignores_unrelated_blocked_invalid_and_malformed_lines() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".env"),
            "RYEOSD_URL=https://example\n\
             PATH=/evil\n\
             SSL_CERT_FILE=/x\n\
             a key with spaces=nope\n\
             this is not valid\n\
             MY_SECRET=ok\n",
        )
        .unwrap();
        // Only MY_SECRET is wanted; every unrelated line (blocked, invalid,
        // malformed) is ignored rather than failing the resolution.
        let map = read_overlay(tmp.path(), &wanted(&["MY_SECRET"])).unwrap();
        assert_eq!(map.get("MY_SECRET").map(String::as_str), Some("ok"));
        assert!(!map.contains_key("RYEOSD_URL"));
        assert!(!map.contains_key("PATH"));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn empty_wanted_reads_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".env"), "RYEOSD_URL=x\n").unwrap();
        let map = read_overlay(tmp.path(), &wanted(&[])).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn malformed_line_for_wanted_key_fails() {
        let tmp = tempfile::tempdir().unwrap();
        // A bare wanted key with no `=` is a likely typo — surface it.
        std::fs::write(tmp.path().join(".env"), "MY_SECRET\n").unwrap();
        let err = read_overlay(tmp.path(), &wanted(&["MY_SECRET"])).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("MY_SECRET") && msg.contains("malformed"),
            "got: {msg}"
        );
    }
}
