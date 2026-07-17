//! Runtime-authority audit between two bundle manifests.
//!
//! Re-signing a manifest re-asserts every authority it declares; the useful
//! review question is "what authority changed", not "what bytes changed". This
//! action parses two manifests and reports ONLY the runtime-authority delta, in
//! grant terms, ordered by risk (a new wildcard authoring pattern first, removed
//! grants last), so a re-sign campaign reviews as authority deltas rather than
//! YAML diffs.
//!
//! Operates purely on two manifest files — no daemon, no signing key. Each path
//! may be a generated `.ai/manifest.yaml` (signature header tolerated) or a
//! `.ai/manifest.source.yaml`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};

use ryeos_bundle::manifest::{
    BundleEventOperation, BundleManifest, BundleManifestSource, RuntimeVaultOperation,
};

/// Relative risk of a single authority change. Ordered highest-to-lowest so a
/// sort by `risk` surfaces new wildcard authoring patterns first and removed
/// grants last, matching how a reviewer wants to read the delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    /// A newly granted item-authoring pattern that is a wildcard.
    NewWildcard,
    /// An existing grant broadened (more operations/verbs).
    Widened,
    /// A grant added (non-wildcard authoring pattern, event kind, vault
    /// namespace, or provides/requires/uses kind).
    Added,
    /// An existing grant narrowed (fewer operations/verbs).
    Narrowed,
    /// A grant removed entirely.
    Removed,
}

impl Risk {
    /// Fixed-width marker for the rendered report.
    fn marker(self) -> &'static str {
        match self {
            Risk::NewWildcard => "!! NEW WILDCARD",
            Risk::Widened => ">> WIDENED     ",
            Risk::Added => "++ ADDED       ",
            Risk::Narrowed => "<< NARROWED    ",
            Risk::Removed => "-- REMOVED     ",
        }
    }
}

/// One authority-delta line: a risk rank plus a self-describing grant message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditFinding {
    pub risk: Risk,
    pub message: String,
}

/// The full runtime-authority delta between two manifests.
#[derive(Debug, Clone)]
pub struct AuthorityAudit {
    pub old_label: String,
    pub new_label: String,
    pub findings: Vec<AuditFinding>,
}

impl AuthorityAudit {
    /// Render a paste-into-a-review report, findings ordered by risk.
    pub fn render(&self) -> String {
        let mut out = format!(
            "runtime-authority audit: {} -> {}\n",
            self.old_label, self.new_label
        );
        if self.findings.is_empty() {
            out.push_str("\nNo runtime-authority changes between the two manifests.\n");
            return out;
        }
        let mut findings = self.findings.clone();
        findings.sort_by(|a, b| a.risk.cmp(&b.risk).then_with(|| a.message.cmp(&b.message)));
        out.push('\n');
        for f in &findings {
            out.push_str(f.risk.marker());
            out.push(' ');
            out.push_str(&f.message);
            out.push('\n');
        }
        out
    }
}

/// Snake-case wire form of a bundle-event operation (matches the manifest serde
/// representation), so operation sets compare by value without needing `Ord`.
fn event_op_str(op: &BundleEventOperation) -> &'static str {
    match op {
        BundleEventOperation::Append => "append",
        BundleEventOperation::Scan => "scan",
    }
}

/// Snake-case wire form of a runtime-vault operation (verb).
fn vault_op_str(op: &RuntimeVaultOperation) -> &'static str {
    match op {
        RuntimeVaultOperation::Put => "put",
        RuntimeVaultOperation::Get => "get",
        RuntimeVaultOperation::Delete => "delete",
        RuntimeVaultOperation::List => "list",
    }
}

/// True when an item-authoring pattern grants over a wildcard kind or namespace.
fn is_wildcard_pattern(kind: &str, namespace: &str) -> bool {
    kind.contains('*') || namespace.contains('*')
}

fn join(set: &BTreeSet<&str>) -> String {
    set.iter().copied().collect::<Vec<_>>().join(", ")
}

/// Diff two manifests' runtime-authority (and provides/requires/uses kinds),
/// producing an unordered finding list. `AuthorityAudit::render` orders it.
pub fn diff_authority(old: &BundleManifest, new: &BundleManifest) -> Vec<AuditFinding> {
    let mut findings: Vec<AuditFinding> = Vec::new();

    // ── item_authoring ──
    let old_auth: BTreeSet<(&str, &str)> = old
        .runtime_authority
        .item_authoring
        .iter()
        .map(|d| (d.kind.as_str(), d.namespace.as_str()))
        .collect();
    let new_auth: BTreeSet<(&str, &str)> = new
        .runtime_authority
        .item_authoring
        .iter()
        .map(|d| (d.kind.as_str(), d.namespace.as_str()))
        .collect();
    for &(kind, ns) in new_auth.difference(&old_auth) {
        let risk = if is_wildcard_pattern(kind, ns) {
            Risk::NewWildcard
        } else {
            Risk::Added
        };
        findings.push(AuditFinding {
            risk,
            message: format!("item_authoring: kind='{kind}' namespace='{ns}'"),
        });
    }
    for &(kind, ns) in old_auth.difference(&new_auth) {
        findings.push(AuditFinding {
            risk: Risk::Removed,
            message: format!("item_authoring: kind='{kind}' namespace='{ns}'"),
        });
    }

    // ── bundle_events ──
    let old_events: BTreeMap<&str, BTreeSet<&str>> = old
        .runtime_authority
        .bundle_events
        .iter()
        .map(|d| {
            (
                d.event_kind.as_str(),
                d.operations.iter().map(event_op_str).collect(),
            )
        })
        .collect();
    let new_events: BTreeMap<&str, BTreeSet<&str>> = new
        .runtime_authority
        .bundle_events
        .iter()
        .map(|d| {
            (
                d.event_kind.as_str(),
                d.operations.iter().map(event_op_str).collect(),
            )
        })
        .collect();
    diff_operation_map(
        &old_events,
        &new_events,
        "bundle_events",
        "kind",
        "operations",
        &mut findings,
    );

    // ── runtime_vault ──
    let old_vault: BTreeMap<&str, BTreeSet<&str>> = old
        .runtime_authority
        .runtime_vault
        .iter()
        .map(|d| {
            (
                d.namespace.as_str(),
                d.operations.iter().map(vault_op_str).collect(),
            )
        })
        .collect();
    let new_vault: BTreeMap<&str, BTreeSet<&str>> = new
        .runtime_authority
        .runtime_vault
        .iter()
        .map(|d| {
            (
                d.namespace.as_str(),
                d.operations.iter().map(vault_op_str).collect(),
            )
        })
        .collect();
    diff_operation_map(
        &old_vault,
        &new_vault,
        "runtime_vault",
        "namespace",
        "verbs",
        &mut findings,
    );

    // ── provides / requires / uses kinds ──
    diff_kind_list(
        &old.provides_kinds,
        &new.provides_kinds,
        "provides_kinds",
        &mut findings,
    );
    diff_kind_list(
        &old.requires_kinds,
        &new.requires_kinds,
        "requires_kinds",
        &mut findings,
    );
    diff_kind_list(
        &old.uses_kinds,
        &new.uses_kinds,
        "uses_kinds",
        &mut findings,
    );

    findings
}

/// Diff a `key -> operation-set` map (bundle_events by event kind, runtime_vault
/// by namespace): keys added/removed, and per-key operations widened/narrowed.
fn diff_operation_map(
    old: &BTreeMap<&str, BTreeSet<&str>>,
    new: &BTreeMap<&str, BTreeSet<&str>>,
    family: &str,
    key_label: &str,
    op_label: &str,
    findings: &mut Vec<AuditFinding>,
) {
    for (key, ops) in new {
        match old.get(key) {
            None => findings.push(AuditFinding {
                risk: Risk::Added,
                message: format!("{family}: {key_label} '{key}' ({})", join(ops)),
            }),
            Some(old_ops) => {
                let widened: BTreeSet<&str> = ops.difference(old_ops).copied().collect();
                let narrowed: BTreeSet<&str> = old_ops.difference(ops).copied().collect();
                if !widened.is_empty() {
                    findings.push(AuditFinding {
                        risk: Risk::Widened,
                        message: format!(
                            "{family}: {key_label} '{key}' {op_label} +[{}]",
                            join(&widened)
                        ),
                    });
                }
                if !narrowed.is_empty() {
                    findings.push(AuditFinding {
                        risk: Risk::Narrowed,
                        message: format!(
                            "{family}: {key_label} '{key}' {op_label} -[{}]",
                            join(&narrowed)
                        ),
                    });
                }
            }
        }
    }
    for key in old.keys() {
        if !new.contains_key(key) {
            findings.push(AuditFinding {
                risk: Risk::Removed,
                message: format!("{family}: {key_label} '{key}'"),
            });
        }
    }
}

/// Diff two kind-string lists (provides/requires/uses): set-added and
/// set-removed entries.
fn diff_kind_list(old: &[String], new: &[String], family: &str, findings: &mut Vec<AuditFinding>) {
    let old_set: BTreeSet<&str> = old.iter().map(String::as_str).collect();
    let new_set: BTreeSet<&str> = new.iter().map(String::as_str).collect();
    for kind in new_set.difference(&old_set) {
        findings.push(AuditFinding {
            risk: Risk::Added,
            message: format!("{family}: +{kind}"),
        });
    }
    for kind in old_set.difference(&new_set) {
        findings.push(AuditFinding {
            risk: Risk::Removed,
            message: format!("{family}: -{kind}"),
        });
    }
}

/// Load a manifest for audit from a file path. Accepts either a generated
/// `manifest.yaml` (signature header tolerated) or a `manifest.source.yaml`;
/// a source is normalized to the same field set with empty `provides_kinds`
/// (source manifests derive provides from schemas, unavailable from the file
/// alone).
pub fn load_audit_manifest(path: &Path) -> Result<BundleManifest> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read manifest {}", path.display()))?;
    let body = lillux::signature::strip_signature_lines(&raw);

    if let Ok(manifest) = serde_yaml::from_str::<BundleManifest>(&body) {
        return Ok(manifest);
    }
    let src: BundleManifestSource = serde_yaml::from_str(&body)
        .with_context(|| format!("parse manifest {}", path.display()))?;
    Ok(BundleManifest {
        name: src.name,
        version: src.version,
        description: src.description,
        provides_kinds: Vec::new(),
        requires_kinds: src.requires_kinds,
        uses_kinds: src.uses_kinds,
        runtime_authority: src.runtime_authority,
        smoke: src.smoke,
        shadows: src.shadows,
        isolation_backends: src.isolation_backends,
    })
}

/// Audit the runtime-authority delta from `old_path` to `new_path`.
pub fn run_manifest_audit(old_path: &Path, new_path: &Path) -> Result<AuthorityAudit> {
    let old = load_audit_manifest(old_path).context("load old manifest")?;
    let new = load_audit_manifest(new_path).context("load new manifest")?;
    let findings = diff_authority(&old, &new);
    Ok(AuthorityAudit {
        old_label: format!("{}@{}", old.name, old.version),
        new_label: format!("{}@{}", new.name, new.version),
        findings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_bundle::manifest::{
        BundleEventDecl, ItemAuthorDecl, RuntimeAuthorityDecls, RuntimeVaultDecl,
    };

    fn manifest(name: &str, ra: RuntimeAuthorityDecls) -> BundleManifest {
        BundleManifest {
            name: name.to_string(),
            version: "0.1.0".to_string(),
            description: String::new(),
            provides_kinds: vec![],
            requires_kinds: vec![],
            uses_kinds: vec![],
            runtime_authority: ra,
            smoke: vec![],
            shadows: vec![],
            isolation_backends: vec![],
        }
    }

    #[test]
    fn new_wildcard_authoring_pattern_is_highest_risk_and_renders_first() {
        let old = manifest("arc", RuntimeAuthorityDecls::default());
        let new = manifest(
            "arc",
            RuntimeAuthorityDecls {
                item_authoring: vec![
                    ItemAuthorDecl {
                        kind: "tool".to_string(),
                        namespace: "arc/play".to_string(),
                    },
                    ItemAuthorDecl {
                        kind: "tool".to_string(),
                        namespace: "arc/*".to_string(),
                    },
                ],
                ..Default::default()
            },
        );
        let findings = diff_authority(&old, &new);
        let wildcard = findings
            .iter()
            .find(|f| f.message.contains("arc/*"))
            .expect("wildcard finding");
        assert_eq!(wildcard.risk, Risk::NewWildcard);
        let concrete = findings
            .iter()
            .find(|f| f.message.contains("arc/play"))
            .expect("concrete finding");
        assert_eq!(concrete.risk, Risk::Added);

        let audit = AuthorityAudit {
            old_label: "arc@0.1.0".to_string(),
            new_label: "arc@0.2.0".to_string(),
            findings,
        };
        let rendered = audit.render();
        let wildcard_line = rendered.find("arc/*").unwrap();
        let play_line = rendered.find("arc/play").unwrap();
        assert!(
            wildcard_line < play_line,
            "wildcard must render before concrete add:\n{rendered}"
        );
        assert!(rendered.contains("NEW WILDCARD"));
    }

    #[test]
    fn bundle_events_widened_and_narrowed() {
        let old = manifest(
            "arc",
            RuntimeAuthorityDecls {
                bundle_events: vec![
                    BundleEventDecl {
                        event_kind: "ev_grow".to_string(),
                        operations: vec![BundleEventOperation::Append],
                    },
                    BundleEventDecl {
                        event_kind: "ev_shrink".to_string(),
                        operations: vec![BundleEventOperation::Append, BundleEventOperation::Scan],
                    },
                ],
                ..Default::default()
            },
        );
        let new = manifest(
            "arc",
            RuntimeAuthorityDecls {
                bundle_events: vec![
                    BundleEventDecl {
                        event_kind: "ev_grow".to_string(),
                        operations: vec![BundleEventOperation::Append, BundleEventOperation::Scan],
                    },
                    BundleEventDecl {
                        event_kind: "ev_shrink".to_string(),
                        operations: vec![BundleEventOperation::Append],
                    },
                ],
                ..Default::default()
            },
        );
        let findings = diff_authority(&old, &new);
        let widened = findings
            .iter()
            .find(|f| f.message.contains("ev_grow"))
            .expect("widened");
        assert_eq!(widened.risk, Risk::Widened);
        assert!(widened.message.contains("+[scan]"), "{}", widened.message);
        let narrowed = findings
            .iter()
            .find(|f| f.message.contains("ev_shrink"))
            .expect("narrowed");
        assert_eq!(narrowed.risk, Risk::Narrowed);
        assert!(narrowed.message.contains("-[scan]"), "{}", narrowed.message);
    }

    #[test]
    fn runtime_vault_namespace_and_verb_changes() {
        let old = manifest(
            "arc",
            RuntimeAuthorityDecls {
                runtime_vault: vec![RuntimeVaultDecl {
                    namespace: "arc/state".to_string(),
                    operations: vec![RuntimeVaultOperation::Get, RuntimeVaultOperation::List],
                }],
                ..Default::default()
            },
        );
        let new = manifest(
            "arc",
            RuntimeAuthorityDecls {
                runtime_vault: vec![
                    RuntimeVaultDecl {
                        namespace: "arc/state".to_string(),
                        operations: vec![RuntimeVaultOperation::Get, RuntimeVaultOperation::Put],
                    },
                    RuntimeVaultDecl {
                        namespace: "arc/cache".to_string(),
                        operations: vec![RuntimeVaultOperation::Put],
                    },
                ],
                ..Default::default()
            },
        );
        let findings = diff_authority(&old, &new);
        assert!(findings.iter().any(|f| f.risk == Risk::Added
            && f.message.contains("arc/cache")
            && f.message.contains("put")));
        assert!(findings.iter().any(|f| f.risk == Risk::Widened
            && f.message.contains("arc/state")
            && f.message.contains("+[put]")));
        assert!(findings.iter().any(|f| f.risk == Risk::Narrowed
            && f.message.contains("arc/state")
            && f.message.contains("-[list]")));
    }

    #[test]
    fn removed_grants_render_last() {
        let old = manifest(
            "arc",
            RuntimeAuthorityDecls {
                item_authoring: vec![ItemAuthorDecl {
                    kind: "tool".to_string(),
                    namespace: "arc/old".to_string(),
                }],
                ..Default::default()
            },
        );
        let mut new = manifest("arc", RuntimeAuthorityDecls::default());
        new.provides_kinds = vec!["graph".to_string()];

        let findings = diff_authority(&old, &new);
        let removed = findings
            .iter()
            .find(|f| f.message.contains("arc/old"))
            .expect("removed");
        assert_eq!(removed.risk, Risk::Removed);
        let added = findings
            .iter()
            .find(|f| f.message.contains("provides_kinds: +graph"))
            .expect("added kind");
        assert_eq!(added.risk, Risk::Added);

        let audit = AuthorityAudit {
            old_label: "arc@0.1.0".to_string(),
            new_label: "arc@0.2.0".to_string(),
            findings,
        };
        let rendered = audit.render();
        assert!(
            rendered.find("+graph").unwrap() < rendered.find("arc/old").unwrap(),
            "added must render before removed:\n{rendered}"
        );
    }

    #[test]
    fn identical_manifests_report_no_changes() {
        let old = manifest("arc", RuntimeAuthorityDecls::default());
        let new = manifest("arc", RuntimeAuthorityDecls::default());
        let audit = AuthorityAudit {
            old_label: "arc@0.1.0".to_string(),
            new_label: "arc@0.1.0".to_string(),
            findings: diff_authority(&old, &new),
        };
        assert!(audit.findings.is_empty());
        assert!(audit.render().contains("No runtime-authority changes"));
    }

    #[test]
    fn load_tolerates_signature_header_and_source_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let generated = tmp.path().join("manifest.yaml");
        std::fs::write(
            &generated,
            "# ryeos:signed:abc\nname: arc\nversion: \"0.1.0\"\nprovides_kinds: []\nrequires_kinds: []\n",
        )
        .unwrap();
        let m = load_audit_manifest(&generated).expect("load generated");
        assert_eq!(m.name, "arc");

        let source = tmp.path().join("manifest.source.yaml");
        std::fs::write(
            &source,
            "name: arc\nversion: \"0.1.0\"\nruntime_authority:\n  item_authoring:\n    - kind: tool\n      namespace: arc/play\n",
        )
        .unwrap();
        let s = load_audit_manifest(&source).expect("load source");
        assert_eq!(s.runtime_authority.item_authoring.len(), 1);
    }
}
