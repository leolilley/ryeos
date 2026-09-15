use std::fmt::Write as _;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest as _, Sha256};

const MAX_PRODUCT_BYTES: u64 = 128;

#[derive(Serialize)]
struct QualificationResult {
    schema: &'static str,
    subject_manifest_hash: String,
    claims: [&'static str; 1],
    probe_evidence: ProbeEvidence,
}

#[derive(Serialize)]
struct ProbeEvidence {
    auxiliary_manifest_hash: String,
    network_contacted: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("fixture dynamic-product verifier refused input: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    let root = arguments
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| "one absolute project root is required".to_owned())?;
    if arguments.next().is_some() || !root.is_absolute() {
        return Err("one absolute project root is required".to_owned());
    }

    let subject = read_bounded_regular(&root.join("qualification/subject"))?;
    let auxiliary = read_bounded_regular(&root.join("qualification/auxiliary"))?;
    let token = subject
        .strip_suffix(b"\n")
        .ok_or_else(|| "subject must end in exactly one newline".to_owned())?;
    if token.is_empty()
        || token.len() > 32
        || !token[0].is_ascii_lowercase()
        || token[1..]
            .iter()
            .any(|byte| !byte.is_ascii_lowercase() && *byte != b'-')
    {
        return Err("subject is not one bounded lowercase token".to_owned());
    }
    let mut expected_auxiliary = Vec::with_capacity(token.len() + 5);
    expected_auxiliary.extend_from_slice(token);
    expected_auxiliary.extend_from_slice(b"-aux\n");
    if auxiliary != expected_auxiliary {
        return Err("auxiliary does not match the admitted subject".to_owned());
    }

    let result = QualificationResult {
        schema: "ryeos.product_qualification_result.v1",
        subject_manifest_hash: file_manifest_hash(&subject),
        claims: ["bounded_payload_pair"],
        probe_evidence: ProbeEvidence {
            auxiliary_manifest_hash: file_manifest_hash(&auxiliary),
            network_contacted: false,
        },
    };
    let encoded = serde_json::to_vec(&result).map_err(|error| error.to_string())?;
    std::io::stdout()
        .write_all(&encoded)
        .and_then(|()| std::io::stdout().write_all(b"\n"))
        .map_err(|error| error.to_string())
}

fn read_bounded_regular(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.len() == 0 || metadata.len() > MAX_PRODUCT_BYTES
    {
        return Err(format!(
            "{} is not one non-empty bounded regular file",
            path.display()
        ));
    }
    let mut content = Vec::with_capacity(metadata.len() as usize);
    File::open(path)
        .and_then(|file| file.take(MAX_PRODUCT_BYTES + 1).read_to_end(&mut content))
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if content.len() as u64 != metadata.len() {
        return Err(format!("{} changed while it was inspected", path.display()));
    }
    Ok(content)
}

fn file_manifest_hash(content: &[u8]) -> String {
    let blob_hash = hex_digest(content);
    // This is the canonical v2 one-file Product manifest. All object keys are
    // in lexical order and every interpolated value is an integer or a
    // lowercase SHA-256 digest, so no escaping or locale behavior is involved.
    let manifest = format!(
        "{{\"entries\":[{{\"blob_hash\":\"{blob_hash}\",\"kind\":\"file\",\"mode\":420,\"path\":\"content\",\"size\":{size}}}],\"entry_count\":1,\"kind\":\"external_content_manifest\",\"schema\":\"ryeos.external_content.tree.v2\",\"total_bytes\":{size}}}",
        size = content.len()
    );
    hex_digest(manifest.as_bytes())
}

fn hex_digest(content: &[u8]) -> String {
    let digest = Sha256::digest(content);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}
