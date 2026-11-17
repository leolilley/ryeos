//! Dotted-path access into an event record, and the closed formatters.

use serde_json::Value;

use std::collections::{HashMap, HashSet};

use crate::{Find, Format, Part, Slot, When};

/// The value at `path`, or `None` when any segment is missing or null. A
/// segment of digits indexes an array.
pub(crate) fn get<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for segment in path.split('.') {
        current = match (current, segment.parse::<usize>()) {
            (Value::Array(items), Ok(index)) => items.get(index)?,
            _ => current.get(segment)?,
        };
    }
    (!current.is_null()).then_some(current)
}

/// A scalar rendered as text; objects and arrays render as compact JSON.
pub(crate) fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// What a condition or slot may consult besides the event itself.
pub(crate) struct Context<'a> {
    /// Threads on the spine.
    pub spine: &'a HashSet<String>,
    /// Events folded before this one, oldest first.
    pub history: &'a [Value],
    /// Subjects supplied for threads, by thread id.
    pub subjects: &'a HashMap<String, Value>,
}

pub(crate) fn when_holds(when: &Option<When>, event: &Value, cx: &Context) -> bool {
    let Some(when) = when else { return true };
    let on_spine = || {
        event
            .get("thread_id")
            .and_then(Value::as_str)
            .is_some_and(|t| cx.spine.contains(t))
    };
    when.has.as_ref().is_none_or(|p| get(event, p).is_some())
        && when.lacks.as_ref().is_none_or(|p| get(event, p).is_none())
        && when
            .eq
            .as_ref()
            .is_none_or(|(p, literal)| get(event, p) == Some(literal))
        && when
            .ne
            .as_ref()
            .is_none_or(|(p, literal)| get(event, p) != Some(literal))
        && when
            .one_of
            .as_ref()
            .is_none_or(|(p, literals)| get(event, p).is_some_and(|value| literals.contains(value)))
        && when.on_spine.is_none_or(|want| on_spine() == want)
}

fn format_value(value: &Value, format: Option<Format>) -> String {
    match (format, value.as_u64()) {
        (Some(Format::Bytes), Some(n)) => bytes(n),
        (Some(Format::Count), Some(n)) => count(n),
        (Some(Format::ShortHash), _) => text(value).chars().take(8).collect(),
        _ => text(value),
    }
}

fn bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
    }
}

fn count(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

/// The slot's text, or `None` when nothing in it is present.
pub(crate) fn slot(slot: &Slot, event: &Value, cx: &Context) -> Option<String> {
    match slot {
        Slot::Lookup { lookup } => lookup_value(lookup, event, cx),
        Slot::Path(path) => get(event, path).map(text),
        Slot::First { first } => first.iter().find_map(|path| get(event, path)).map(text),
        Slot::Join { join } => {
            let parts: Vec<String> = join
                .iter()
                .filter_map(|part| match part {
                    Part::Path(path) => get(event, path).map(text),
                    Part::Spec {
                        path,
                        format,
                        prefix,
                    } => get(event, path).map(|v| {
                        format!(
                            "{}{}",
                            prefix.as_deref().unwrap_or(""),
                            format_value(v, *format)
                        )
                    }),
                })
                .collect();
            (!parts.is_empty()).then(|| parts.join(" · "))
        }
    }
}

fn lookup_value(lookup: &crate::Lookup, event: &Value, cx: &Context) -> Option<String> {
    let thread = event.get("thread_id")?;
    // `array` and `find` select an element together; neither alone is a lookup.
    let element_match = match (&lookup.array, &lookup.find) {
        (Some(array), Some(find)) => {
            let (field, wanted) = match find {
                Find::Source((field, source)) => (field, get(event, source)?),
                Find::Literal { field, equals } => (field, equals),
            };
            Some((array, field, wanted))
        }
        (None, None) => None,
        _ => return None,
    };
    let read = |previous: &Value| match element_match {
        None => get(previous, &lookup.get).map(text),
        Some((array, field, wanted)) => get(previous, array)?
            .as_array()?
            .iter()
            .find(|element| get(element, field) == Some(wanted))
            .and_then(|element| get(element, &lookup.get))
            .map(text),
    };
    let mut candidates = cx.history.iter().rev().filter(|previous| {
        previous.get("event_type").and_then(Value::as_str) == Some(lookup.event.as_str())
            && previous.get("thread_id") == Some(thread)
            && when_holds(&lookup.when, previous, cx)
    });
    if lookup.when.is_some() {
        candidates.next().and_then(read)
    } else {
        candidates.find_map(read)
    }
}
