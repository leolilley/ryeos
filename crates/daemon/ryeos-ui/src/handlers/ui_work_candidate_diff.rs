//! The diff of one changed file of a retained candidate.
//!
//! The request names a placement thread and a project-relative path, nothing
//! else. The daemon re-derives the candidate's base and candidate snapshots
//! for the seat caller, looks the path up in both file trees, reads the two
//! contents from the CAS under a size bound, and returns the change as one
//! bounded, sanitised change set whose basis is `recorded`: it is derived
//! from retained content, not from anything a worker or a client said. No
//! path is ever opened on disk, so a path that is not in the trees is simply
//! not a change.

use std::sync::Arc;

use anyhow::{Context, Result};
use ryeos_changeset::{Basis, Caps, ChangeSet, MAX_EDIT_BYTES, Op, Truncated, TruncatedReason};
use serde::Deserialize;
use serde_json::{Value, json};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_state::objects::ProjectTree;

use super::ui_work::{RetainedCandidate, candidate_trees, retained_candidate};

const CONFIG_REF: &str = "config:ryeos/ui/candidate-diff";
const CONFIG_PATH: &str = "ryeos/ui/candidate-diff.yaml";

/// Neither side of a file is read past this, whatever the config says: it is
/// the most text the change-set builder diffs.
const MAX_FILE_BYTES_CEILING: u64 = MAX_EDIT_BYTES as u64;

/// How far a review diff may grow. Signed deployment policy.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateDiffConfig {
    /// A file larger than this on either side is reported by size, not read.
    pub max_file_bytes: u64,
    /// The caps of the returned change set.
    pub caps: Caps,
}

impl CandidateDiffConfig {
    /// Parse a verified config body. The envelope fields (`category`,
    /// `version`, `description`) are dropped first; any other unknown field
    /// is refused.
    pub fn from_value(mut value: Value) -> Result<Self, String> {
        if let Some(object) = value.as_object_mut() {
            for envelope in ["category", "version", "description"] {
                object.remove(envelope);
            }
        }
        let mut config: Self = serde_json::from_value(value).map_err(|error| error.to_string())?;
        config.max_file_bytes = config.max_file_bytes.min(MAX_FILE_BYTES_CEILING);
        Ok(config)
    }
}

/// One side of a changed file as the CAS holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Side {
    /// The file does not exist on this side.
    Absent,
    Text(String),
    /// Content that is not text, with its size.
    Binary(u64),
    /// Content over the size bound, with its size; it was not read.
    TooLarge(u64),
}

impl Side {
    fn bytes(&self) -> u64 {
        match self {
            Side::Absent => 0,
            Side::Text(text) => text.len() as u64,
            Side::Binary(bytes) | Side::TooLarge(bytes) => *bytes,
        }
    }
}

/// Reads the content behind one tree entry. The file object's recorded size
/// is checked before any content is read.
fn read_side(cas: &lillux::CasStore, file_hash: &str, max_bytes: u64) -> Result<Side> {
    let file = ryeos_state::project_materialization::load_project_file_bounded(cas, file_hash)?
        .context("retained candidate file object is missing")?;
    if file.size > max_bytes {
        return Ok(Side::TooLarge(file.size));
    }
    let bytes = cas
        .get_blob_bounded(&file.blob_hash, max_bytes)?
        .context("retained candidate file content is missing")?;
    Ok(match String::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => Side::Text(text),
        Ok(text) => Side::Binary(text.len() as u64),
        Err(error) => Side::Binary(error.as_bytes().len() as u64),
    })
}

/// What the request asked for.
enum Found {
    /// The path is the same in both trees, or in neither.
    NotChanged,
    /// The path is a change but cannot be shown as text a reader may trust.
    NotShown,
    Change(ChangeSet),
}

/// The change of `path` between the two trees. `read` returns the content
/// behind a tree entry's file hash; it is only ever given hashes taken from
/// the trees.
fn file_change(
    base_tree: &ProjectTree,
    candidate_tree: &ProjectTree,
    path: &str,
    caps: &Caps,
    mut read: impl FnMut(&str) -> Result<Side>,
) -> Result<Found> {
    let (base_hash, candidate_hash) = (base_tree.files.get(path), candidate_tree.files.get(path));
    let op = match (base_hash, candidate_hash) {
        (None, None) => return Ok(Found::NotChanged),
        (Some(base), Some(candidate)) if base == candidate => return Ok(Found::NotChanged),
        (None, Some(_)) => Op::Add,
        (Some(_), None) => Op::Delete,
        (Some(_), Some(_)) => Op::Modify,
    };
    let mut side = |hash: Option<&String>| match hash {
        Some(hash) => read(hash),
        None => Ok(Side::Absent),
    };
    let (before, after) = (side(base_hash)?, side(candidate_hash)?);
    let too_large = |side: &Side| matches!(side, Side::TooLarge(_));
    let binary = |side: &Side| matches!(side, Side::Binary(_));
    let text = |side: &Side| match side {
        Side::Text(text) => Some(text.clone()),
        _ => None,
    };
    let set = if too_large(&before) || too_large(&after) {
        ChangeSet::opaque(
            path,
            op,
            Basis::Recorded,
            false,
            Some(Truncated {
                reason: TruncatedReason::TooLarge,
                omitted_lines: 0,
                old_bytes: Some(before.bytes()),
                new_bytes: Some(after.bytes()),
            }),
        )
    } else if binary(&before) || binary(&after) {
        ChangeSet::opaque(path, op, Basis::Recorded, true, None)
    } else {
        ChangeSet::from_contents(
            path,
            text(&before).as_deref(),
            text(&after).as_deref(),
            Basis::Recorded,
            caps,
        )
    };
    Ok(set.map_or(Found::NotShown, Found::Change))
}

fn response(state: &str, reason: &str, set: Option<&ChangeSet>) -> Result<Value> {
    Ok(json!({
        "schema_version": "ryeos.ui.work_candidate_diff.v1",
        "availability": [{"state": state, "reason": reason}],
        "basis": Basis::Recorded,
        "files": match set {
            Some(set) => serde_json::to_value(&set.files)?,
            None => json!([]),
        },
        "omitted_files": set.map_or(0, |set| set.omitted_files),
    }))
}

fn load_config(state: &AppState) -> Result<CandidateDiffConfig> {
    let value = super::ui_attention::load_verified_config(state, CONFIG_PATH, CONFIG_REF)?;
    Ok(CandidateDiffConfig::from_value(value)
        .map_err(|error| HandlerError::Internal(format!("{CONFIG_REF}: {error}")))?)
}

/// The project-relative path the request names. Any other field of the
/// request (a hash a client might add) is never read.
fn requested_path(params: &Value) -> Result<&str, HandlerError> {
    params
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| HandlerError::BadRequest("path is required".into()))
}

pub async fn handle(params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let path = requested_path(&params)?;
    let (base_snapshot_hash, candidate_snapshot_hash) =
        match retained_candidate(&params, &ctx, &state)? {
            RetainedCandidate::Unavailable { state, reason } => {
                return response(state, reason, None);
            }
            RetainedCandidate::Retained {
                base_snapshot_hash,
                candidate_snapshot_hash,
                ..
            } => (base_snapshot_hash, candidate_snapshot_hash),
        };
    let config = load_config(&state)?;
    let cas_read = state.acquire_cas_read()?;
    let cas = cas_read.cas();
    let (base_tree, candidate_tree) =
        candidate_trees(cas, &base_snapshot_hash, &candidate_snapshot_hash)?;
    let found = file_change(&base_tree, &candidate_tree, path, &config.caps, |hash| {
        read_side(cas, hash, config.max_file_bytes)
    })?;
    match found {
        Found::NotChanged => response("unavailable", "path_not_changed", None),
        Found::NotShown => response("unavailable", "path_not_shown", None),
        Found::Change(set) => response("available", "candidate_retained", Some(&set)),
    }
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/work/candidate/diff",
    endpoint: "ui.ryeos.work.candidate.diff",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn hash(digit: char) -> String {
        digit.to_string().repeat(64)
    }

    fn tree(files: &[(&str, char)]) -> ProjectTree {
        ProjectTree {
            files: files
                .iter()
                .map(|(path, digit)| ((*path).to_string(), hash(*digit)))
                .collect(),
        }
    }

    fn config() -> CandidateDiffConfig {
        let text = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../../bundles/ryeos-ui/.ai/config/ryeos/ui/candidate-diff.yaml"),
        )
        .unwrap();
        CandidateDiffConfig::from_value(serde_yaml::from_str(&text).unwrap()).unwrap()
    }

    /// The change of `path` with contents served by file hash; returns the
    /// response and the hashes that were read.
    fn change(
        base: &ProjectTree,
        candidate: &ProjectTree,
        path: &str,
        contents: &[(char, Side)],
    ) -> (Value, Vec<String>) {
        let contents: BTreeMap<String, Side> = contents
            .iter()
            .map(|(digit, side)| (hash(*digit), side.clone()))
            .collect();
        let mut read = Vec::new();
        let found = file_change(base, candidate, path, &config().caps, |file_hash| {
            read.push(file_hash.to_string());
            contents
                .get(file_hash)
                .cloned()
                .context("a hash outside the trees was read")
        })
        .unwrap();
        let value = match found {
            Found::NotChanged => response("unavailable", "path_not_changed", None),
            Found::NotShown => response("unavailable", "path_not_shown", None),
            Found::Change(set) => response("available", "candidate_retained", Some(&set)),
        }
        .unwrap();
        (value, read)
    }

    fn text(content: &str) -> Side {
        Side::Text(content.to_string())
    }

    #[test]
    fn a_modified_file_is_a_recorded_change_with_line_numbers() {
        let base = tree(&[("src/a.rs", '1'), ("same.rs", '3')]);
        let candidate = tree(&[("src/a.rs", '2'), ("same.rs", '3')]);
        let (value, read) = change(
            &base,
            &candidate,
            "src/a.rs",
            &[
                ('1', text("one\ntwo\nthree\n")),
                ('2', text("one\n2\nthree\n")),
            ],
        );
        assert_eq!(
            value,
            json!({
                "schema_version": "ryeos.ui.work_candidate_diff.v1",
                "availability": [{"state": "available", "reason": "candidate_retained"}],
                "basis": "recorded",
                "omitted_files": 0,
                "files": [{
                    "path": "src/a.rs", "op": "modify", "added": 1, "removed": 1,
                    "binary": false,
                    "hunks": [{"old_start": 1, "new_start": 1,
                               "lines": [" one", "-two", "+2", " three"]}]}],
            })
        );
        assert_eq!(read, [hash('1'), hash('2')]);
    }

    #[test]
    fn added_and_deleted_files_have_one_side() {
        let base = tree(&[("gone.txt", '1')]);
        let candidate = tree(&[("new.txt", '2')]);
        let (added, read) = change(&base, &candidate, "new.txt", &[('2', text("a\n"))]);
        assert_eq!(added["files"][0]["op"], json!("add"));
        assert_eq!(added["files"][0]["hunks"][0]["lines"], json!(["+a"]));
        assert_eq!(read, [hash('2')]);
        let (deleted, read) = change(&base, &candidate, "gone.txt", &[('1', text("a\n"))]);
        assert_eq!(deleted["files"][0]["op"], json!("delete"));
        assert_eq!(deleted["files"][0]["hunks"][0]["lines"], json!(["-a"]));
        assert_eq!(read, [hash('1')]);
    }

    /// A path that is not a change of this candidate reads nothing, whatever
    /// it looks like: no path is ever opened on disk.
    #[test]
    fn a_path_that_is_not_a_change_reads_nothing() {
        let base = tree(&[("same.rs", '3'), ("a.rs", '1')]);
        let candidate = tree(&[("same.rs", '3'), ("a.rs", '2')]);
        for path in [
            "same.rs",
            "missing.rs",
            "../a.rs",
            "/etc/passwd",
            "./a.rs",
            "a.rs/",
            "A.RS",
        ] {
            let (value, read) = change(&base, &candidate, path, &[]);
            assert_eq!(
                value["availability"],
                json!([{"state": "unavailable", "reason": "path_not_changed"}]),
                "{path}"
            );
            assert_eq!(value["files"], json!([]), "{path}");
            assert!(read.is_empty(), "{path}");
        }
    }

    /// The request carries a thread and a path. Hashes a client adds are not
    /// read: both contents come from the trees the daemon derived.
    #[test]
    fn hashes_supplied_by_the_client_are_ignored() {
        let params = json!({
            "thread_id": "T-1", "path": "a.rs",
            "base_file_hash": hash('8'), "candidate_file_hash": hash('9'),
            "base_snapshot_hash": hash('8'), "candidate_snapshot_hash": hash('9'),
        });
        assert_eq!(requested_path(&params).unwrap(), "a.rs");
        let base = tree(&[("a.rs", '1')]);
        let candidate = tree(&[("a.rs", '2')]);
        let (value, read) = change(
            &base,
            &candidate,
            requested_path(&params).unwrap(),
            &[
                ('1', text("old\n")),
                ('2', text("new\n")),
                ('8', text("planted before\n")),
                ('9', text("planted after\n")),
            ],
        );
        assert_eq!(read, [hash('1'), hash('2')]);
        assert_eq!(
            value["files"][0]["hunks"][0]["lines"],
            json!(["-old", "+new"])
        );
        assert!(requested_path(&json!({"thread_id": "T-1"})).is_err());
        assert!(requested_path(&json!({"thread_id": "T-1", "path": ""})).is_err());
        assert!(requested_path(&json!({"thread_id": "T-1", "path": 7})).is_err());
    }

    #[test]
    fn a_file_over_the_size_bound_is_reported_by_size_and_not_diffed() {
        let base = tree(&[("big.bin", '1')]);
        let candidate = tree(&[("big.bin", '2')]);
        let (value, _) = change(
            &base,
            &candidate,
            "big.bin",
            &[('1', text("small\n")), ('2', Side::TooLarge(9_000_000))],
        );
        assert_eq!(
            value["files"][0],
            json!({"path": "big.bin", "op": "modify", "added": 0, "removed": 0,
                   "binary": false, "hunks": [],
                   "truncated": {"reason": "too_large", "old_bytes": 6,
                                 "new_bytes": 9_000_000}})
        );
    }

    #[test]
    fn a_binary_file_is_marked_and_has_no_hunks() {
        let base = tree(&[("img.png", '1')]);
        let candidate = tree(&[("img.png", '2')]);
        let (value, _) = change(
            &base,
            &candidate,
            "img.png",
            &[('1', Side::Binary(10)), ('2', Side::Binary(12))],
        );
        assert_eq!(
            value["files"][0],
            json!({"path": "img.png", "op": "modify", "added": 0, "removed": 0,
                   "binary": true, "hunks": []})
        );
    }

    #[test]
    fn hostile_content_is_sanitised_and_bounded_by_the_signed_caps() {
        let base = tree(&[("a.txt", '1')]);
        let candidate = tree(&[("a.txt", '2')]);
        let long = "y".repeat(5_000);
        let after: String = (0..5_000)
            .map(|index| format!("{index}\u{1b}[31m{long}\u{202e}\n"))
            .take(40)
            .collect();
        let (value, _) = change(
            &base,
            &candidate,
            "a.txt",
            &[
                ('1', text("\u{1b}]0;title\u{07}before\u{0}\n")),
                ('2', text(&after)),
            ],
        );
        let drawn = value.to_string();
        assert!(!drawn.contains("\\u001b") && !drawn.contains('\u{202e}'));
        assert!(drawn.len() <= ryeos_changeset::MAX_BLOCK_BYTES + 512);
        let lines = value["files"][0]["hunks"][0]["lines"].as_array().unwrap();
        assert_eq!(lines[0], json!("-before"));
        let caps = config().caps;
        assert!(
            lines
                .iter()
                .all(|line| line.as_str().unwrap().chars().count() <= caps.line_chars + 2)
        );
        assert_eq!(value["files"][0]["added"], json!(40));
    }

    #[test]
    fn the_signed_config_states_the_caps_and_is_held_under_the_ceiling() {
        let shipped = config();
        assert_eq!(shipped.max_file_bytes, 262_144);
        assert_eq!(
            shipped.caps,
            Caps {
                files: 1,
                lines_per_file: 2_000,
                total_lines: 2_000,
                line_chars: 400
            }
        );
        let wide = CandidateDiffConfig::from_value(json!({
            "category": "ryeos/ui", "version": "1.0.0", "description": "x",
            "max_file_bytes": u64::MAX,
            "caps": {"files": 1, "lines_per_file": 10, "total_lines": 10, "line_chars": 10},
        }))
        .unwrap();
        assert_eq!(wide.max_file_bytes, MAX_FILE_BYTES_CEILING);
        assert!(
            CandidateDiffConfig::from_value(json!({"max_file_bytes": 1, "caps": {}, "extra": 1}))
                .is_err()
        );
    }

    #[test]
    fn content_is_read_from_the_cas_under_the_size_bound() {
        let root = tempfile::tempdir().unwrap();
        let cas = lillux::CasStore::new(root.path().to_path_buf());
        let store = |content: &[u8]| {
            let blob_hash = cas.store_blob(content).unwrap();
            let file = ryeos_state::objects::ProjectFile {
                blob_hash,
                size: content.len() as u64,
                normalized_mode: ryeos_state::objects::ProjectFile::REGULAR_MODE,
            };
            cas.store_object(&file.to_value()).unwrap()
        };
        let text_file = store(b"hello\n");
        let binary_file = store(&[0xff, 0xfe, 0x00, 0x01]);
        let nul_file = store(b"a\0b");
        assert_eq!(read_side(&cas, &text_file, 64).unwrap(), text("hello\n"));
        assert_eq!(read_side(&cas, &binary_file, 64).unwrap(), Side::Binary(4));
        assert_eq!(read_side(&cas, &nul_file, 64).unwrap(), Side::Binary(3));
        assert_eq!(read_side(&cas, &text_file, 5).unwrap(), Side::TooLarge(6));
        assert!(read_side(&cas, &hash('7'), 64).is_err());
    }
}
