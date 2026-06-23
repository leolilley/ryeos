//! The caller-intent method-call unit: `{ method, args }`.
//!
//! A single type for "the method the caller wants, with its args", carried
//! verbatim across every dispatch entry boundary — the HTTP `/execute` `call`
//! block, the graph callback wire, accepted-launch options, and remote
//! forwarding. Both fields are optional: a caller may name a method, supply
//! args, both, or neither.
//!
//! This is *pre-resolution* intent — distinct from
//! [`ryeos_runtime::method_wire::MethodCallEnvelope`], which is the
//! *post-resolution* wire to the runtime process (method required, args
//! validated into a payload). Keeping the two apart is deliberate: one
//! expresses what was asked, the other what was resolved.
//!
//! `method` is the control plane — it selects daemon-owned
//! projection/validation/trust before the runtime is spawned — while `args`
//! is the data plane, validated against the method's `ArgDecl` spec.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A caller's method selection plus its args. See module docs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MethodCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
}

impl MethodCall {
    /// The requested method name, if any.
    pub fn method(&self) -> Option<&str> {
        self.method.as_deref()
    }

    /// The requested method args, if any.
    pub fn args(&self) -> Option<&Value> {
        self.args.as_ref()
    }

    /// True when neither a method nor args were supplied — a `call` block
    /// that carries no intent (treat as "no method call").
    pub fn is_empty(&self) -> bool {
        self.method.is_none() && self.args.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deserializes_all_shapes() {
        let empty: MethodCall = serde_json::from_value(json!({})).unwrap();
        assert!(empty.is_empty());

        let m: MethodCall = serde_json::from_value(json!({"method": "query"})).unwrap();
        assert_eq!(m.method(), Some("query"));
        assert!(m.args().is_none());

        let a: MethodCall = serde_json::from_value(json!({"args": {"q": "x"}})).unwrap();
        assert!(a.method().is_none());
        assert_eq!(a.args(), Some(&json!({"q": "x"})));

        let both: MethodCall =
            serde_json::from_value(json!({"method": "query", "args": {"q": "x"}})).unwrap();
        assert_eq!(both.method(), Some("query"));
        assert_eq!(both.args(), Some(&json!({"q": "x"})));
        assert!(!both.is_empty());
    }

    #[test]
    fn rejects_unknown_fields() {
        let err = serde_json::from_value::<MethodCall>(json!({"op": "query"}));
        assert!(err.is_err(), "unknown field must be rejected");
    }

    // `None` fields must be omitted, not serialized as `null`, so the wire
    // shape matches what `/execute` and the callback expect (and so a
    // round-trip is stable).
    #[test]
    fn serializes_without_null_fields() {
        let method_only = MethodCall {
            method: Some("query".into()),
            args: None,
        };
        assert_eq!(
            serde_json::to_value(&method_only).unwrap(),
            json!({"method": "query"})
        );

        let args_only = MethodCall {
            method: None,
            args: Some(json!({"q": "x"})),
        };
        assert_eq!(
            serde_json::to_value(&args_only).unwrap(),
            json!({"args": {"q": "x"}})
        );

        assert_eq!(
            serde_json::to_value(MethodCall::default()).unwrap(),
            json!({})
        );
    }
}
