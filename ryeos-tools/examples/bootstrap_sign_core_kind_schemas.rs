//! Bootstrap sign core kind schemas + parser tools — one-shot maintainer script.
//!
//! Signs:
//!   1. `*.kind-schema.yaml` under `<source>/.ai/node/engine/kinds/`
//!   2. `*.yaml` under `<source>/.ai/parsers/`
//!
//! Both must be signed before `rye-bundle-tool sign-items` can load the
//! parser registry (which verifies parser tool signatures at load time).
//! This is the bootstrap break in the signing chain.
//!
//! Usage:
//!   cargo run --example bootstrap_sign_core_kind_schemas -p ryeos-tools -- \
//!       --source <core-bundle-root> --key <author.pem>
//!   cargo run --example bootstrap_sign_core_kind_schemas -p ryeos-tools -- \
//!       --source <core-bundle-root> --seed <0..=255>

use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Parser;
use lillux::crypto::{DecodePrivateKey, SigningKey};
use serde::Serialize;

#[derive(Parser)]
struct Args {
    /// Core bundle root (directory containing `.ai/`).
    #[arg(long)]
    source: PathBuf,

    /// Path to a PEM-encoded Ed25519 signing key. Mutually exclusive with --seed.
    #[arg(long, conflicts_with = "seed")]
    key: Option<PathBuf>,

    /// Deterministic signing key seed byte (0..=255). Mutually exclusive with --key.
    #[arg(long)]
    seed: Option<u8>,
}

#[derive(Serialize)]
struct Report {
    kind_schemas_signed: Vec<String>,
    parsers_signed: Vec<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let signing_key = match (args.key.as_deref(), args.seed) {
        (Some(_), Some(_)) => bail!("pass either --key or --seed, not both"),
        (Some(p), None) => {
            let pem = fs::read_to_string(p)
                .with_context(|| format!("failed to read key {}", p.display()))?;
            SigningKey::from_pkcs8_pem(&pem)
                .with_context(|| format!("failed to decode key {}", p.display()))?
        }
        (None, Some(s)) => SigningKey::from_bytes(&[s; 32]),
        (None, None) => bail!("--key <pem> or --seed <0..=255> is required"),
    };

    // ── Phase 1: Sign kind schemas ──
    let kinds_dir = args
        .source
        .join(".ai")
        .join("node")
        .join("engine")
        .join("kinds");

    if !kinds_dir.is_dir() {
        bail!(
            "source is not a core bundle — no kind schemas at {}",
            kinds_dir.display()
        );
    }

    let mut kind_schemas_signed: Vec<String> = Vec::new();

    let entries = fs::read_dir(&kinds_dir)
        .with_context(|| format!("failed to read {}", kinds_dir.display()))?;

    for entry in entries {
        let entry = entry?;
        let kind_name = entry.file_name();
        let kind_name_str = kind_name.to_string_lossy();
        let kind_dir = entry.path();

        if !kind_dir.is_dir() {
            continue;
        }

        let schema_file = kind_dir.join(format!("{kind_name_str}.kind-schema.yaml"));

        if !schema_file.is_file() {
            let other_files: Vec<String> = fs::read_dir(&kind_dir)?
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .map(|e| {
                    e.path()
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string()
                })
                .collect();

            if !other_files.is_empty() {
                bail!(
                    "unexpected files in kinds/{}: {:?} — only {}.kind-schema.yaml is allowed",
                    kind_name_str,
                    other_files,
                    kind_name_str
                );
            }
            continue;
        }

        let content = fs::read_to_string(&schema_file)
            .with_context(|| format!("read {}", schema_file.display()))?;
        let stripped = lillux::signature::strip_signature_lines(&content);
        let parsed: serde_yaml::Value = serde_yaml::from_str(&stripped)
            .with_context(|| format!("parse YAML {}", schema_file.display()))?;

        if let Some(cat) = parsed.get("category").and_then(|v| v.as_str()) {
            let expected_cat = format!("engine/kinds/{kind_name_str}");
            if cat != expected_cat {
                bail!(
                    "category mismatch in {}: expected '{}', got '{}'",
                    schema_file.display(),
                    expected_cat,
                    cat
                );
            }
        } else {
            bail!("missing category field in {}", schema_file.display());
        }

        sign_file_in_place(&schema_file, &content, &signing_key)?;
        kind_schemas_signed.push(format!("engine/kinds/{kind_name_str}"));
        eprintln!("signed: {kind_name_str}.kind-schema.yaml");
    }

    // ── Phase 2: Sign parser tool descriptors ──
    let parsers_dir = args.source.join(".ai").join("parsers");
    let mut parsers_signed: Vec<String> = Vec::new();

    if parsers_dir.is_dir() {
        let mut yaml_files: Vec<PathBuf> = Vec::new();
        collect_yaml_files(&parsers_dir, &mut yaml_files);

        for yaml_file in &yaml_files {
            let content = fs::read_to_string(yaml_file)
                .with_context(|| format!("read {}", yaml_file.display()))?;
            sign_file_in_place(yaml_file, &content, &signing_key)?;

            let rel = yaml_file.strip_prefix(&parsers_dir).unwrap_or(yaml_file);
            parsers_signed.push(rel.to_string_lossy().to_string());
            eprintln!("signed parser: {}", rel.display());
        }
    }

    kind_schemas_signed.sort();
    parsers_signed.sort();
    println!(
        "{}",
        serde_json::to_string_pretty(&Report {
            kind_schemas_signed,
            parsers_signed,
        })?
    );
    Ok(())
}

fn sign_file_in_place(path: &PathBuf, content: &str, signing_key: &SigningKey) -> Result<()> {
    let body = lillux::signature::strip_signature_lines(content);
    let signed = lillux::signature::sign_content(&body, signing_key, "#", None);
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, &signed).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn collect_yaml_files(dir: &PathBuf, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_yaml_files(&path, out);
        } else if path.is_file() {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext == "yaml" || ext == "yml" {
                out.push(path);
            }
        }
    }
}
