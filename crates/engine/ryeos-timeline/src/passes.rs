//! Passes over folded entries: clusters, then recaps.

use std::collections::BTreeMap;

use crate::{Cluster, Definition, Entry, EntryKind, Item, Recap};

pub(crate) fn run(entries: Vec<Entry>, definition: &Definition) -> Vec<Item> {
    recap(cluster(entries, definition))
}

/// A finished entry that says what it counts as: an activity that is done or
/// stopped, or a link (whose outcome may be unknown from this chain).
fn candidate(entry: &Entry) -> bool {
    entry.counts_as.is_some()
        && match entry.kind {
            EntryKind::Activity => matches!(entry.state.as_deref(), Some("done" | "stopped")),
            EntryKind::Link => {
                matches!(entry.state.as_deref(), Some("done" | "stopped" | "unknown"))
            }
            _ => false,
        }
}

fn same_run(a: &Entry, b: &Entry) -> bool {
    a.exchange == b.exchange && a.parent == b.parent
}

fn cluster(entries: Vec<Entry>, definition: &Definition) -> Vec<Item> {
    let mut items = Vec::new();
    let mut run: Vec<Entry> = Vec::new();
    for entry in entries {
        if candidate(&entry) && run.last().is_none_or(|last| same_run(last, &entry)) {
            run.push(entry);
            continue;
        }
        flush(&mut run, &mut items, definition);
        if candidate(&entry) {
            run.push(entry);
        } else {
            items.push(Item::Entry(entry));
        }
    }
    flush(&mut run, &mut items, definition);
    items
}

fn flush(run: &mut Vec<Entry>, items: &mut Vec<Item>, definition: &Definition) {
    let members = std::mem::take(run);
    if members.len() < 2 {
        items.extend(members.into_iter().map(Item::Entry));
        return;
    }
    items.push(Item::Cluster(Cluster {
        id: format!("cl:{}", members[0].id),
        headline: headline(&members, definition),
        members,
    }));
}

/// Counted nouns joined by ` · `, in vocabulary order, then unknown nouns by key.
fn headline(members: &[Entry], definition: &Definition) -> String {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for member in members {
        if let Some(noun) = &member.counts_as {
            *counts.entry(noun.as_str()).or_insert(0) += 1;
        }
    }
    let mut parts = Vec::new();
    for noun in &definition.vocabulary {
        if let Some(n) = counts.remove(noun.key.as_str()) {
            parts.push(format!(
                "{n} {}",
                if n == 1 { &noun.one } else { &noun.many }
            ));
        }
    }
    parts.extend(counts.into_iter().map(|(key, n)| format!("{n} {key}")));
    parts.join(" · ")
}

fn exchange(item: &Item) -> Option<&str> {
    match item {
        Item::Entry(entry) => entry.exchange.as_deref(),
        Item::Cluster(cluster) => cluster.members.first().and_then(|m| m.exchange.as_deref()),
        Item::Recap(_) => None,
    }
}

fn entries_of(item: &Item) -> &[Entry] {
    match item {
        Item::Entry(entry) => std::slice::from_ref(entry),
        Item::Cluster(cluster) => &cluster.members,
        Item::Recap(_) => &[],
    }
}

/// One recap per exchange in which work precedes a closing entry (an answer or
/// an outcome) and that took measurable time, placed before the closing entry.
fn recap(mut items: Vec<Item>) -> Vec<Item> {
    let mut exchanges: Vec<String> = Vec::new();
    for item in &items {
        if let Some(id) = exchange(item)
            && !exchanges.iter().any(|e| e == id)
        {
            exchanges.push(id.to_string());
        }
    }
    let mut inserts: Vec<(usize, Recap)> = Vec::new();
    for id in exchanges {
        let members: Vec<usize> = (0..items.len())
            .filter(|&i| exchange(&items[i]) == Some(id.as_str()))
            .collect();
        let first_work = members.iter().position(|&i| {
            entries_of(&items[i])
                .iter()
                .any(|e| e.kind == EntryKind::Activity)
        });
        let last_prose = members.iter().rposition(|&i| {
            matches!(&items[i], Item::Entry(e) if matches!(e.kind, EntryKind::Prose | EntryKind::Outcome))
        });
        let (Some(work_at), Some(prose_at)) = (first_work, last_prose) else {
            continue;
        };
        if prose_at < work_at {
            continue;
        }
        let all: Vec<&Entry> = members
            .iter()
            .flat_map(|&i| entries_of(&items[i]))
            .collect();
        let work = all.iter().filter(|e| e.kind == EntryKind::Activity).count();
        let start = all.first().and_then(|e| epoch(&e.ts.0));
        let end = all.last().and_then(|e| epoch(&e.ts.1));
        let duration_s = match (start, end) {
            (Some(s), Some(e)) if e > s => Some(e - s),
            _ => None,
        };
        if duration_s.is_none() {
            continue;
        }
        inserts.push((
            members[prose_at],
            Recap {
                id: format!("rc:{id}"),
                exchange: id,
                work,
                duration_s,
            },
        ));
    }
    inserts.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    for (at, recap) in inserts {
        items.insert(at, Item::Recap(recap));
    }
    items
}

/// Seconds since the epoch for `YYYY-MM-DDTHH:MM:SSZ`.
fn epoch(ts: &str) -> Option<i64> {
    let b = ts.as_bytes();
    if b.len() != 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[19] != b'Z' {
        return None;
    }
    let num = |a: usize, z: usize| ts.get(a..z)?.parse::<i64>().ok();
    let (y, m, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, s) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + s)
}

#[cfg(test)]
mod tests {
    use super::epoch;

    #[test]
    fn epoch_matches_known_instants() {
        assert_eq!(epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(epoch("2026-11-16T03:26:55Z"), Some(1_794_799_615));
        assert_eq!(epoch("not a time"), None);
    }
}
