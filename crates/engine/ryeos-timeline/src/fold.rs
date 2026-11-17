//! The event pass: rules turn events into entries.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::Value;

use crate::path::Context;
use crate::{Definition, Emission, Entry, StateSpec, Timeline, ToneRule, passes, path};

/// Folds `events` (ascending `chain_seq`) into a timeline.
pub fn fold(events: &[Value], definition: &Definition) -> Result<Timeline, String> {
    let mut folder = Folder::new(definition.clone());
    folder.push(events)?;
    Ok(folder.timeline())
}

/// A resumable event pass. Pushing events in any split yields the same
/// timeline as folding them all at once.
///
/// The spine is a property of the whole log: an edge arriving late can put an
/// earlier event's thread on the spine and change what its rules did. When a
/// push grows the spine the pass restarts from the first event; otherwise only
/// the new events are applied.
pub struct Folder {
    definition: Definition,
    events: Vec<Value>,
    /// Every spine edge seen so far, in both directions.
    links: HashMap<String, Vec<String>>,
    spine: HashSet<String>,
    state: State,
    rewritten_at: Option<i64>,
}

impl Folder {
    pub fn new(definition: Definition) -> Self {
        Self {
            definition,
            events: Vec::new(),
            links: HashMap::new(),
            spine: HashSet::new(),
            state: State::default(),
            rewritten_at: None,
        }
    }

    /// The `chain_seq` of the push that last re-derived entries older than
    /// itself (a spine growth), if any. A consumer holding entries from before
    /// that push cannot trust them to be current.
    pub fn rewritten_at(&self) -> Option<i64> {
        self.rewritten_at
    }

    /// Appends events in ascending `chain_seq`, after those already held. On
    /// error nothing is appended.
    pub fn push(&mut self, new_events: &[Value]) -> Result<(), String> {
        let mut previous = last_seq(&self.events).unwrap_or(i64::MIN);
        for event in new_events {
            let seq = event
                .get("chain_seq")
                .and_then(Value::as_i64)
                .ok_or("event without chain_seq")?;
            if seq <= previous {
                return Err(format!("events out of order at chain_seq {seq}"));
            }
            previous = seq;
            if event.get("event_type").and_then(Value::as_str).is_none() {
                return Err("event without event_type".to_string());
            }
        }
        let start = self.events.len();
        self.events.extend(new_events.iter().cloned());
        for event in new_events {
            self.add_links(event);
        }
        let spine = self.reachable();
        if spine != self.spine {
            self.spine = spine;
            self.state = State::default();
            self.apply_from(0);
            if start > 0 {
                self.rewritten_at = last_seq(&self.events);
            }
        } else {
            self.apply_from(start);
        }
        Ok(())
    }

    /// The timeline of the events pushed so far.
    pub fn timeline(&self) -> Timeline {
        Timeline {
            items: passes::run(self.state.entries.clone(), &self.definition),
            undeclared: self.state.undeclared.clone(),
        }
    }

    fn add_links(&mut self, event: &Value) {
        let Some(event_type) = event.get("event_type").and_then(Value::as_str) else {
            return;
        };
        for edge in self
            .definition
            .spine
            .iter()
            .filter(|e| e.event == event_type)
        {
            if let (Some(from), Some(to)) =
                (path::get(event, &edge.from), path::get(event, &edge.to))
            {
                let (from, to) = (path::text(from), path::text(to));
                self.links.entry(from.clone()).or_default().push(to.clone());
                self.links.entry(to).or_default().push(from);
            }
        }
    }

    /// The root thread plus every thread linked to it by the definition's
    /// spine edges, in either direction.
    fn reachable(&self) -> HashSet<String> {
        let root = self.events.iter().find_map(|e| {
            e.get("chain_root_id")
                .or_else(|| e.get("thread_id"))
                .and_then(Value::as_str)
        });
        let Some(root) = root else {
            return HashSet::new();
        };
        let mut seen = HashSet::from([root.to_string()]);
        let mut queue = vec![root.to_string()];
        while let Some(thread) = queue.pop() {
            for next in self.links.get(&thread).into_iter().flatten() {
                if seen.insert(next.clone()) {
                    queue.push(next.clone());
                }
            }
        }
        seen
    }

    fn apply_from(&mut self, start: usize) {
        for index in start..self.events.len() {
            let event = &self.events[index];
            let cx = Context {
                spine: &self.spine,
                history: &self.events[..index],
            };
            apply_event(&mut self.state, &self.definition, event, &cx);
        }
    }
}

fn last_seq(events: &[Value]) -> Option<i64> {
    events.last()?.get("chain_seq")?.as_i64()
}

fn apply_event(state: &mut State, definition: &Definition, event: &Value, cx: &Context) {
    // Validated by `push`.
    let seq = event["chain_seq"].as_i64().unwrap_or_default();
    let event_type = event["event_type"].as_str().unwrap_or_default();
    let rule = definition
        .events
        .get(event_type)
        .and_then(|rules| rules.iter().find(|r| path::when_holds(&r.when, event, cx)));
    let Some(rule) = rule else {
        *state.undeclared.entry(event_type.to_string()).or_insert(0) += 1;
        return;
    };
    if rule.drop {
        return;
    }
    let Some(over) = &rule.over else {
        for emission in &rule.emit {
            if path::when_holds(&emission.when, event, cx) {
                state.apply(emission, event, seq, None, cx);
            }
        }
        return;
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
            if path::when_holds(&emission.when, &view, cx) {
                state.apply(emission, &view, seq, Some(index), cx);
            }
        }
    }
}

#[derive(Default, Clone)]
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
