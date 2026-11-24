//! What a caller may see of a service's answer.
//!
//! A service item declares, in its signed `disclosure` block, which leaves of
//! its result each caller class may see. The operator sees the whole result.
//! A field named in no tier is therefore visible to the operator only, so a
//! field a later release adds is closed to every other class until the item
//! names it. This module is mechanism only: it knows classes, pointers and
//! bounds, never a service or a field by name.
//!
//! The decision is made where a service's result is produced, for every
//! caller, so the same answer shape reaches a seat, a script and a remote
//! caller of the same class.

use anyhow::{Result, bail};
use serde_json::{Map, Value};

use crate::node_policy::sections::read_disclosure::{KeepPath, KeepSegment};

/// How much of a result a caller may see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisclosureClass {
    /// The node's configured operator: the whole result.
    Operator,
    /// Any other admitted principal: the `principal` tier.
    Principal,
    /// A caller with no authentication: the `public` tier.
    Public,
}

impl DisclosureClass {
    pub fn is_operator(self) -> bool {
        self == Self::Operator
    }
}

/// Largest answer a non-operator caller receives, as compact JSON. A larger
/// answer is refused, never cut.
pub const DEFAULT_MAX_RESULT_BYTES: u64 = 65_536;
/// Most elements any one array of a non-operator answer may hold.
pub const DEFAULT_MAX_ITEMS: u32 = 256;

/// The tiers of one service item.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServiceDisclosure {
    principal: Vec<KeepPath>,
    public: Vec<KeepPath>,
    free_text: Vec<KeepPath>,
    interim: Vec<KeepPath>,
    bounds: DisclosureBounds,
}

impl ServiceDisclosure {
    /// Parse the item's `disclosure` mapping. Unknown keys and a free-text
    /// pointer that a wider tier also names are refused, so a field that can
    /// carry a path or probe output cannot reach a non-operator by accident.
    pub fn parse(value: &Value) -> Result<Self> {
        let Value::Object(map) = value else {
            bail!("`disclosure` must be a mapping");
        };
        if let Some(unknown) = map.keys().find(|key| {
            !matches!(
                key.as_str(),
                "principal"
                    | "public"
                    | "free_text"
                    | "interim_free_text"
                    | "max_result_bytes"
                    | "max_items"
            )
        }) {
            bail!("`disclosure` has an unknown key `{unknown}`");
        }
        let list = |name: &str| -> Result<Vec<KeepPath>> {
            match map.get(name) {
                None => Ok(Vec::new()),
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|item| {
                        let text = item.as_str().ok_or_else(|| {
                            anyhow::anyhow!("`disclosure.{name}` entries must be strings")
                        })?;
                        KeepPath::parse(text)
                    })
                    .collect(),
                Some(_) => bail!("`disclosure.{name}` must be a list of pointers"),
            }
        };
        let mut parsed = Self {
            principal: list("principal")?,
            public: list("public")?,
            free_text: list("free_text")?,
            interim: list("interim_free_text")?,
            bounds: declared_bounds(map)?,
        };
        // An interim entry is free text the principal tier carries for now,
        // because the seat that owns the answer reads it today. It is the
        // only way a free-text leaf reaches a principal, it is named apart so
        // it can be reviewed and removed, and it may not also be withheld.
        for interim in &parsed.interim {
            for withheld in &parsed.free_text {
                if overlaps(interim.segments(), withheld.segments()) {
                    bail!(
                        "interim free-text pointer `{}` overlaps withheld pointer `{}`",
                        interim.text(),
                        withheld.text()
                    );
                }
            }
        }
        for interim in &parsed.interim {
            if interim.segments().last() == Some(&KeepSegment::Any) {
                bail!(
                    "interim free-text pointer `{}` must name one leaf, not every child",
                    interim.text()
                );
            }
            if let Some(shown) = parsed
                .public
                .iter()
                .find(|shown| overlaps(interim.segments(), shown.segments()))
            {
                bail!(
                    "interim free-text pointer `{}` overlaps public pointer `{}`; it is for a principal alone",
                    interim.text(),
                    shown.text()
                );
            }
        }
        parsed.principal.extend(parsed.interim.iter().cloned());
        for free in &parsed.free_text {
            for shown in parsed.principal.iter().chain(&parsed.public) {
                if overlaps(free.segments(), shown.segments()) {
                    bail!(
                        "free-text pointer `{}` overlaps tier pointer `{}`; free text is operator-only",
                        free.text(),
                        shown.text()
                    );
                }
            }
        }
        // A public leaf must also be a principal leaf: a wider audience never
        // sees what a narrower one does not.
        for shown in &parsed.public {
            if !parsed
                .principal
                .iter()
                .any(|wider| covers(wider.segments(), shown.segments()))
            {
                bail!(
                    "public pointer `{}` is not covered by a principal pointer",
                    shown.text()
                );
            }
        }
        Ok(parsed)
    }

    /// The most a non-operator answer of this item may hold.
    pub fn bounds(&self) -> DisclosureBounds {
        self.bounds
    }

    /// The free-text leaves this item lets a principal see for now.
    pub fn interim_free_text(&self) -> impl Iterator<Item = &str> {
        self.interim.iter().map(KeepPath::text)
    }

    fn tier(&self, class: DisclosureClass) -> Option<&[KeepPath]> {
        match class {
            DisclosureClass::Operator => None,
            DisclosureClass::Principal => Some(&self.principal),
            DisclosureClass::Public => Some(&self.public),
        }
    }
}

/// Why an answer was not given to a non-operator caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisclosureRefusal {
    /// The service item declares no `disclosure` block, so nothing of it is
    /// disclosed to a non-operator.
    NotDisclosable,
    /// The answer is larger than the tier's bound.
    ResultTooLarge,
    /// An array of the answer is longer than the tier's bound.
    TooManyItems,
}

impl DisclosureRefusal {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotDisclosable => "read_not_disclosable",
            Self::ResultTooLarge => "read_result_too_large",
            Self::TooManyItems => "read_too_many_items",
        }
    }
}

impl std::fmt::Display for DisclosureRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match self {
            Self::NotDisclosable => "this service does not disclose its answer to this caller",
            Self::ResultTooLarge => "the answer is larger than this caller may receive",
            Self::TooManyItems => "an array of the answer is longer than this caller may receive",
        };
        write!(formatter, "{}: {reason}", self.code())
    }
}

impl std::error::Error for DisclosureRefusal {}

/// The bounds of a non-operator answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisclosureBounds {
    pub max_result_bytes: u64,
    pub max_items: u32,
}

/// Largest bound an item may declare for itself. An answer bigger than this is
/// never disclosed to a non-operator, whatever the item says.
pub const CEILING_MAX_RESULT_BYTES: u64 = 8 * 1024 * 1024;
pub const CEILING_MAX_ITEMS: u32 = 4096;

fn declared_bounds(map: &Map<String, Value>) -> Result<DisclosureBounds> {
    let mut bounds = DisclosureBounds::default();
    if let Some(value) = map.get("max_result_bytes") {
        let bytes = value
            .as_u64()
            .filter(|bytes| (1..=CEILING_MAX_RESULT_BYTES).contains(bytes))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "`disclosure.max_result_bytes` must be an integer from 1 to {CEILING_MAX_RESULT_BYTES}"
                )
            })?;
        bounds.max_result_bytes = bytes;
    }
    if let Some(value) = map.get("max_items") {
        let items = value
            .as_u64()
            .and_then(|items| u32::try_from(items).ok())
            .filter(|items| (1..=CEILING_MAX_ITEMS).contains(items))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "`disclosure.max_items` must be an integer from 1 to {CEILING_MAX_ITEMS}"
                )
            })?;
        bounds.max_items = items;
    }
    Ok(bounds)
}

impl Default for DisclosureBounds {
    fn default() -> Self {
        Self {
            max_result_bytes: DEFAULT_MAX_RESULT_BYTES,
            max_items: DEFAULT_MAX_ITEMS,
        }
    }
}

/// Shape `result` for `class`. The operator gets it whole. Any other class
/// gets only its tier's leaves, within the bounds. A tier that names no leaf
/// is an empty answer (`null`), which is how a service that only acknowledges
/// says so. A service with no declaration is refused.
pub fn shape_result(
    class: DisclosureClass,
    disclosure: Option<&ServiceDisclosure>,
    result: Value,
    bounds: DisclosureBounds,
) -> Result<Value, DisclosureRefusal> {
    if class.is_operator() {
        return Ok(result);
    }
    let Some(tier) = disclosure.and_then(|declared| declared.tier(class)) else {
        return Err(DisclosureRefusal::NotDisclosable);
    };
    if tier.is_empty() {
        return Ok(Value::Null);
    }
    let paths: Vec<&[KeepSegment]> = tier.iter().map(KeepPath::segments).collect();
    let kept = project(&result, &paths).unwrap_or(Value::Null);
    if longest_array(&kept) > usize::try_from(bounds.max_items).unwrap_or(usize::MAX) {
        return Err(DisclosureRefusal::TooManyItems);
    }
    let fits = serde_json::to_vec(&kept)
        .ok()
        .and_then(|bytes| u64::try_from(bytes.len()).ok())
        .is_some_and(|length| length <= bounds.max_result_bytes);
    if fits {
        Ok(kept)
    } else {
        Err(DisclosureRefusal::ResultTooLarge)
    }
}

/// What a non-operator caller is told about a failed service: a stable code
/// and a fixed sentence. The failure's own text can carry a path, probe output
/// or a key, so it goes to the operator and the log only.
pub fn redacted_failure(error_text: &str) -> (String, &'static str) {
    const FIXED: &str = "the service could not complete the request";
    let code = error_text
        .split_once(':')
        .map(|(prefix, _)| prefix.trim())
        .filter(|prefix| {
            !prefix.is_empty()
                && prefix.len() <= 64
                && prefix
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        })
        .unwrap_or("service_failed");
    (code.to_owned(), FIXED)
}

/// Whether two pointers can name the same value: one is a prefix of the other,
/// segment by segment, where `*` matches any one segment.
fn overlaps(left: &[KeepSegment], right: &[KeepSegment]) -> bool {
    left.iter().zip(right).all(|(a, b)| segments_meet(a, b))
}

/// Whether `wider` retains everything `narrower` retains: `wider` is a prefix
/// of `narrower` (or equal), `*` standing for any one segment.
fn covers(wider: &[KeepSegment], narrower: &[KeepSegment]) -> bool {
    wider.len() <= narrower.len()
        && wider
            .iter()
            .zip(narrower)
            .all(|(w, n)| matches!(w, KeepSegment::Any) || w == n)
}

fn segments_meet(a: &KeepSegment, b: &KeepSegment) -> bool {
    match (a, b) {
        (KeepSegment::Any, _) | (_, KeepSegment::Any) => true,
        (KeepSegment::Key(x), KeepSegment::Key(y)) => x == y,
    }
}

/// The part of `value` the pointers retain. A pointer that ends retains the
/// whole value it reaches; objects and arrays on the way keep their kind even
/// when nothing under them is retained, so an empty list stays an empty list.
/// `None` only when a scalar is reached with pointer left to follow.
pub fn project(value: &Value, paths: &[&[KeepSegment]]) -> Option<Value> {
    if paths.iter().any(|path| path.is_empty()) {
        return Some(value.clone());
    }
    match value {
        Value::Object(map) => {
            let mut kept = Map::new();
            for (key, child) in map {
                let rest: Vec<&[KeepSegment]> = paths
                    .iter()
                    .filter_map(|path| match path.split_first() {
                        Some((KeepSegment::Any, rest)) => Some(rest),
                        Some((KeepSegment::Key(name), rest)) if name == key => Some(rest),
                        _ => None,
                    })
                    .collect();
                if rest.is_empty() {
                    continue;
                }
                if let Some(child) = project(child, &rest) {
                    kept.insert(key.clone(), child);
                }
            }
            Some(Value::Object(kept))
        }
        Value::Array(items) => {
            let mut kept = Vec::new();
            for (index, child) in items.iter().enumerate() {
                let position = index.to_string();
                let rest: Vec<&[KeepSegment]> = paths
                    .iter()
                    .filter_map(|path| match path.split_first() {
                        Some((KeepSegment::Any, rest)) => Some(rest),
                        Some((KeepSegment::Key(name), rest)) if *name == position => Some(rest),
                        _ => None,
                    })
                    .collect();
                if rest.is_empty() {
                    continue;
                }
                if let Some(child) = project(child, &rest) {
                    kept.push(child);
                }
            }
            Some(Value::Array(kept))
        }
        _ => None,
    }
}

/// The longest array anywhere in `value`.
pub fn longest_array(value: &Value) -> usize {
    match value {
        Value::Array(items) => items
            .iter()
            .map(longest_array)
            .max()
            .unwrap_or(0)
            .max(items.len()),
        Value::Object(map) => map.values().map(longest_array).max().unwrap_or(0),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn declared(value: Value) -> ServiceDisclosure {
        ServiceDisclosure::parse(&value).expect("valid disclosure")
    }

    fn answer() -> Value {
        json!({
            "node_id": "n1",
            "version": "1",
            "uds_path": "/run/node.sock",
            "rows": [
                {"id": 1, "name": "a", "detail": "/home/operator/x", "new_field": "leak"},
                {"id": 2, "name": "b", "detail": "probe: /proc/self", "new_field": "leak"}
            ]
        })
    }

    #[test]
    fn an_interim_free_text_leaf_reaches_a_principal_and_never_the_public() {
        let disclosure = declared(json!({
            "principal": ["/node_id"],
            "free_text": ["/uds_path"],
            "interim_free_text": ["/rows/*/detail"],
        }));
        let shown = shape_result(
            DisclosureClass::Principal,
            Some(&disclosure),
            answer(),
            DisclosureBounds::default(),
        )
        .unwrap();
        assert!(shown.pointer("/rows/0/detail").is_some());
        assert!(shown.get("uds_path").is_none());
        assert_eq!(
            disclosure.interim_free_text().collect::<Vec<_>>(),
            vec!["/rows/*/detail"]
        );
        assert_eq!(
            shape_result(
                DisclosureClass::Public,
                Some(&disclosure),
                answer(),
                DisclosureBounds::default()
            ),
            Ok(Value::Null)
        );
    }

    #[test]
    fn an_interim_leaf_may_not_be_withheld_public_or_a_whole_child_set() {
        for bad in [
            json!({"free_text": ["/rows"], "interim_free_text": ["/rows/*/detail"]}),
            json!({"principal": ["/rows/*/detail"], "public": ["/rows/*/detail"], "interim_free_text": ["/rows/*/detail"]}),
            json!({"interim_free_text": ["/rows/*"]}),
        ] {
            assert!(ServiceDisclosure::parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn an_item_may_raise_its_own_bounds_within_the_ceiling_and_no_further() {
        let wide = declared(json!({
            "principal": ["/rows/*/id"],
            "max_items": 4096,
            "max_result_bytes": 1_000_000,
        }));
        assert_eq!(wide.bounds().max_items, 4096);
        assert_eq!(wide.bounds().max_result_bytes, 1_000_000);
        assert_eq!(declared(json!({})).bounds(), DisclosureBounds::default());
        for bad in [
            json!({"max_items": 4097}),
            json!({"max_items": 0}),
            json!({"max_result_bytes": 8 * 1024 * 1024 + 1}),
            json!({"max_items": "many"}),
        ] {
            assert!(ServiceDisclosure::parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_operator_sees_the_whole_answer_even_with_no_declaration() {
        let shown = shape_result(
            DisclosureClass::Operator,
            None,
            answer(),
            DisclosureBounds::default(),
        )
        .unwrap();
        assert_eq!(shown, answer());
    }

    #[test]
    fn a_service_with_no_declaration_is_closed_to_every_other_class() {
        for class in [DisclosureClass::Principal, DisclosureClass::Public] {
            assert_eq!(
                shape_result(class, None, answer(), DisclosureBounds::default()),
                Err(DisclosureRefusal::NotDisclosable)
            );
        }
    }

    #[test]
    fn a_principal_gets_only_the_tier_leaves_and_never_a_field_it_does_not_name() {
        let disclosure = declared(json!({
            "principal": ["/node_id", "/version", "/rows/*/id", "/rows/*/name"],
            "free_text": ["/rows/*/detail", "/uds_path"],
        }));
        let shown = shape_result(
            DisclosureClass::Principal,
            Some(&disclosure),
            answer(),
            DisclosureBounds::default(),
        )
        .unwrap();
        assert_eq!(
            shown,
            json!({
                "node_id": "n1",
                "version": "1",
                "rows": [{"id": 1, "name": "a"}, {"id": 2, "name": "b"}]
            })
        );
        let text = shown.to_string();
        for gone in [
            "/home/operator",
            "/proc/self",
            "uds_path",
            "new_field",
            "leak",
        ] {
            assert!(!text.contains(gone), "{text}");
        }
    }

    #[test]
    fn the_public_tier_is_separate_and_closed_unless_named() {
        let disclosure = declared(json!({
            "principal": ["/node_id", "/version"],
            "public": ["/node_id"],
        }));
        let shown = shape_result(
            DisclosureClass::Public,
            Some(&disclosure),
            answer(),
            DisclosureBounds::default(),
        )
        .unwrap();
        assert_eq!(shown, json!({"node_id": "n1"}));
        let none = declared(json!({"principal": ["/node_id"]}));
        assert_eq!(
            shape_result(
                DisclosureClass::Public,
                Some(&none),
                answer(),
                DisclosureBounds::default()
            ),
            Ok(Value::Null)
        );
    }

    #[test]
    fn an_empty_tier_is_an_empty_answer_for_an_acknowledging_service() {
        let disclosure = declared(json!({"principal": []}));
        assert_eq!(
            shape_result(
                DisclosureClass::Principal,
                Some(&disclosure),
                json!({"ok": true, "path": "/x"}),
                DisclosureBounds::default()
            ),
            Ok(Value::Null)
        );
    }

    #[test]
    fn bounds_refuse_and_never_cut() {
        let disclosure = declared(json!({"principal": ["/rows/*/id", "/rows/*/name"]}));
        let many =
            json!({"rows": (0..5).map(|n| json!({"id": n, "name": "x"})).collect::<Vec<_>>()});
        assert_eq!(
            shape_result(
                DisclosureClass::Principal,
                Some(&disclosure),
                many.clone(),
                DisclosureBounds {
                    max_result_bytes: 1_000_000,
                    max_items: 4
                }
            ),
            Err(DisclosureRefusal::TooManyItems)
        );
        assert_eq!(
            shape_result(
                DisclosureClass::Principal,
                Some(&disclosure),
                many,
                DisclosureBounds {
                    max_result_bytes: 20,
                    max_items: 100
                }
            ),
            Err(DisclosureRefusal::ResultTooLarge)
        );
    }

    #[test]
    fn the_schema_refuses_free_text_in_a_tier_and_a_public_leaf_no_principal_sees() {
        assert!(
            ServiceDisclosure::parse(&json!({
                "principal": ["/rows/*"], "free_text": ["/rows/*/detail"]
            }))
            .is_err()
        );
        assert!(
            ServiceDisclosure::parse(&json!({
                "principal": ["/a"], "free_text": ["/a"]
            }))
            .is_err()
        );
        assert!(
            ServiceDisclosure::parse(&json!({
                "principal": ["/a"], "public": ["/b"]
            }))
            .is_err()
        );
        assert!(ServiceDisclosure::parse(&json!({"operator": ["/a"]})).is_err());
        assert!(ServiceDisclosure::parse(&json!({"principal": "/a"})).is_err());
        assert!(ServiceDisclosure::parse(&json!({
            "principal": ["/rows/*/id"], "free_text": ["/rows/*/detail"], "public": ["/rows/*/id"]
        })).is_ok());
    }

    #[test]
    fn a_failure_reaches_a_non_operator_as_a_code_and_a_fixed_sentence() {
        let (code, message) =
            redacted_failure("records_unverified_requires_capability: needs /home/operator/key");
        assert_eq!(code, "records_unverified_requires_capability");
        assert!(!message.contains("/home"));
        let (code, _) = redacted_failure("open /var/lib/node/state.db: permission denied");
        assert_eq!(code, "service_failed");
        let (code, _) = redacted_failure("");
        assert_eq!(code, "service_failed");
    }
}
