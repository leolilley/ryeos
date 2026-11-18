//! Presentation: a subject's display hints laid over an entry.
//!
//! A subject is a bounded JSON object the host supplies for a thread. Its
//! `presentation` block says which of the entry's inputs fill which slots and
//! where a file change is; the definition's [`Present`] says which slots may
//! be filled, which blocks may be added and how far each may grow. Text from a
//! subject is untrusted, so it is stripped and capped here, once, whatever
//! draws it.

use ryeos_changeset::{Basis, Caps, ChangeSet};
use serde_json::{Map, Value};

use crate::{Block, Entry, MAX_ENTRY_BLOCKS, Noun, Present, path};

/// Lays `subject`'s presentation over `entry`, reading `inputs` as roots.
pub(crate) fn present(
    entry: &mut Entry,
    present: &Present,
    subject: &Value,
    inputs: &Map<String, Value>,
    vocabulary: &[Noun],
) {
    let Some(block) = subject.get("presentation").and_then(Value::as_object) else {
        return;
    };
    let roots = Value::Object(inputs.clone());
    for (slot, cap) in &present.slots {
        let Some(paths) = block.get(slot).and_then(Value::as_array) else {
            continue;
        };
        let text = paths
            .iter()
            .filter_map(Value::as_str)
            .find_map(|candidate| {
                let text = scalar(path::get(&roots, candidate)?)?;
                let text = clean(&text, *cap);
                (!text.is_empty()).then_some(text)
            });
        let Some(text) = text else { continue };
        if let Some(into) = present.displace.get(slot)
            && !entry.slots.contains_key(into)
            && let Some(displaced) = entry.slots.get(slot).cloned()
        {
            entry.slots.insert(into.clone(), displaced);
        }
        entry.slots.insert(slot.clone(), text);
    }
    if let Some(key) = block.get("counts_as").and_then(Value::as_str)
        && vocabulary.iter().any(|noun| noun.key == key)
    {
        entry.counts_as = Some(key.to_string());
    }
    if let Some(caps) = &present.blocks.changes
        && entry.blocks.len() < MAX_ENTRY_BLOCKS
        && let Some(set) = block
            .get("changes")
            .and_then(|declared| changes(declared, &roots, caps))
    {
        entry.blocks.push(Block::Changes(set));
    }
}

/// The change set a subject's `changes` declaration yields, or `None` when the
/// declaration is not exactly one well-formed source, a path leaves the
/// `arguments` and `result` roots, a value is missing or not text, or the
/// text is not a change the change-set builder accepts.
fn changes(declared: &Value, roots: &Value, caps: &Caps) -> Option<ChangeSet> {
    let declared = declared.as_object()?;
    if declared.len() != 1 {
        return None;
    }
    if let Some(source) = declared.get("patch") {
        let (basis, text) = sourced(source, roots)?;
        return ChangeSet::from_patch(text, basis, caps);
    }
    let edit = declared.get("edit")?.as_object()?;
    if edit.len() != 3 {
        return None;
    }
    let (path_basis, path) = sourced(edit.get("path")?, roots)?;
    let (before_basis, before) = sourced(edit.get("before")?, roots)?;
    let (after_basis, after) = sourced(edit.get("after")?, roots)?;
    // Only a change read wholly from the result is the subject's own report.
    let reported = [path_basis, before_basis, after_basis]
        .iter()
        .all(|basis| *basis == Basis::Reported);
    let basis = if reported {
        Basis::Reported
    } else {
        Basis::Requested
    };
    ChangeSet::from_edit(path, before, after, basis, caps)
}

/// The text at a declared source path and what reading that root is evidence
/// of: the call's arguments are a request, its result is a report.
fn sourced<'a>(source: &Value, roots: &'a Value) -> Option<(Basis, &'a str)> {
    let source = source.as_str()?;
    let basis = match source.split('.').next()? {
        "arguments" => Basis::Requested,
        "result" => Basis::Reported,
        _ => return None,
    };
    Some((basis, path::get(roots, source)?.as_str()?))
}

fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(_) | Value::Bool(_) => Some(value.to_string()),
        _ => None,
    }
}

/// Removes control characters (newline and tab stay) and bidirectional
/// controls, trims, and cuts at `cap` characters with an elision mark.
fn clean(text: &str, cap: usize) -> String {
    let kept: String = text
        .chars()
        .filter(|c| (!c.is_control() || matches!(c, '\n' | '\t')) && !is_bidi_control(*c))
        .collect();
    let kept = kept.trim();
    if kept.chars().count() <= cap {
        return kept.to_string();
    }
    let mut cut: String = kept.chars().take(cap).collect();
    cut.push('…');
    cut
}

fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

#[cfg(test)]
mod tests {
    use super::clean;

    #[test]
    fn control_and_bidi_characters_are_stripped() {
        assert_eq!(clean("a\u{1b}[31mb\u{202e}c\u{0}", 100), "a[31mbc");
        assert_eq!(
            clean("line one\nline two\ttab", 100),
            "line one\nline two\ttab"
        );
    }

    #[test]
    fn long_text_is_cut_with_an_elision_mark() {
        assert_eq!(clean("abcdef", 3), "abc…");
        assert_eq!(clean("abc", 3), "abc");
        assert_eq!(clean("  \u{202e} ", 3), "");
    }
}
