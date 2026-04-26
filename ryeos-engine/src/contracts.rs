//! Data contracts — the seam between daemon and engine.
//!
//! These are the concrete structs that flow across the boundary.
//! The daemon calls concrete `Engine` methods and receives these types.
//! No trait boundary, no dyn dispatch at the seam.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::canonical_ref::CanonicalRef;

// ── Wiring contract: ValueShape ──────────────────────────────────────

/// Declared shape contract for a parsed `serde_json::Value` flowing
/// across a wiring seam (parser → composer today; route → item later).
/// Subset/superset comparable at boot — keep it small.
///
/// Not JSON Schema. Just enough for what we need today and trivially
/// extensible later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValueShape {
    /// Top-level type. `mapping` is the only non-trivial case today.
    pub root_type: ShapeType,
    /// Required fields (only meaningful when root_type == Mapping).
    pub required: std::collections::BTreeMap<String, FieldType>,
    /// Optional fields the producer MAY emit. Documented but not
    /// enforced in subset check — extra fields are always allowed.
    pub optional: std::collections::BTreeMap<String, FieldType>,
}

/// On-the-wire form for `ValueShape`. Used purely for deserialization:
/// `deny_unknown_fields` so a typo doesn't silently widen the shape,
/// then `try_from` runs structural validation (no fields on non-mapping
/// roots, no empty unions) before producing the public type.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ValueShapeRaw {
    #[serde(default = "ValueShape::default_root_type")]
    root_type: ShapeType,
    #[serde(default)]
    required: std::collections::BTreeMap<String, FieldType>,
    #[serde(default)]
    optional: std::collections::BTreeMap<String, FieldType>,
}

impl<'de> Deserialize<'de> for ValueShape {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = ValueShapeRaw::deserialize(deserializer)?;
        if raw.root_type != ShapeType::Mapping && !raw.required.is_empty() {
            return Err(serde::de::Error::custom(format!(
                "`required` is only meaningful when root_type == mapping (got {:?})",
                raw.root_type
            )));
        }
        if raw.root_type != ShapeType::Mapping && !raw.optional.is_empty() {
            return Err(serde::de::Error::custom(format!(
                "`optional` is only meaningful when root_type == mapping (got {:?})",
                raw.root_type
            )));
        }
        for (name, ft) in raw.required.iter().chain(raw.optional.iter()) {
            if let FieldType::Union(ps) = ft {
                if ps.is_empty() {
                    return Err(serde::de::Error::custom(format!(
                        "field `{name}`: empty union is not a valid type"
                    )));
                }
            }
        }
        Ok(ValueShape {
            root_type: raw.root_type,
            required: raw.required,
            optional: raw.optional,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShapeType {
    Mapping,
    Sequence,
    Scalar,
    Any,
}

/// Per-field type. Use a Vec to allow union types (`[string, "null"]`).
/// `Any` permits anything. Keep it tiny.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FieldType {
    Single(PrimType),
    Union(Vec<PrimType>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrimType {
    String,
    Integer,
    Boolean,
    Mapping,
    Sequence,
    Null,
    Any,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractViolation {
    RootTypeMismatch {
        needed: ShapeType,
        produced: ShapeType,
    },
    MissingRequiredField {
        name: String,
        needed: FieldType,
        /// Set when the producer declared this field but only as
        /// `optional`. A required consumer demand is NOT satisfied
        /// by an optional producer emission — the producer MAY skip
        /// the field, so the consumer can't rely on it. The hint
        /// helps authors fix the producer side fast.
        produced_as_optional: Option<FieldType>,
    },
    FieldTypeMismatch {
        name: String,
        needed: FieldType,
        produced: FieldType,
    },
}

impl ValueShape {
    fn default_root_type() -> ShapeType {
        ShapeType::Mapping
    }

    /// Explicit empty-mapping contract. Use when a kind or parser
    /// genuinely makes no field-level claims at boot but must still
    /// declare its shape so absence is a deliberate, reviewed choice.
    pub fn any_mapping() -> Self {
        Self {
            root_type: ShapeType::Mapping,
            required: std::collections::BTreeMap::new(),
            optional: std::collections::BTreeMap::new(),
        }
    }

    /// Check that this shape (the CONSUMER's needs) is satisfiable by
    /// `producer`. Returns ALL violations — empty Vec means compatible.
    ///
    /// Subset semantics: producer may emit MORE fields (extras OK).
    ///
    /// `Any` is asymmetric:
    ///   * **Consumer Any** accepts anything from the producer — the
    ///     consumer is opting out of constraints on this position.
    ///   * **Producer Any** does NOT satisfy a specific consumer
    ///     demand. Producer-side `Any` means "I make no claim about
    ///     what I emit"; a consumer that asks for `string` cannot
    ///     trust that, so the wiring is unsound and we report it.
    pub fn is_satisfied_by(&self, producer: &ValueShape) -> Vec<ContractViolation> {
        let mut violations = Vec::new();

        // Consumer Any: accepts any producer; no field checks.
        if self.root_type == ShapeType::Any {
            return violations;
        }
        if self.root_type != producer.root_type {
            // Producer Any against a specific consumer is a real
            // mismatch — producer makes no guarantee about its root.
            violations.push(ContractViolation::RootTypeMismatch {
                needed: self.root_type,
                produced: producer.root_type,
            });
            // Don't bail: keep aggregating field-level violations so
            // authors see the full picture in one shot. If producer
            // root is non-mapping, its required/optional maps are
            // empty (deserializer enforces this), so the loop below
            // naturally reports every required field as missing.
        }

        // Field-level checks only meaningful for mappings.
        for (name, needed) in &self.required {
            // ONLY producer.required satisfies consumer.required.
            // A field present only as producer.optional means the
            // producer MAY omit it, so the consumer can't rely on it.
            match producer.required.get(name) {
                Some(p) => {
                    if !field_type_covers(needed, p) {
                        violations.push(ContractViolation::FieldTypeMismatch {
                            name: name.clone(),
                            needed: needed.clone(),
                            produced: p.clone(),
                        });
                    }
                }
                None => {
                    let produced_as_optional = producer.optional.get(name).cloned();
                    violations.push(ContractViolation::MissingRequiredField {
                        name: name.clone(),
                        needed: needed.clone(),
                        produced_as_optional,
                    });
                }
            }
        }

        violations
    }
}

/// True iff every primitive the producer might emit is acceptable to
/// the consumer.
///
/// Asymmetric `Any`:
///   * Consumer set containing `Any` → accepts everything (consumer
///     opted out of typing this field).
///   * Producer `Any` against a specific consumer set → rejected. The
///     producer makes no claim, so a consumer demanding `string` (or
///     `string|null`) can't trust the wiring.
fn field_type_covers(consumer: &FieldType, producer: &FieldType) -> bool {
    let consumer_set: Vec<PrimType> = match consumer {
        FieldType::Single(p) => vec![*p],
        FieldType::Union(ps) => ps.clone(),
    };
    let producer_set: Vec<PrimType> = match producer {
        FieldType::Single(p) => vec![*p],
        FieldType::Union(ps) => ps.clone(),
    };
    if consumer_set.iter().any(|p| *p == PrimType::Any) {
        return true;
    }
    // Every producer possibility (including `Any`) must be a member
    // of the consumer's accepted set. `Any` is never a member of a
    // specific set, so producer `Any` here is a hard fail.
    producer_set.iter().all(|p| consumer_set.contains(p))
}

#[cfg(test)]
mod value_shape_tests {
    use super::*;

    fn shape_mapping(
        required: &[(&str, FieldType)],
        optional: &[(&str, FieldType)],
    ) -> ValueShape {
        ValueShape {
            root_type: ShapeType::Mapping,
            required: required
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
            optional: optional
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        }
    }

    #[test]
    fn identical_shapes_pass() {
        let a = shape_mapping(&[("body", FieldType::Single(PrimType::String))], &[]);
        let b = a.clone();
        assert!(a.is_satisfied_by(&b).is_empty());
    }

    #[test]
    fn missing_required_field_detected() {
        let consumer = shape_mapping(&[("body", FieldType::Single(PrimType::String))], &[]);
        let producer = shape_mapping(&[], &[]);
        let v = consumer.is_satisfied_by(&producer);
        assert_eq!(v.len(), 1);
        assert!(matches!(
            v[0],
            ContractViolation::MissingRequiredField { ref name, .. } if name == "body"
        ));
    }

    #[test]
    fn required_consumer_not_satisfied_by_optional_producer() {
        // A required consumer demand cannot be satisfied by an
        // optional producer emission: the producer is allowed to
        // skip the field entirely. Reported as MissingRequiredField
        // with a hint that it was found as optional.
        let consumer = shape_mapping(&[("body", FieldType::Single(PrimType::String))], &[]);
        let producer = shape_mapping(&[], &[("body", FieldType::Single(PrimType::String))]);
        let v = consumer.is_satisfied_by(&producer);
        assert_eq!(v.len(), 1, "expected one missing-required violation, got {v:?}");
        match &v[0] {
            ContractViolation::MissingRequiredField {
                name,
                produced_as_optional,
                ..
            } => {
                assert_eq!(name, "body");
                assert_eq!(
                    produced_as_optional,
                    &Some(FieldType::Single(PrimType::String)),
                    "hint should indicate the producer declared the field as optional"
                );
            }
            other => panic!("expected MissingRequiredField, got {other:?}"),
        }
    }

    #[test]
    fn required_consumer_satisfied_by_required_producer() {
        // Sanity: identical required-on-both is fine. Used to be the
        // misleading "required satisfied by optional" test.
        let consumer = shape_mapping(&[("body", FieldType::Single(PrimType::String))], &[]);
        let producer = shape_mapping(&[("body", FieldType::Single(PrimType::String))], &[]);
        assert!(consumer.is_satisfied_by(&producer).is_empty());
    }

    #[test]
    fn missing_required_with_no_optional_hint() {
        let consumer = shape_mapping(&[("body", FieldType::Single(PrimType::String))], &[]);
        let producer = shape_mapping(&[], &[]);
        let v = consumer.is_satisfied_by(&producer);
        assert_eq!(v.len(), 1);
        match &v[0] {
            ContractViolation::MissingRequiredField {
                name,
                produced_as_optional,
                ..
            } => {
                assert_eq!(name, "body");
                assert_eq!(produced_as_optional, &None);
            }
            other => panic!("expected MissingRequiredField, got {other:?}"),
        }
    }

    #[test]
    fn type_mismatch_detected() {
        let consumer = shape_mapping(&[("body", FieldType::Single(PrimType::String))], &[]);
        let producer = shape_mapping(&[("body", FieldType::Single(PrimType::Integer))], &[]);
        let v = consumer.is_satisfied_by(&producer);
        assert_eq!(v.len(), 1);
        assert!(matches!(v[0], ContractViolation::FieldTypeMismatch { .. }));
    }

    #[test]
    fn consumer_any_accepts_any_producer() {
        // Consumer Any at root: no constraints, anything is fine.
        let consumer_root_any = ValueShape {
            root_type: ShapeType::Any,
            required: Default::default(),
            optional: Default::default(),
        };
        let producer_mapping = shape_mapping(
            &[("body", FieldType::Single(PrimType::String))],
            &[],
        );
        let producer_seq = ValueShape {
            root_type: ShapeType::Sequence,
            required: Default::default(),
            optional: Default::default(),
        };
        let producer_root_any = ValueShape {
            root_type: ShapeType::Any,
            required: Default::default(),
            optional: Default::default(),
        };
        assert!(consumer_root_any.is_satisfied_by(&producer_mapping).is_empty());
        assert!(consumer_root_any.is_satisfied_by(&producer_seq).is_empty());
        assert!(consumer_root_any.is_satisfied_by(&producer_root_any).is_empty());

        // Consumer field-level Any: accepts any producer field type.
        let consumer = shape_mapping(&[("body", FieldType::Single(PrimType::Any))], &[]);
        let producer_str = shape_mapping(&[("body", FieldType::Single(PrimType::String))], &[]);
        let producer_int = shape_mapping(&[("body", FieldType::Single(PrimType::Integer))], &[]);
        let producer_any = shape_mapping(&[("body", FieldType::Single(PrimType::Any))], &[]);
        assert!(consumer.is_satisfied_by(&producer_str).is_empty());
        assert!(consumer.is_satisfied_by(&producer_int).is_empty());
        assert!(consumer.is_satisfied_by(&producer_any).is_empty());
    }

    #[test]
    fn producer_any_does_not_satisfy_specific_consumer() {
        // Specific consumer + producer Any at root → RootTypeMismatch.
        let consumer = shape_mapping(&[("body", FieldType::Single(PrimType::String))], &[]);
        let producer_root_any = ValueShape {
            root_type: ShapeType::Any,
            required: Default::default(),
            optional: Default::default(),
        };
        let v = consumer.is_satisfied_by(&producer_root_any);
        assert!(
            v.iter().any(|x| matches!(
                x,
                ContractViolation::RootTypeMismatch { needed: ShapeType::Mapping, produced: ShapeType::Any }
            )),
            "producer ShapeType::Any must NOT satisfy a Mapping consumer; got {v:?}"
        );

        // Specific consumer field + producer Any field → FieldTypeMismatch.
        let producer = shape_mapping(&[("body", FieldType::Single(PrimType::Any))], &[]);
        let v = consumer.is_satisfied_by(&producer);
        assert_eq!(v.len(), 1);
        assert!(
            matches!(v[0], ContractViolation::FieldTypeMismatch { .. }),
            "producer PrimType::Any must NOT satisfy a String consumer; got {v:?}"
        );

        // Producer Any inside a union also rejected by specific consumer.
        let producer_union_any = shape_mapping(
            &[("body", FieldType::Union(vec![PrimType::String, PrimType::Any]))],
            &[],
        );
        let v = consumer.is_satisfied_by(&producer_union_any);
        assert!(
            v.iter().any(|x| matches!(x, ContractViolation::FieldTypeMismatch { .. })),
            "producer union containing Any must be rejected; got {v:?}"
        );
    }

    #[test]
    fn union_consumer_accepts_member_producer() {
        let consumer = shape_mapping(
            &[(
                "extends",
                FieldType::Union(vec![PrimType::String, PrimType::Null]),
            )],
            &[],
        );
        let producer_string =
            shape_mapping(&[("extends", FieldType::Single(PrimType::String))], &[]);
        let producer_null = shape_mapping(&[("extends", FieldType::Single(PrimType::Null))], &[]);
        let producer_int = shape_mapping(&[("extends", FieldType::Single(PrimType::Integer))], &[]);
        assert!(consumer.is_satisfied_by(&producer_string).is_empty());
        assert!(consumer.is_satisfied_by(&producer_null).is_empty());
        assert!(!consumer.is_satisfied_by(&producer_int).is_empty());
    }

    #[test]
    fn union_producer_subset_of_union_consumer() {
        let consumer = shape_mapping(
            &[(
                "x",
                FieldType::Union(vec![PrimType::String, PrimType::Null]),
            )],
            &[],
        );
        let producer = shape_mapping(
            &[(
                "x",
                FieldType::Union(vec![PrimType::String, PrimType::Null]),
            )],
            &[],
        );
        assert!(consumer.is_satisfied_by(&producer).is_empty());

        // Producer might emit Integer too — not in consumer's set.
        let producer_wider = shape_mapping(
            &[(
                "x",
                FieldType::Union(vec![PrimType::String, PrimType::Null, PrimType::Integer]),
            )],
            &[],
        );
        assert!(!consumer.is_satisfied_by(&producer_wider).is_empty());
    }

    #[test]
    fn root_type_mismatch_detected() {
        let consumer = ValueShape {
            root_type: ShapeType::Mapping,
            required: Default::default(),
            optional: Default::default(),
        };
        let producer = ValueShape {
            root_type: ShapeType::Sequence,
            required: Default::default(),
            optional: Default::default(),
        };
        let v = consumer.is_satisfied_by(&producer);
        assert!(v
            .iter()
            .any(|x| matches!(x, ContractViolation::RootTypeMismatch { .. })));
    }

    #[test]
    fn all_violations_returned_not_bailing_on_first() {
        let consumer = shape_mapping(
            &[
                ("a", FieldType::Single(PrimType::String)),
                ("b", FieldType::Single(PrimType::String)),
                ("c", FieldType::Single(PrimType::String)),
            ],
            &[],
        );
        let producer = ValueShape {
            root_type: ShapeType::Sequence,
            required: Default::default(),
            optional: Default::default(),
        };
        let v = consumer.is_satisfied_by(&producer);
        // 1 root mismatch + 3 missing fields
        assert_eq!(v.len(), 4, "got: {v:?}");
    }

    // ── Strictness hardening: deserialization rejects nonsense ───────

    #[test]
    fn deserialize_rejects_unknown_fields() {
        // `deny_unknown_fields` on the wire form: a typo like
        // `requierd` must blow up loudly instead of silently widening
        // the contract to accept anything.
        let yaml = "\
root_type: mapping
requierd:
  body: string
";
        let err = serde_yaml::from_str::<ValueShape>(yaml).unwrap_err();
        assert!(
            format!("{err}").contains("unknown field"),
            "expected unknown-field error, got: {err}"
        );
    }

    #[test]
    fn deserialize_rejects_required_on_non_mapping_root() {
        let yaml = "\
root_type: sequence
required:
  body: string
";
        let err = serde_yaml::from_str::<ValueShape>(yaml).unwrap_err();
        assert!(
            format!("{err}").contains("`required` is only meaningful when root_type == mapping"),
            "got: {err}"
        );
    }

    #[test]
    fn deserialize_rejects_optional_on_non_mapping_root() {
        let yaml = "\
root_type: scalar
optional:
  body: string
";
        let err = serde_yaml::from_str::<ValueShape>(yaml).unwrap_err();
        assert!(
            format!("{err}").contains("`optional` is only meaningful when root_type == mapping"),
            "got: {err}"
        );
    }

    #[test]
    fn deserialize_rejects_empty_union() {
        let yaml = "\
root_type: mapping
required:
  body: []
";
        let err = serde_yaml::from_str::<ValueShape>(yaml).unwrap_err();
        assert!(
            format!("{err}").contains("empty union"),
            "got: {err}"
        );
    }

    #[test]
    fn deserialize_rejects_empty_union_in_optional() {
        let yaml = "\
root_type: mapping
optional:
  extends: []
";
        let err = serde_yaml::from_str::<ValueShape>(yaml).unwrap_err();
        assert!(
            format!("{err}").contains("empty union"),
            "got: {err}"
        );
    }

    #[test]
    fn deserialize_accepts_well_formed_shape() {
        // Sanity: the live bundle's directive contract round-trips
        // through the strict deserializer cleanly.
        let yaml = "\
root_type: mapping
required:
  body: string
optional:
  extends: [string, \"null\"]
  permissions: mapping
  context: mapping
";
        let shape: ValueShape = serde_yaml::from_str(yaml).expect("well-formed shape parses");
        assert_eq!(shape.root_type, ShapeType::Mapping);
        assert_eq!(
            shape.required.get("body").unwrap(),
            &FieldType::Single(PrimType::String)
        );
        assert_eq!(
            shape.optional.get("extends").unwrap(),
            &FieldType::Union(vec![PrimType::String, PrimType::Null])
        );
    }
}

// ── Signature envelope ───────────────────────────────────────────────

/// How a `rye:signed:...` payload is embedded in a source file.
///
/// Varies by file type — loaded from extractor YAML, never hardcoded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureEnvelope {
    /// Comment prefix character(s), e.g. `"#"`, `"//"`, `"<!--"`
    pub prefix: String,
    /// Optional comment suffix, e.g. `"-->"` for markdown/HTML wrapping
    pub suffix: Option<String>,
    /// Whether the signature line goes after a shebang line
    pub after_shebang: bool,
}

// ── Source format (carried on each ResolvedItem) ─────────────────────

/// Per-item format facts derived from the `KindRegistry` during
/// resolution. Downstream consumers (trust verification, chain builder)
/// use this instead of consulting the full registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedSourceFormat {
    /// The matched file extension, e.g. `".py"`, `".md"`
    pub extension: String,
    /// Canonical parser tool ref, e.g.
    /// `"parser:rye/core/python/ast"`. The `ParserDispatcher`
    /// resolves this through `ParserRegistry`.
    pub parser: String,
    /// Signature embedding envelope for this file type
    pub signature: SignatureEnvelope,
}

// ── Item spaces ──────────────────────────────────────────────────────

/// The three-tier resolution space where an item was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItemSpace {
    Project,
    User,
    System,
}

impl ItemSpace {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
            Self::System => "system",
        }
    }
}

// ── Project context ──────────────────────────────────────────────────

/// Portable project identity for local and remote execution.
///
/// Always present on requests. `None` is the explicit "no project" variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectContext {
    None,
    LocalPath { path: PathBuf },
    SnapshotHash { hash: String },
    ProjectRef { principal: String, ref_name: String },
}

// ── Materialized project context ─────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MaterializedProjectContext {
    pub project_root: Option<PathBuf>,
    pub source: ProjectContext,
}

// ── Signature header ─────────────────────────────────────────────────

/// Parsed canonical signed payload extracted from a source file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureHeader {
    pub timestamp: String,
    pub content_hash: String,
    pub signature_b64: String,
    pub signer_fingerprint: String,
}

// ── Item metadata ────────────────────────────────────────────────────

/// Lightweight metadata extracted during resolution.
///
/// Contains enough to discover the executor and build a dispatch plan.
/// Full body parsing is the executor/adapter's responsibility.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ItemMetadata {
    /// Executor ID from `__executor_id__` or frontmatter
    pub executor_id: Option<String>,
    /// Item version from `__version__`
    pub version: Option<String>,
    /// Item description
    pub description: Option<String>,
    /// Item category
    pub category: Option<String>,
    /// Vault secret IDs this item requires (e.g. `["openai-api-key"]`).
    /// The daemon resolves these per-principal and injects as `RYE_VAULT_*` env vars.
    #[serde(default)]
    pub required_secrets: Vec<String>,
    /// Arbitrary additional metadata fields (kind-specific fields live here)
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

// ── Resolution output ────────────────────────────────────────────────

/// A candidate that was shadowed by a higher-priority match during resolution.
#[derive(Debug, Clone)]
pub struct ShadowedCandidate {
    pub label: String,
    pub space: ItemSpace,
    pub path: PathBuf,
}

/// Result of successful item resolution.
#[derive(Debug, Clone)]
pub struct ResolvedItem {
    pub canonical_ref: CanonicalRef,
    pub kind: String,
    pub source_path: PathBuf,
    pub source_space: ItemSpace,
    /// Label of the root that won resolution, e.g. "system(node)", "user"
    pub resolved_from: String,
    /// Lower-priority candidates that were shadowed by the winner
    pub shadowed: Vec<ShadowedCandidate>,
    pub materialized_project_root: Option<PathBuf>,
    pub content_hash: String,
    pub signature_header: Option<SignatureHeader>,
    pub source_format: ResolvedSourceFormat,
    pub metadata: ItemMetadata,
}

// ── Trust classes ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrustClass {
    /// Signed by a trusted signer
    Trusted,
    /// Signed but signer is not in the trust store
    Untrusted,
    /// Unsigned item (may be allowed in dev/project space)
    Unsigned,
}

/// Signer identity from the signature header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignerFingerprint(pub String);

/// Pinned version reference for signed/temporal resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedVersion {
    pub content_hash: String,
    pub signature: String,
}

// ── Verification output ──────────────────────────────────────────────

/// Result of trust and integrity verification.
#[derive(Debug, Clone)]
pub struct VerifiedItem {
    pub resolved: ResolvedItem,
    pub signer: Option<SignerFingerprint>,
    pub trust_class: TrustClass,
    pub pinned_version: Option<PinnedVersion>,
}

// ── Launch mode ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchMode {
    Inline,
    Detached,
}

// ── Execution hints ──────────────────────────────────────────────────

/// Open map passed through to executors. The engine does not interpret
/// its contents.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionHints {
    #[serde(flatten)]
    pub values: HashMap<String, Value>,
}

// ── Principal ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Principal {
    pub fingerprint: String,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegatedPrincipal {
    pub protocol_version: String,
    pub delegation_id: String,
    pub caller_fingerprint: String,
    pub origin_site_id: String,
    pub audience_site_id: String,
    pub delegated_scopes: Vec<String>,
    pub budget_lease_id: Option<String>,
    pub request_hash: String,
    pub idempotency_key: String,
    pub issued_at: String,
    pub expires_at: String,
    pub non_redelegable: bool,
    pub origin_signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectivePrincipal {
    Local(Principal),
    Delegated(DelegatedPrincipal),
}

// ── Plan context (resolve/verify/build_plan) ─────────────────────────

/// Context for the planning phases: resolve, verify, build_plan.
///
/// Does NOT carry thread IDs or daemon runtime bindings.
/// This is what makes `validate_only` safe.
#[derive(Debug, Clone)]
pub struct PlanContext {
    pub requested_by: EffectivePrincipal,
    pub project_context: ProjectContext,
    pub current_site_id: String,
    pub origin_site_id: String,
    pub execution_hints: ExecutionHints,
    /// When true, the daemon should not call `execute_plan` after
    /// `build_plan` succeeds. The engine does not enforce this — it is
    /// safe structurally because `PlanContext` does not carry thread IDs.
    pub validate_only: bool,
}

// ── Engine context (execute_plan) ────────────────────────────────────

/// Context for plan execution. Carries everything in `PlanContext` plus
/// daemon-allocated thread identity and runtime bindings.
#[derive(Debug, Clone)]
pub struct EngineContext {
    pub thread_id: String,
    pub chain_root_id: String,
    pub current_site_id: String,
    pub origin_site_id: String,
    pub upstream_site_id: Option<String>,
    pub upstream_thread_id: Option<String>,
    pub continuation_from_id: Option<String>,
    pub requested_by: EffectivePrincipal,
    pub project_context: ProjectContext,
    pub launch_mode: LaunchMode,
}

// ── Plan IR ──────────────────────────────────────────────────────────

/// Unique identifier for a plan node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlanNodeId(pub String);

/// Plan capabilities declared by the execution plan.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanCapabilities {
    pub requires_model: bool,
    pub requires_subprocess: bool,
    pub requires_network: bool,
    pub custom: Vec<String>,
}

/// Materialization requirement for plan execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaterializationRequirement {
    pub kind: String,
    pub ref_string: String,
}

/// Normalized subprocess specification — the single source of truth for
/// what to spawn. Compiled from the executor chain's runtime config by
/// the plan builder. The dispatch layer just runs this struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubprocessSpec {
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub stdin_data: Option<String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Per-tool execution policy populated by `DecorateSpec`-phase
    /// runtime handlers (`native_async`, future `native_resume`,
    /// `execution_owner`). Default = empty → preserves baseline
    /// behavior for tools that declare none of these.
    #[serde(default)]
    pub execution: ExecutionDecorations,
}

/// Typed bag of `DecorateSpec`-phase outputs. Each field is `Option`
/// so absence ⇒ "preserve current default". Future decorate handlers
/// add siblings here without breaking the top-level spec shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionDecorations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_async: Option<NativeAsyncSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_resume: Option<NativeResumeSpec>,
}

/// Resume policy declared by the `native_resume` runtime handler.
/// Presence in the spec ⇒ the tool is replay-aware: the daemon will
/// allocate a per-thread checkpoint dir, inject `RYE_CHECKPOINT_DIR`
/// at spawn time, and on daemon restart attempt automatic resume up
/// to `max_auto_resume_attempts` times before marking the thread
/// failed. The tool is responsible for writing checkpoints into the
/// supplied directory and for being idempotent / replay-safe on
/// startup (`RYE_RESUME=1` is injected on resume spawns).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativeResumeSpec {
    /// Hint to the tool for how often to checkpoint. Engine and daemon
    /// do not enforce this — purely advisory.
    pub checkpoint_interval_secs: u64,
    /// Hard ceiling on automatic resume attempts after daemon restart.
    /// `1` (default) = single retry. `0` = never auto-resume (still
    /// declares replay-awareness for manual resume tooling).
    pub max_auto_resume_attempts: u32,
}

impl Default for NativeResumeSpec {
    fn default() -> Self {
        Self {
            checkpoint_interval_secs: 30,
            max_auto_resume_attempts: 1,
        }
    }
}

/// Cancellation + streaming policy declared by the `native_async`
/// runtime handler. Presence in the spec ⇒ this tool drives its own
/// event stream (the runner injects `RYE_NATIVE_ASYNC=1`) and the
/// daemon cancellation routes through `cancellation_mode`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeAsyncSpec {
    pub cancellation_mode: CancellationMode,
}

/// How the runner terminates the subprocess on cancellation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CancellationMode {
    /// SIGKILL the process group immediately.
    Hard,
    /// SIGTERM, wait `grace_secs`, then SIGKILL.
    Graceful { grace_secs: u64 },
}

impl Default for CancellationMode {
    fn default() -> Self {
        CancellationMode::Graceful { grace_secs: 5 }
    }
}

fn default_timeout_secs() -> u64 {
    300
}

/// A node in the execution plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "node_type", rename_all = "snake_case")]
pub enum PlanNode {
    DispatchSubprocess {
        id: PlanNodeId,
        /// The fully resolved subprocess specification.
        spec: SubprocessSpec,
        /// Audit: the root item's source path.
        #[serde(default)]
        tool_path: Option<PathBuf>,
        /// Audit: executor IDs traversed during chain resolution.
        #[serde(default)]
        executor_chain: Vec<String>,
    },
    SpawnChild {
        id: PlanNodeId,
        child_ref: String,
        thread_kind: String,
        edge_type: String,
    },
    Complete {
        id: PlanNodeId,
    },
}

impl PlanNode {
    pub fn id(&self) -> &PlanNodeId {
        match self {
            Self::DispatchSubprocess { id, .. }
            | Self::SpawnChild { id, .. }
            | Self::Complete { id, .. } => id,
        }
    }
}

/// Normalized execution plan — the engine's output from `build_plan`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionPlan {
    pub plan_id: String,
    pub root_executor_id: String,
    pub root_ref: String,
    pub item_kind: String,
    pub nodes: Vec<PlanNode>,
    pub entrypoint: PlanNodeId,
    pub capabilities: PlanCapabilities,
    pub materialization_requirements: Vec<MaterializationRequirement>,
    pub cache_key: String,
    /// Daemon supervision profile hint, derived from the root item's kind.
    #[serde(default)]
    pub thread_kind: Option<String>,
    /// Executor IDs traversed during chain resolution.
    #[serde(default)]
    pub executor_chain: Vec<String>,
}

// ── Execution completion ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadTerminalStatus {
    Completed,
    Failed,
    Cancelled,
    Continued,
    Killed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionArtifact {
    pub artifact_type: String,
    pub uri: String,
    #[serde(default)]
    pub content_hash: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalCost {
    #[serde(default)]
    pub turns: i64,
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub spend: f64,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContinuationRequest {
    pub reason: String,
    pub successor_parameters: Option<Value>,
}

/// Structured completion returned from plan execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionCompletion {
    pub status: ThreadTerminalStatus,
    #[serde(default)]
    pub outcome_code: Option<String>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<Value>,
    #[serde(default)]
    pub artifacts: Vec<ExecutionArtifact>,
    #[serde(default)]
    pub final_cost: Option<FinalCost>,
    #[serde(default)]
    pub continuation_request: Option<ContinuationRequest>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

// ── Budget lease / settlement (daemon-to-daemon) ─────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetLease {
    pub lease_id: String,
    pub issuer_site_id: String,
    pub parent_thread_id: String,
    pub reserved_max_spend: f64,
    pub issued_at: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpendReport {
    pub lease_id: String,
    pub spend_report_id: String,
    pub report_seq: i64,
    pub amount: f64,
    #[serde(default)]
    pub runtime_metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalSettlement {
    pub lease_id: String,
    pub settlement_id: String,
    pub final_spend: f64,
    pub terminal_status: String,
}

// ── Capability lease / settlement ────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityLease {
    pub lease_id: String,
    pub capability_id: String,
    pub issuer_site_id: String,
    pub parent_thread_id: String,
    pub max_uses: i64,
    pub constraints_hash: String,
    pub issued_at: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityUseReport {
    pub lease_id: String,
    pub use_report_id: String,
    pub report_seq: i64,
    pub uses: i64,
    #[serde(default)]
    pub runtime_metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityFinalSettlement {
    pub lease_id: String,
    pub settlement_id: String,
    pub final_use_count: i64,
    pub terminal_status: String,
}

// ── Event contracts ──────────────────────────────────────────────────

/// What adapters and the engine send to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventAppendRequest {
    pub event_type: String,
    pub payload: Value,
}

/// What the daemon persists and streams.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub event_id: String,
    pub thread_id: String,
    pub chain_root_id: String,
    pub site_id: String,
    pub event_type: String,
    pub payload: Value,
    pub timestamp: String,
    pub sequence: i64,
}


