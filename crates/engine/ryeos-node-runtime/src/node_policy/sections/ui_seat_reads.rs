//! Node-owned policy for what a UI seat may read through a bound source.
//!
//! A UI seat reaches a read service only when a node-authored Surface lists it
//! in its source lane and the seat's own grant holds the service's
//! capabilities. This section is a third, independent narrowing: for every
//! service a seat reads it names the caller classes admitted, the parameters
//! that must match or are forced, the parameters a seat may not send, the
//! fields of the result that may be disclosed, and the size the disclosed
//! result may reach. It never grants anything: a read with no entry is not
//! admitted, and an entry cannot add a capability, a Surface lane, or a
//! record the service itself would refuse.
//!
//! The section knows services, parameter names and JSON pointers only. It
//! names no screen, record kind or consumer. Registration makes it a required
//! member of every complete node-policy generation; there is no daemon
//! fallback and an absent section is not an empty one.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "ui_seat_reads";

/// The only schema this section is read under.
pub const CURRENT_SCHEMA: u32 = 1;

/// Pin marker replaced by the session's project root; refused when the
/// session has none.
pub const MARKER_PROJECT_ROOT: &str = "@session:project_root";
/// Pin marker replaced by the session's project root, or null without one.
pub const MARKER_PROJECT_ROOT_OR_NULL: &str = "@session:project_root_or_null";
/// Pin marker replaced by the session principal's id.
pub const MARKER_PRINCIPAL_ID: &str = "@session:principal_id";

const SESSION_MARKER_PREFIX: &str = "@session:";
const MAX_PARAMETER_NAME_BYTES: usize = 64;

/// Who a seat is, as far as read policy goes. Derived from the one fact the
/// daemon holds about a seat: whether its session carries a hosted principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatClass {
    /// A session minted on the local operator lane: no hosted principal.
    LocalOperator,
    /// A session minted for a hosted principal.
    HostedPrincipal,
}

impl SeatClass {
    /// The class of a session from its hosted-principal field.
    pub fn of_session(user_principal_id: Option<&str>) -> Self {
        match user_principal_id {
            None => Self::LocalOperator,
            Some(_) => Self::HostedPrincipal,
        }
    }
}

/// One segment of a keep pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeepSegment {
    /// An object key, or an array index written in decimal.
    Key(String),
    /// Exactly one object key or one array index.
    Any,
}

/// A JSON pointer into a service result with `*` allowed as a whole segment.
/// A pointer retains the entire value it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepPath {
    text: String,
    segments: Vec<KeepSegment>,
}

impl KeepPath {
    pub fn parse(text: &str) -> Result<Self> {
        let Some(rest) = text.strip_prefix('/') else {
            bail!("keep pointer `{text}` must start with `/`");
        };
        let mut segments = Vec::new();
        for raw in rest.split('/') {
            if raw.is_empty() {
                bail!("keep pointer `{text}` has an empty segment");
            }
            if raw == "*" {
                segments.push(KeepSegment::Any);
                continue;
            }
            let mut decoded = String::with_capacity(raw.len());
            let mut chars = raw.chars();
            while let Some(character) = chars.next() {
                if character != '~' {
                    decoded.push(character);
                    continue;
                }
                match chars.next() {
                    Some('0') => decoded.push('~'),
                    Some('1') => decoded.push('/'),
                    _ => bail!("keep pointer `{text}` has an invalid `~` escape"),
                }
            }
            segments.push(KeepSegment::Key(decoded));
        }
        Ok(Self {
            text: text.to_owned(),
            segments,
        })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn segments(&self) -> &[KeepSegment] {
        &self.segments
    }
}

impl<'de> Deserialize<'de> for KeepPath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

impl Serialize for KeepPath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text)
    }
}

/// One entry of the policy: the exact admission of one service for the
/// classes it names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeatReadEntry {
    /// One canonical `service:` ref.
    pub target: String,
    /// Classes this entry admits. A class absent from every entry for the
    /// target is refused.
    pub classes: Vec<SeatClass>,
    /// Every listed parameter must be present after pinning with a value in
    /// its set. No entry here means any parameters match.
    #[serde(default)]
    pub when: BTreeMap<String, Vec<Value>>,
    /// Parameters forced to a value, replacing whatever the Surface or the
    /// client supplied. A string beginning `@session:` must be a known marker.
    #[serde(default)]
    pub pin: BTreeMap<String, Value>,
    /// Parameters a seat may not send. A call that carries one is refused.
    #[serde(default)]
    pub deny_params: Vec<String>,
    /// Allowlist of the result fields disclosed. Anything not named, including
    /// fields a later release adds, is dropped.
    pub keep: Vec<KeepPath>,
    /// Largest disclosed result, as compact JSON. A larger result is refused.
    pub max_result_bytes: u64,
    /// Most elements any one array of the disclosed result may hold. A longer
    /// array is refused, never cut.
    pub max_items: u32,
}

impl SeatReadEntry {
    fn validate(&self, index: usize) -> Result<()> {
        let at = format!("ui_seat_reads.reads[{index}]");
        validate_service_ref(&self.target).with_context(|| format!("{at}.target"))?;
        if self.classes.is_empty() {
            bail!("{at} names no class");
        }
        let distinct: BTreeSet<_> = self.classes.iter().collect();
        if distinct.len() != self.classes.len() {
            bail!("{at} repeats a class");
        }
        for (name, set) in &self.when {
            validate_parameter_name(name).with_context(|| format!("{at}.when"))?;
            if set.is_empty() {
                bail!("{at}.when.{name} names no value");
            }
            for value in set {
                if !matches!(
                    value,
                    Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null
                ) {
                    bail!("{at}.when.{name} may list only scalar values");
                }
            }
        }
        for (name, value) in &self.pin {
            validate_parameter_name(name).with_context(|| format!("{at}.pin"))?;
            match value {
                Value::String(text) if text.starts_with(SESSION_MARKER_PREFIX) => {
                    if ![
                        MARKER_PROJECT_ROOT,
                        MARKER_PROJECT_ROOT_OR_NULL,
                        MARKER_PRINCIPAL_ID,
                    ]
                    .contains(&text.as_str())
                    {
                        bail!("{at}.pin.{name} names unknown session marker `{text}`");
                    }
                }
                Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null => {}
                _ => bail!("{at}.pin.{name} must be a scalar or a session marker"),
            }
        }
        let mut denied = BTreeSet::new();
        for name in &self.deny_params {
            validate_parameter_name(name).with_context(|| format!("{at}.deny_params"))?;
            if !denied.insert(name.as_str()) {
                bail!("{at}.deny_params repeats `{name}`");
            }
            if self.pin.contains_key(name) {
                bail!("{at} both pins and denies parameter `{name}`");
            }
        }
        if self.keep.is_empty() {
            bail!("{at}.keep is empty: an entry must disclose something");
        }
        if self.max_result_bytes == 0 {
            bail!("{at}.max_result_bytes must be non-zero");
        }
        if self.max_items == 0 {
            bail!("{at}.max_items must be non-zero");
        }
        Ok(())
    }

    /// Every parameter name this entry mentions, for checking against the
    /// target's declared parameters.
    pub fn mentioned_parameters(&self) -> impl Iterator<Item = &str> {
        self.when
            .keys()
            .chain(self.pin.keys())
            .map(String::as_str)
            .chain(self.deny_params.iter().map(String::as_str))
    }

    fn shares_class_with(&self, other: &Self) -> bool {
        self.classes
            .iter()
            .any(|class| other.classes.contains(class))
    }

    /// Whether one parameter set could satisfy both `when` conditions. Each
    /// parameter is independent, so the conditions overlap exactly when every
    /// parameter both name has a value in common.
    fn when_overlaps(&self, other: &Self) -> bool {
        self.when.iter().all(|(name, mine)| {
            other
                .when
                .get(name)
                .is_none_or(|theirs| mine.iter().any(|value| theirs.contains(value)))
        })
    }
}

/// The compiled `ui_seat_reads` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSeatReadsPolicy {
    pub schema: u32,
    pub reads: Vec<SeatReadEntry>,
}

impl TypedNodePolicy for UiSeatReadsPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

impl UiSeatReadsPolicy {
    /// A policy with no entry: every read is refused.
    pub fn admitting_nothing() -> Self {
        Self {
            schema: CURRENT_SCHEMA,
            reads: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema != CURRENT_SCHEMA {
            bail!("UI seat reads node policy schema is not current");
        }
        for (index, entry) in self.reads.iter().enumerate() {
            entry.validate(index)?;
        }
        for (index, entry) in self.reads.iter().enumerate() {
            for later in &self.reads[index + 1..] {
                if entry.target == later.target
                    && entry.shares_class_with(later)
                    && entry.when_overlaps(later)
                {
                    bail!(
                        "ui_seat_reads has two entries for `{}` that can admit the same call",
                        entry.target
                    );
                }
            }
        }
        Ok(())
    }

    /// The entries for one target, in file order.
    pub fn entries_for(&self, target: &str) -> Vec<&SeatReadEntry> {
        self.reads
            .iter()
            .filter(|entry| entry.target == target)
            .collect()
    }
}

fn validate_service_ref(target: &str) -> Result<()> {
    let parsed = ryeos_engine::canonical_ref::CanonicalRef::parse(target)
        .map_err(|error| anyhow::anyhow!("not a canonical ref: {error}"))?;
    if parsed.kind != "service" {
        bail!("`{target}` is not a service ref");
    }
    if parsed.suffix.is_some() || parsed.to_string() != target {
        bail!("`{target}` is not a plain canonical service ref");
    }
    Ok(())
}

fn validate_parameter_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > MAX_PARAMETER_NAME_BYTES
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        bail!("`{name}` is not a valid parameter name");
    }
    Ok(())
}

pub struct UiSeatReadsPolicySection;

impl NodePolicySection for UiSeatReadsPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> Result<Arc<dyn ErasedNodePolicy>> {
        let record: UiSeatReadsPolicy =
            serde_json::from_value(body.clone()).context("parse UI seat reads node policy")?;
        record.validate()?;
        Ok(Arc::new(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn context() -> NodePolicyContext {
        NodePolicyContext {
            section: SECTION_NAME.to_owned(),
            source_file: "/node/policies/ui_seat_reads.yaml".into(),
            signer_fingerprint: "ab".repeat(32),
        }
    }

    fn entry() -> Value {
        json!({
            "target": "service:node/status",
            "classes": ["local_operator", "hosted_principal"],
            "keep": ["/node_id", "/isolation/mode"],
            "max_result_bytes": 65536,
            "max_items": 50
        })
    }

    fn body(entries: Vec<Value>) -> Value {
        json!({"schema": 1, "reads": entries})
    }

    fn parse(value: Value) -> Result<Arc<dyn ErasedNodePolicy>> {
        UiSeatReadsPolicySection.parse(&context(), &value)
    }

    #[test]
    fn parses_a_complete_entry() {
        let mut full = entry();
        full["when"] = json!({"record_kind": ["a", "b"], "n": [1]});
        full["pin"] = json!({
            "holder": "@session:principal_id",
            "root": "@session:project_root",
            "maybe_root": "@session:project_root_or_null",
            "fixed": "x"
        });
        full["deny_params"] = json!(["include_unverified"]);
        let parsed = parse(body(vec![full])).unwrap();
        let policy = parsed.as_any().downcast_ref::<UiSeatReadsPolicy>().unwrap();
        assert_eq!(policy.reads.len(), 1);
        assert_eq!(policy.reads[0].keep[1].text(), "/isolation/mode");
        assert_eq!(policy.entries_for("service:node/status").len(), 1);
        assert_eq!(policy.entries_for("service:node/other").len(), 0);
    }

    #[test]
    fn an_empty_read_list_is_valid_and_admits_nothing() {
        let parsed = parse(body(vec![])).unwrap();
        let policy = parsed.as_any().downcast_ref::<UiSeatReadsPolicy>().unwrap();
        assert!(policy.reads.is_empty());
        assert_eq!(policy, &UiSeatReadsPolicy::admitting_nothing());
    }

    #[test]
    fn rejects_absent_stale_and_unknown_fields() {
        assert!(parse(json!({"reads": []})).is_err());
        assert!(parse(json!({"schema": 1})).is_err());
        for schema in [0, 2] {
            assert!(parse(json!({"schema": schema, "reads": []})).is_err());
        }
        assert!(parse(json!({"schema": 1, "reads": [], "fallback": 1})).is_err());
        for name in ["target", "classes", "keep", "max_result_bytes", "max_items"] {
            let mut missing = entry();
            missing.as_object_mut().unwrap().remove(name);
            assert!(parse(body(vec![missing])).is_err(), "missing {name}");
        }
        let mut unknown = entry();
        unknown["grant"] = json!(["ryeos.execute.service.*"]);
        assert!(parse(body(vec![unknown])).is_err());
    }

    #[test]
    fn rejects_non_canonical_and_non_service_targets() {
        for target in [
            "node/status",
            "tool:node/status",
            "service:",
            "service:../status",
            "service:node/status@t:2026",
            "service:node//status",
            "",
        ] {
            let mut bad = entry();
            bad["target"] = json!(target);
            assert!(parse(body(vec![bad])).is_err(), "target {target}");
        }
    }

    #[test]
    fn rejects_bad_classes_and_bounds() {
        for classes in [
            json!([]),
            json!(["local_operator", "local_operator"]),
            json!(["remote_operator"]),
        ] {
            let mut bad = entry();
            bad["classes"] = classes;
            assert!(parse(body(vec![bad])).is_err());
        }
        for (field, value) in [("max_result_bytes", 0), ("max_items", 0)] {
            let mut bad = entry();
            bad[field] = json!(value);
            assert!(parse(body(vec![bad])).is_err(), "{field}");
        }
    }

    #[test]
    fn rejects_bad_pointers() {
        for pointer in ["", "/", "node_id", "/a//b", "/a/~2", "/a/~", "a/b"] {
            let mut bad = entry();
            bad["keep"] = json!([pointer]);
            assert!(parse(body(vec![bad])).is_err(), "pointer `{pointer}`");
        }
        let mut empty = entry();
        empty["keep"] = json!([]);
        assert!(parse(body(vec![empty])).is_err());
        assert_eq!(
            KeepPath::parse("/a~1b/~0/*/3").unwrap().segments(),
            &[
                KeepSegment::Key("a/b".into()),
                KeepSegment::Key("~".into()),
                KeepSegment::Any,
                KeepSegment::Key("3".into()),
            ]
        );
    }

    #[test]
    fn rejects_bad_when_pin_and_deny_params() {
        let cases = [
            ("when", json!({"": ["x"]})),
            ("when", json!({"kind": []})),
            ("when", json!({"kind": [{"a": 1}]})),
            ("when", json!({"bad name": ["x"]})),
            ("pin", json!({"root": "@session:unknown"})),
            ("pin", json!({"root": {"nested": 1}})),
            ("pin", json!({"root": ["@session:project_root"]})),
            ("pin", json!({"bad-name": 1})),
            ("deny_params", json!(["a", "a"])),
            ("deny_params", json!(["bad name"])),
        ];
        for (field, value) in cases {
            let mut bad = entry();
            bad[field] = value;
            assert!(parse(body(vec![bad])).is_err(), "{field}");
        }
        let mut both = entry();
        both["pin"] = json!({"holder": "@session:principal_id"});
        both["deny_params"] = json!(["holder"]);
        assert!(parse(body(vec![both])).is_err());
    }

    #[test]
    fn refuses_entries_that_can_admit_the_same_call() {
        // Same target, same class, no condition on either: ambiguous.
        assert!(parse(body(vec![entry(), entry()])).is_err());

        // One entry unconditional and the other conditional overlap.
        let mut conditional = entry();
        conditional["when"] = json!({"k": ["a"]});
        assert!(parse(body(vec![entry(), conditional.clone()])).is_err());

        // Conditions with a shared value overlap; disjoint values do not.
        let mut other = entry();
        other["when"] = json!({"k": ["a", "b"]});
        assert!(parse(body(vec![conditional.clone(), other])).is_err());
        let mut disjoint = entry();
        disjoint["when"] = json!({"k": ["b"]});
        assert!(parse(body(vec![conditional.clone(), disjoint])).is_ok());

        // Conditions on different parameters can both hold.
        let mut elsewhere = entry();
        elsewhere["when"] = json!({"j": ["a"]});
        assert!(parse(body(vec![conditional, elsewhere])).is_err());

        // Disjoint classes never overlap, whatever the conditions.
        let mut local = entry();
        local["classes"] = json!(["local_operator"]);
        let mut hosted = entry();
        hosted["classes"] = json!(["hosted_principal"]);
        assert!(parse(body(vec![local, hosted])).is_ok());

        // Different targets never overlap.
        let mut elsewhere = entry();
        elsewhere["target"] = json!("service:bundle/list");
        assert!(parse(body(vec![entry(), elsewhere])).is_ok());
    }

    #[test]
    fn class_follows_the_hosted_principal_field() {
        assert_eq!(SeatClass::of_session(None), SeatClass::LocalOperator);
        assert_eq!(
            SeatClass::of_session(Some("fp:abc")),
            SeatClass::HostedPrincipal
        );
    }
}
