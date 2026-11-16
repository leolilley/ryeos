//! The event pass: rules turn events into entries.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::Value;

use crate::path::Context;
use crate::{Definition, Emission, Entry, StateSpec, Timeline, ToneRule, passes, path};

/// Folds `events` (ascending `chain_seq`) into a timeline.
pub fn fold(events: &[Value], definition: &Definition) -> Result<Timeline, String> {
    let spine = spine(events, definition);
    let mut state = State::default();
    let mut previous = i64::MIN;
    for (index, event) in events.iter().enumerate() {
        let seq = event
            .get("chain_seq")
            .and_then(Value::as_i64)
            .ok_or("event without chain_seq")?;
        if seq <= previous {
            return Err(format!("events out of order at chain_seq {seq}"));
        }
        previous = seq;
        let event_type = event
            .get("event_type")
            .and_then(Value::as_str)
            .ok_or("event without event_type")?;
        let cx = Context {
            spine: &spine,
            history: &events[..index],
        };
        let rule = definition
            .events
            .get(event_type)
            .and_then(|rules| rules.iter().find(|r| path::when_holds(&r.when, event, &cx)));
        let Some(rule) = rule else {
            *state.undeclared.entry(event_type.to_string()).or_insert(0) += 1;
            continue;
        };
        if rule.drop {
            continue;
        }
        let Some(over) = &rule.over else {
            for emission in &rule.emit {
                if path::when_holds(&emission.when, event, &cx) {
                    state.apply(emission, event, seq, None, &cx);
                }
            }
            continue;
        };
        let elements = path::get(event, over)
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for (index, element) in elements.iter().enumerate() {
            let mut view = event.clone();
            if let Value::Object(fields) = &mut view {
                fields.insert("item".into(), element.clone());
                fields.insert("item_index".into(), Value::from(index));
            }
            for emission in &rule.emit {
                if path::when_holds(&emission.when, &view, &cx) {
                    state.apply(emission, &view, seq, Some(index), &cx);
                }
            }
        }
    }
    Ok(Timeline {
        items: passes::run(state.entries, definition),
        undeclared: state.undeclared,
    })
}

/// The root thread plus every thread linked to it by the definition's spine
/// edges, in either direction.
fn spine(events: &[Value], definition: &Definition) -> HashSet<String> {
    let root = events.iter().find_map(|e| {
        e.get("chain_root_id")
            .or_else(|| e.get("thread_id"))
            .and_then(Value::as_str)
    });
    let Some(root) = root else {
        return HashSet::new();
    };
    let mut links: HashMap<String, Vec<String>> = HashMap::new();
    for event in events {
        let Some(event_type) = event.get("event_type").and_then(Value::as_str) else {
            continue;
        };
        for edge in definition.spine.iter().filter(|e| e.event == event_type) {
            if let (Some(from), Some(to)) =
                (path::get(event, &edge.from), path::get(event, &edge.to))
            {
                let (from, to) = (path::text(from), path::text(to));
                links.entry(from.clone()).or_default().push(to.clone());
                links.entry(to).or_default().push(from);
            }
        }
    }
    let mut seen = HashSet::from([root.to_string()]);
    let mut queue = vec![root.to_string()];
    while let Some(thread) = queue.pop() {
        for next in links.get(&thread).into_iter().flatten() {
            if seen.insert(next.clone()) {
                queue.push(next.clone());
            }
        }
    }
    seen
}

#[derive(Default)]
struct State {
    entries: Vec<Entry>,
    by_id: HashMap<String, usize>,
    last_flow: Option<usize>,
    exchange: Option<String>,
    /// Adopted thread id to the entry its entries nest under.
    adopted: HashMap<String, String>,
    undeclared: BTreeMap<String, u32>,
}

impl State {
    fn apply(
        &mut self,
        emission: &Emission,
        event: &Value,
        seq: i64,
        element: Option<usize>,
        cx: &Context,
    ) {
        let thread = event
            .get("thread_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let ts = event
            .get("ts")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let slots = project(emission, event, cx);

        if let Some(adopt) = &emission.adopt
            && let Some(child) = path::get(event, &adopt.thread)
            && let Some(parent) = keyed_id(adopt.under.kind.prefix(), &adopt.under.key, event)
        {
            self.adopted.insert(path::text(child), parent);
        }

        if emission.flow
            && let Some(index) = self.last_flow
            && self.entries[index].kind == emission.kind
            && self.entries[index].thread == thread
        {
            let entry = &mut self.entries[index];
            for (name, text) in slots {
                entry.slots.entry(name).or_default().push_str(&text);
            }
            entry.seq.1 = seq;
            entry.ts.1 = ts;
            return;
        }
        if !emission.flow {
            self.last_flow = None;
        }

        let id = entry_id(emission.kind.prefix(), &emission.key, event, seq, element);
        let parent = emission
            .parent
            .as_ref()
            .and_then(|p| keyed_id(p.kind.prefix(), &p.key, event))
            .or_else(|| self.adopted.get(&thread).cloned());
        let tone = tone(&emission.tone, event);
        let state = emission.state.as_ref().map(|spec| state_text(spec, event));

        if let Some(&index) = self.by_id.get(&id) {
            let entry = &mut self.entries[index];
            entry.slots.extend(slots);
            if state.is_some() {
                entry.state = state;
            }
            if tone.is_some() {
                entry.tone = tone;
            }
            if emission.counts_as.is_some() {
                entry.counts_as = emission.counts_as.clone();
            }
            entry.seq.1 = seq;
            entry.ts.1 = ts;
            return;
        }

        if emission.starts_exchange {
            self.exchange = Some(id.clone());
        }
        let index = self.entries.len();
        self.entries.push(Entry {
            id: id.clone(),
            kind: emission.kind,
            state,
            tone,
            thread,
            parent,
            exchange: self.exchange.clone(),
            counts_as: emission.counts_as.clone(),
            slots,
            seq: (seq, seq),
            ts: (ts.clone(), ts),
        });
        self.by_id.insert(id, index);
        if emission.flow {
            self.last_flow = Some(index);
        }
    }
}

fn entry_id(
    prefix: &str,
    key: &[String],
    event: &Value,
    seq: i64,
    element: Option<usize>,
) -> String {
    keyed_id(prefix, key, event).unwrap_or_else(|| match element {
        Some(index) => format!("{prefix}:{seq}/{index}"),
        None => format!("{prefix}:{seq}"),
    })
}

/// `prefix:value/value` from the key paths, or `None` when the key is empty or
/// any part is missing.
fn keyed_id(prefix: &str, key: &[String], event: &Value) -> Option<String> {
    if key.is_empty() {
        return None;
    }
    let parts: Option<Vec<String>> = key
        .iter()
        .map(|p| path::get(event, p).map(path::text))
        .collect();
    parts.map(|parts| format!("{prefix}:{}", parts.join("/")))
}

fn project(emission: &Emission, event: &Value, cx: &Context) -> BTreeMap<String, String> {
    emission
        .project
        .iter()
        .filter_map(|(name, slot)| path::slot(slot, event, cx).map(|text| (name.clone(), text)))
        .collect()
}

fn state_text(spec: &StateSpec, event: &Value) -> String {
    match spec {
        StateSpec::Literal(s) => s.clone(),
        StateSpec::FromPath { from, map, default } => path::get(event, from)
            .and_then(|v| map.get(&path::text(v)))
            .unwrap_or(default)
            .clone(),
    }
}

fn tone(rules: &[ToneRule], event: &Value) -> Option<String> {
    for rule in rules {
        if let Some(field) = &rule.field
            && let Some(value) = path::get(event, field)
            && let Some(tone) = rule.map.get(&path::text(value))
        {
            return Some(tone.clone());
        }
        if let Some(default) = &rule.default {
            return Some(default.clone());
        }
    }
    None
}
