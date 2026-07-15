//! Seat state as a deterministic fold over seat events.
//!
//! The seat is a thread; its state (input route, selection, watch set) is
//! a last-writer-wins fold over `seat.facet` events on the seat thread's
//! braid, ordered by sequence. This module is the engine-side fold plus a
//! local append log used while the engine itself holds authority — the
//! event shape matches the seat-thread braid so swapping authority to the
//! daemon is a transport change, never a representation change.
//!
//! Facet keys are an open vocabulary. Engine lenses read only the
//! engine-reserved keys below; content-declared affordances may write any
//! key outside the reserved set (e.g. `tile.<id>.filter`). Unknown keys
//! fold harmlessly.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Engine-reserved facet keys. Engine lenses read only these.
pub const KEY_INPUT_ROUTE: &str = "input.route";
pub const KEY_SELECTION: &str = "selection";
pub const KEY_WATCH: &str = "watch";

/// One event on the seat braid. `seq` mirrors the braid's chain sequence:
/// monotonic, assigned by whoever holds append authority (this log while
/// engine-local; the daemon once the seat thread lands).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeatEvent {
    pub seq: u64,
    #[serde(flatten)]
    pub kind: SeatEventKind,
}

/// Seat event kinds. Open vocabulary: foreign kinds arriving off the
/// braid must fold harmlessly, so the fold only interprets what it knows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event_type", content = "payload", rename_all = "snake_case")]
pub enum SeatEventKind {
    #[serde(rename = "seat.facet")]
    Facet { key: String, value: Value },
}

/// Append-ordered local seat log (slice-1 staging for the seat thread).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SeatLog {
    events: Vec<SeatEvent>,
    next_seq: u64,
}

impl SeatLog {
    pub fn append(&mut self, kind: SeatEventKind) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.events.push(SeatEvent { seq, kind });
        seq
    }

    pub fn append_facet(&mut self, key: impl Into<String>, value: Value) -> u64 {
        self.append(SeatEventKind::Facet {
            key: key.into(),
            value,
        })
    }

    pub fn append_replayed(&mut self, event: SeatEvent) {
        self.next_seq = self.next_seq.max(event.seq.saturating_add(1));
        self.events.push(event);
    }

    pub fn events(&self) -> &[SeatEvent] {
        &self.events
    }

    /// Fold the log into facet state: last-writer-wins per key, ordered
    /// by seq. Returns the folded map and the seq of the last event that
    /// wrote each key.
    pub fn fold(&self) -> SeatFold {
        let mut fold = SeatFold::default();
        for event in &self.events {
            fold.apply(event);
        }
        fold
    }
}

/// Deterministic facet fold: key -> (last write seq, value).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SeatFold {
    facets: BTreeMap<String, (u64, Value)>,
}

impl SeatFold {
    pub fn apply(&mut self, event: &SeatEvent) {
        match &event.kind {
            SeatEventKind::Facet { key, value } => {
                self.facets.insert(key.clone(), (event.seq, value.clone()));
            }
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.facets.get(key).map(|(_, value)| value)
    }

    /// Snapshot every facet's current value (dropping the write seqs). Used by
    /// the lens stack to capture the facet context a step-in leaves, so a pop
    /// can re-append the ones that changed.
    pub fn snapshot(&self) -> BTreeMap<String, Value> {
        self.facets
            .iter()
            .map(|(key, (_, value))| (key.clone(), value.clone()))
            .collect()
    }

    /// Seq of the last event that wrote `key`; None if never written.
    pub fn seq_of(&self, key: &str) -> Option<u64> {
        self.facets.get(key).map(|(seq, _)| *seq)
    }

    /// Typed lens for the input route. Absent or unparseable values fold
    /// to the no-target route rather than erroring — unknown shapes from
    /// newer writers must not break older readers.
    pub fn input_route(&self) -> InputRoute {
        self.get(KEY_INPUT_ROUTE)
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default()
    }
}

/// Where plain text lands on submit. Live, retargetable seat state —
/// never a permanent binding. The surface's `input:` block declares the
/// starting value; launches ratchet `thread`; retargeting is grammar.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InputRoute {
    /// The invocation template plain text fills. `None` = no target: the
    /// submit path warns instead of guessing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invoke: Option<InvokeTemplate>,
    /// Conversation ratchet: the chain head a follow-up continues from.
    /// Moves with each turn (the latest successor) so a submit braids onto
    /// the newest turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    /// The conversation's chain root — constant for the life of the chain.
    /// The feed follows THIS (the whole braid), not `thread` (the moving
    /// head): a follow-up retargets the head but keeps showing the
    /// conversation from its root. Set on the first turn (root == head),
    /// preserved across continuations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_root: Option<String>,
    /// Which node (reserved for the multi-node phase).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    /// Fixed extras merged into the invocation parameters
    /// (e.g. `directive: <profile ref>` for the thread-input service).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

impl InputRoute {
    /// Resolve the route declared by a surface's `input:` block, if any.
    pub fn from_surface_input(input: Option<&Value>) -> Option<Self> {
        let route = input?.get("route")?;
        serde_json::from_value(route.clone()).ok()
    }

    pub fn has_target(&self) -> bool {
        self.invoke.is_some()
    }
}

/// The pinned invocation template. The client never interprets refs or
/// kinds — it constructs invocations and the substrate decides.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InvokeTemplate {
    /// Rye plane, direct service invocation (the ground-verb default:
    /// `service:threads/input` with text bound to its `input` field).
    Service {
        #[serde(rename = "ref")]
        item_ref: String,
    },
    /// Rye plane, pinned command token prefix (text appended as ONE tail
    /// argument, never shell-split).
    Command { tokens: Vec<String> },
    /// Ui plane: text written to a facet key (e.g. a tile's filter).
    UiFacet { key: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fold_is_last_writer_wins_per_key() {
        let mut log = SeatLog::default();
        log.append_facet(KEY_SELECTION, json!({"item": "a"}));
        let route_seq = log.append_facet(KEY_INPUT_ROUTE, json!({"thread": "T-1"}));
        log.append_facet(KEY_SELECTION, json!({"item": "b"}));

        let fold = log.fold();
        assert_eq!(fold.get(KEY_SELECTION), Some(&json!({"item": "b"})));
        assert_eq!(fold.seq_of(KEY_INPUT_ROUTE), Some(route_seq));
        assert_eq!(fold.input_route().thread.as_deref(), Some("T-1"));
    }

    #[test]
    fn replay_fold_equals_live_fold_full_cycle() {
        // The proof that "the seat is a thread": fold-from-braid (reattach)
        // must equal the live fold. Open, append facets, persist the braid,
        // simulate a restart with a fresh log, replay, assert the folds match.
        let mut live = SeatLog::default();
        live.append_facet(KEY_SELECTION, json!({"item": "a"}));
        live.append_facet(KEY_INPUT_ROUTE, json!({"thread": "T-7"}));
        live.append_facet(KEY_SELECTION, json!({"item": "b"}));
        let before = live.fold();

        // Persist the braid exactly as the daemon would (seat.facet events).
        let braid: Vec<Value> = live
            .events()
            .iter()
            .map(|event| serde_json::to_value(event).unwrap())
            .collect();

        // Restart: a fresh, empty log reattaches by replaying the braid.
        let mut reattached = SeatLog::default();
        for event in &braid {
            let seat_event: SeatEvent = serde_json::from_value(event.clone()).unwrap();
            reattached.append_replayed(seat_event);
        }

        assert_eq!(
            reattached.fold(),
            before,
            "replay fold must equal the live fold"
        );
        // Appending after reattach continues the sequence, not restarts it.
        let seq = reattached.append_facet(KEY_SELECTION, json!({"item": "c"}));
        assert_eq!(seq, 3);
        assert_eq!(
            reattached.fold().get(KEY_SELECTION),
            Some(&json!({"item": "c"}))
        );
    }

    #[test]
    fn replayed_events_advance_next_seq() {
        let mut log = SeatLog::default();
        log.append_replayed(SeatEvent {
            seq: 7,
            kind: SeatEventKind::Facet {
                key: KEY_SELECTION.to_string(),
                value: json!({"item": "a"}),
            },
        });

        let seq = log.append_facet(KEY_SELECTION, json!({"item": "b"}));

        assert_eq!(seq, 8);
        assert_eq!(log.fold().get(KEY_SELECTION), Some(&json!({"item": "b"})));
    }

    #[test]
    fn unknown_keys_fold_harmlessly() {
        let mut log = SeatLog::default();
        log.append_facet("tile.t3.filter", json!("schedules"));
        let fold = log.fold();
        assert_eq!(fold.get("tile.t3.filter"), Some(&json!("schedules")));
        assert!(!fold.input_route().has_target());
    }

    #[test]
    fn route_lens_defaults_on_absent_or_invalid() {
        let fold = SeatLog::default().fold();
        assert_eq!(fold.input_route(), InputRoute::default());

        let mut log = SeatLog::default();
        log.append_facet(KEY_INPUT_ROUTE, json!("not a route"));
        assert_eq!(log.fold().input_route(), InputRoute::default());
    }

    #[test]
    fn surface_input_block_declares_service_route() {
        let input = json!({
            "route": {
                "invoke": { "type": "service", "ref": "service:threads/input" },
                "params": {
                    "target": {
                        "kind": "fresh",
                        "item_ref": "directive:ryeos/ops/base",
                        "project_path": "/tmp/project",
                        "ref_bindings": { "model": "directive:ryeos/ops/base" }
                    }
                }
            }
        });
        let route = InputRoute::from_surface_input(Some(&input)).expect("route parses");
        assert_eq!(
            route.invoke,
            Some(InvokeTemplate::Service {
                item_ref: "service:threads/input".to_string()
            })
        );
        assert_eq!(
            route.params.pointer("/target/item_ref").and_then(Value::as_str),
            Some("directive:ryeos/ops/base")
        );
        assert!(route.thread.is_none());
    }

    #[test]
    fn surface_input_block_command_and_ui_forms() {
        let command =
            json!({ "route": { "invoke": { "type": "command", "tokens": ["thread", "list"] } } });
        assert_eq!(
            InputRoute::from_surface_input(Some(&command))
                .unwrap()
                .invoke,
            Some(InvokeTemplate::Command {
                tokens: vec!["thread".into(), "list".into()]
            })
        );

        let ui = json!({ "route": { "invoke": { "type": "ui_facet", "key": "tile.t1.filter" } } });
        assert_eq!(
            InputRoute::from_surface_input(Some(&ui)).unwrap().invoke,
            Some(InvokeTemplate::UiFacet {
                key: "tile.t1.filter".into()
            })
        );
    }

    #[test]
    fn guard_old_route_enum_form_is_rejected() {
        // The pre-seat RyeOsInputRoute serde forms must NOT parse as a
        // route. No dual forms: the old tagged-enum shapes are dead.
        let old_ryeos_context = json!({ "route": { "type": "ryeos_context" } });
        let parsed = InputRoute::from_surface_input(Some(&old_ryeos_context));
        assert!(
            parsed.is_none() || !parsed.unwrap().has_target(),
            "old ryeos_context form must not yield a targeted route"
        );

        let old_directive = json!({
            "route": { "type": "directive", "directive_ref": "directive:x" }
        });
        let parsed = InputRoute::from_surface_input(Some(&old_directive));
        assert!(
            parsed.is_none() || !parsed.unwrap().has_target(),
            "old directive form must not yield a targeted route"
        );
    }

    #[test]
    fn seat_event_serde_matches_braid_shape() {
        let mut log = SeatLog::default();
        log.append_facet(KEY_INPUT_ROUTE, json!({"thread": "T-9"}));
        let event_json = serde_json::to_value(&log.events()[0]).unwrap();
        assert_eq!(event_json["event_type"], "seat.facet");
        assert_eq!(event_json["payload"]["key"], KEY_INPUT_ROUTE);
        assert_eq!(event_json["seq"], 0);
    }
}
