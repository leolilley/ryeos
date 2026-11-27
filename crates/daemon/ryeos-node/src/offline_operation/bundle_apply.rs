//! `node.bundle.apply-staged`: activate a staged bundle by a signed request.
//!
//! The request names a stage by its tree digest and the bundle by name, and
//! pins the bundle set it expects the apply to leave. The bytes never ride the
//! request: they come from the node's own staging area, verified under the
//! node's signature, and the install copies and re-verifies them itself (see
//! [`OfflineHost::apply_staged_bundle`]). Only the stage record's bundle name
//! and tree digest are used, and both are re-derived from the bytes; nothing
//! else the record says is read as authority.
//!
//! Who may sign: the node's own operator key, and nobody else.
//! [`super::envelope::Envelope::check`] admits a signer in one of two ways,
//! the key whose fingerprint is the node's operator genesis (the local form,
//! which names no grant) or a key holding a verified `remote_principal` grant.
//! A key linked to the operator on another machine is the second kind: it is
//! a grant, never the local form. The operation is `local_signer_only`, and
//! the envelope refuses every grant-holder for such an operation
//! (`LocalSignerOnly`, after the scope check, and before delegation is
//! consulted), so a linked key cannot activate a stage. Delegation of this
//! operation waits for a grant that can name the exact stage.
//!
//! A refusal is a result, not a failure: the handler returns
//! `{"outcome":"refused","code":...}`, which the applier records as an
//! ordinary chained ledger receipt that consumes the request, after writing a
//! node-signed marker in the stage directory. Only a fault of the environment
//! (disk, lock, I/O) is an error, which retries at the next start.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::bundle_staging::apply::{ApplyRefusal, ApplyRefusalCode};
use crate::bundle_staging::{StagingArea, StagingRefusal};

use super::contract::is_digest;
use super::host::StagedBundleApply;
use super::table::HandlerContext;

/// Schema tag of the document whose digest `expected_bundle_set` carries.
pub const BUNDLE_SET_SCHEMA: &str = "ryeos.bundle-set/v1";

/// The arguments of `node.bundle.apply-staged`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleApplyStagedArguments {
    /// The bundle's name; must equal the staged record's.
    pub name: String,
    /// The stage's tree digest, which is also its directory name.
    pub staged_digest: String,
    /// Install over an installed bundle of that name. Without it an installed
    /// name is refused `bundle_already_installed`.
    #[serde(default)]
    pub replace: bool,
    /// The digest [`bundle_set_after`] gives for the set this apply leaves.
    /// It is signed with the request and checked before any byte is copied.
    pub expected_bundle_set: String,
}

impl BundleApplyStagedArguments {
    pub fn validate(&self) -> Result<()> {
        validate_bundle_name(&self.name)?;
        ensure!(
            is_digest(&self.staged_digest),
            "`staged_digest` is not a lowercase SHA-256 digest"
        );
        ensure!(
            is_digest(&self.expected_bundle_set),
            "`expected_bundle_set` is not a lowercase SHA-256 digest"
        );
        Ok(())
    }
}

/// A bundle name is 1 to 64 of `a-z`, `0-9`, `_` and `-`.
pub fn validate_bundle_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'),
        "bundle name `{name}` is not 1-64 lowercase letters, digits, `_` or `-`"
    );
    Ok(())
}

/// The names of the bundles the node has registered, from the registration
/// files under `.ai/node/bundles`. A node with none has an empty set.
pub fn installed_bundle_names(app_root: &Path) -> Result<BTreeSet<String>> {
    let directory = app_root.join(ryeos_engine::AI_DIR).join("node/bundles");
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BTreeSet::new());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read bundle registrations {}", directory.display()));
        }
    };
    let mut names = BTreeSet::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("read {}", directory.display()))?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            bail!("a bundle registration name is not UTF-8");
        };
        let Some(stem) = file_name.strip_suffix(".yaml") else {
            continue;
        };
        ensure!(
            entry.file_type()?.is_file(),
            "bundle registration {file_name} is not a regular file"
        );
        validate_bundle_name(stem)
            .with_context(|| format!("bundle registration {file_name} names no valid bundle"))?;
        names.insert(stem.to_owned());
    }
    Ok(names)
}

/// The digest that names the bundle set an apply leaves: the sorted names of
/// every installed bundle once `name` is among them, and the one bundle that
/// changes, by name and tree digest. A signer computes it from `bundle/list`
/// and the stage digest; it does not depend on registration timestamps.
pub fn bundle_set_after(
    installed: &BTreeSet<String>,
    name: &str,
    tree_digest: &str,
) -> Result<String> {
    let mut names = installed.clone();
    names.insert(name.to_owned());
    let document = json!({
        "schema": BUNDLE_SET_SCHEMA,
        "names": names,
        "applied": { "name": name, "tree_digest": tree_digest },
    });
    let text = lillux::canonical_json(&document)?;
    Ok(lillux::sha256_hex(text.as_bytes()))
}

/// What a node-signed policy generation says it is now, for the receipt. A
/// node with no generation reports `absent`; one whose generation cannot be
/// read reports `unreadable`: neither stops the apply, whose admission reads
/// the policy again and refuses on its own.
fn current_policy_generation(app_root: &Path) -> String {
    let directory = ryeos_node_runtime::node_policy::generation::policy_directory(app_root);
    match lillux::inspect_optional_entry_no_follow(&directory) {
        Ok(None) => return "absent".to_owned(),
        Ok(Some(_)) => {}
        Err(_) => return "unreadable".to_owned(),
    }
    let loaded = (|| -> Result<String> {
        let trust = ryeos_engine::trust::TrustStore::load_node_registry(app_root)?;
        let snapshot = ryeos_node_runtime::node_policy::load_snapshot(
            app_root,
            &trust,
            &ryeos_node_runtime::node_policy::NodePolicyTable::new(),
        )?;
        Ok(snapshot.generation_digest().to_owned())
    })();
    loaded.unwrap_or_else(|_| "unreadable".to_owned())
}

const APPLIED_NOTE: &str = "the tree digest was recomputed from the install's own copy and again after it was moved into place; signers were verified against the trust store as it stands at apply; the offer window (not_after) is not checked at apply; the stage's policy generation is recorded beside the current one and neither is compared, except that a stage in the publication lane is audited, and its lifts are derived, from the publication_lane policy as it stands at apply";

pub(super) fn bundle_apply_staged(ctx: &HandlerContext<'_>) -> Result<Value> {
    let arguments: BundleApplyStagedArguments = ctx.arguments()?;
    arguments.validate()?;
    match apply(ctx, &arguments) {
        Ok(result) => Ok(result),
        Err(error) => match ApplyRefusal::find(&error) {
            Some(refusal) => refuse(ctx, &arguments, refusal),
            None => Err(error),
        },
    }
}

fn refused_result(
    arguments: &BundleApplyStagedArguments,
    code: ApplyRefusalCode,
    detail: &str,
    marker_written: bool,
) -> Value {
    json!({
        "outcome": "refused",
        "code": code.as_str(),
        "name": arguments.name,
        "staged_digest": arguments.staged_digest,
        "detail": detail,
        "marker_written": marker_written,
        "note": "nothing was installed or registered",
    })
}

/// Record a refusal: a node-signed marker in the stage directory when the
/// stage's record verifies, then the refused result the applier chains into
/// the ledger. A stage whose record does not verify gets no marker.
fn refuse(
    ctx: &HandlerContext<'_>,
    arguments: &BundleApplyStagedArguments,
    refusal: &ApplyRefusal,
) -> Result<Value> {
    let mut marker_written = false;
    // Nothing is written for a stage the node cannot vouch for: no stage, or a
    // stage whose record (or directory) does not verify.
    let vouched = !matches!(
        refusal.code(),
        ApplyRefusalCode::StageNotFound | ApplyRefusalCode::StageRecordUnverified
    );
    let area = match open_area(ctx.app_root) {
        Ok(area) => area,
        Err(error) if ApplyRefusal::find(&error).is_some() => None,
        Err(error) => return Err(error),
    };
    if vouched && let Some(area) = area {
        match area.write_refusal_marker(
            &arguments.staged_digest,
            ctx.request_digest,
            refusal.code(),
            &ctx.request.signer_fingerprint,
            ctx.node.signing_key(),
        ) {
            Ok(_) => marker_written = true,
            // The record does not verify, or the stage is absent: there is
            // nothing the node can testify about, so it writes nothing.
            Err(StagingRefusal::Invalid(_)) => {}
            Err(fault) => {
                return Err(anyhow::Error::new(fault).context("write the refusal marker"));
            }
        }
    }
    Ok(refused_result(
        arguments,
        refusal.code(),
        refusal.detail(),
        marker_written,
    ))
}

/// Open the staging area without creating it. An area the node cannot trust
/// is a refusal; an unreadable one is a fault.
fn open_area(app_root: &Path) -> Result<Option<StagingArea>> {
    match StagingArea::open_if_present(app_root) {
        Ok(area) => Ok(area),
        Err(StagingRefusal::Invalid(reason)) => Err(anyhow::Error::new(ApplyRefusal::new(
            ApplyRefusalCode::StageRecordUnverified,
            reason,
        ))),
        Err(fault) => Err(anyhow::Error::new(fault).context("open the bundle staging area")),
    }
}

fn apply(ctx: &HandlerContext<'_>, arguments: &BundleApplyStagedArguments) -> Result<Value> {
    let verifying = ctx.node.signing_key().verifying_key();
    let area = open_area(ctx.app_root)?;

    // A rerun of a request that was already refused (the node died after the
    // marker, before the receipt) returns the same refusal.
    if let Some(area) = &area
        && let Some(marker) = area.refusal_marker(&arguments.staged_digest, &verifying)
        && marker.request_digest == ctx.request_digest
    {
        return Ok(refused_result(
            arguments,
            marker.code,
            "recorded by an earlier attempt of this request",
            true,
        ));
    }

    // The bundle set this apply leaves must be the one the signer expects,
    // before any byte is copied. The digest argument is checked here; the
    // stage itself is read below and must hash to it.
    let installed = installed_bundle_names(ctx.app_root)?;
    let after = bundle_set_after(&installed, &arguments.name, &arguments.staged_digest)?;
    if after != arguments.expected_bundle_set {
        return Err(anyhow::Error::new(ApplyRefusal::new(
            ApplyRefusalCode::BundleSetMismatch,
            format!(
                "applying this stage leaves the bundle set {after}, not the {} the request names",
                arguments.expected_bundle_set
            ),
        )));
    }

    let Some(area) = area else {
        return Err(anyhow::Error::new(ApplyRefusal::new(
            ApplyRefusalCode::StageNotFound,
            "the node has no staging area",
        )));
    };
    let stage = area.read_for_apply(&arguments.staged_digest, &verifying)?;
    // `stage.evidence` holds the evidence bytes that were verified. The lane
    // audit reads those bytes and never the files again.
    if stage.record.bundle_name != arguments.name {
        return Err(anyhow::Error::new(ApplyRefusal::new(
            ApplyRefusalCode::StageNameMismatch,
            format!(
                "the request names `{}` and the stage holds `{}`",
                arguments.name, stage.record.bundle_name
            ),
        )));
    }

    let report = ctx.host.apply_staged_bundle(StagedBundleApply {
        app_root: ctx.app_root,
        state_lock: ctx.state_lock,
        node: ctx.node,
        staged: &stage.bundle,
        evidence: &stage.evidence,
        replace: arguments.replace,
    })?;

    // The stage has been applied; failing to remove it is reported, never a
    // failure of the apply.
    let stage_removed = match area.remove(&arguments.staged_digest) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(%error, stage = %arguments.staged_digest, "applied bundle stage was not removed");
            false
        }
    };
    // A rerun that found the stage already installed did not replace anything
    // this time, so what an earlier attempt replaced is not known.
    let converged = report.get("converged") == Some(&Value::Bool(true));
    let replaced = if converged {
        Value::Null
    } else {
        report
            .get("replaced")
            .cloned()
            .unwrap_or(Value::Bool(false))
    };
    Ok(json!({
        "outcome": "applied",
        "name": arguments.name,
        "staged_digest": arguments.staged_digest,
        "replaced": replaced,
        "converged": converged,
        "bundle_set_after": after,
        "source": stage.record.source,
        "requested_by": stage.record.requested_by,
        "stage_policy_generation": stage.record.policy_generation,
        "current_policy_generation": current_policy_generation(ctx.app_root),
        "stage_removed": stage_removed,
        // What the publication lane admitted and lifted, as the node's policy
        // stood at this apply; null for a stage that is not in the lane.
        "lane": report.get("lane").cloned().unwrap_or(Value::Null),
        "note": APPLIED_NOTE,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments() -> BundleApplyStagedArguments {
        BundleApplyStagedArguments {
            name: "site".to_owned(),
            staged_digest: "a".repeat(64),
            replace: false,
            expected_bundle_set: "b".repeat(64),
        }
    }

    #[test]
    fn arguments_validate_name_and_both_digests() {
        arguments().validate().unwrap();
        let long = "x".repeat(65);
        for bad in ["", "Site", "a/b", "a.b", long.as_str()] {
            let mut args = arguments();
            args.name = bad.to_owned();
            assert!(args.validate().is_err(), "{bad}");
        }
        let upper = "A".repeat(64);
        let short = "a".repeat(63);
        for bad in ["", "sha256:aaaa", upper.as_str(), short.as_str()] {
            let mut args = arguments();
            args.staged_digest = bad.to_owned();
            assert!(args.validate().is_err(), "{bad}");
            let mut args = arguments();
            args.expected_bundle_set = bad.to_owned();
            assert!(args.validate().is_err(), "{bad}");
        }
    }

    #[test]
    fn unknown_and_missing_argument_fields_are_refused() {
        assert!(
            serde_json::from_value::<BundleApplyStagedArguments>(json!({
                "name": "site", "staged_digest": "a".repeat(64),
                "expected_bundle_set": "b".repeat(64), "extra": 1
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<BundleApplyStagedArguments>(json!({
                "name": "site", "staged_digest": "a".repeat(64)
            }))
            .is_err()
        );
        let parsed: BundleApplyStagedArguments = serde_json::from_value(json!({
            "name": "site", "staged_digest": "a".repeat(64),
            "expected_bundle_set": "b".repeat(64)
        }))
        .unwrap();
        assert!(!parsed.replace);
    }

    #[test]
    fn the_bundle_set_digest_covers_names_and_the_applied_tree() {
        let installed: BTreeSet<String> = ["alpha", "core"].map(str::to_owned).into();
        let digest = "c".repeat(64);
        let base = bundle_set_after(&installed, "site", &digest).unwrap();
        assert!(is_digest(&base));
        // Insertion order and an already-installed name do not change it.
        let reordered: BTreeSet<String> = ["core", "alpha"].map(str::to_owned).into();
        assert_eq!(base, bundle_set_after(&reordered, "site", &digest).unwrap());
        let with_site: BTreeSet<String> = ["alpha", "core", "site"].map(str::to_owned).into();
        assert_eq!(base, bundle_set_after(&with_site, "site", &digest).unwrap());
        // Another applied digest, another applied name, another installed
        // name each change it.
        assert_ne!(
            base,
            bundle_set_after(&installed, "site", &"d".repeat(64)).unwrap()
        );
        assert_ne!(
            base,
            bundle_set_after(&installed, "other", &digest).unwrap()
        );
        let more: BTreeSet<String> = ["alpha", "core", "zed"].map(str::to_owned).into();
        assert_ne!(base, bundle_set_after(&more, "site", &digest).unwrap());
        // The pinned shape: canonical JSON of the schema, sorted names and the
        // applied bundle.
        let expected = lillux::sha256_hex(
            format!(
                "{{\"applied\":{{\"name\":\"site\",\"tree_digest\":\"{digest}\"}},\"names\":[\"alpha\",\"core\",\"site\"],\"schema\":\"{BUNDLE_SET_SCHEMA}\"}}"
            )
            .as_bytes(),
        );
        assert_eq!(base, expected);
    }

    #[test]
    fn installed_names_come_from_the_registration_files() {
        let dir = tempfile::tempdir().unwrap();
        assert!(installed_bundle_names(dir.path()).unwrap().is_empty());
        let registrations = dir.path().join(".ai/node/bundles");
        std::fs::create_dir_all(&registrations).unwrap();
        std::fs::write(registrations.join("core.yaml"), b"x").unwrap();
        std::fs::write(registrations.join("site-2.yaml"), b"x").unwrap();
        std::fs::write(registrations.join(".core.yaml.tmp"), b"x").unwrap();
        let names = installed_bundle_names(dir.path()).unwrap();
        assert_eq!(
            names.into_iter().collect::<Vec<_>>(),
            ["core".to_owned(), "site-2".to_owned()]
        );
        std::fs::write(registrations.join("Bad Name.yaml"), b"x").unwrap();
        assert!(installed_bundle_names(dir.path()).is_err());
    }
}
