use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use sha2::{Digest, Sha256};

// ── Public library primitives ──────────────────────────────────────

pub fn sha256_hex(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

pub fn valid_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn shard_path(root: &Path, namespace: &str, hash: &str, ext: &str) -> PathBuf {
    root.join(namespace)
        .join(&hash[..2])
        .join(&hash[2..4])
        .join(format!("{hash}{ext}"))
}

pub fn atomic_write(target: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = target.with_extension(format!("tmp.{}", std::process::id()));
    let mut file = fs::File::create(&tmp)?;
    file.write_all(data)?;
    file.sync_all()?;
    fs::rename(&tmp, target)?;
    Ok(())
}

/// Materialize a blob from CAS to a target path, setting Unix permission
/// bits so the result is executable. Like `atomic_write` but preserves
/// the exec mode from the `ItemSource` record.
///
/// On non-Unix platforms, the mode is ignored (the file is still written).
pub fn materialize_executable(target: &Path, data: &[u8], mode: u32) -> Result<()> {
    atomic_write(target, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(target, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

fn escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\x08' => out.push_str("\\b"),
            '\x0C' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < '\x20' => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c if c.is_ascii() => out.push(c),
            c if (c as u32) <= 0xFFFF => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => {
                let n = c as u32 - 0x10000;
                out.push_str(&format!(
                    "\\u{:04x}\\u{:04x}",
                    0xD800 + (n >> 10),
                    0xDC00 + (n & 0x3FF)
                ));
            }
        }
    }
    out.push('"');
    out
}

pub fn canonical_json(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => escape_string(s),
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let entries: Vec<String> = keys
                .iter()
                .map(|k| format!("{}:{}", escape_string(k), canonical_json(&map[*k])))
                .collect();
            format!("{{{}}}", entries.join(","))
        }
        serde_json::Value::Array(arr) => {
            format!(
                "[{}]",
                arr.iter().map(canonical_json).collect::<Vec<_>>().join(",")
            )
        }
    }
}

// ── CasStore ───────────────────────────────────────────────────────

pub struct CasStore {
    root: PathBuf,
}

impl CasStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn has_blob(&self, hash: &str) -> bool {
        valid_hash(hash) && shard_path(&self.root, "blobs", hash, "").exists()
    }

    pub fn has_object(&self, hash: &str) -> bool {
        valid_hash(hash) && shard_path(&self.root, "objects", hash, ".json").exists()
    }

    pub fn has(&self, hash: &str) -> bool {
        self.has_blob(hash) || self.has_object(hash)
    }

    pub fn get_blob(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        if !valid_hash(hash) {
            return Ok(None);
        }
        let path = shard_path(&self.root, "blobs", hash, "");
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(fs::read(&path)?))
    }

    pub fn get_object(&self, hash: &str) -> Result<Option<serde_json::Value>> {
        if !valid_hash(hash) {
            return Ok(None);
        }
        let path = shard_path(&self.root, "objects", hash, ".json");
        if !path.exists() {
            return Ok(None);
        }
        let data = fs::read(&path)?;
        Ok(Some(serde_json::from_slice(&data)?))
    }

    pub fn store_blob(&self, data: &[u8]) -> Result<String> {
        let hash = sha256_hex(data);
        let dest = shard_path(&self.root, "blobs", &hash, "");
        if dest.exists() {
            return Ok(hash);
        }
        atomic_write(&dest, data)?;
        Ok(hash)
    }

    pub fn store_object(&self, value: &serde_json::Value) -> Result<String> {
        let json = canonical_json(value);
        let hash = sha256_hex(json.as_bytes());
        let dest = shard_path(&self.root, "objects", &hash, ".json");
        if dest.exists() {
            return Ok(hash);
        }
        atomic_write(&dest, json.as_bytes())?;
        Ok(hash)
    }
}

// ── CLI interface ──────────────────────────────────────────────────

use clap::Subcommand;

#[derive(Subcommand)]
pub enum CasAction {
    /// Store content from stdin
    Store {
        #[arg(long)]
        root: String,
        #[arg(long)]
        blob: bool,
    },
    /// Fetch content by hash
    Fetch {
        #[arg(long)]
        root: String,
        #[arg(long)]
        hash: String,
        #[arg(long)]
        blob: bool,
    },
    /// Verify integrity of stored content
    Verify {
        #[arg(long)]
        root: String,
        #[arg(long)]
        hash: String,
        #[arg(long)]
        blob: bool,
    },
    /// Check if a hash exists
    Has {
        #[arg(long)]
        root: String,
        #[arg(long)]
        hash: String,
    },
}

pub fn run(action: CasAction) -> serde_json::Value {
    if matches!(
        &action,
        CasAction::Fetch { .. } | CasAction::Verify { .. } | CasAction::Has { .. }
    ) {
        let h = match &action {
            CasAction::Fetch { hash, .. }
            | CasAction::Verify { hash, .. }
            | CasAction::Has { hash, .. } => hash,
            _ => unreachable!(),
        };
        if !valid_hash(h) {
            eprintln!("invalid hash: expected 64 hex chars");
            std::process::exit(1);
        }
    }
    match action {
        CasAction::Store { root, blob } => cli_store(&root, blob),
        CasAction::Fetch { root, hash, blob } => cli_fetch(&root, &hash, blob),
        CasAction::Verify { root, hash, blob } => {
            let store = CasStore::new(PathBuf::from(&root));
            if blob {
                match store.get_blob(&hash) {
                    Ok(Some(data)) => {
                        serde_json::json!({ "valid": sha256_hex(&data) == hash, "hash": hash })
                    }
                    _ => serde_json::json!({ "valid": false, "hash": hash }),
                }
            } else {
                match store.get_object(&hash) {
                    Ok(Some(val)) => {
                        let canon = canonical_json(&val);
                        serde_json::json!({ "valid": sha256_hex(canon.as_bytes()) == hash, "hash": hash })
                    }
                    _ => serde_json::json!({ "valid": false, "hash": hash }),
                }
            }
        }
        CasAction::Has { root, hash } => {
            let store = CasStore::new(PathBuf::from(&root));
            serde_json::json!({ "exists": store.has(&hash), "hash": hash })
        }
    }
}

fn cli_store(root: &str, blob: bool) -> serde_json::Value {
    let mut data = Vec::new();
    if let Err(e) = std::io::Read::read_to_end(&mut std::io::stdin(), &mut data) {
        return serde_json::json!({ "error": format!("stdin: {e}") });
    }
    let store = CasStore::new(PathBuf::from(root));
    if blob {
        match store.store_blob(&data) {
            Ok(hash) => serde_json::json!({ "hash": hash }),
            Err(e) => serde_json::json!({ "error": e.to_string() }),
        }
    } else {
        let value: serde_json::Value = match serde_json::from_slice(&data) {
            Ok(v) => v,
            Err(e) => return serde_json::json!({ "error": format!("invalid JSON: {e}") }),
        };
        match store.store_object(&value) {
            Ok(hash) => serde_json::json!({ "hash": hash }),
            Err(e) => serde_json::json!({ "error": e.to_string() }),
        }
    }
}

fn cli_fetch(root: &str, hash: &str, blob: bool) -> serde_json::Value {
    let store = CasStore::new(PathBuf::from(root));
    let data = if blob {
        store.get_blob(hash)
    } else {
        store
            .get_object(hash)
            .map(|opt| opt.map(|v| canonical_json(&v).into_bytes()))
    };
    match data {
        Ok(Some(bytes)) => {
            let _ = std::io::Write::write_all(&mut std::io::stdout(), &bytes);
            std::process::exit(0);
        }
        _ => {
            eprintln!("not found");
            std::process::exit(1);
        }
    }
}
