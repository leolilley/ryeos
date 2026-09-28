//! Observe one exact runtime member through the admitted private project-input
//! route. This native fixture is an input-delivery prerequisite for a real
//! verifier, NOT a product qualifier: it executes no subject and asserts no
//! isolation, credential exclusion, writer exclusion or lifecycle capability.
//! Startup, descriptor traversal and bounded stable reads are owned by Lillux.

use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};
use std::path::{Component, Path};

const MAX_MEMBER_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 512;
const MAX_REQUEST_BYTES: usize = 64;

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        // Never copy input bytes or private paths into public diagnostics.
        Err(_) => std::process::ExitCode::FAILURE,
    }
}

fn run() -> Result<()> {
    use lillux::invocation::{InvocationBounds, StartupInvocation};
    let bounds = InvocationBounds {
        max_arguments: 3,
        max_argument_bytes: 4096,
        max_total_argument_bytes: 8192,
        max_input_bytes: MAX_REQUEST_BYTES,
        max_output_bytes: 2048,
    };
    let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(10));
    // SAFETY: the admitted opaque one-shot launch supplies unique inherited
    // input/output pipe ends. This single-threaded entrypoint has created no
    // Rust stdio owner/task; the parent retains only the opposite pipe ends.
    let mut invocation =
        unsafe { StartupInvocation::take_inherited_pipes(0, 1, bounds, deadline) }?;
    ensure!(
        invocation.arguments().len() == 3,
        "expected exact member and digest arguments"
    );
    let member = invocation.arguments()[1]
        .to_str()
        .context("member is not UTF-8")?
        .to_owned();
    let expected = invocation.arguments()[2]
        .to_str()
        .context("digest is not UTF-8")?
        .to_owned();
    let input = invocation.read_input()?;
    validate_request(&member, &expected, &input)?;
    // cwd is daemon-owned private input scratch for this logically Projectless
    // execution, not project authority. No host project locator or environment
    // variable selects it, and member traversal cannot follow symlinks.
    let directory =
        lillux::PinnedDirectory::open(Path::new("."))?.context("private input scratch absent")?;
    let observation = observe(&directory, &member, &expected, &input)?;
    let mut output = serde_json::to_vec(&observation)?;
    output.push(b'\n');
    invocation.write_output(&output)?;
    Ok(())
}

fn validate_request<'a>(member: &'a str, expected: &str, input: &[u8]) -> Result<&'a Path> {
    ensure!(
        !member.is_empty()
            && member.len() <= MAX_PATH_BYTES
            && !member.contains('\\')
            && !member.chars().any(char::is_control),
        "invalid member path"
    );
    ensure!(
        member
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".."),
        "member must be normalized"
    );
    let path = Path::new(member);
    ensure!(
        !path.is_absolute()
            && path
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "member must be relative"
    );
    ensure!(
        expected.len() == 64
            && expected
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "expected digest must be canonical lowercase SHA-256"
    );
    ensure!(
        !input.is_empty() && input.len() <= MAX_REQUEST_BYTES,
        "input exceeds bound"
    );
    let request: Value = serde_json::from_slice(input).context("expected empty JSON object")?;
    ensure!(
        request.as_object().is_some_and(|object| object.is_empty()),
        "expected only an empty JSON object"
    );
    Ok(path)
}

fn observe(
    directory: &lillux::PinnedDirectory,
    member: &str,
    expected: &str,
    input: &[u8],
) -> Result<Value> {
    let member_path = validate_request(member, expected, input)?;
    let file = directory
        .open_pinned_regular_descendant(member_path, false)?
        .context("admitted runtime member absent")?;
    let prior = file.observation()?;
    ensure!(
        prior.size() <= MAX_MEMBER_BYTES,
        "runtime member exceeds bound"
    );
    let bytes = file.read_stable_bounded(&prior, MAX_MEMBER_BYTES)?;
    let hash = lillux::sha256_hex(&bytes);
    ensure!(
        hash == expected,
        "runtime member differs from expected digest"
    );
    Ok(json!({
        "schema":"test.runtime_input_observation.v1",
        "member":member,
        "sha256":hash,
        "bytes":bytes.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_noncanonical_paths_hashes_and_nonempty_requests() {
        let hash = "a".repeat(64);
        assert!(validate_request("inputs/runtime", &hash, b"{}").is_ok());
        for member in [
            "",
            "/abs",
            "../parent",
            "a/../b",
            "./a",
            "a//b",
            "a/",
            "a\\b",
            "a\nb",
        ] {
            assert!(
                validate_request(member, &hash, b"{}").is_err(),
                "{member:?}"
            );
        }
        assert!(validate_request(&"a".repeat(513), &hash, b"{}").is_err());
        for bad in ["A".repeat(64), "g".repeat(64), "a".repeat(63)] {
            assert!(validate_request("runtime", &bad, b"{}").is_err());
        }
        for request in [
            b"".as_slice(),
            b"null",
            b"[]",
            b"{\"ignored\":true}",
            b"{}{}",
        ] {
            assert!(validate_request("runtime", &hash, request).is_err());
        }
        assert!(validate_request("runtime", &hash, &vec![b' '; 65]).is_err());
    }

    #[test]
    fn observes_exact_bytes_and_refuses_missing_mismatch_or_nonregular() {
        // Harness-only fixture setup. Product reads use only Lillux above.
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("inputs")).unwrap();
        std::fs::write(root.path().join("inputs/runtime"), b"exact runtime input").unwrap();
        let directory = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let hash = lillux::sha256_hex(b"exact runtime input");
        assert_eq!(
            observe(&directory, "inputs/runtime", &hash, b"{}").unwrap(),
            json!({
                "schema":"test.runtime_input_observation.v1", "member":"inputs/runtime",
                "sha256":hash, "bytes":19,
            })
        );
        assert!(observe(&directory, "inputs/missing", &hash, b"{}").is_err());
        assert!(observe(&directory, "inputs/runtime", &"0".repeat(64), b"{}").is_err());
        assert!(observe(&directory, "inputs", &hash, b"{}").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_leaf_and_ancestor_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("runtime"), b"outside").unwrap();
        std::os::unix::fs::symlink(outside.path().join("runtime"), root.path().join("leaf"))
            .unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("ancestor")).unwrap();
        let directory = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let hash = lillux::sha256_hex(b"outside");
        assert!(observe(&directory, "leaf", &hash, b"{}").is_err());
        assert!(observe(&directory, "ancestor/runtime", &hash, b"{}").is_err());
    }

    #[test]
    fn refuses_oversized_regular_file_before_reading() {
        let root = tempfile::tempdir().unwrap();
        std::fs::File::create(root.path().join("large"))
            .unwrap()
            .set_len(MAX_MEMBER_BYTES + 1)
            .unwrap();
        let directory = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        assert!(observe(&directory, "large", &"0".repeat(64), b"{}").is_err());
    }
}
