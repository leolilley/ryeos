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
    /// Whether unknown fields in descriptor values produce warnings
    /// during instance validation.
    ///
    /// - `None` (default, omitted): unknown fields are silently ignored.
    /// - `Some(Warn)`: unknown fields produce validation warnings.
    ///
    /// Migrated kind schemas opt into warnings by declaring
    /// `strict_fields: warn`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict_fields: Option<StrictFieldsPolicy>,
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
    #[serde(default)]
    strict_fields: Option<String>,
}

/// How unknown fields in descriptor values are handled during instance
/// validation. Only `Warn` is implemented in v1; `None` (omitted) means
/// unknown fields are silently ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum StrictFieldsPolicy {
    /// Unknown fields produce validation warnings but do not block
    /// resolution. Opt-in via `strict_fields: warn` on the kind schema.
    Warn,
}

impl Default for StrictFieldsPolicy {
    fn default() -> Self {
        StrictFieldsPolicy::Warn
    }
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
        // Union emptiness and FieldType constraint validation is now
        // handled inside FieldType::try_from (called by FieldType's
        // Deserialize impl), so we don't duplicate it here.
        Ok(ValueShape {
            root_type: raw.root_type,
            required: raw.required,
            optional: raw.optional,
            strict_fields: raw
                .strict_fields
                .map(|s| match s.as_str() {
                    "warn" => Ok(StrictFieldsPolicy::Warn),
                    other => Err(serde::de::Error::custom(format!(
                        "unknown `strict_fields` value: \"{other}\" (only `warn` is supported in v1)"
                    ))),
                })
                .transpose()?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ShapeType {
    Mapping,
    #[serde(alias = "array")]
    Sequence,
    Scalar,
    Any,
}

/// Per-field type. Use a Vec to allow union types (`[string, "null"]`).
/// `Any` permits anything. Keep it tiny.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FieldType {
    Single {
        prim: PrimType,
        /// Closed set of allowed string literal values. Only valid when
        /// `prim == String`. Enforced at schema load time.
        /// Serialized as `enum` in YAML for brevity.
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "enum")]
        enum_values: Option<Vec<String>>,
        /// Nested contract for mapping sub-fields. Only valid when
        /// `prim == Mapping`. Enforced at schema load time.
        /// Serialized as `contract` in YAML.
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "contract")]
        nested_contract: Option<Box<ValueShape>>,
        /// Element type for sequence fields. Only valid when
        /// `prim == Sequence`. Enforced at schema load time.
        /// Serialized as `elements` in YAML.
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "elements")]
        element_type: Option<Box<FieldType>>,
    },
    Union {
        prims: Vec<PrimType>,
    },
}

impl Default for FieldType {
    fn default() -> Self {
        FieldType::Single {
            prim: PrimType::Any,
            enum_values: None,
            nested_contract: None,
            element_type: None,
        }
    }
}

/// On-the-wire form for `FieldType`. Uses `deny_unknown_fields` so
/// typos in new keys are caught. Converts to the public type via
/// `try_from` after structural validation.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum FieldTypeRaw {
    Single {
        prim: PrimType,
        #[serde(default, rename = "enum")]
        enum_values: Option<Vec<String>>,
        #[serde(default)]
        contract: Option<ValueShape>,
        #[serde(default)]
        elements: Option<Box<FieldTypeRaw>>,
    },
    Union {
        prims: Vec<PrimType>,
    },
}

impl TryFrom<FieldTypeRaw> for FieldType {
    type Error = String;

    fn try_from(raw: FieldTypeRaw) -> Result<Self, String> {
        match raw {
            FieldTypeRaw::Union { prims } => {
                if prims.is_empty() {
                    return Err("empty union is not a valid type".to_string());
                }
                Ok(FieldType::Union { prims })
            }
            FieldTypeRaw::Single {
                prim,
                enum_values,
                contract,
                elements,
            } => {
                if let Some(ref enums) = enum_values {
                    if prim != PrimType::String {
                        return Err(format!(
                            "field: `enum` is only valid on `prim: string` (got `prim: {prim:?}`)"
                        ));
                    }
                    if enums.is_empty() {
                        return Err("field: `enum` list must not be empty".to_string());
                    }
                }
                if contract.is_some() && prim != PrimType::Mapping {
                    return Err(format!(
                        "field: `contract` is only valid on `prim: mapping` (got `prim: {prim:?}`)"
                    ));
                }
                // Nested contract root must be Mapping (it describes
                // sub-fields of a mapping value).
                if let Some(ref c) = contract {
                    if c.root_type != ShapeType::Mapping {
                        return Err(format!(
                            "field: nested `contract` root_type must be mapping (got {:?})",
                            c.root_type
                        ));
                    }
                }
                if elements.is_some() && prim != PrimType::Sequence {
                    return Err(format!(
                        "field: `elements` is only valid on `prim: sequence` (got `prim: {prim:?}`)"
                    ));
                }

                // Recursively validate nested FieldTypeRaw -> FieldType
                let nested_contract = contract
                    .map(Box::new)
                    .map(|c| Ok::<Box<ValueShape>, String>(c))
                    .transpose()?;
                let element_type = elements
                    .map(|e| FieldType::try_from(*e).map(Box::new))
                    .transpose()?;

                Ok(FieldType::Single {
                    prim,
                    enum_values,
                    nested_contract,
                    element_type,
                })
            }
        }
    }
}

impl<'de> Deserialize<'de> for FieldType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = FieldTypeRaw::deserialize(deserializer)?;
        FieldType::try_from(raw).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PrimType {
    String,
    Integer,
    Boolean,
    Mapping,
    #[serde(alias = "array")]
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

// ── Instance validation types (Slice 1) ─────────────────────────

/// Violation code for instance-level validation errors. These are
/// distinct from the boot-time `ContractViolation` which describes
/// shape-to-shape compatibility between schemas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceViolationCode {
    /// Root value type doesn't match the contract root type.
    RootTypeMismatch,
    /// A required field is missing from the value.
    MissingRequiredField,
    /// A field has the wrong primitive type.
    TypeMismatch,
    /// A string field has a value not in the declared enum set.
    EnumMismatch,
    /// A sequence element fails the declared element-type contract.
    SequenceElementMismatch,
    /// A nested mapping field fails its declared sub-contract.
    NestedViolation,
    /// A field is present in the value but not in the contract.
    UnexpectedField,
}

impl std::fmt::Display for InstanceViolationCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RootTypeMismatch => write!(f, "root_type_mismatch"),
            Self::MissingRequiredField => write!(f, "missing_required_field"),
            Self::TypeMismatch => write!(f, "type_mismatch"),
            Self::EnumMismatch => write!(f, "enum_mismatch"),
            Self::SequenceElementMismatch => write!(f, "sequence_element_mismatch"),
            Self::NestedViolation => write!(f, "nested_violation"),
            Self::UnexpectedField => write!(f, "unexpected_field"),
        }
    }
}

/// A single violation found during instance validation. Carries a
/// dotted path, a machine-readable code, and human-readable
/// expected/found descriptions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceViolation {
    /// Dotted path to the failing value, e.g. `"launch.mode"`,
    /// `"affordances[0].id"`.
    pub path: String,
    /// Machine-readable violation classification.
    pub code: InstanceViolationCode,
    /// Human-readable description of what was expected.
    pub expected: String,
    /// Human-readable description of what was found.
    pub found: String,
}

/// Result of validating a concrete `serde_json::Value` against a
/// `ValueShape`. Errors block resolution; warnings are author-facing
/// lint output.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InstanceValidationReport {
    /// Blocking violations that prevent resolution.
    pub errors: Vec<InstanceViolation>,
    /// Non-blocking warnings for author feedback.
    pub warnings: Vec<InstanceViolation>,
}

impl InstanceValidationReport {
    /// Returns true if there are no blocking errors.
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

impl std::fmt::Display for InstanceValidationReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} error(s), {} warning(s)",
            self.errors.len(),
            self.warnings.len()
        )?;
        for v in &self.errors {
            write!(
                f,
                "\n  [{}] {}: expected {}, found {}",
                v.code, v.path, v.expected, v.found
            )?;
        }
        for v in &self.warnings {
            write!(
                f,
                "\n  [{}] {}: expected {}, found {}",
                v.code, v.path, v.expected, v.found
            )?;
        }
        Ok(())
    }
}

/// Helper to format a JSON value type as a human-readable string.
fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn is_json_integer(value: &serde_json::Value) -> bool {
    matches!(value, Value::Number(n) if n.is_i64() || n.is_u64())
}

/// Helper to format an enum's allowed values for error messages.
fn format_enum_values(values: &[String]) -> String {
    format!(
        "one of [{}]",
        values
            .iter()
            .map(|v| format!("{:?}", v))
            .collect::<Vec<_>>()
            .join(", ")
    )
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
            strict_fields: None,
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
///
/// Extended semantics for rich FieldType features:
///   * **Enum values:** Producer enum set must be a subset of the
///     consumer enum set (if both declare enums). If the consumer
///     declares enums and the producer does not, the producer's values
///     are unconstrained → rejected.
///   * **Nested contracts:** Both sides must be Mapping; the producer's
///     nested contract must satisfy the consumer's nested contract
///     (recursive subset check).
///   * **Element types:** Both sides must be Sequence; the producer's
///     element type must satisfy the consumer's element type.
pub(crate) fn field_type_covers(consumer: &FieldType, producer: &FieldType) -> bool {
    match (consumer, producer) {
        (FieldType::Single { prim: c_prim, .. }, FieldType::Single { prim: p_prim, .. }) => {
            if *c_prim == PrimType::Any {
                return true;
            }
            if *p_prim == PrimType::Any {
                return false;
            }
            if c_prim != p_prim {
                return false;
            }
            // Same prim — check rich-field features.
            field_single_covers(consumer, producer)
        }
        (FieldType::Single { .. }, _) if single_has_rich_constraints(consumer) => {
            // A primitive-only union producer cannot prove richer
            // constraints such as enum membership, nested mapping
            // fields, or sequence element type. Reject rather than
            // falling back to primitive-set comparison.
            false
        }
        _ => {
            // Union handling — fall back to primitive-set comparison.
            let consumer_set: Vec<PrimType> = match consumer {
                FieldType::Single { prim, .. } => vec![*prim],
                FieldType::Union { prims } => prims.clone(),
            };
            let producer_set: Vec<PrimType> = match producer {
                FieldType::Single { prim, .. } => vec![*prim],
                FieldType::Union { prims } => prims.clone(),
            };
            if consumer_set.contains(&PrimType::Any) {
                return true;
            }
            producer_set.iter().all(|p| consumer_set.contains(p))
        }
    }
}

fn single_has_rich_constraints(ft: &FieldType) -> bool {
    matches!(
        ft,
        FieldType::Single {
            enum_values: Some(_),
            ..
        } | FieldType::Single {
            nested_contract: Some(_),
            ..
        } | FieldType::Single {
            element_type: Some(_),
            ..
        }
    )
}

/// Subset check for two `Single` variants with the *same* `PrimType`.
fn field_single_covers(consumer: &FieldType, producer: &FieldType) -> bool {
    match (consumer, producer) {
        (
            FieldType::Single {
                enum_values: c_enum,
                nested_contract: c_nested,
                element_type: c_elem,
                ..
            },
            FieldType::Single {
                enum_values: p_enum,
                nested_contract: p_nested,
                element_type: p_elem,
                ..
            },
        ) => {
            // Enum subset: producer's allowed values must be a subset
            // of consumer's. If consumer declares enums, producer must
            // also declare enums (or at least a compatible subset).
            if let (Some(c_vals), Some(p_vals)) = (c_enum, p_enum) {
                if !p_vals.iter().all(|v| c_vals.contains(v)) {
                    return false;
                }
            } else if c_enum.is_some() && p_enum.is_none() {
                // Consumer restricts to specific values but producer
                // makes no claim → unsound.
                return false;
            }

            // Nested contract subset (Mapping fields).
            if let (Some(c_contract), Some(p_contract)) = (c_nested, p_nested) {
                if !c_contract.is_satisfied_by(p_contract).is_empty() {
                    return false;
                }
            } else if c_nested.is_some() && p_nested.is_none() {
                // Consumer requires a sub-field structure but producer
                // doesn't declare one.
                return false;
            }

            // Element type subset (Sequence fields).
            if let (Some(c_elem), Some(p_elem)) = (c_elem, p_elem) {
                if !field_type_covers(c_elem, p_elem) {
                    return false;
                }
            } else if c_elem.is_some() && p_elem.is_none() {
                return false;
            }

            true
        }
        _ => unreachable!("field_single_covers called with non-Single variants"),
    }
}

// ── Instance validation (Slice 1) ─────────────────────────────────

impl ValueShape {
    /// Validate a concrete `serde_json::Value` against this shape.
    /// Returns an `InstanceValidationReport` with errors (blocking)
    /// and warnings (non-blocking).
    ///
    /// This is the runtime equivalent of `is_satisfied_by` (which
    /// compares two shapes). Here we check a *shape* against a
    /// *value*.
    pub fn validate_instance(&self, value: &Value) -> InstanceValidationReport {
        let mut report = InstanceValidationReport::default();
        validate_value(self, value, &[], &mut report);
        report
    }
}

/// Recursive instance validation. Walks the shape and value together,
/// accumulating errors and warnings into the report.
fn validate_value(
    shape: &ValueShape,
    value: &Value,
    path: &[&str],
    report: &mut InstanceValidationReport,
) {
    /// Build a dotted path from a parent path slice and a field name.
    /// Avoids a leading dot when the parent path is empty.
    fn build_path(parent: &[&str], name: &str) -> String {
        if parent.is_empty() {
            name.to_string()
        } else {
            format!("{}.{}", parent.join("."), name)
        }
    }

    match shape.root_type {
        ShapeType::Any => {
            // Any root accepts anything — no validation.
        }
        ShapeType::Mapping => {
            if !value.is_object() {
                report.errors.push(InstanceViolation {
                    path: path.join("."),
                    code: InstanceViolationCode::RootTypeMismatch,
                    expected: "object".to_string(),
                    found: json_type_name(value).to_string(),
                });
                return;
            }
            let obj = value.as_object().unwrap();

            // Check required fields.
            for (name, ft) in &shape.required {
                match obj.get(name) {
                    Some(val) => {
                        validate_field(val, ft, path, name, report);
                    }
                    None => {
                        report.errors.push(InstanceViolation {
                            path: build_path(path, name),
                            code: InstanceViolationCode::MissingRequiredField,
                            expected: "field to be present".to_string(),
                            found: "field is missing".to_string(),
                        });
                    }
                }
            }

            // Check optional fields (if present, validate their shape).
            for (name, ft) in &shape.optional {
                if let Some(val) = obj.get(name) {
                    validate_field(val, ft, path, name, report);
                }
            }

            // Unknown fields → warnings when strict_fields is enabled.
            if shape.strict_fields == Some(StrictFieldsPolicy::Warn) {
                for key in obj.keys() {
                    if !shape.required.contains_key(key.as_str())
                        && !shape.optional.contains_key(key.as_str())
                    {
                        report.warnings.push(InstanceViolation {
                            path: build_path(path, key),
                            code: InstanceViolationCode::UnexpectedField,
                            expected: "known field".to_string(),
                            found: format!("unknown field {:?}", key),
                        });
                    }
                }
            }
        }
        ShapeType::Sequence => {
            if !value.is_array() {
                report.errors.push(InstanceViolation {
                    path: path.join("."),
                    code: InstanceViolationCode::RootTypeMismatch,
                    expected: "array".to_string(),
                    found: json_type_name(value).to_string(),
                });
                return;
            }
            // If all elements should be validated, check each one.
            // This is handled per-field via `validate_field` with
            // element_type, so we don't need top-level element
            // validation here.
        }
        ShapeType::Scalar => {
            if value.is_object() || value.is_array() {
                report.errors.push(InstanceViolation {
                    path: path.join("."),
                    code: InstanceViolationCode::RootTypeMismatch,
                    expected: "scalar (string, number, boolean, null)".to_string(),
                    found: json_type_name(value).to_string(),
                });
            }
        }
    }
}

/// Validate a single field's value against its `FieldType`.
fn validate_field(
    value: &Value,
    ft: &FieldType,
    parent_path: &[&str],
    field_name: &str,
    report: &mut InstanceValidationReport,
) {
    let field_path = if parent_path.is_empty() {
        field_name.to_string()
    } else {
        format!("{}.{}", parent_path.join("."), field_name)
    };

    match ft {
        FieldType::Single {
            prim,
            enum_values,
            nested_contract,
            element_type,
        } => {
            // Check enum constraint.
            if let Some(ref allowed) = enum_values {
                if let Some(s) = value.as_str() {
                    if !allowed.contains(&s.to_string()) {
                        report.errors.push(InstanceViolation {
                            path: field_path.clone(),
                            code: InstanceViolationCode::EnumMismatch,
                            expected: format_enum_values(allowed),
                            found: format!("{:?}", s),
                        });
                        return; // Don't cascade type errors.
                    }
                } else {
                    report.errors.push(InstanceViolation {
                        path: field_path.clone(),
                        code: InstanceViolationCode::EnumMismatch,
                        expected: "string value".to_string(),
                        found: format!("{} ({})", json_type_name(value), value),
                    });
                    return;
                }
            }

            // Check primitive type (skip if enum already matched).
            let prim_match = match prim {
                PrimType::String => value.is_string(),
                PrimType::Integer => is_json_integer(value),
                PrimType::Boolean => value.is_boolean(),
                PrimType::Mapping => value.is_object(),
                PrimType::Sequence => value.is_array(),
                PrimType::Null => value.is_null(),
                PrimType::Any => true,
            };
            if !prim_match {
                report.errors.push(InstanceViolation {
                    path: field_path.clone(),
                    code: InstanceViolationCode::TypeMismatch,
                    expected: format!("{:?}", prim),
                    found: json_type_name(value).to_string(),
                });
                return;
            }

            // Recurse into nested contract.
            if let Some(ref contract) = nested_contract {
                validate_value(contract, value, &[field_path.as_str()], report);
            }

            // Validate sequence elements.
            if let Some(ref elem_ft) = element_type {
                if let Some(arr) = value.as_array() {
                    for (i, elem) in arr.iter().enumerate() {
                        let elem_name = format!("{}[{}]", field_name, i);
                        validate_field(elem, elem_ft, parent_path, &elem_name, report);
                    }
                }
                // If value is not an array but prim == Sequence, we
                // already reported a TypeMismatch above.
            }
        }
        FieldType::Union { prims } => {
            let matches = match value {
                Value::Null => prims.contains(&PrimType::Null),
                Value::Bool(_) => prims.contains(&PrimType::Boolean),
                Value::Number(n) => {
                    prims.contains(&PrimType::Integer) && (n.is_i64() || n.is_u64())
                }
                Value::String(_) => prims.contains(&PrimType::String),
                Value::Array(_) => prims.contains(&PrimType::Sequence),
                Value::Object(_) => prims.contains(&PrimType::Mapping),
            };
            if !matches {
                report.errors.push(InstanceViolation {
                    path: field_path.clone(),
                    code: InstanceViolationCode::TypeMismatch,
                    expected: format!(
                        "one of [{}]",
                        prims
                            .iter()
                            .map(|p| format!("{:?}", p))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    found: json_type_name(value).to_string(),
                });
            }
        }
    }
}

#[cfg(test)]
mod value_shape_tests {
    use super::*;

    fn ft_string() -> FieldType {
        FieldType::Single {
            prim: PrimType::String,
            enum_values: None,
            nested_contract: None,
            element_type: None,
        }
    }

    fn ft_integer() -> FieldType {
        FieldType::Single {
            prim: PrimType::Integer,
            enum_values: None,
            nested_contract: None,
            element_type: None,
        }
    }

    fn ft_mapping() -> FieldType {
        FieldType::Single {
            prim: PrimType::Mapping,
            enum_values: None,
            nested_contract: None,
            element_type: None,
        }
    }

    fn ft_sequence() -> FieldType {
        FieldType::Single {
            prim: PrimType::Sequence,
            enum_values: None,
            nested_contract: None,
            element_type: None,
        }
    }

    fn ft_any() -> FieldType {
        FieldType::Single {
            prim: PrimType::Any,
            enum_values: None,
            nested_contract: None,
            element_type: None,
        }
    }

    fn ft_null() -> FieldType {
        FieldType::Single {
            prim: PrimType::Null,
            enum_values: None,
            nested_contract: None,
            element_type: None,
        }
    }

    fn ft_union(prims: &[PrimType]) -> FieldType {
        FieldType::Union {
            prims: prims.to_vec(),
        }
    }

    fn ft_string_enum(values: &[&str]) -> FieldType {
        FieldType::Single {
            prim: PrimType::String,
            enum_values: Some(values.iter().map(|s| s.to_string()).collect()),
            nested_contract: None,
            element_type: None,
        }
    }

    fn ft_sequence_of(element: FieldType) -> FieldType {
        FieldType::Single {
            prim: PrimType::Sequence,
            enum_values: None,
            nested_contract: None,
            element_type: Some(Box::new(element)),
        }
    }

    fn ft_mapping_with(contract: ValueShape) -> FieldType {
        FieldType::Single {
            prim: PrimType::Mapping,
            enum_values: None,
            nested_contract: Some(Box::new(contract)),
            element_type: None,
        }
    }

    fn shape_mapping(required: &[(&str, FieldType)], optional: &[(&str, FieldType)]) -> ValueShape {
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
            strict_fields: None,
        }
    }

    #[test]
    fn identical_shapes_pass() {
        let a = shape_mapping(&[("body", ft_string())], &[]);
        let b = a.clone();
        assert!(a.is_satisfied_by(&b).is_empty());
    }

    #[test]
    fn missing_required_field_detected() {
        let consumer = shape_mapping(&[("body", ft_string())], &[]);
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
        let consumer = shape_mapping(&[("body", ft_string())], &[]);
        let producer = shape_mapping(&[], &[("body", ft_string())]);
        let v = consumer.is_satisfied_by(&producer);
        assert_eq!(
            v.len(),
            1,
            "expected one missing-required violation, got {v:?}"
        );
        match &v[0] {
            ContractViolation::MissingRequiredField {
                name,
                produced_as_optional,
                ..
            } => {
                assert_eq!(name, "body");
                assert_eq!(
                    produced_as_optional,
                    &Some(ft_string()),
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
        let consumer = shape_mapping(&[("body", ft_string())], &[]);
        let producer = shape_mapping(&[("body", ft_string())], &[]);
        assert!(consumer.is_satisfied_by(&producer).is_empty());
    }

    #[test]
    fn missing_required_with_no_optional_hint() {
        let consumer = shape_mapping(&[("body", ft_string())], &[]);
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
        let consumer = shape_mapping(&[("body", ft_string())], &[]);
        let producer = shape_mapping(&[("body", ft_integer())], &[]);
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
            strict_fields: None,
        };
        let producer_mapping = shape_mapping(&[("body", ft_string())], &[]);
        let producer_seq = ValueShape {
            root_type: ShapeType::Sequence,
            required: Default::default(),
            optional: Default::default(),
            strict_fields: None,
        };
        let producer_root_any = ValueShape {
            root_type: ShapeType::Any,
            required: Default::default(),
            optional: Default::default(),
            strict_fields: None,
        };
        assert!(consumer_root_any
            .is_satisfied_by(&producer_mapping)
            .is_empty());
        assert!(consumer_root_any.is_satisfied_by(&producer_seq).is_empty());
        assert!(consumer_root_any
            .is_satisfied_by(&producer_root_any)
            .is_empty());

        // Consumer field-level Any: accepts any producer field type.
        let consumer = shape_mapping(&[("body", ft_any())], &[]);
        let producer_str = shape_mapping(&[("body", ft_string())], &[]);
        let producer_int = shape_mapping(&[("body", ft_integer())], &[]);
        let producer_any = shape_mapping(&[("body", ft_any())], &[]);
        assert!(consumer.is_satisfied_by(&producer_str).is_empty());
        assert!(consumer.is_satisfied_by(&producer_int).is_empty());
        assert!(consumer.is_satisfied_by(&producer_any).is_empty());
    }

    #[test]
    fn producer_any_does_not_satisfy_specific_consumer() {
        // Specific consumer + producer Any at root → RootTypeMismatch.
        let consumer = shape_mapping(&[("body", ft_string())], &[]);
        let producer_root_any = ValueShape {
            root_type: ShapeType::Any,
            required: Default::default(),
            optional: Default::default(),
            strict_fields: None,
        };
        let v = consumer.is_satisfied_by(&producer_root_any);
        assert!(
            v.iter().any(|x| matches!(
                x,
                ContractViolation::RootTypeMismatch {
                    needed: ShapeType::Mapping,
                    produced: ShapeType::Any
                }
            )),
            "producer ShapeType::Any must NOT satisfy a Mapping consumer; got {v:?}"
        );

        // Specific consumer field + producer Any field → FieldTypeMismatch.
        let producer = shape_mapping(&[("body", ft_any())], &[]);
        let v = consumer.is_satisfied_by(&producer);
        assert_eq!(v.len(), 1);
        assert!(
            matches!(v[0], ContractViolation::FieldTypeMismatch { .. }),
            "producer PrimType::Any must NOT satisfy a String consumer; got {v:?}"
        );

        // Producer Any inside a union also rejected by specific consumer.
        let producer_union_any = shape_mapping(
            &[(
                "body",
                FieldType::Union {
                    prims: vec![PrimType::String, PrimType::Any],
                },
            )],
            &[],
        );
        let v = consumer.is_satisfied_by(&producer_union_any);
        assert!(
            v.iter()
                .any(|x| matches!(x, ContractViolation::FieldTypeMismatch { .. })),
            "producer union containing Any must be rejected; got {v:?}"
        );
    }

    #[test]
    fn union_consumer_accepts_member_producer() {
        let consumer = shape_mapping(
            &[(
                "extends",
                FieldType::Union {
                    prims: vec![PrimType::String, PrimType::Null],
                },
            )],
            &[],
        );
        let producer_string = shape_mapping(&[("extends", ft_string())], &[]);
        let producer_null = shape_mapping(&[("extends", ft_null())], &[]);
        let producer_int = shape_mapping(&[("extends", ft_integer())], &[]);
        assert!(consumer.is_satisfied_by(&producer_string).is_empty());
        assert!(consumer.is_satisfied_by(&producer_null).is_empty());
        assert!(!consumer.is_satisfied_by(&producer_int).is_empty());
    }

    #[test]
    fn union_producer_subset_of_union_consumer() {
        let consumer = shape_mapping(
            &[(
                "x",
                FieldType::Union {
                    prims: vec![PrimType::String, PrimType::Null],
                },
            )],
            &[],
        );
        let producer = shape_mapping(
            &[(
                "x",
                FieldType::Union {
                    prims: vec![PrimType::String, PrimType::Null],
                },
            )],
            &[],
        );
        assert!(consumer.is_satisfied_by(&producer).is_empty());

        // Producer might emit Integer too — not in consumer's set.
        let producer_wider = shape_mapping(
            &[(
                "x",
                FieldType::Union {
                    prims: vec![PrimType::String, PrimType::Null, PrimType::Integer],
                },
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
            strict_fields: None,
        };
        let producer = ValueShape {
            root_type: ShapeType::Sequence,
            required: Default::default(),
            optional: Default::default(),
            strict_fields: None,
        };
        let v = consumer.is_satisfied_by(&producer);
        assert!(v
            .iter()
            .any(|x| matches!(x, ContractViolation::RootTypeMismatch { .. })));
    }

    #[test]
    fn all_violations_returned_not_bailing_on_first() {
        let consumer = shape_mapping(
            &[("a", ft_string()), ("b", ft_string()), ("c", ft_string())],
            &[],
        );
        let producer = ValueShape {
            root_type: ShapeType::Sequence,
            required: Default::default(),
            optional: Default::default(),
            strict_fields: None,
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
  body: { type: single, prim: string }
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
  body: { type: single, prim: string }
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
  body: { type: single, prim: string }
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
  body: { type: union, prims: [] }
";
        let err = serde_yaml::from_str::<ValueShape>(yaml).unwrap_err();
        assert!(format!("{err}").contains("empty union"), "got: {err}");
    }

    #[test]
    fn deserialize_rejects_empty_union_in_optional() {
        let yaml = "\
root_type: mapping
optional:
  extends: { type: union, prims: [] }
";
        let err = serde_yaml::from_str::<ValueShape>(yaml).unwrap_err();
        assert!(format!("{err}").contains("empty union"), "got: {err}");
    }

    #[test]
    fn deserialize_accepts_well_formed_shape() {
        // Sanity: the live bundle's directive contract round-trips
        // through the strict deserializer cleanly.
        let yaml = "\
root_type: mapping
required:
  body: { type: single, prim: string }
optional:
  extends: { type: union, prims: [string, \"null\"] }
  requires: { type: single, prim: mapping }
  context: { type: single, prim: mapping }
";
        let shape: ValueShape = serde_yaml::from_str(yaml).expect("well-formed shape parses");
        assert_eq!(shape.root_type, ShapeType::Mapping);
        assert_eq!(shape.required.get("body").unwrap(), &ft_string());
        assert_eq!(
            shape.optional.get("extends").unwrap(),
            &FieldType::Union {
                prims: vec![PrimType::String, PrimType::Null]
            }
        );
    }

    // ── Slice 0: DSL extension tests ──────────────────────────────

    #[test]
    fn nested_mapping_field_parses() {
        let yaml = "\
root_type: mapping
required:
  launch:
    type: single
    prim: mapping
    contract:
      root_type: mapping
      required:
        mode: { type: single, prim: string }
        binary_ref: { type: single, prim: string }
      optional:
        args: { type: single, prim: mapping }
";
        let shape: ValueShape = serde_yaml::from_str(yaml).expect("nested mapping parses");
        let launch = shape.required.get("launch").unwrap();
        assert!(matches!(
            launch,
            FieldType::Single {
                prim: PrimType::Mapping,
                ..
            }
        ));
        if let FieldType::Single {
            nested_contract: Some(contract),
            ..
        } = launch
        {
            assert!(contract.required.contains_key("mode"));
            assert!(contract.required.contains_key("binary_ref"));
            assert!(contract.optional.contains_key("args"));
        } else {
            panic!("expected nested_contract on launch field");
        }
    }

    #[test]
    fn enum_constraint_field_parses() {
        let yaml = "\
root_type: mapping
required:
  mode: { type: single, prim: string, enum: [cli_exec, daemon_ui] }
";
        let shape: ValueShape = serde_yaml::from_str(yaml).expect("enum field parses");
        let mode = shape.required.get("mode").unwrap();
        if let FieldType::Single {
            enum_values: Some(ref vals),
            ..
        } = mode
        {
            assert_eq!(vals, &vec!["cli_exec".to_string(), "daemon_ui".to_string()]);
        } else {
            panic!("expected enum_values on mode field");
        }
    }

    #[test]
    fn typed_sequence_element_parses() {
        let yaml = "\
root_type: mapping
optional:
  items:
    type: single
    prim: sequence
    elements: { type: single, prim: string }
";
        let shape: ValueShape = serde_yaml::from_str(yaml).expect("typed sequence parses");
        let items = shape.optional.get("items").unwrap();
        if let FieldType::Single {
            element_type: Some(ref elem),
            ..
        } = items
        {
            assert_eq!(**elem, ft_string());
        } else {
            panic!("expected element_type on items field");
        }
    }

    #[test]
    fn strict_fields_warn_parses() {
        let yaml = "\
root_type: mapping
required: {}
strict_fields: warn
";
        let shape: ValueShape = serde_yaml::from_str(yaml).expect("strict_fields: warn parses");
        assert_eq!(shape.strict_fields, Some(StrictFieldsPolicy::Warn));
    }

    #[test]
    fn strict_fields_defaults_to_none() {
        let yaml = "\
root_type: mapping
required: {}
";
        let shape: ValueShape =
            serde_yaml::from_str(yaml).expect("shape without strict_fields parses");
        assert_eq!(shape.strict_fields, None);
    }

    #[test]
    fn enum_on_non_string_rejected() {
        let yaml = "\
root_type: mapping
required:
  count: { type: single, prim: integer, enum: [\"a\", \"b\"] }
";
        let err = serde_yaml::from_str::<ValueShape>(yaml).unwrap_err();
        assert!(
            format!("{err}").contains("`enum` is only valid on `prim: string`"),
            "got: {err}"
        );
    }

    #[test]
    fn contract_on_non_mapping_rejected() {
        let yaml = "\
root_type: mapping
required:
  tags:
    type: single
    prim: sequence
    contract:
      root_type: mapping
      required: {}
";
        let err = serde_yaml::from_str::<ValueShape>(yaml).unwrap_err();
        assert!(
            format!("{err}").contains("`contract` is only valid on `prim: mapping`"),
            "got: {err}"
        );
    }

    #[test]
    fn elements_on_non_sequence_rejected() {
        let yaml = "\
root_type: mapping
required:
  name:
    type: single
    prim: string
    elements: { type: single, prim: string }
";
        let err = serde_yaml::from_str::<ValueShape>(yaml).unwrap_err();
        assert!(
            format!("{err}").contains("`elements` is only valid on `prim: sequence`"),
            "got: {err}"
        );
    }

    #[test]
    fn empty_enum_rejected() {
        let yaml = "\
root_type: mapping
required:
  mode: { type: single, prim: string, enum: [] }
";
        let err = serde_yaml::from_str::<ValueShape>(yaml).unwrap_err();
        assert!(
            format!("{err}").contains("`enum` list must not be empty"),
            "got: {err}"
        );
    }

    #[test]
    fn existing_kind_schemas_parse_unchanged() {
        // All existing kind schemas must parse with the new
        // FieldType format. The new fields are all optional.
        let schemas = [
            // directive
            "\
root_type: mapping
required:
  body: { type: single, prim: string }
optional:
  extends: { type: union, prims: [string, \"null\"] }
  requires: { type: single, prim: mapping }
  context: { type: single, prim: mapping }
",
            // surface
            "\
root_type: mapping
required:
  layout: { type: single, prim: mapping }
optional:
  extends: { type: union, prims: [string, \"null\"] }
  input: { type: single, prim: mapping }
  ambient: { type: single, prim: mapping }
  affordances: { type: single, prim: array }
  instruments: { type: single, prim: array }
  capabilities: { type: single, prim: mapping }
",
            // client
            "\
root_type: mapping
required:
  launch: { type: single, prim: mapping }
  serves: { type: single, prim: mapping }
optional:
  version: { type: single, prim: string }
  description: { type: single, prim: string }
  capabilities: { type: single, prim: mapping }
",
            // tool (empty contract)
            "\
root_type: mapping
required: {}
",
            // service (empty contract)
            "\
root_type: mapping
required: {}
",
        ];
        for (i, yaml) in schemas.iter().enumerate() {
            let result = serde_yaml::from_str::<ValueShape>(yaml);
            assert!(result.is_ok(), "schema {i} failed to parse: {result:?}");
        }
    }

    #[test]
    fn subset_semantics_with_enum_values() {
        // Consumer declares enum → producer must be a subset.
        let consumer = shape_mapping(&[("mode", ft_string_enum(&["cli_exec", "daemon_ui"]))], &[]);
        let producer_subset = shape_mapping(&[("mode", ft_string_enum(&["cli_exec"]))], &[]);
        let producer_same =
            shape_mapping(&[("mode", ft_string_enum(&["cli_exec", "daemon_ui"]))], &[]);
        let producer_wider = shape_mapping(
            &[("mode", ft_string_enum(&["cli_exec", "daemon_ui", "web"]))],
            &[],
        );
        // Producer without enum but consumer with enum → unsound.
        let producer_no_enum = shape_mapping(&[("mode", ft_string())], &[]);

        assert!(
            consumer.is_satisfied_by(&producer_subset).is_empty(),
            "subset should pass"
        );
        assert!(
            consumer.is_satisfied_by(&producer_same).is_empty(),
            "same should pass"
        );
        assert!(
            !consumer.is_satisfied_by(&producer_wider).is_empty(),
            "wider should fail"
        );
        assert!(
            !consumer.is_satisfied_by(&producer_no_enum).is_empty(),
            "no-enum should fail"
        );
    }

    #[test]
    fn subset_semantics_with_nested_contracts() {
        let nested_consumer =
            shape_mapping(&[("mode", ft_string_enum(&["cli_exec", "daemon_ui"]))], &[]);
        let nested_producer = shape_mapping(&[("mode", ft_string_enum(&["cli_exec"]))], &[]);
        let consumer = shape_mapping(&[("launch", ft_mapping_with(nested_consumer))], &[]);
        let producer_ok = shape_mapping(&[("launch", ft_mapping_with(nested_producer))], &[]);
        // Producer with plain mapping (no nested contract) → unsound.
        let producer_plain = shape_mapping(&[("launch", ft_mapping())], &[]);
        // Producer missing launch entirely.
        let producer_missing = shape_mapping(&[], &[]);

        assert!(
            consumer.is_satisfied_by(&producer_ok).is_empty(),
            "nested subset should pass"
        );
        assert!(
            !consumer.is_satisfied_by(&producer_plain).is_empty(),
            "plain mapping should fail"
        );
        assert!(
            !consumer.is_satisfied_by(&producer_missing).is_empty(),
            "missing field should fail"
        );
    }

    #[test]
    fn subset_semantics_with_element_types() {
        // is_satisfied_by only checks required fields, so put the
        // sequence field in required to test element-type subset
        // semantics.
        let consumer = shape_mapping(&[("tags", ft_sequence_of(ft_string()))], &[]);
        let producer_same = shape_mapping(&[("tags", ft_sequence_of(ft_string()))], &[]);
        let producer_wider_elem = shape_mapping(&[("tags", ft_sequence_of(ft_any()))], &[]);
        let producer_plain_seq = shape_mapping(&[("tags", ft_sequence())], &[]);
        let producer_wrong_elem = shape_mapping(&[("tags", ft_sequence_of(ft_integer()))], &[]);

        assert!(
            consumer.is_satisfied_by(&producer_same).is_empty(),
            "same element type should pass"
        );
        assert!(
            !consumer.is_satisfied_by(&producer_wider_elem).is_empty(),
            "any element type should fail (producer makes no claim)"
        );
        assert!(
            !consumer.is_satisfied_by(&producer_plain_seq).is_empty(),
            "plain sequence without element type should fail"
        );
        assert!(
            !consumer.is_satisfied_by(&producer_wrong_elem).is_empty(),
            "wrong element type should fail"
        );
    }

    #[test]
    fn constrained_enum_consumer_rejects_union_producer() {
        let consumer = shape_mapping(&[("mode", ft_string_enum(&["cli_exec", "daemon_ui"]))], &[]);
        let producer_union = shape_mapping(&[("mode", ft_union(&[PrimType::String]))], &[]);

        assert!(
            !consumer.is_satisfied_by(&producer_union).is_empty(),
            "primitive-only union producer must not satisfy enum-constrained consumer"
        );
    }

    #[test]
    fn constrained_nested_contract_consumer_rejects_union_producer() {
        let nested_consumer = shape_mapping(&[("mode", ft_string())], &[]);
        let consumer = shape_mapping(&[("launch", ft_mapping_with(nested_consumer))], &[]);
        let producer_union = shape_mapping(&[("launch", ft_union(&[PrimType::Mapping]))], &[]);

        assert!(
            !consumer.is_satisfied_by(&producer_union).is_empty(),
            "primitive-only union producer must not satisfy nested-contract consumer"
        );
    }

    #[test]
    fn constrained_element_type_consumer_rejects_union_producer() {
        let consumer = shape_mapping(&[("tags", ft_sequence_of(ft_string()))], &[]);
        let producer_union = shape_mapping(&[("tags", ft_union(&[PrimType::Sequence]))], &[]);

        assert!(
            !consumer.is_satisfied_by(&producer_union).is_empty(),
            "primitive-only union producer must not satisfy element-constrained consumer"
        );
    }

    #[test]
    fn deep_nesting_two_levels() {
        // Nested mapping inside nested mapping: launch.config.env
        let yaml = "\
root_type: mapping
required:
  launch:
    type: single
    prim: mapping
    contract:
      root_type: mapping
      required:
        mode: { type: single, prim: string, enum: [cli_exec, daemon_ui] }
      optional:
        config:
          type: single
          prim: mapping
          contract:
            root_type: mapping
            required: {}
            optional:
              env:
                type: single
                prim: sequence
                elements:
                  type: single
                  prim: mapping
                  contract:
                    root_type: mapping
                    required:
                      key: { type: single, prim: string }
                      value: { type: single, prim: string }
                    optional: {}
";
        let shape: ValueShape = serde_yaml::from_str(yaml).expect("deep nesting parses");

        // Spot-check: launch has a nested contract with mode enum.
        let launch = shape.required.get("launch").unwrap();
        if let FieldType::Single {
            nested_contract: Some(ref contract),
            ..
        } = launch
        {
            let mode = contract.required.get("mode").unwrap();
            assert_eq!(mode, &ft_string_enum(&["cli_exec", "daemon_ui"]));

            // config is optional with its own nested contract.
            let config = contract.optional.get("config").unwrap();
            if let FieldType::Single {
                nested_contract: Some(ref cfg_contract),
                ..
            } = config
            {
                let env = cfg_contract.optional.get("env").unwrap();
                if let FieldType::Single {
                    element_type: Some(ref elem),
                    ..
                } = env
                {
                    // Element type is a mapping with key+value required.
                    if let FieldType::Single {
                        nested_contract: Some(ref env_contract),
                        ..
                    } = elem.as_ref()
                    {
                        assert!(env_contract.required.contains_key("key"));
                        assert!(env_contract.required.contains_key("value"));
                    } else {
                        panic!("env element should have nested contract");
                    }
                } else {
                    panic!("env should have element_type");
                }
            } else {
                panic!("config should have nested_contract");
            }
        } else {
            panic!("launch should have nested_contract");
        }
    }

    #[test]
    fn roundtrip_serialization() {
        // A rich shape round-trips through serialize → deserialize.
        let shape = shape_mapping(
            &[(
                "launch",
                FieldType::Single {
                    prim: PrimType::Mapping,
                    enum_values: None,
                    nested_contract: Some(Box::new(shape_mapping(
                        &[("mode", ft_string_enum(&["cli_exec", "daemon_ui"]))],
                        &[],
                    ))),
                    element_type: None,
                },
            )],
            &[(
                "tags",
                ft_sequence_of(ft_mapping_with(shape_mapping(
                    &[("key", ft_string())],
                    &[("value", ft_string())],
                ))),
            )],
        );
        let yaml = serde_yaml::to_string(&shape).expect("serialize");
        let back: ValueShape = serde_yaml::from_str(&yaml).expect("deserialize roundtrip");
        assert_eq!(shape, back);
    }

    // ── Deserialization edge cases ─────────────────────────────────

    #[test]
    fn unknown_strict_fields_value_rejected() {
        let yaml = "\
root_type: mapping
required: {}
strict_fields: deny
";
        let err = serde_yaml::from_str::<ValueShape>(yaml).unwrap_err();
        assert!(
            format!("{err}").contains("unknown `strict_fields` value"),
            "got: {err}"
        );
    }

    #[test]
    fn typo_in_contract_key_rejected() {
        let yaml = "\
root_type: mapping
required:
  config:
    type: single
    prim: mapping
    conract:
      root_type: mapping
      required: {}
      optional: {}
";
        let err = serde_yaml::from_str::<ValueShape>(yaml).unwrap_err();
        assert!(
            format!("{err}").contains("unknown field `conract`"),
            "got: {err}"
        );
    }

    #[test]
    fn typo_in_elements_key_rejected() {
        let yaml = "\
root_type: mapping
required:
  items:
    type: single
    prim: sequence
    elementz: { type: single, prim: string }
";
        let err = serde_yaml::from_str::<ValueShape>(yaml).unwrap_err();
        assert!(
            format!("{err}").contains("unknown field `elementz`"),
            "got: {err}"
        );
    }

    #[test]
    fn nested_contract_non_mapping_root_rejected() {
        let yaml = "\
root_type: mapping
required:
  config:
    type: single
    prim: mapping
    contract:
      root_type: sequence
      required: {}
      optional: {}
";
        let err = serde_yaml::from_str::<ValueShape>(yaml).unwrap_err();
        assert!(
            format!("{err}").contains("nested `contract` root_type must be mapping"),
            "got: {err}"
        );
    }

    // ── Slice 1: Instance validation tests ───────────────────────────

    fn validate(shape: &ValueShape, value: &Value) -> InstanceValidationReport {
        shape.validate_instance(value)
    }

    #[test]
    fn mapping_root_rejects_missing_required() {
        let shape = shape_mapping(&[("body", ft_string())], &[]);
        let value = serde_json::json!({});
        let report = validate(&shape, &value);
        assert!(!report.is_ok());
        assert_eq!(report.errors.len(), 1);
        assert_eq!(
            report.errors[0].code,
            InstanceViolationCode::MissingRequiredField
        );
        assert!(report.errors[0].path.contains("body"));
    }

    #[test]
    fn mapping_root_allows_missing_optional() {
        let shape = shape_mapping(&[], &[("extra", ft_string())]);
        let value = serde_json::json!({});
        let report = validate(&shape, &value);
        assert!(
            report.is_ok(),
            "missing optional should not error: {report:?}"
        );
    }

    #[test]
    fn nested_mapping_reports_dotted_path() {
        let shape = shape_mapping(
            &[],
            &[(
                "launch",
                ft_mapping_with(shape_mapping(
                    &[("mode", ft_string_enum(&["cli_exec", "daemon_ui"]))],
                    &[],
                )),
            )],
        );
        let value = serde_json::json!({
            "launch": { "mode": "invalid_value" }
        });
        let report = validate(&shape, &value);
        assert!(!report.is_ok());
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].code, InstanceViolationCode::EnumMismatch);
        assert_eq!(report.errors[0].path, "launch.mode");
        assert!(report.errors[0].found.contains("invalid_value"));
    }

    #[test]
    fn nested_mapping_missing_inner_required() {
        let shape = shape_mapping(
            &[],
            &[(
                "config",
                ft_mapping_with(shape_mapping(&[("name", ft_string())], &[])),
            )],
        );
        let value = serde_json::json!({
            "config": {}
        });
        let report = validate(&shape, &value);
        assert!(!report.is_ok());
        assert_eq!(report.errors[0].path, "config.name");
        assert_eq!(
            report.errors[0].code,
            InstanceViolationCode::MissingRequiredField
        );
    }

    #[test]
    fn enum_accepts_valid_value() {
        let shape = shape_mapping(&[("mode", ft_string_enum(&["cli_exec", "daemon_ui"]))], &[]);
        let value = serde_json::json!({ "mode": "cli_exec" });
        let report = validate(&shape, &value);
        assert!(report.is_ok(), "valid enum should pass: {report:?}");
    }

    #[test]
    fn enum_rejects_invalid_value_with_code() {
        let shape = shape_mapping(&[("mode", ft_string_enum(&["cli_exec", "daemon_ui"]))], &[]);
        let value = serde_json::json!({ "mode": "web" });
        let report = validate(&shape, &value);
        assert!(!report.is_ok());
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].code, InstanceViolationCode::EnumMismatch);
    }

    #[test]
    fn enum_rejects_wrong_type() {
        let shape = shape_mapping(&[("mode", ft_string_enum(&["cli_exec", "daemon_ui"]))], &[]);
        let value = serde_json::json!({ "mode": 42 });
        let report = validate(&shape, &value);
        assert!(!report.is_ok());
        assert_eq!(report.errors[0].code, InstanceViolationCode::EnumMismatch);
    }

    #[test]
    fn enum_on_non_string_value_reports_type_mismatch() {
        let shape = shape_mapping(&[("mode", ft_string_enum(&["cli_exec", "daemon_ui"]))], &[]);
        let value = serde_json::json!({ "mode": [1, 2] });
        let report = validate(&shape, &value);
        assert!(!report.is_ok());
        assert_eq!(report.errors[0].code, InstanceViolationCode::EnumMismatch);
    }

    #[test]
    fn integer_rejects_fractional_number() {
        let shape = shape_mapping(&[("count", ft_integer())], &[]);
        let report = validate(&shape, &serde_json::json!({ "count": 1.5 }));

        assert!(!report.is_ok());
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].path, "count");
        assert_eq!(report.errors[0].code, InstanceViolationCode::TypeMismatch);
    }

    #[test]
    fn sequence_element_index_in_path() {
        let shape = shape_mapping(&[], &[("items", ft_sequence_of(ft_string()))]);
        let value = serde_json::json!({
            "items": ["ok", 42, null]
        });
        let report = validate(&shape, &value);
        assert!(!report.is_ok());
        assert_eq!(report.errors.len(), 2);
        assert_eq!(report.errors[0].path, "items[1]");
        assert_eq!(report.errors[0].code, InstanceViolationCode::TypeMismatch);
        assert_eq!(report.errors[1].path, "items[2]");
        assert_eq!(report.errors[1].code, InstanceViolationCode::TypeMismatch);
    }

    #[test]
    fn sequence_element_mapping_contract_reports_indexed_path() {
        let shape = shape_mapping(
            &[],
            &[(
                "items",
                ft_sequence_of(ft_mapping_with(shape_mapping(&[("id", ft_string())], &[]))),
            )],
        );
        let value = serde_json::json!({
            "items": [
                {"id": "a"},
                {"id": 42}
            ]
        });
        let report = validate(&shape, &value);
        assert!(!report.is_ok());
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].path, "items[1].id");
        assert_eq!(report.errors[0].code, InstanceViolationCode::TypeMismatch);
    }

    #[test]
    fn sequence_allows_empty() {
        let shape = shape_mapping(&[], &[("items", ft_sequence_of(ft_string()))]);
        let value = serde_json::json!({ "items": [] });
        let report = validate(&shape, &value);
        assert!(report.is_ok(), "empty sequence should pass: {report:?}");
    }

    #[test]
    fn union_accepts_any_variant() {
        let shape = shape_mapping(
            &[],
            &[(
                "extends",
                FieldType::Union {
                    prims: vec![PrimType::String, PrimType::Null],
                },
            )],
        );
        let report_str = validate(&shape, &serde_json::json!({"extends": "hello"}));
        assert!(
            report_str.is_ok(),
            "string variant should pass: {report_str:?}"
        );

        let report_null = validate(&shape, &serde_json::json!({"extends": null}));
        assert!(
            report_null.is_ok(),
            "null variant should pass: {report_null:?}"
        );
    }

    #[test]
    fn union_integer_rejects_fractional_number() {
        let shape = shape_mapping(
            &[],
            &[(
                "value",
                FieldType::Union {
                    prims: vec![PrimType::Integer, PrimType::Null],
                },
            )],
        );
        let report = validate(&shape, &serde_json::json!({"value": 1.5}));

        assert!(!report.is_ok());
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].path, "value");
        assert_eq!(report.errors[0].code, InstanceViolationCode::TypeMismatch);
    }

    #[test]
    fn union_rejects_non_variant() {
        let shape = shape_mapping(
            &[],
            &[(
                "extends",
                FieldType::Union {
                    prims: vec![PrimType::String, PrimType::Null],
                },
            )],
        );
        let report = validate(&shape, &serde_json::json!({"extends": 42}));
        assert!(!report.is_ok());
        assert_eq!(report.errors[0].code, InstanceViolationCode::TypeMismatch);
    }

    #[test]
    fn null_in_union_accepted() {
        let shape = shape_mapping(
            &[],
            &[(
                "value",
                FieldType::Union {
                    prims: vec![PrimType::String, PrimType::Null],
                },
            )],
        );
        let report = validate(&shape, &serde_json::json!({"value": null}));
        assert!(report.is_ok(), "null in union should pass: {report:?}");
    }

    #[test]
    fn unexpected_field_is_warning_not_error() {
        // Build via YAML so we can set strict_fields: warn.
        let yaml = "\
root_type: mapping
required:
  body: { type: single, prim: string }
strict_fields: warn
";
        let shape: ValueShape = serde_yaml::from_str(yaml).unwrap();
        let value = serde_json::json!({
            "body": "hello",
            "unknown": "field"
        });
        let report = validate(&shape, &value);
        assert!(report.is_ok(), "unknown field should be warning, not error");
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(
            report.warnings[0].code,
            InstanceViolationCode::UnexpectedField
        );
        assert!(report.warnings[0].path.contains("unknown"));
    }

    #[test]
    fn strict_fields_none_does_not_warn() {
        let shape = shape_mapping(&[("body", ft_string())], &[]);
        // strict_fields is None (default).
        let value = serde_json::json!({
            "body": "hello",
            "unknown": "field"
        });
        let report = validate(&shape, &value);
        assert!(
            report.is_ok(),
            "unknown field with None strict_fields should not warn"
        );
        assert!(
            report.warnings.is_empty(),
            "unexpected warnings: {:?}",
            report.warnings
        );
    }

    #[test]
    fn root_type_mapping_accepts_object() {
        let shape = shape_mapping(&[], &[]);
        let report = validate(&shape, &serde_json::json!({ "key": "val" }));
        assert!(report.is_ok());
    }

    #[test]
    fn root_type_mapping_rejects_string() {
        let shape = shape_mapping(&[], &[]);
        let report = validate(&shape, &serde_json::json!("hello"));
        assert!(!report.is_ok());
        assert_eq!(
            report.errors[0].code,
            InstanceViolationCode::RootTypeMismatch
        );
    }

    #[test]
    fn root_type_mapping_rejects_array() {
        let shape = shape_mapping(&[], &[]);
        let report = validate(&shape, &serde_json::json!([1, 2]));
        assert!(!report.is_ok());
        assert_eq!(
            report.errors[0].code,
            InstanceViolationCode::RootTypeMismatch
        );
    }

    #[test]
    fn root_type_any_accepts_anything() {
        let shape = ValueShape {
            root_type: ShapeType::Any,
            required: Default::default(),
            optional: Default::default(),
            strict_fields: None,
        };
        assert!(validate(&shape, &serde_json::json!("hello")).is_ok());
        assert!(validate(&shape, &serde_json::json!({"a": 1})).is_ok());
        assert!(validate(&shape, &serde_json::json!([1, 2])).is_ok());
        assert!(validate(&shape, &serde_json::json!(null)).is_ok());
    }

    #[test]
    fn root_type_scalar_rejects_object() {
        let shape = ValueShape {
            root_type: ShapeType::Scalar,
            required: Default::default(),
            optional: Default::default(),
            strict_fields: None,
        };
        let report = validate(&shape, &serde_json::json!({ "key": "val" }));
        assert!(!report.is_ok());
        assert_eq!(
            report.errors[0].code,
            InstanceViolationCode::RootTypeMismatch
        );
    }

    #[test]
    fn root_type_scalar_rejects_array() {
        let shape = ValueShape {
            root_type: ShapeType::Scalar,
            required: Default::default(),
            optional: Default::default(),
            strict_fields: None,
        };
        let report = validate(&shape, &serde_json::json!([1, 2]));
        assert!(!report.is_ok());
        assert_eq!(
            report.errors[0].code,
            InstanceViolationCode::RootTypeMismatch
        );
    }

    #[test]
    fn root_type_scalar_accepts_primitives() {
        let shape = ValueShape {
            root_type: ShapeType::Scalar,
            required: Default::default(),
            optional: Default::default(),
            strict_fields: None,
        };
        assert!(validate(&shape, &serde_json::json!("hello")).is_ok());
        assert!(validate(&shape, &serde_json::json!(42)).is_ok());
        assert!(validate(&shape, &serde_json::json!(true)).is_ok());
        assert!(validate(&shape, &serde_json::json!(null)).is_ok());
    }

    #[test]
    fn human_readable_expected_and_found_are_useful() {
        let shape = shape_mapping(&[("mode", ft_string_enum(&["cli_exec", "daemon_ui"]))], &[]);
        let value = serde_json::json!({ "mode": "web" });
        let report = validate(&shape, &value);
        assert_eq!(report.errors.len(), 1);
        let v = &report.errors[0];
        assert!(
            v.expected.contains("cli_exec"),
            "expected: {:?}",
            v.expected
        );
        assert!(v.found.contains("web"), "found: {:?}", v.found);
    }

    #[test]
    fn multiple_errors_reported() {
        let shape = shape_mapping(&[("name", ft_string()), ("count", ft_integer())], &[]);
        let value = serde_json::json!({
            "count": "not a number",
            "extra": "present"
        });
        let report = validate(&shape, &value);
        assert_eq!(report.errors.len(), 2);
        // Missing required + type mismatch.
        assert!(report
            .errors
            .iter()
            .any(|e| e.code == InstanceViolationCode::MissingRequiredField));
        assert!(report
            .errors
            .iter()
            .any(|e| e.code == InstanceViolationCode::TypeMismatch));
    }

    #[test]
    fn report_is_ok_when_no_errors() {
        let shape = shape_mapping(&[("name", ft_string())], &[]);
        let value = serde_json::json!({ "name": "test" });
        let report = validate(&shape, &value);
        assert!(report.is_ok());
        assert!(report.errors.is_empty());
        assert!(report.warnings.is_empty());
    }
}

// ── Kind-contract regression tests (Slice 7) ─────────────────────────
//
// Prove that real descriptor shapes satisfy the migrated kind contracts
// and that malformed variants are correctly rejected. These tests
// construct ValueShape objects programmatically matching the actual
// kind schemas, so they don't depend on file loading or signatures.

#[cfg(test)]
mod kind_contract_regressions {
    use super::*;

    // ── Helpers ──────────────────────────────────────────────────

    fn validate(shape: &ValueShape, value: &serde_json::Value) -> InstanceValidationReport {
        shape.validate_instance(value)
    }

    fn ft_string() -> FieldType {
        FieldType::Single {
            prim: PrimType::String,
            enum_values: None,
            nested_contract: None,
            element_type: None,
        }
    }
    fn ft_string_enum(values: &[&str]) -> FieldType {
        FieldType::Single {
            prim: PrimType::String,
            enum_values: Some(values.iter().map(|s| s.to_string()).collect()),
            nested_contract: None,
            element_type: None,
        }
    }
    fn ft_mapping() -> FieldType {
        FieldType::Single {
            prim: PrimType::Mapping,
            enum_values: None,
            nested_contract: None,
            element_type: None,
        }
    }
    fn ft_mapping_with(contract: ValueShape) -> FieldType {
        FieldType::Single {
            prim: PrimType::Mapping,
            enum_values: None,
            nested_contract: Some(Box::new(contract)),
            element_type: None,
        }
    }
    fn ft_sequence_of(element: FieldType) -> FieldType {
        FieldType::Single {
            prim: PrimType::Sequence,
            enum_values: None,
            nested_contract: None,
            element_type: Some(Box::new(element)),
        }
    }
    fn ft_boolean() -> FieldType {
        FieldType::Single {
            prim: PrimType::Boolean,
            enum_values: None,
            nested_contract: None,
            element_type: None,
        }
    }
    fn ft_union(prims: &[PrimType]) -> FieldType {
        FieldType::Union {
            prims: prims.to_vec(),
        }
    }
    fn shape(required: &[(&str, FieldType)], optional: &[(&str, FieldType)]) -> ValueShape {
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
            strict_fields: None,
        }
    }

    // ── client kind ─────────────────────────────────────────────

    fn client_shape() -> ValueShape {
        shape(
            &[
                (
                    "launch",
                    ft_mapping_with(shape(
                        &[
                            ("mode", ft_string_enum(&["cli_exec", "daemon_ui"])),
                            ("binary_ref", ft_string()),
                        ],
                        &[("args", ft_mapping())],
                    )),
                ),
                (
                    "serves",
                    ft_mapping_with(shape(
                        &[("kind", ft_string())],
                        &[("renderer", ft_string())],
                    )),
                ),
            ],
            &[
                ("version", ft_string()),
                ("description", ft_string()),
                ("capabilities", ft_mapping()),
            ],
        )
    }

    #[test]
    fn client_valid_descriptor_passes() {
        let value = serde_json::json!({
            "launch": {
                "mode": "cli_exec",
                "binary_ref": "bin/{triple}/ryeos-tui",
                "args": { "surface": "--surface" }
            },
            "serves": { "kind": "surface", "renderer": "terminal" },
            "capabilities": { "requires_daemon": true },
            "version": "1.0.0",
            "description": "Terminal UI"
        });
        let report = validate(&client_shape(), &value);
        assert!(report.is_ok(), "valid client should pass: {report}");
    }

    #[test]
    fn client_rejects_invalid_launch_mode_enum() {
        let value = serde_json::json!({
            "launch": { "mode": "local", "binary_ref": "bin/x" },
            "serves": { "kind": "surface" }
        });
        let report = validate(&client_shape(), &value);
        assert!(!report.is_ok());
        assert!(report
            .errors
            .iter()
            .any(|e| e.path == "launch.mode" && e.code == InstanceViolationCode::EnumMismatch));
    }

    #[test]
    fn client_rejects_missing_launch_binary_ref() {
        let value = serde_json::json!({
            "launch": { "mode": "cli_exec" },
            "serves": { "kind": "surface" }
        });
        let report = validate(&client_shape(), &value);
        assert!(!report.is_ok());
        assert!(report.errors.iter().any(|e| e.path == "launch.binary_ref"
            && e.code == InstanceViolationCode::MissingRequiredField));
    }

    #[test]
    fn client_rejects_missing_serves_kind() {
        let value = serde_json::json!({
            "launch": { "mode": "cli_exec", "binary_ref": "bin/x" },
            "serves": {}
        });
        let report = validate(&client_shape(), &value);
        assert!(!report.is_ok());
        assert!(report
            .errors
            .iter()
            .any(|e| e.path == "serves.kind"
                && e.code == InstanceViolationCode::MissingRequiredField));
    }

    #[test]
    fn client_rejects_missing_launch_entirely() {
        let value = serde_json::json!({
            "serves": { "kind": "surface" }
        });
        let report = validate(&client_shape(), &value);
        assert!(!report.is_ok());
        assert!(report
            .errors
            .iter()
            .any(|e| e.path == "launch" && e.code == InstanceViolationCode::MissingRequiredField));
    }

    // ── service kind ─────────────────────────────────────────────

    fn service_shape() -> ValueShape {
        shape(
            &[("endpoint", ft_string())],
            &[
                ("name", ft_string()),
                ("version", ft_string()),
                (
                    "availability",
                    ft_string_enum(&["both", "daemon_only", "offline"]),
                ),
                ("offline_execute", ft_string()),
                ("required_caps", ft_sequence_of(ft_string())),
                ("description", ft_string()),
                ("schema", ft_mapping()),
            ],
        )
    }

    #[test]
    fn service_valid_descriptor_passes() {
        let value = serde_json::json!({
            "endpoint": "verify",
            "description": "Verify signed items",
            "required_caps": ["ryeos.execute.service.verify"],
            "availability": "offline"
        });
        let report = validate(&service_shape(), &value);
        assert!(report.is_ok(), "valid service should pass: {report}");
    }

    #[test]
    fn service_rejects_missing_endpoint() {
        let value = serde_json::json!({
            "description": "no endpoint"
        });
        let report = validate(&service_shape(), &value);
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.path == "endpoint"
                    && e.code == InstanceViolationCode::MissingRequiredField)
        );
    }

    #[test]
    fn service_rejects_invalid_availability_enum() {
        let value = serde_json::json!({
            "endpoint": "test",
            "availability": "sometimes"
        });
        let report = validate(&service_shape(), &value);
        assert!(!report.is_ok());
        assert!(report
            .errors
            .iter()
            .any(|e| e.path == "availability" && e.code == InstanceViolationCode::EnumMismatch));
    }

    // ── handler kind ─────────────────────────────────────────────

    fn handler_shape() -> ValueShape {
        shape(
            &[
                ("category", ft_string()),
                ("name", ft_string()),
                ("kind", ft_string_enum(&["handler"])),
                ("serves", ft_string_enum(&["parser", "composer"])),
                ("binary_ref", ft_string()),
                ("abi_version", ft_string()),
            ],
            &[
                ("required_caps", ft_sequence_of(ft_string())),
                ("description", ft_string()),
            ],
        )
    }

    #[test]
    fn handler_valid_descriptor_passes() {
        let value = serde_json::json!({
            "category": "ryeos/core",
            "name": "identity",
            "kind": "handler",
            "serves": "composer",
            "binary_ref": "bin/x86_64-unknown-linux-gnu/rye-composer-identity",
            "abi_version": "v1",
            "required_caps": [],
            "description": "Identity composer"
        });
        let report = validate(&handler_shape(), &value);
        assert!(report.is_ok(), "valid handler should pass: {report}");
    }

    #[test]
    fn handler_rejects_invalid_serves_enum() {
        let value = serde_json::json!({
            "category": "ryeos/core",
            "name": "test",
            "kind": "handler",
            "serves": "executor",
            "binary_ref": "bin/x",
            "abi_version": "v1"
        });
        let report = validate(&handler_shape(), &value);
        assert!(!report.is_ok());
        assert!(report
            .errors
            .iter()
            .any(|e| e.path == "serves" && e.code == InstanceViolationCode::EnumMismatch));
    }

    // ── runtime kind ─────────────────────────────────────────────

    fn runtime_shape() -> ValueShape {
        shape(
            &[
                ("kind", ft_string_enum(&["runtime"])),
                ("serves", ft_string()),
                ("binary_ref", ft_string()),
                ("abi_version", ft_string()),
            ],
            &[
                ("default", ft_boolean()),
                ("required_caps", ft_sequence_of(ft_string())),
                ("description", ft_string()),
                (
                    "schema",
                    ft_mapping_with(shape(
                        &[("envelope", ft_string()), ("result", ft_string())],
                        &[],
                    )),
                ),
            ],
        )
    }

    #[test]
    fn runtime_valid_descriptor_passes() {
        let value = serde_json::json!({
            "kind": "runtime",
            "serves": "ryeos/core/python",
            "binary_ref": "bin/x86_64-unknown-linux-gnu/rye-runtime-python",
            "abi_version": "v1",
            "default": true,
            "description": "Python runtime"
        });
        let report = validate(&runtime_shape(), &value);
        assert!(report.is_ok(), "valid runtime should pass: {report}");
    }

    #[test]
    fn runtime_valid_with_schema_passes() {
        let value = serde_json::json!({
            "kind": "runtime",
            "serves": "ryeos/core/python",
            "binary_ref": "bin/x",
            "abi_version": "v1",
            "schema": { "envelope": "launch_envelope_v1", "result": "runtime_result_v1" }
        });
        let report = validate(&runtime_shape(), &value);
        assert!(report.is_ok(), "runtime with schema should pass: {report}");
    }

    #[test]
    fn runtime_rejects_invalid_kind_enum() {
        let value = serde_json::json!({
            "kind": "tool",
            "serves": "ryeos/core/python",
            "binary_ref": "bin/x",
            "abi_version": "v1"
        });
        let report = validate(&runtime_shape(), &value);
        assert!(!report.is_ok());
        assert!(report
            .errors
            .iter()
            .any(|e| e.path == "kind" && e.code == InstanceViolationCode::EnumMismatch));
    }

    #[test]
    fn runtime_rejects_incomplete_schema() {
        let value = serde_json::json!({
            "kind": "runtime",
            "serves": "ryeos/core/python",
            "binary_ref": "bin/x",
            "abi_version": "v1",
            "schema": { "envelope": "launch_v1" }
        });
        let report = validate(&runtime_shape(), &value);
        assert!(!report.is_ok());
        assert!(report
            .errors
            .iter()
            .any(|e| e.path == "schema.result"
                && e.code == InstanceViolationCode::MissingRequiredField));
    }

    // ── surface kind ─────────────────────────────────────────────

    fn surface_shape() -> ValueShape {
        shape(
            &[(
                "layout",
                ft_mapping_with(shape(&[("root", ft_string())], &[("nodes", ft_mapping())])),
            )],
            &[
                ("extends", ft_union(&[PrimType::String, PrimType::Null])),
                ("input", ft_mapping()),
                ("ambient", ft_mapping()),
                (
                    "affordances",
                    ft_sequence_of(ft_mapping_with(shape(
                        &[("id", ft_string())],
                        &[
                            ("label", ft_string()),
                            ("category", ft_string()),
                            ("icon", ft_string()),
                            ("caps", ft_sequence_of(ft_string())),
                        ],
                    ))),
                ),
                ("instruments", ft_sequence_of(ft_mapping())),
                ("capabilities", ft_mapping()),
            ],
        )
    }

    #[test]
    fn surface_valid_composed_value_passes() {
        let value = serde_json::json!({
            "layout": {
                "root": "main",
                "nodes": {
                    "main": { "type": "split", "axis": "horizontal" }
                }
            },
            "ambient": { "show_background": true },
            "affordances": [
                { "id": "view.threads", "label": "Threads", "category": "View" },
                { "id": "layout.reset", "category": "Layout" }
            ]
        });
        let report = validate(&surface_shape(), &value);
        assert!(report.is_ok(), "valid surface should pass: {report}");
    }

    #[test]
    fn surface_rejects_missing_layout_root() {
        let value = serde_json::json!({
            "layout": { "nodes": {} }
        });
        let report = validate(&surface_shape(), &value);
        assert!(!report.is_ok());
        assert!(report
            .errors
            .iter()
            .any(|e| e.path == "layout.root"
                && e.code == InstanceViolationCode::MissingRequiredField));
    }

    #[test]
    fn surface_rejects_affordance_without_id() {
        let value = serde_json::json!({
            "layout": { "root": "main" },
            "affordances": [
                { "label": "No ID" }
            ]
        });
        let report = validate(&surface_shape(), &value);
        assert!(!report.is_ok());
        assert!(report
            .errors
            .iter()
            .any(|e| e.path.starts_with("affordances[")
                && e.code == InstanceViolationCode::MissingRequiredField));
    }

    #[test]
    fn surface_accepts_extends_chain_child() {
        // Child surface with extends pointing to parent.
        // Validation only checks the composed value, not extends resolution.
        let value = serde_json::json!({
            "extends": "surface:ryeos/studio/base",
            "layout": { "root": "main" },
            "affordances": [
                { "id": "view.graph", "label": "Graph", "category": "Graph" }
            ]
        });
        let report = validate(&surface_shape(), &value);
        assert!(report.is_ok(), "extends-chain child should pass: {report}");
    }
}

// ── Signature envelope ───────────────────────────────────────────────

/// How a `ryeos:signed:...` payload is embedded in a source file.
///
/// Varies by file type — loaded from extractor YAML, never hardcoded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct ResolvedSourceFormat {
    /// The matched file extension, e.g. `".py"`, `".md"`
    pub extension: String,
    /// Canonical parser tool ref, e.g.
    /// `"parser:ryeos/core/python/tool-header"`. The `ParserDispatcher`
    /// resolves this through `ParserRegistry`.
    pub parser: String,
    /// Signature embedding envelope for this file type
    pub signature: SignatureEnvelope,
}

// ── Item spaces ──────────────────────────────────────────────────────

/// The resolution space where an item was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ItemSpace {
    Project,
    Bundle,
}

impl ItemSpace {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Bundle => "bundle",
        }
    }
}

// ── Project context ──────────────────────────────────────────────────

/// Portable project identity for local and remote execution.
///
/// Always present on requests. `None` is the explicit "no project" variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct ItemMetadata {
    /// Executor ID extracted from item metadata
    pub executor_id: Option<String>,
    /// Item version extracted from item metadata
    pub version: Option<String>,
    /// Item description
    pub description: Option<String>,
    /// Item category
    pub category: Option<String>,
    /// Vault secret IDs this item requires (e.g. `["openai-api-key"]`).
    /// The daemon resolves these per-principal and injects as `RYEOS_VAULT_*` env vars.
    #[serde(default)]
    pub required_secrets: Vec<String>,
    /// Kind-specific metadata fields routed here by the metadata extraction
    /// pipeline (assign_extracted_field in kind_registry.rs). Producers
    /// populate this deliberately via the catch-all arms; it is NOT an
    /// automatic serde pass-through from flatten.
    ///
    /// Known keys: endpoint, required_caps, handler, serves, default,
    /// binary_ref, abi_version, name, section.
    #[serde(default)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct SignerFingerprint(pub String);

/// Pinned version reference for signed/temporal resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum LaunchMode {
    Inline,
    Detached,
}

// ── Execution hints ──────────────────────────────────────────────────

/// Executor-specific hints forwarded verbatim through the pipeline.
/// The engine does not interpret contents; used only for cache-key
/// hashing and resume-context persistence.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionHints {
    #[serde(default)]
    pub values: HashMap<String, Value>,
}

// ── Principal ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Principal {
    pub fingerprint: String,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EffectivePrincipal {
    Local(Principal),
    Delegated(Box<DelegatedPrincipal>),
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
#[serde(deny_unknown_fields)]
pub struct PlanNodeId(pub String);

/// Plan capabilities declared by the execution plan.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanCapabilities {
    pub requires_model: bool,
    pub requires_subprocess: bool,
    pub requires_network: bool,
    pub custom: Vec<String>,
}

/// Materialization requirement for plan execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializationRequirement {
    pub kind: String,
    pub ref_string: String,
}

/// Normalized subprocess specification — the single source of truth for
/// what to spawn. Compiled from the executor chain's runtime config by
/// the plan builder. The dispatch layer just runs this struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanSubprocessSpec {
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Source category for each env entry. This lets the daemon apply
    /// final subprocess env policy without guessing from key names.
    ///
    /// Kept as a sidecar instead of changing `env` wire shape so older
    /// serialized specs remain readable and current callers can keep
    /// using `spec.env` as the final key/value map.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env_sources: HashMap<String, RuntimeEnvSource>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeEnvSource {
    EnginePlan,
    RuntimeDescriptor,
    RuntimeInterpreter,
    RuntimePathMutation,
}

/// Typed bag of `DecorateSpec`-phase outputs. Each field is `Option`
/// so absence ⇒ "preserve current default". Future decorate handlers
/// add siblings here without breaking the top-level spec shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionDecorations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_async: Option<NativeAsyncSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_resume: Option<NativeResumeSpec>,
}

/// Resume policy declared by the `native_resume` runtime handler.
/// Presence in the spec ⇒ the tool is replay-aware: the daemon will
/// allocate a per-thread checkpoint dir, inject `RYEOS_CHECKPOINT_DIR`
/// at spawn time, and on daemon restart attempt automatic resume up
/// to `max_auto_resume_attempts` times before marking the thread
/// failed. The tool is responsible for writing checkpoints into the
/// supplied directory and for being idempotent / replay-safe on
/// startup (`RYEOS_RESUME=1` is injected on resume spawns).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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
/// event stream (the runner injects `RYEOS_NATIVE_ASYNC=1`) and the
/// daemon cancellation routes through `cancellation_mode`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeAsyncSpec {
    pub cancellation_mode: CancellationMode,
}

/// How the runner terminates the subprocess on cancellation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
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
#[serde(tag = "node_type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanNode {
    DispatchSubprocess {
        id: PlanNodeId,
        /// The fully resolved subprocess specification.
        spec: PlanSubprocessSpec,
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
#[serde(deny_unknown_fields)]
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
    /// When set (from `--debug-raw` via `execution_hints`), the dispatcher
    /// attaches a `debug` block (resolved cmd/args/cwd/env keys + exit code and
    /// size-limited raw stdout/stderr) to the completion. Default `false` —
    /// the normal execution path is unaffected.
    #[serde(default)]
    pub debug_raw: bool,
}

// ── Execution completion ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ThreadTerminalStatus {
    Completed,
    Failed,
    Cancelled,
    Continued,
    Killed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionArtifact {
    pub artifact_type: String,
    pub uri: String,
    #[serde(default)]
    pub content_hash: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct ContinuationRequest {
    pub reason: String,
    pub successor_parameters: Option<Value>,
}

/// Structured completion returned from plan execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct BudgetLease {
    pub lease_id: String,
    pub issuer_site_id: String,
    pub parent_thread_id: String,
    pub reserved_max_spend: f64,
    pub issued_at: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpendReport {
    pub lease_id: String,
    pub spend_report_id: String,
    pub report_seq: i64,
    pub amount: f64,
    #[serde(default)]
    pub runtime_metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalSettlement {
    pub lease_id: String,
    pub settlement_id: String,
    pub final_spend: f64,
    pub terminal_status: String,
}

// ── Capability lease / settlement ────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct CapabilityUseReport {
    pub lease_id: String,
    pub use_report_id: String,
    pub report_seq: i64,
    pub uses: i64,
    #[serde(default)]
    pub runtime_metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityFinalSettlement {
    pub lease_id: String,
    pub settlement_id: String,
    pub final_use_count: i64,
    pub terminal_status: String,
}

// ── Event contracts ──────────────────────────────────────────────────

/// What adapters and the engine send to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventAppendRequest {
    pub event_type: String,
    pub payload: Value,
}

/// What the daemon persists and streams.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
