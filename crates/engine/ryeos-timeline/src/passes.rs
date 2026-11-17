//! Passes over folded entries: clusters (operations and notices), then the
//! turn fold.

use std::collections::{BTreeMap, HashMap};

use crate::{Cluster, Definition, Entry, EntryKind, Item, Lane, LaneRow, Recap};

pub(crate) fn run(entries: Vec<Entry>, definition: &Definition) -> Vec<Item> {
    turn_fold(cluster(entries, definition), definition)
}

/// The status lane: the first table row that matches some entry's type and
/// latest state. Entries are entities, so each holds its latest state. When
/// several entries match a row, the most recent one is named. `None` when no
/// row matches.
pub(crate) fn lane(entries: &[Entry], table: &[LaneRow]) -> Option<Lane> {
    table.iter().find_map(|row| {
        entries
            .iter()
            .filter(|entry| {
                entry.kind == row.kind && entry.state.as_deref() == Some(row.state.as_str())
            })
            .max_by_key(|entry| entry.seq.1)
            .map(|entry| Lane {
                label: row.label.clone(),
                entry: entry.id.clone(),
            })
    })
}

/// A failed or alarming entry never hides inside a group: its state or tone is
/// one the definition lists as escaping.
fn escapes(entry: &Entry, definition: &Definition) -> bool {
    let escape = &definition.folds.escape;
    entry
        .state
        .as_ref()
        .is_some_and(|s| escape.states.contains(s))
        || entry
            .tone
            .as_ref()
            .is_some_and(|t| escape.tones.contains(t))
}

/// What kind of run an entry can join.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Run {
    /// Finished operations that say what they count as.
    Work,
    /// Markers, other than the one that opens an exchange.
    Notice,
}

/// Prose, messages, thoughts, links, asks, outcomes, escaping entries and
/// unfinished operations join no run and so break the one in progress.
fn classify(entry: &Entry, definition: &Definition) -> Option<Run> {
    if escapes(entry, definition) {
        return None;
    }
    match entry.kind {
        EntryKind::Activity
            if entry.counts_as.is_some()
                && entry
                    .state
                    .as_ref()
                    .is_some_and(|s| definition.folds.group_states.contains(s)) =>
        {
            Some(Run::Work)
        }
        EntryKind::Marker
            if definition.folds.notice.is_some()
                && entry.exchange.as_deref() != Some(entry.id.as_str()) =>
        {
            Some(Run::Notice)
        }
        _ => None,
    }
}

/// A run stays within one exchange and one nesting parent.
fn same_run(a: &Entry, b: &Entry) -> bool {
    a.exchange == b.exchange && a.parent == b.parent
}

fn cluster(entries: Vec<Entry>, definition: &Definition) -> Vec<Item> {
    let mut items = Vec::new();
    let mut run: Vec<Entry> = Vec::new();
    let mut run_kind = None;
    for entry in entries {
        let kind = classify(&entry, definition);
        let continues = kind.is_some()
            && kind == run_kind
            && run.last().is_some_and(|last| same_run(last, &entry));
        if !continues {
            flush(&mut run, run_kind, &mut items, definition);
        }
        match kind {
            Some(_) => {
                run_kind = kind;
                run.push(entry);
            }
            None => items.push(Item::Entry(entry)),
        }
    }
    flush(&mut run, run_kind, &mut items, definition);
    items
}

/// A run of two or more becomes a cluster; a lone member stays a row. The
/// cluster's identity comes from its first member.
fn flush(run: &mut Vec<Entry>, kind: Option<Run>, items: &mut Vec<Item>, definition: &Definition) {
    let members = std::mem::take(run);
    if members.len() < 2 {
        items.extend(members.into_iter().map(Item::Entry));
        return;
    }
    items.push(Item::Cluster(Cluster {
        id: format!("cl:{}", members[0].id),
        headline: headline(&members, kind, definition),
        members,
    }));
}

/// Counted nouns joined by ` · `, in vocabulary order, then unknown nouns by
/// key. A notice run counts its members under the definition's notice noun.
fn headline(members: &[Entry], kind: Option<Run>, definition: &Definition) -> String {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for member in members {
        let noun = match kind {
            Some(Run::Notice) => definition.folds.notice.as_deref(),
            _ => member.counts_as.as_deref(),
        };
        if let Some(noun) = noun {
            *counts.entry(noun).or_insert(0) += 1;
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
        Item::Recap(recap) => Some(recap.exchange.as_str()),
    }
}

fn entries_of(item: &Item) -> Vec<&Entry> {
    match item {
        Item::Entry(entry) => vec![entry],
        Item::Cluster(cluster) => cluster.members.iter().collect(),
        Item::Recap(recap) => recap.members.iter().flat_map(entries_of).collect(),
    }
}

/// Where an exchange's answer and outcome sit among the items.
#[derive(Default)]
struct Exchange {
    last_prose: Option<usize>,
    last_outcome: Option<usize>,
    /// The exchange is open until it has an answer and something ends it.
    finished: bool,
    /// Highest `chain_seq` among the entries that decided the fold: the
    /// answer and what ended the exchange. A recap formed when the exchange
    /// finishes carries it, so it reads as changed from that moment.
    settled: i64,
}

/// The turn fold: a finished exchange's work folds under a recap.
///
/// An exchange is finished when it has an answer (a prose entry) and
/// something ended it after that: a later exchange began, or its own outcome
/// followed the answer. An exchange still receiving events, or one that never
/// produced an answer (it failed or was cut off), stays open.
///
/// Work is an activity, link or thought, or a cluster of them, that does not
/// escape. Prose, messages, notices, asks, outcomes and escaping entries stay
/// visible. To keep the order of entries fixed, each unbroken stretch of work
/// that precedes the exchange's last answer becomes one recap; the visible
/// entries between stretches stay where they were. A stretch whose time range
/// cannot be measured stays open.
fn turn_fold(items: Vec<Item>, definition: &Definition) -> Vec<Item> {
    let mut exchanges: HashMap<String, Exchange> = HashMap::new();
    for (at, item) in items.iter().enumerate() {
        let (Item::Entry(entry), Some(id)) = (item, exchange(item)) else {
            continue;
        };
        let info = exchanges.entry(id.to_string()).or_default();
        match entry.kind {
            EntryKind::Prose => info.last_prose = Some(at),
            EntryKind::Outcome => info.last_outcome = Some(at),
            _ => {}
        }
    }
    let last_exchange = items.iter().rev().find_map(exchange).map(str::to_string);
    for (id, info) in &mut exchanges {
        info.finished = info.last_prose.is_some_and(|answer| {
            last_exchange.as_deref() != Some(id.as_str())
                || info.last_outcome.is_some_and(|outcome| outcome > answer)
        });
        let Some(answer) = info.last_prose.filter(|_| info.finished) else {
            continue;
        };
        let next_exchange = items[answer + 1..]
            .iter()
            .find(|item| exchange(item).is_some_and(|other| other != id.as_str()));
        let outcome = info
            .last_outcome
            .filter(|&outcome| outcome > answer)
            .map(|outcome| &items[outcome]);
        info.settled = [Some(&items[answer]), next_exchange, outcome]
            .into_iter()
            .flatten()
            .map(Item::last_seq)
            .max()
            .unwrap_or(0);
    }
    let folds_at = |at: usize, item: &Item| -> bool {
        let Some(info) = exchange(item).and_then(|id| exchanges.get(id)) else {
            return false;
        };
        info.finished
            && info.last_prose.is_some_and(|answer| at < answer)
            && match item {
                Item::Entry(entry) => {
                    matches!(
                        entry.kind,
                        EntryKind::Activity | EntryKind::Link | EntryKind::Thought
                    ) && !escapes(entry, definition)
                }
                // Operation clusters are work; notice clusters stay visible.
                Item::Cluster(cluster) => cluster
                    .members
                    .iter()
                    .all(|m| m.kind == EntryKind::Activity),
                Item::Recap(_) => false,
            }
    };

    let mut out = Vec::new();
    let mut stretch: Vec<Item> = Vec::new();
    for (at, item) in items.into_iter().enumerate() {
        let joins = folds_at(at, &item)
            && stretch
                .last()
                .is_none_or(|last| exchange(last) == exchange(&item));
        if !joins {
            close(&mut stretch, &mut out, &exchanges);
        }
        if folds_at(at, &item) {
            stretch.push(item);
        } else {
            out.push(item);
        }
    }
    close(&mut stretch, &mut out, &exchanges);
    out
}

/// Fold a finished stretch into a recap, or leave its items open when its
/// duration cannot be measured.
fn close(stretch: &mut Vec<Item>, out: &mut Vec<Item>, exchanges: &HashMap<String, Exchange>) {
    let members = std::mem::take(stretch);
    let all: Vec<&Entry> = members.iter().flat_map(entries_of).collect();
    let start = all.iter().filter_map(|e| epoch(&e.ts.0)).min();
    let end = all.iter().filter_map(|e| epoch(&e.ts.1)).max();
    let (Some(first), Some(start), Some(end)) = (all.first(), start, end) else {
        out.extend(members);
        return;
    };
    let work = all.iter().filter(|e| e.kind == EntryKind::Activity).count();
    let exchange = first.exchange.clone().unwrap_or_default();
    let settled = exchanges.get(&exchange).map_or(0, |info| info.settled);
    out.push(Item::Recap(Recap {
        id: format!("rc:{}", first.id),
        exchange,
        work,
        duration_s: end.checked_sub(start),
        seq: all.iter().map(|e| e.seq.1).max().unwrap_or(0).max(settled),
        members,
    }));
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
