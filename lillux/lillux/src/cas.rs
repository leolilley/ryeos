use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use clap::Subcommand;
use sha2::{Digest, Sha256};

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
    // Validate hashes before slicing to prevent panics
    let needs_hash = matches!(&action, CasAction::Fetch { .. } | CasAction::Verify { .. } | CasAction::Has { .. });
    if needs_hash {
        let h = match &action {
            CasAction::Fetch { hash, .. } | CasAction::Verify { hash, .. } | CasAction::Has { hash, .. } => hash,
            _ => unreachable!(),
        };
        if !valid_hash(h) {
            return serde_json::json!({ "error": "invalid hash: expected 64 hex chars" });
        }
    }
    match action {
        CasAction::Store { root, blob } => do_store(&root, blob),
        CasAction::Fetch { root, hash, blob } => do_fetch(&root, &hash, blob),
        CasAction::Verify { root, hash, blob } => {
            let (ns, ext) = if blob { ("blobs", "") } else { ("objects", ".json") };
            match fs::read(shard(&root, ns, &hash, ext)) {
                Ok(data) => serde_json::json!({ "valid": sha256_hex(&data) == hash, "hash": hash }),
                Err(_) => serde_json::json!({ "valid": false, "hash": hash }),
            }
        }
        CasAction::Has { root, hash } => {
            let exists = shard(&root, "blobs", &hash, "").exists() || shard(&root, "objects", &hash, ".json").exists();
            serde_json::json!({ "exists": exists, "hash": hash })
        }
    }
}

fn valid_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn shard(root: &str, ns: &str, hash: &str, ext: &str) -> PathBuf {
    PathBuf::from(root).join(ns).join(&hash[..2]).join(&hash[2..4]).join(format!("{hash}{ext}"))
}

fn sha256_hex(data: &[u8]) -> String { format!("{:x}", Sha256::digest(data)) }

/// Escape a string the same way Python's json.dumps(ensure_ascii=True) does:
/// all non-ASCII codepoints become \uXXXX (BMP) or \uXXXX\uXXXX (surrogate pair).
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
            c if c < '\x20' => { out.push_str(&format!("\\u{:04x}", c as u32)); }
            c if c.is_ascii() => out.push(c),
            c if (c as u32) <= 0xFFFF => { out.push_str(&format!("\\u{:04x}", c as u32)); }
            c => {
                // Non-BMP: encode as surrogate pair, matching Python
                let n = c as u32 - 0x10000;
                out.push_str(&format!("\\u{:04x}\\u{:04x}", 0xD800 + (n >> 10), 0xDC00 + (n & 0x3FF)));
            }
        }
    }
    out.push('"');
    out
}

fn canonical_json(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => escape_string(s),
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let entries: Vec<String> = keys.iter()
                .map(|k| format!("{}:{}", escape_string(k), canonical_json(&map[*k])))
                .collect();
            format!("{{{}}}", entries.join(","))
        }
        serde_json::Value::Array(arr) => {
            format!("[{}]", arr.iter().map(canonical_json).collect::<Vec<_>>().join(","))
        }
    }
}

fn atomic_write(target: &PathBuf, data: &[u8]) -> Result<(), String> {
    if let Some(p) = target.parent() { fs::create_dir_all(p).map_err(|e| format!("mkdir: {e}"))?; }
    let tmp = target.with_extension(format!("tmp.{}", std::process::id()));
    fs::File::create(&tmp).map_err(|e| format!("tmpfile: {e}"))?.write_all(data).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("write: {e}")
    })?;
    fs::rename(&tmp, target).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("rename: {e}")
    })
}

fn do_store(root: &str, blob: bool) -> serde_json::Value {
    let mut data = Vec::new();
    if let Err(e) = io::stdin().read_to_end(&mut data) {
        return serde_json::json!({ "error": format!("stdin: {e}") });
    }
    if blob {
        let hash = sha256_hex(&data);
        let target = shard(root, "blobs", &hash, "");
        if !target.exists() { if let Err(e) = atomic_write(&target, &data) { return serde_json::json!({ "error": e }); } }
        serde_json::json!({ "hash": hash })
    } else {
        let value: serde_json::Value = match serde_json::from_slice(&data) {
            Ok(v) => v,
            Err(e) => return serde_json::json!({ "error": format!("invalid JSON: {e}") }),
        };
        let canonical = canonical_json(&value);
        let hash = sha256_hex(canonical.as_bytes());
        let target = shard(root, "objects", &hash, ".json");
        if !target.exists() { if let Err(e) = atomic_write(&target, canonical.as_bytes()) { return serde_json::json!({ "error": e }); } }
        serde_json::json!({ "hash": hash })
    }
}

fn do_fetch(root: &str, hash: &str, blob: bool) -> serde_json::Value {
    let (ns, ext) = if blob { ("blobs", "") } else { ("objects", ".json") };
    match fs::read(shard(root, ns, hash, ext)) {
        Ok(data) => {
            // Write stored bytes directly — no re-serialization, no drift
            let _ = io::stdout().lock().write_all(&data);
            std::process::exit(0);
        }
        Err(_) => serde_json::json!({ "error": "not found" }),
    }
}
