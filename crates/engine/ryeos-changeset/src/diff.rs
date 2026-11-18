//! Line diff of two texts, with a cap on the work done.

use crate::RawHunk;

/// Unchanged lines kept either side of a change.
const CONTEXT: usize = 3;

/// Marker line that follows a line with no terminating newline.
pub(crate) const NO_NEWLINE: &str = "\\ No newline at end of file";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Keep,
    Remove,
    Add,
}

/// The hunks that turn `before` into `after`, with [`CONTEXT`] lines of
/// context. Lines are compared with their terminators, so a change in the
/// final newline is a change.
///
/// `max_distance` bounds the search: when the texts differ by more edits than
/// that, the differing middle is reported as one whole replacement (every old
/// line removed, then every new line added), which is still a correct diff.
/// With `numbered` the hunks carry their line numbers in each text.
pub(crate) fn diff_lines(
    before: &str,
    after: &str,
    numbered: bool,
    max_distance: usize,
) -> Vec<RawHunk> {
    let old: Vec<&str> = before.split_inclusive('\n').collect();
    let new: Vec<&str> = after.split_inclusive('\n').collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let old_mid = &old[prefix..old.len() - suffix];
    let new_mid = &new[prefix..new.len() - suffix];

    let mut edits: Vec<(Kind, &str)> = Vec::with_capacity(old.len() + new.len());
    edits.extend(old[..prefix].iter().map(|line| (Kind::Keep, *line)));
    match shortest_edit(old_mid, new_mid, max_distance) {
        Some(middle) => edits.extend(middle),
        None => {
            edits.extend(old_mid.iter().map(|line| (Kind::Remove, *line)));
            edits.extend(new_mid.iter().map(|line| (Kind::Add, *line)));
        }
    }
    edits.extend(
        old[old.len() - suffix..]
            .iter()
            .map(|line| (Kind::Keep, *line)),
    );
    hunks(&edits, numbered)
}

/// Myers' shortest edit script, or `None` when it needs more than
/// `max_distance` edits. Memory grows with the square of the distance, which
/// is why the distance is capped.
fn shortest_edit<'a>(
    old: &[&'a str],
    new: &[&'a str],
    max_distance: usize,
) -> Option<Vec<(Kind, &'a str)>> {
    let (n, m) = (old.len() as isize, new.len() as isize);
    if n + m == 0 {
        return Some(Vec::new());
    }
    let max = ((n + m) as usize).min(max_distance) as isize;
    let offset = max + 1;
    // `furthest[offset + k]` is the furthest x reached on diagonal k.
    let mut furthest = vec![0isize; (2 * max + 3) as usize];
    // Round d's snapshot holds diagonals -(d + 1)..=(d + 1), taken before the
    // round ran.
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut found = false;
    'search: for d in 0..=max {
        trace.push(furthest[(offset - d - 1) as usize..=(offset + d + 1) as usize].to_vec());
        let mut k = -d;
        while k <= d {
            let at = (offset + k) as usize;
            let mut x = if k == -d || (k != d && furthest[at - 1] < furthest[at + 1]) {
                furthest[at + 1]
            } else {
                furthest[at - 1] + 1
            };
            let mut y = x - k;
            while x < n && y < m && old[x as usize] == new[y as usize] {
                x += 1;
                y += 1;
            }
            furthest[at] = x;
            if x >= n && y >= m {
                found = true;
                break 'search;
            }
            k += 2;
        }
    }
    if !found {
        return None;
    }

    let mut reversed: Vec<(Kind, &str)> = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (0..trace.len() as isize).rev() {
        let snapshot = &trace[d as usize];
        // Diagonal k sits at `k + d + 1` in round d's snapshot.
        let value = |k: isize| snapshot[(k + d + 1) as usize];
        let k = x - y;
        let previous_k = if k == -d || (k != d && value(k - 1) < value(k + 1)) {
            k + 1
        } else {
            k - 1
        };
        let previous_x = value(previous_k);
        let previous_y = previous_x - previous_k;
        while x > previous_x && y > previous_y {
            x -= 1;
            y -= 1;
            reversed.push((Kind::Keep, old[x as usize]));
        }
        if d > 0 {
            if x == previous_x {
                y -= 1;
                reversed.push((Kind::Add, new[y as usize]));
            } else {
                x -= 1;
                reversed.push((Kind::Remove, old[x as usize]));
            }
        }
    }
    reversed.reverse();
    Some(reversed)
}

/// Groups an edit script into hunks: each run of changes with its context,
/// runs closer than twice the context sharing a hunk.
fn hunks(edits: &[(Kind, &str)], numbered: bool) -> Vec<RawHunk> {
    let mut out = Vec::new();
    let (mut old_before, mut new_before) = (0usize, 0usize);
    let mut next = 0usize;
    while let Some(first) = (next..edits.len()).find(|&index| edits[index].0 != Kind::Keep) {
        let start = first.saturating_sub(CONTEXT).max(next);
        let mut last = first;
        let mut index = first + 1;
        while index < edits.len() {
            if edits[index].0 != Kind::Keep {
                last = index;
            } else if index - last > 2 * CONTEXT {
                break;
            }
            index += 1;
        }
        let end = (last + CONTEXT + 1).min(edits.len());
        // Only unchanged lines sit between hunks.
        old_before += start - next;
        new_before += start - next;
        let (mut old_count, mut new_count) = (0usize, 0usize);
        let mut lines = Vec::with_capacity(end - start);
        for (kind, line) in &edits[start..end] {
            let mark = match kind {
                Kind::Keep => {
                    old_count += 1;
                    new_count += 1;
                    ' '
                }
                Kind::Remove => {
                    old_count += 1;
                    '-'
                }
                Kind::Add => {
                    new_count += 1;
                    '+'
                }
            };
            match line.strip_suffix('\n') {
                Some(text) => lines.push(format!("{mark}{text}")),
                None => {
                    lines.push(format!("{mark}{line}"));
                    lines.push(NO_NEWLINE.to_string());
                }
            }
        }
        // A side with no lines in the hunk is addressed by the line before it.
        let start_of = |before: usize, count: usize| before + usize::from(count > 0);
        out.push(RawHunk {
            old_start: numbered.then(|| start_of(old_before, old_count)),
            new_start: numbered.then(|| start_of(new_before, new_count)),
            section: None,
            lines,
        });
        old_before += old_count;
        new_before += new_count;
        next = end;
    }
    out
}
