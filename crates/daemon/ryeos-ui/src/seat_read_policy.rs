//! The one decision a UI seat's read passes through.
//!
//! A bound source reaches a read service when its Surface lists it, the
//! seat's grant holds the service's capabilities, and the node's signed
//! `ui_seat_reads` policy has an entry for it that admits the seat's class.
//! [`decide`] is the only place that last judgement is made. It is called at
//! mint (is there any admitting entry, and does it fit the service?), at
//! dispatch before the call (apply pins, refuse denied parameters, select the
//! entry by its conditions) and, through the returned [`Shaping`], at
//! disclosure (drop every field not kept, then enforce the bounds).
//!
//! The policy only narrows. Nothing here grants a capability, widens a lane
//! or admits a record the service would refuse, and every doubt is a refusal.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use ryeos_node_runtime::node_policy::sections::ui_seat_reads::{
    KeepSegment, MARKER_PRINCIPAL_ID, MARKER_PROJECT_ROOT, MARKER_PROJECT_ROOT_OR_NULL, SeatClass,
    SeatReadEntry, UiSeatReadsPolicy,
};

/// Services under this namespace are the UI plane's own projections. Each
/// scopes and authorizes its result itself from the calling seat, so the
/// policy does not gate them. The namespace is the canonical ref's own
/// structure: kind `service`, no suffix, and at least one segment after the
/// `ui/ryeos-ui/` directory. It is read from no flag and no handler name.
pub const UI_PLANE_SERVICE_PREFIX: &str = "ui/ryeos-ui/";

/// Whether a canonical ref names a UI-plane service, exempt from the policy.
pub fn is_ui_plane_service(target_ref: &str) -> bool {
    let Ok(parsed) = ryeos_engine::canonical_ref::CanonicalRef::parse(target_ref) else {
        return false;
    };
    parsed.kind == "service"
        && parsed.suffix.is_none()
        && parsed
            .bare_id
            .strip_prefix(UI_PLANE_SERVICE_PREFIX)
            .is_some_and(|rest| !rest.is_empty())
}

/// Why a seat read was not admitted. Display never carries a parameter value
/// or any part of a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeatReadRefusal {
    /// No entry of the policy admits this class for this service.
    NoEntry { target: String },
    /// An entry names a parameter the service does not declare.
    UndeclaredParameter { target: String, name: String },
    /// The call's parameters are not a JSON object.
    ParametersNotAnObject { target: String },
    /// The call carries a parameter the entry forbids a seat to send.
    ParameterDenied { target: String, name: String },
    /// A pin needs a session value the session does not have.
    MarkerUnavailable { target: String, name: String },
    /// No entry's conditions hold for the call.
    NoMatchingEntry { target: String },
    /// More than one entry admits the call.
    AmbiguousEntries { target: String },
    /// The kept result is larger than the entry allows.
    ResultTooLarge { target: String },
    /// An array of the kept result is longer than the entry allows.
    TooManyItems { target: String },
    /// The result is not a structure the entry can keep any of.
    NothingDisclosable { target: String },
}

impl SeatReadRefusal {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoEntry { .. } => "seat_read_no_entry",
            Self::UndeclaredParameter { .. } => "seat_read_undeclared_parameter",
            Self::ParametersNotAnObject { .. } => "seat_read_parameters_not_an_object",
            Self::ParameterDenied { .. } => "seat_read_parameter_denied",
            Self::MarkerUnavailable { .. } => "seat_read_marker_unavailable",
            Self::NoMatchingEntry { .. } => "seat_read_no_matching_entry",
            Self::AmbiguousEntries { .. } => "seat_read_ambiguous_entries",
            Self::ResultTooLarge { .. } => "seat_read_result_too_large",
            Self::TooManyItems { .. } => "seat_read_too_many_items",
            Self::NothingDisclosable { .. } => "seat_read_nothing_disclosable",
        }
    }
}

impl std::fmt::Display for SeatReadRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = self.code();
        match self {
            Self::NoEntry { target } => write!(
                f,
                "{code}: the node's seat read policy has no entry admitting this seat for `{target}`"
            ),
            Self::UndeclaredParameter { target, name } => write!(
                f,
                "{code}: the seat read policy entry for `{target}` names parameter `{name}`, which the service does not declare"
            ),
            Self::ParametersNotAnObject { target } => {
                write!(f, "{code}: parameters for `{target}` are not an object")
            }
            Self::ParameterDenied { target, name } => write!(
                f,
                "{code}: a seat may not send parameter `{name}` to `{target}`"
            ),
            Self::MarkerUnavailable { target, name } => write!(
                f,
                "{code}: parameter `{name}` of `{target}` is pinned to a session value this session does not have"
            ),
            Self::NoMatchingEntry { target } => write!(
                f,
                "{code}: no seat read policy entry for `{target}` admits these parameters"
            ),
            Self::AmbiguousEntries { target } => write!(
                f,
                "{code}: more than one seat read policy entry for `{target}` admits these parameters"
            ),
            Self::ResultTooLarge { target } => write!(
                f,
                "{code}: the disclosable result of `{target}` exceeds the policy's byte bound"
            ),
            Self::TooManyItems { target } => write!(
                f,
                "{code}: the disclosable result of `{target}` exceeds the policy's item bound"
            ),
            Self::NothingDisclosable { target } => write!(
                f,
                "{code}: the result of `{target}` has no shape the policy can disclose"
            ),
        }
    }
}

impl std::error::Error for SeatReadRefusal {}

/// What the session contributes to pinned parameters.
#[derive(Debug, Clone, Copy)]
pub struct SessionFacts<'a> {
    pub principal_id: &'a str,
    pub project_root: Option<&'a str>,
}

/// The parameters of one dispatch request, as authored and supplied.
#[derive(Debug, Clone)]
pub struct RequestFacts<'a> {
    pub params: Value,
    pub session: SessionFacts<'a>,
}

/// An admitted read.
#[derive(Debug)]
pub struct Admitted<'a> {
    /// The parameters to execute with, after pinning. `None` at mint, where
    /// no call exists, and for a UI-plane service whose parameters are not
    /// touched.
    pub params: Option<Value>,
    /// How to shape the result, or `None` for a UI-plane service.
    pub shaping: Option<Shaping<'a>>,
}

/// The disclosure half of an admitted read.
#[derive(Debug)]
pub struct Shaping<'a> {
    target: String,
    entry: &'a SeatReadEntry,
}

impl Shaping<'_> {
    /// Keep only the fields the entry names, then refuse a result that is
    /// larger than its bounds. Nothing is ever cut to fit.
    pub fn apply(&self, result: Value) -> Result<Value, SeatReadRefusal> {
        let paths: Vec<&[KeepSegment]> =
            self.entry.keep.iter().map(|path| path.segments()).collect();
        let Some(kept) = project(&result, &paths) else {
            return Err(SeatReadRefusal::NothingDisclosable {
                target: self.target.clone(),
            });
        };
        if longest_array(&kept) > usize::try_from(self.entry.max_items).unwrap_or(usize::MAX) {
            return Err(SeatReadRefusal::TooManyItems {
                target: self.target.clone(),
            });
        }
        let fits = serde_json::to_vec(&kept)
            .ok()
            .and_then(|bytes| u64::try_from(bytes.len()).ok())
            .is_some_and(|length| length <= self.entry.max_result_bytes);
        if fits {
            Ok(kept)
        } else {
            Err(SeatReadRefusal::ResultTooLarge {
                target: self.target.clone(),
            })
        }
    }
}

/// Decide one seat read.
///
/// With `request` absent this is the mint-time question: does any entry
/// admit this class for this service, and does every entry for it fit the
/// service's declared parameters. With a request it is the dispatch-time
/// decision, returning the parameters to execute and the shaping to apply.
pub fn decide<'a>(
    policy: &'a UiSeatReadsPolicy,
    class: SeatClass,
    target_ref: &str,
    parameter_schema: &BTreeMap<String, String>,
    request: Option<RequestFacts<'_>>,
) -> Result<Admitted<'a>, SeatReadRefusal> {
    if is_ui_plane_service(target_ref) {
        return Ok(Admitted {
            params: request.map(|request| request.params),
            shaping: None,
        });
    }
    let candidates: Vec<&SeatReadEntry> = policy
        .entries_for(target_ref)
        .filter(|entry| entry.classes.contains(&class))
        .collect();
    if candidates.is_empty() {
        return Err(SeatReadRefusal::NoEntry {
            target: target_ref.to_owned(),
        });
    }
    for entry in &candidates {
        if let Some(name) = entry
            .mentioned_parameters()
            .find(|name| !parameter_schema.contains_key(*name))
        {
            return Err(SeatReadRefusal::UndeclaredParameter {
                target: target_ref.to_owned(),
                name: name.to_owned(),
            });
        }
    }
    let Some(request) = request else {
        return Ok(Admitted {
            params: None,
            shaping: None,
        });
    };

    let supplied = match request.params {
        Value::Object(map) => map,
        Value::Null => Map::new(),
        _ => {
            return Err(SeatReadRefusal::ParametersNotAnObject {
                target: target_ref.to_owned(),
            });
        }
    };
    let mut matching: Vec<(&SeatReadEntry, Map<String, Value>)> = Vec::new();
    let mut unavailable: Option<SeatReadRefusal> = None;
    for entry in candidates {
        let mut pinned = supplied.clone();
        let mut resolvable = true;
        for (name, value) in &entry.pin {
            match resolve_pin(value, request.session) {
                Some(resolved) => {
                    pinned.insert(name.clone(), resolved);
                }
                None => {
                    resolvable = false;
                    unavailable.get_or_insert(SeatReadRefusal::MarkerUnavailable {
                        target: target_ref.to_owned(),
                        name: name.clone(),
                    });
                    break;
                }
            }
        }
        if !resolvable {
            continue;
        }
        let holds = entry
            .when
            .iter()
            .all(|(name, set)| pinned.get(name).is_some_and(|value| set.contains(value)));
        if holds {
            matching.push((entry, pinned));
        }
    }
    let (entry, pinned) = match matching.len() {
        0 => {
            return Err(unavailable.unwrap_or(SeatReadRefusal::NoMatchingEntry {
                target: target_ref.to_owned(),
            }));
        }
        1 => matching.remove(0),
        _ => {
            return Err(SeatReadRefusal::AmbiguousEntries {
                target: target_ref.to_owned(),
            });
        }
    };
    if let Some(name) = entry
        .deny_params
        .iter()
        .find(|name| supplied.contains_key(name.as_str()))
    {
        return Err(SeatReadRefusal::ParameterDenied {
            target: target_ref.to_owned(),
            name: name.clone(),
        });
    }
    Ok(Admitted {
        params: Some(Value::Object(pinned)),
        shaping: Some(Shaping {
            target: target_ref.to_owned(),
            entry,
        }),
    })
}

fn resolve_pin(value: &Value, session: SessionFacts<'_>) -> Option<Value> {
    match value {
        Value::String(marker) if marker == MARKER_PRINCIPAL_ID => {
            Some(Value::String(session.principal_id.to_owned()))
        }
        Value::String(marker) if marker == MARKER_PROJECT_ROOT => session
            .project_root
            .map(|root| Value::String(root.to_owned())),
        Value::String(marker) if marker == MARKER_PROJECT_ROOT_OR_NULL => Some(
            session
                .project_root
                .map_or(Value::Null, |root| Value::String(root.to_owned())),
        ),
        other => Some(other.clone()),
    }
}

/// The part of `value` the pointers retain. A pointer that ends retains the
/// whole value it reaches; objects and arrays on the way keep their kind even
/// when nothing under them is retained, so an empty list stays an empty list.
/// `None` only when a scalar is reached with pointer left to follow.
fn project(value: &Value, paths: &[&[KeepSegment]]) -> Option<Value> {
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

fn longest_array(value: &Value) -> usize {
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

    const TARGET: &str = "service:test/records";

    fn schema(names: &[&str]) -> BTreeMap<String, String> {
        names
            .iter()
            .map(|name| ((*name).to_owned(), "string".to_owned()))
            .collect()
    }

    fn policy(entries: Vec<Value>) -> UiSeatReadsPolicy {
        let policy: UiSeatReadsPolicy =
            serde_json::from_value(json!({"schema": 1, "reads": entries})).unwrap();
        policy.validate().unwrap();
        policy
    }

    fn entry(extra: Value) -> Value {
        let mut base = json!({
            "target": TARGET,
            "classes": ["local_operator", "hosted_principal"],
            "keep": ["/kind", "/rows/*/id"],
            "max_result_bytes": 4096,
            "max_items": 3
        });
        for (key, value) in extra.as_object().unwrap() {
            base[key] = value.clone();
        }
        base
    }

    fn request(params: Value) -> Option<RequestFacts<'static>> {
        Some(RequestFacts {
            params,
            session: SessionFacts {
                principal_id: "fp:seat",
                project_root: Some("/project"),
            },
        })
    }

    fn call(
        policy: &UiSeatReadsPolicy,
        class: SeatClass,
        params: Value,
    ) -> Result<Admitted<'_>, SeatReadRefusal> {
        decide(
            policy,
            class,
            TARGET,
            &schema(&["kind", "holder", "root", "include_unverified"]),
            request(params),
        )
    }

    #[test]
    fn ui_plane_exemption_is_exact() {
        for exempt in [
            "service:ui/ryeos-ui/threads/list",
            "service:ui/ryeos-ui/a",
            "service:ui/ryeos-ui/field/project",
        ] {
            assert!(is_ui_plane_service(exempt), "{exempt}");
        }
        for gated in [
            "service:ui/ryeos-ui",
            "service:ui/ryeos-ui/",
            "service:ui/ryeos-uix/threads/list",
            "service:ui/session/surfaces/open",
            "service:ui/launch/mint",
            "service:xui/ryeos-ui/threads/list",
            "service:ryeos-ui/threads/list",
            "service:node/status",
            "service:ui/ryeos-ui/threads/list@t:2026",
            "tool:ui/ryeos-ui/threads/list",
            "directive:ui/ryeos-ui/threads/list",
            "ui/ryeos-ui/threads/list",
            "service:ui/ryeos-ui/../node/status",
            "",
        ] {
            assert!(!is_ui_plane_service(gated), "{gated}");
        }
    }

    #[test]
    fn an_exempt_service_needs_no_entry_and_is_left_alone() {
        let empty = UiSeatReadsPolicy::admitting_nothing();
        let admitted = decide(
            &empty,
            SeatClass::HostedPrincipal,
            "service:ui/ryeos-ui/threads/list",
            &BTreeMap::new(),
            request(json!({"x": 1})),
        )
        .unwrap();
        assert_eq!(admitted.params, Some(json!({"x": 1})));
        assert!(admitted.shaping.is_none());
        let at_mint = decide(
            &empty,
            SeatClass::LocalOperator,
            "service:ui/ryeos-ui/threads/list",
            &BTreeMap::new(),
            None,
        )
        .unwrap();
        assert!(at_mint.params.is_none() && at_mint.shaping.is_none());
    }

    #[test]
    fn no_entry_is_refused_at_mint_and_at_dispatch() {
        let empty = UiSeatReadsPolicy::admitting_nothing();
        for class in [SeatClass::LocalOperator, SeatClass::HostedPrincipal] {
            for facts in [None, request(json!({}))] {
                let refusal = decide(&empty, class, TARGET, &schema(&[]), facts).unwrap_err();
                assert_eq!(refusal.code(), "seat_read_no_entry");
            }
        }
        // An entry for another service does not admit this one.
        let other = policy(vec![entry(json!({"target": "service:test/other"}))]);
        assert!(matches!(
            call(&other, SeatClass::LocalOperator, json!({})),
            Err(SeatReadRefusal::NoEntry { .. })
        ));
    }

    #[test]
    fn classes_are_split() {
        let local_only = policy(vec![entry(json!({"classes": ["local_operator"]}))]);
        assert!(call(&local_only, SeatClass::LocalOperator, json!({})).is_ok());
        assert!(matches!(
            call(&local_only, SeatClass::HostedPrincipal, json!({})),
            Err(SeatReadRefusal::NoEntry { .. })
        ));
        assert!(matches!(
            decide(
                &local_only,
                SeatClass::HostedPrincipal,
                TARGET,
                &schema(&[]),
                None
            ),
            Err(SeatReadRefusal::NoEntry { .. })
        ));

        // Different shaping per class for one service.
        let split = policy(vec![
            entry(json!({"classes": ["local_operator"], "keep": ["/kind", "/path"]})),
            entry(json!({"classes": ["hosted_principal"], "keep": ["/kind"]})),
        ]);
        let result = json!({"kind": "k", "path": "/home/leo/x"});
        let local = call(&split, SeatClass::LocalOperator, json!({})).unwrap();
        assert_eq!(
            local.shaping.unwrap().apply(result.clone()).unwrap(),
            json!({"kind": "k", "path": "/home/leo/x"})
        );
        let hosted = call(&split, SeatClass::HostedPrincipal, json!({})).unwrap();
        assert_eq!(
            hosted.shaping.unwrap().apply(result).unwrap(),
            json!({"kind": "k"})
        );
    }

    #[test]
    fn mint_refuses_an_entry_naming_an_undeclared_parameter() {
        let p = policy(vec![entry(
            json!({"pin": {"holder": "@session:principal_id"}}),
        )]);
        let refusal = decide(
            &p,
            SeatClass::LocalOperator,
            TARGET,
            &schema(&["kind"]),
            None,
        )
        .unwrap_err();
        assert!(matches!(
            refusal,
            SeatReadRefusal::UndeclaredParameter { ref name, .. } if name == "holder"
        ));
        for mentioned in [
            json!({"deny_params": ["gone"]}),
            json!({"when": {"gone": ["x"]}}),
        ] {
            let p = policy(vec![entry(mentioned)]);
            assert!(matches!(
                decide(
                    &p,
                    SeatClass::LocalOperator,
                    TARGET,
                    &schema(&["kind"]),
                    None
                ),
                Err(SeatReadRefusal::UndeclaredParameter { .. })
            ));
        }
        // The same refusal at dispatch, so a stale binding cannot slip it.
        let p = policy(vec![entry(
            json!({"pin": {"holder": "@session:principal_id"}}),
        )]);
        assert!(
            decide(
                &p,
                SeatClass::LocalOperator,
                TARGET,
                &schema(&["kind"]),
                request(json!({}))
            )
            .is_err()
        );
    }

    #[test]
    fn a_pin_overrides_what_the_client_sent() {
        let p = policy(vec![entry(json!({
            "pin": {"holder": "@session:principal_id", "root": "@session:project_root", "kind": "fixed"}
        }))]);
        let admitted = call(
            &p,
            SeatClass::HostedPrincipal,
            json!({"holder": "fp:someone-else", "root": "/elsewhere", "kind": "chosen", "extra": 1}),
        )
        .unwrap();
        assert_eq!(
            admitted.params.unwrap(),
            json!({"holder": "fp:seat", "root": "/project", "kind": "fixed", "extra": 1})
        );
        // Absent parameters are pinned too.
        let admitted = call(&p, SeatClass::LocalOperator, json!({})).unwrap();
        assert_eq!(admitted.params.unwrap()["holder"], "fp:seat");
    }

    #[test]
    fn project_root_pins_follow_the_session() {
        let p = policy(vec![entry(
            json!({"pin": {"root": "@session:project_root"}}),
        )]);
        let no_project = Some(RequestFacts {
            params: json!({}),
            session: SessionFacts {
                principal_id: "fp:seat",
                project_root: None,
            },
        });
        let refusal = decide(
            &p,
            SeatClass::LocalOperator,
            TARGET,
            &schema(&["root"]),
            no_project,
        )
        .unwrap_err();
        assert_eq!(refusal.code(), "seat_read_marker_unavailable");

        let p = policy(vec![entry(
            json!({"pin": {"root": "@session:project_root_or_null"}}),
        )]);
        let no_project = Some(RequestFacts {
            params: json!({"root": "/chosen"}),
            session: SessionFacts {
                principal_id: "fp:seat",
                project_root: None,
            },
        });
        let admitted = decide(
            &p,
            SeatClass::LocalOperator,
            TARGET,
            &schema(&["root"]),
            no_project,
        )
        .unwrap();
        assert_eq!(admitted.params.unwrap(), json!({"root": null}));
    }

    #[test]
    fn denied_parameters_are_refused_whatever_their_value() {
        let p = policy(vec![entry(json!({"deny_params": ["include_unverified"]}))]);
        for value in [json!(true), json!(false), json!(null), json!("x")] {
            let refusal = call(
                &p,
                SeatClass::LocalOperator,
                json!({"include_unverified": value}),
            )
            .unwrap_err();
            assert_eq!(refusal.code(), "seat_read_parameter_denied");
            assert!(!refusal.to_string().contains("\"x\""));
        }
        assert!(call(&p, SeatClass::LocalOperator, json!({"kind": "a"})).is_ok());
    }

    #[test]
    fn when_must_hold_after_pinning() {
        let p = policy(vec![entry(json!({"when": {"kind": ["a", "b"]}}))]);
        assert!(call(&p, SeatClass::LocalOperator, json!({"kind": "a"})).is_ok());
        for params in [json!({"kind": "c"}), json!({}), json!({"kind": null})] {
            let refusal = call(&p, SeatClass::LocalOperator, params).unwrap_err();
            assert_eq!(refusal.code(), "seat_read_no_matching_entry");
        }
        // A pinned value satisfies a condition the client could not.
        let pinned = policy(vec![entry(json!({
            "when": {"kind": ["fixed"]},
            "pin": {"kind": "fixed"}
        }))]);
        assert!(call(&pinned, SeatClass::LocalOperator, json!({"kind": "other"})).is_ok());
        // A condition on a pinned value the client cannot change is not
        // satisfied by what the client sends.
        let overridden = policy(vec![entry(json!({
            "when": {"kind": ["wanted"]},
            "pin": {"kind": "fixed"}
        }))]);
        assert!(
            call(
                &overridden,
                SeatClass::LocalOperator,
                json!({"kind": "wanted"})
            )
            .is_err()
        );
    }

    #[test]
    fn entries_with_disjoint_conditions_select_exactly_one() {
        let p = policy(vec![
            entry(json!({"when": {"kind": ["a"]}, "keep": ["/kind"]})),
            entry(json!({"when": {"kind": ["b"]}, "keep": ["/rows"]})),
        ]);
        let a = call(&p, SeatClass::LocalOperator, json!({"kind": "a"})).unwrap();
        assert_eq!(
            a.shaping
                .unwrap()
                .apply(json!({"kind": 1, "rows": []}))
                .unwrap(),
            json!({"kind": 1})
        );
        let b = call(&p, SeatClass::LocalOperator, json!({"kind": "b"})).unwrap();
        assert_eq!(
            b.shaping
                .unwrap()
                .apply(json!({"kind": 1, "rows": []}))
                .unwrap(),
            json!({"rows": []})
        );
    }

    #[test]
    fn overlapping_entries_that_slip_past_load_are_refused_at_call() {
        // Built without the loader's validation, as a stale value might be.
        let unchecked: UiSeatReadsPolicy = serde_json::from_value(json!({
            "schema": 1,
            "reads": [entry(json!({})), entry(json!({}))]
        }))
        .unwrap();
        assert!(unchecked.validate().is_err());
        let refusal = call(&unchecked, SeatClass::LocalOperator, json!({})).unwrap_err();
        assert_eq!(refusal.code(), "seat_read_ambiguous_entries");
    }

    #[test]
    fn parameters_must_be_an_object() {
        let p = policy(vec![entry(json!({}))]);
        for params in [json!([1]), json!("x"), json!(3)] {
            assert!(matches!(
                call(&p, SeatClass::LocalOperator, params),
                Err(SeatReadRefusal::ParametersNotAnObject { .. })
            ));
        }
        let admitted = call(&p, SeatClass::LocalOperator, Value::Null).unwrap();
        assert_eq!(admitted.params.unwrap(), json!({}));
    }

    #[test]
    fn keep_drops_unknown_and_later_added_fields() {
        let p = policy(vec![entry(json!({
            "keep": ["/kind", "/rows/*/id", "/nested/a"]
        }))]);
        let shaping = call(&p, SeatClass::LocalOperator, json!({}))
            .unwrap()
            .shaping
            .unwrap();
        let result = json!({
            "kind": "k",
            "path": "/srv/secret",
            "added_in_a_later_release": {"deep": 1},
            "rows": [
                {"id": 1, "path": "/x"},
                {"id": 2, "added": true}
            ],
            "nested": {"a": {"b": 1}, "c": 2}
        });
        assert_eq!(
            shaping.apply(result).unwrap(),
            json!({
                "kind": "k",
                "rows": [{"id": 1}, {"id": 2}],
                "nested": {"a": {"b": 1}}
            })
        );
    }

    #[test]
    fn a_pointer_that_ends_keeps_the_whole_subtree() {
        let p = policy(vec![entry(json!({"keep": ["/rows", "/rows/*/id"]}))]);
        let shaping = call(&p, SeatClass::LocalOperator, json!({}))
            .unwrap()
            .shaping
            .unwrap();
        let result = json!({"rows": [{"id": 1, "x": 2}], "other": 1});
        assert_eq!(
            shaping.apply(result).unwrap(),
            json!({"rows": [{"id": 1, "x": 2}]})
        );
    }

    #[test]
    fn wildcards_match_one_key_or_index_and_literal_indices_match_one() {
        let p = policy(vec![entry(json!({"keep": ["/map/*/v", "/rows/1/id"]}))]);
        let shaping = call(&p, SeatClass::LocalOperator, json!({}))
            .unwrap()
            .shaping
            .unwrap();
        let result = json!({
            "map": {"a": {"v": 1, "w": 2}, "b": {"v": 3}},
            "rows": [{"id": 10}, {"id": 11}, {"id": 12}]
        });
        assert_eq!(
            shaping.apply(result).unwrap(),
            json!({"map": {"a": {"v": 1}, "b": {"v": 3}}, "rows": [{"id": 11}]})
        );
        // A wildcard does not reach through more than one level.
        let result = json!({"map": {"a": {"deeper": {"v": 1}}}, "rows": []});
        assert_eq!(
            shaping.apply(result).unwrap(),
            json!({"map": {"a": {}}, "rows": []})
        );
    }

    #[test]
    fn an_empty_list_stays_an_empty_list_and_a_missing_field_is_not_an_error() {
        let p = policy(vec![entry(json!({}))]);
        let shaping = call(&p, SeatClass::LocalOperator, json!({}))
            .unwrap()
            .shaping
            .unwrap();
        assert_eq!(
            shaping.apply(json!({"rows": [], "kind": "k"})).unwrap(),
            json!({"rows": [], "kind": "k"})
        );
        assert_eq!(shaping.apply(json!({})).unwrap(), json!({}));
        assert_eq!(shaping.apply(json!({"unrelated": 1})).unwrap(), json!({}));
    }

    #[test]
    fn a_scalar_result_has_nothing_to_disclose() {
        let p = policy(vec![entry(json!({}))]);
        let shaping = call(&p, SeatClass::LocalOperator, json!({}))
            .unwrap()
            .shaping
            .unwrap();
        for result in [json!("text"), json!(1), json!(null), json!(true)] {
            assert_eq!(
                shaping.apply(result).unwrap_err().code(),
                "seat_read_nothing_disclosable"
            );
        }
    }

    #[test]
    fn bounds_refuse_and_never_truncate() {
        let p = policy(vec![entry(json!({"max_result_bytes": 40, "max_items": 3}))]);
        let shaping = call(&p, SeatClass::LocalOperator, json!({}))
            .unwrap()
            .shaping
            .unwrap();

        // Exactly at the item bound passes; one more is refused.
        let at_bound = json!({"rows": [{"id": 1}, {"id": 2}, {"id": 3}]});
        assert_eq!(shaping.apply(at_bound.clone()).unwrap(), at_bound);
        let over_items = json!({"rows": [{"id": 1}, {"id": 2}, {"id": 3}, {"id": 4}]});
        assert_eq!(
            shaping.apply(over_items).unwrap_err().code(),
            "seat_read_too_many_items"
        );

        // Bytes are judged on what is kept, not on what was dropped.
        let padded = json!({"rows": [{"id": 1, "pad": "x".repeat(10_000)}], "kind": "k"});
        assert_eq!(
            shaping.apply(padded).unwrap(),
            json!({"rows": [{"id": 1}], "kind": "k"})
        );
        let too_big = json!({"kind": "k".repeat(100)});
        assert_eq!(
            shaping.apply(too_big).unwrap_err().code(),
            "seat_read_result_too_large"
        );
        // `{"kind":"` and `"}` take 11 bytes, so 29 more fill the bound exactly.
        let exact = json!({"kind": "k".repeat(29)});
        assert_eq!(serde_json::to_vec(&exact).unwrap().len(), 40);
        assert!(shaping.apply(exact).is_ok());
        assert!(shaping.apply(json!({"kind": "k".repeat(30)})).is_err());
    }

    #[test]
    fn the_item_bound_counts_nested_arrays() {
        let p = policy(vec![entry(json!({"keep": ["/rows"], "max_items": 2}))]);
        let shaping = call(&p, SeatClass::LocalOperator, json!({}))
            .unwrap()
            .shaping
            .unwrap();
        let nested = json!({"rows": [[1, 2, 3]]});
        assert_eq!(
            shaping.apply(nested).unwrap_err().code(),
            "seat_read_too_many_items"
        );
    }

    #[test]
    fn refusal_text_never_carries_values() {
        let p = policy(vec![entry(json!({"deny_params": ["include_unverified"]}))]);
        let refusal = call(
            &p,
            SeatClass::LocalOperator,
            json!({"include_unverified": "SECRET-VALUE"}),
        )
        .unwrap_err();
        assert!(!refusal.to_string().contains("SECRET-VALUE"));
        assert!(refusal.to_string().contains("include_unverified"));
    }
}
