//! The diff row model: a change set turned into the flat rows every renderer
//! draws.
//!
//! The daemon sends a bounded, sanitised change set; nothing here parses or
//! computes a diff. This module numbers lines by walking hunks, folds long
//! unchanged runs, applies the view's display thresholds and the operator's
//! local choices (which files and folds are open, where the cursor is), and
//! builds the rows of the visible window only. Every word a row or a label
//! carries comes from the view's `projections.diff.words`.

use std::collections::{BTreeMap, BTreeSet};

use ryeos_changeset::{Basis, ChangeSet, FileChange, Op, TruncatedReason};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::effect::{RyeOsEffect, RyeOsEffectKind};
use super::keymap::RyeOsDiffKeyContext;
use super::model::RyeOsCore;
use super::view_model::RyeOsTone;
use crate::view_set::ViewLocalState;

/// Rows of one diff materialised around its cursor per frame.
pub(crate) const DIFF_RENDER_WINDOW: usize = 200;

/// The state key of a view whose whole body is one diff.
pub(crate) const VIEW_DIFF_KEY: &str = "view";

/// A typed body under a timeline entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "block", rename_all = "snake_case")]
pub enum RyeOsBlockVm {
    Diff(RyeOsDiffVm),
}

/// A change set as rows. `rows` is the window `first_row..first_row +
/// rows.len()` of `total_rows`; `above` and `below` name what the window
/// leaves out, so no row is dropped silently.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsDiffVm {
    /// The whole block in plain words, for a screen reader or a copy.
    pub label: String,
    pub total_rows: usize,
    pub first_row: usize,
    pub rows: Vec<RyeOsDiffRowVm>,
    /// The row under the diff's own cursor (an index into all rows), when the
    /// operator is inside the block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub above: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub below: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RyeOsDiffRowKind {
    /// The block's header: what the change is and its totals.
    Summary,
    /// A file's header; the collapsed form of the file.
    File,
    /// The start of a hunk: the unchanged gap before it and its section.
    Hunk,
    /// One diff line.
    Line,
    /// A folded run of unchanged lines; opening it shows them.
    Fold,
    /// Lines or files held back by the view's thresholds; opening shows them.
    More,
    /// Content the daemon did not send (a cap, a binary file). Not openable.
    NotShown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsDiffRowVm {
    pub kind: RyeOsDiffRowKind,
    /// What the row shows. For a line: its text without the mark.
    pub text: String,
    /// The row in plain words.
    pub label: String,
    /// A line's mark (space, `-`, `+`, or `\` for a no-newline note). Always
    /// drawn as a character, so the diff survives monochrome and copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mark: Option<char>,
    /// Line numbers in the old and new text; absent where the line is not on
    /// that side or the hunk does not know where it starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_no: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_no: Option<usize>,
    pub tone: RyeOsTone,
    /// Whether activating the row opens or closes something, and which way it
    /// is now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collapsed: Option<bool>,
}

/// The words a diff is drawn and read with, from signed view data. A key the
/// view does not give reads as the key itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct DiffWords(BTreeMap<String, String>);

impl DiffWords {
    fn get<'a>(&'a self, key: &'a str) -> &'a str {
        self.0.get(key).map_or(key, String::as_str)
    }

    /// A sentence the view may or may not give; nothing is said without it.
    pub(crate) fn sentence(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
}

/// Display thresholds, from signed view data.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DiffPolicy {
    pub words: DiffWords,
    /// Context lines kept either side of a folded run; a run longer than
    /// twice this folds.
    pub fold_after: usize,
    /// A block with more diff lines than this starts collapsed to its header.
    pub collapse_lines: usize,
    /// Files drawn before a "more files" row.
    pub files_shown: usize,
    /// Line rows drawn per file before a "more lines" row.
    pub lines_shown: usize,
}

impl Default for DiffPolicy {
    fn default() -> Self {
        Self {
            words: DiffWords::default(),
            fold_after: 3,
            collapse_lines: 400,
            files_shown: 10,
            lines_shown: 200,
        }
    }
}

impl DiffPolicy {
    /// Reads `projections.diff`; absent keys keep the defaults.
    pub(crate) fn from_projections(projections: &Value) -> Self {
        let mut policy = Self::default();
        let Some(diff) = projections.get("diff") else {
            return policy;
        };
        let number = |key: &str, default: usize| {
            diff.get(key)
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(default)
        };
        policy.fold_after = number("fold_after", policy.fold_after);
        policy.collapse_lines = number("collapse_lines", policy.collapse_lines);
        policy.files_shown = number("files_shown", policy.files_shown);
        policy.lines_shown = number("lines_shown", policy.lines_shown);
        if let Some(words) = diff.get("words").and_then(Value::as_object) {
            policy.words = DiffWords(
                words
                    .iter()
                    .filter_map(|(key, word)| Some((key.clone(), word.as_str()?.to_string())))
                    .collect(),
            );
        }
        policy
    }
}

/// The operator's choices inside one diff. Local to the client.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLocalState {
    /// The row under the cursor while the operator is inside the block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<usize>,
    /// The block's own disclosure, when the operator changed it from the
    /// default the view's `collapse_lines` gives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open: Option<bool>,
    /// Files folded to their header, by file index.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub closed_files: BTreeSet<usize>,
    /// Opened unchanged runs, by `(file index, fold index)`.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub open_folds: BTreeSet<(usize, usize)>,
    /// Files whose rows past the view's `lines_shown` are shown.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub all_lines: BTreeSet<usize>,
    /// The files past the view's `files_shown` are shown.
    #[serde(default)]
    pub all_files: bool,
}

/// One row of the plan: indexes only, so planning allocates no text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowRef {
    Summary,
    File(usize),
    Hunk {
        file: usize,
        hunk: usize,
        /// Unchanged lines between the previous hunk (or the file's start)
        /// and this one; absent when the hunks carry no line numbers.
        gap: Option<usize>,
    },
    Line {
        file: usize,
        hunk: usize,
        line: usize,
        old_no: Option<usize>,
        new_no: Option<usize>,
    },
    Fold {
        file: usize,
        fold: usize,
        count: usize,
    },
    MoreLines {
        file: usize,
        count: usize,
    },
    MoreFiles(usize),
    Binary(usize),
    FileCut(usize),
    FilesCut(usize),
}

fn mark_of(line: &str) -> char {
    line.chars().next().unwrap_or(' ')
}

/// Whether the block is drawn past its header.
fn block_open(set: &ChangeSet, policy: &DiffPolicy, state: &DiffLocalState) -> bool {
    state
        .open
        .unwrap_or(set.shown_lines() <= policy.collapse_lines)
}

/// Every row of the diff under the policy and the operator's choices.
fn plan(set: &ChangeSet, policy: &DiffPolicy, state: &DiffLocalState) -> Vec<RowRef> {
    let mut rows = vec![RowRef::Summary];
    if !block_open(set, policy, state) {
        return rows;
    }
    let files_shown = if state.all_files {
        set.files.len()
    } else {
        policy.files_shown.min(set.files.len())
    };
    for (file_index, file) in set.files.iter().enumerate().take(files_shown) {
        rows.push(RowRef::File(file_index));
        if state.closed_files.contains(&file_index) {
            continue;
        }
        plan_file(&mut rows, file_index, file, policy, state);
    }
    if files_shown < set.files.len() {
        rows.push(RowRef::MoreFiles(set.files.len() - files_shown));
    }
    if set.omitted_files > 0 {
        rows.push(RowRef::FilesCut(set.omitted_files));
    }
    rows
}

fn plan_file(
    rows: &mut Vec<RowRef>,
    file_index: usize,
    file: &FileChange,
    policy: &DiffPolicy,
    state: &DiffLocalState,
) {
    let limit = if state.all_lines.contains(&file_index) {
        usize::MAX
    } else {
        policy.lines_shown
    };
    let total: usize = file.shown_lines();
    let mut drawn = 0usize;
    let mut fold_index = 0usize;
    // Where the previous hunk ended in the old text, for the gap before the
    // next one.
    let mut old_end = Some(0usize);
    'hunks: for (hunk_index, hunk) in file.hunks.iter().enumerate() {
        let gap = match (old_end, hunk.old_start) {
            (Some(end), Some(start)) => {
                let has_old = hunk
                    .lines
                    .iter()
                    .any(|line| matches!(mark_of(line), ' ' | '-'));
                // A hunk with no old lines is addressed by the line before it.
                let first = if has_old {
                    start.saturating_sub(1)
                } else {
                    start
                };
                Some(first.saturating_sub(end))
            }
            _ => None,
        };
        if gap.is_some_and(|gap| gap > 0) || hunk.section.is_some() || hunk_index > 0 {
            rows.push(RowRef::Hunk {
                file: file_index,
                hunk: hunk_index,
                gap: gap.filter(|gap| *gap > 0),
            });
        }
        let (mut old_no, mut new_no) = (hunk.old_start, hunk.new_start);
        let has_old = hunk
            .lines
            .iter()
            .any(|line| matches!(mark_of(line), ' ' | '-'));
        let has_new = hunk
            .lines
            .iter()
            .any(|line| matches!(mark_of(line), ' ' | '+'));
        // A side with no lines in the hunk has nothing to number.
        if !has_old {
            old_no = None;
        }
        if !has_new {
            new_no = None;
        }
        let mut line_index = 0usize;
        while line_index < hunk.lines.len() {
            // A run of unchanged lines, and whether it folds.
            let run = hunk.lines[line_index..]
                .iter()
                .take_while(|line| mark_of(line) == ' ')
                .count();
            let folds = run > 2 * policy.fold_after;
            let folded = folds && !state.open_folds.contains(&(file_index, fold_index));
            let fold_range =
                (line_index + policy.fold_after)..(line_index + run - policy.fold_after.min(run));
            let span = run.max(1);
            for offset in 0..span {
                let index = line_index + offset;
                let mark = mark_of(&hunk.lines[index]);
                let (this_old, this_new) = match mark {
                    ' ' => (old_no, new_no),
                    '-' => (old_no, None),
                    '+' => (None, new_no),
                    _ => (None, None),
                };
                if matches!(mark, ' ' | '-') {
                    old_no = old_no.map(|number| number + 1);
                }
                if matches!(mark, ' ' | '+') {
                    new_no = new_no.map(|number| number + 1);
                }
                if folded && fold_range.contains(&index) {
                    if index == fold_range.start {
                        if drawn >= limit {
                            rows.push(RowRef::MoreLines {
                                file: file_index,
                                count: total - index_before(file, hunk_index, index),
                            });
                            break 'hunks;
                        }
                        rows.push(RowRef::Fold {
                            file: file_index,
                            fold: fold_index,
                            count: fold_range.len(),
                        });
                        drawn += 1;
                    }
                    continue;
                }
                if drawn >= limit {
                    rows.push(RowRef::MoreLines {
                        file: file_index,
                        count: total - index_before(file, hunk_index, index),
                    });
                    break 'hunks;
                }
                rows.push(RowRef::Line {
                    file: file_index,
                    hunk: hunk_index,
                    line: index,
                    old_no: this_old,
                    new_no: this_new,
                });
                drawn += 1;
            }
            if folds {
                fold_index += 1;
            }
            line_index += span;
        }
        old_end = match hunk.old_start {
            Some(start) => {
                let old_lines = hunk
                    .lines
                    .iter()
                    .filter(|line| matches!(mark_of(line), ' ' | '-'))
                    .count();
                Some(if old_lines > 0 {
                    (start + old_lines).saturating_sub(1)
                } else {
                    start
                })
            }
            None => None,
        };
    }
    if file.binary {
        rows.push(RowRef::Binary(file_index));
    }
    if file.truncated.is_some() {
        rows.push(RowRef::FileCut(file_index));
    }
}

/// Diff lines of `file` before line `line` of hunk `hunk`.
fn index_before(file: &FileChange, hunk: usize, line: usize) -> usize {
    file.hunks[..hunk]
        .iter()
        .map(|hunk| hunk.lines.len())
        .sum::<usize>()
        + line
}

fn basis_key(basis: Basis) -> &'static str {
    match basis {
        Basis::Requested => "basis.requested",
        Basis::Reported => "basis.reported",
        Basis::Recorded => "basis.recorded",
    }
}

fn op_key(op: Op) -> &'static str {
    match op {
        Op::Add => "op.add",
        Op::Modify => "op.modify",
        Op::Delete => "op.delete",
        Op::Rename => "op.rename",
    }
}

fn reason_key(reason: TruncatedReason) -> &'static str {
    match reason {
        TruncatedReason::Lines => "reason.lines",
        TruncatedReason::Total => "reason.total",
        TruncatedReason::Bytes => "reason.bytes",
        TruncatedReason::Window => "reason.window",
        TruncatedReason::TooLarge => "reason.too_large",
    }
}

fn counted<'a>(count: usize, one: &'a str, many: &'a str, words: &'a DiffWords) -> &'a str {
    words.get(if count == 1 { one } else { many })
}

/// A file in plain words: path, operation, counts and hunks.
fn file_label(file: &FileChange, words: &DiffWords) -> String {
    let path = match &file.from {
        Some(from) => format!("{from} {} {}", words.get("renamed_to"), file.path),
        None => file.path.clone(),
    };
    format!(
        "{path}, {}, {} {}, {} {}, {} {}",
        words.get(op_key(file.op)),
        file.added,
        words.get("added"),
        file.removed,
        words.get("removed"),
        file.hunks.len(),
        counted(file.hunks.len(), "hunk", "hunks", words),
    )
}

/// The basis as drawn: a requested change on an entry that did not apply it
/// says so.
fn basis_word<'a>(set: &ChangeSet, applied: bool, words: &'a DiffWords) -> &'a str {
    if set.basis == Basis::Requested && !applied {
        words.get("basis.not_applied")
    } else {
        words.get(basis_key(set.basis))
    }
}

/// The whole block in plain words.
fn block_label(set: &ChangeSet, applied: bool, words: &DiffWords) -> String {
    let files = set.files.len() + set.omitted_files;
    let listed: Vec<String> = set
        .files
        .iter()
        .map(|file| {
            format!(
                "{} {}, {} {}, {} {}",
                file.path,
                words.get(op_key(file.op)),
                file.added,
                words.get("added"),
                file.removed,
                words.get("removed"),
            )
        })
        .collect();
    format!(
        "{} {} {files} {}: {}",
        basis_word(set, applied, words),
        words.get("change_to"),
        counted(files, "file", "files", words),
        listed.join("; "),
    )
}

fn row_vm(
    set: &ChangeSet,
    applied: bool,
    policy: &DiffPolicy,
    state: &DiffLocalState,
    row: RowRef,
) -> RyeOsDiffRowVm {
    let words = &policy.words;
    let plain = |kind, text: String, tone, collapsed| RyeOsDiffRowVm {
        kind,
        label: text.clone(),
        text,
        mark: None,
        old_no: None,
        new_no: None,
        tone,
        collapsed,
    };
    match row {
        RowRef::Summary => {
            let (added, removed) = set.files.iter().fold((0, 0), |(added, removed), file| {
                (added + file.added, removed + file.removed)
            });
            let files = set.files.len() + set.omitted_files;
            let text = format!(
                "{} · {files} {} · +{added} -{removed}",
                basis_word(set, applied, words),
                counted(files, "file", "files", words),
            );
            RyeOsDiffRowVm {
                label: block_label(set, applied, words),
                ..plain(
                    RyeOsDiffRowKind::Summary,
                    text,
                    if set.basis == Basis::Requested && !applied {
                        RyeOsTone::Warn
                    } else {
                        RyeOsTone::Neutral
                    },
                    Some(!block_open(set, policy, state)),
                )
            }
        }
        RowRef::File(index) => {
            let file = &set.files[index];
            let path = match &file.from {
                Some(from) => format!("{from} → {}", file.path),
                None => file.path.clone(),
            };
            let text = format!(
                "{path} · {} · +{} -{}",
                words.get(op_key(file.op)),
                file.added,
                file.removed
            );
            RyeOsDiffRowVm {
                label: file_label(file, words),
                ..plain(
                    RyeOsDiffRowKind::File,
                    text,
                    RyeOsTone::Neutral,
                    Some(state.closed_files.contains(&index)),
                )
            }
        }
        RowRef::Hunk { file, hunk, gap } => {
            let section = set.files[file].hunks[hunk].section.as_deref();
            let gap = gap.map(|gap| {
                format!(
                    "{gap} {}",
                    counted(gap, "unchanged_line", "unchanged_lines", words)
                )
            });
            let text = [gap.as_deref(), section]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
            plain(RyeOsDiffRowKind::Hunk, text, RyeOsTone::Neutral, None)
        }
        RowRef::Line {
            file,
            hunk,
            line,
            old_no,
            new_no,
        } => {
            let raw = &set.files[file].hunks[hunk].lines[line];
            let mark = mark_of(raw);
            let text = raw[mark.len_utf8().min(raw.len())..].to_string();
            let (word, tone) = match mark {
                '+' => (words.get("line.added"), RyeOsTone::Good),
                '-' => (words.get("line.removed"), RyeOsTone::Danger),
                ' ' => (words.get("line.unchanged"), RyeOsTone::Neutral),
                _ => (words.get("line.note"), RyeOsTone::Neutral),
            };
            let numbers: Vec<String> = [old_no, new_no]
                .into_iter()
                .flatten()
                .map(|number| number.to_string())
                .collect();
            let label = if numbers.is_empty() {
                format!("{word}: {text}")
            } else {
                format!("{} {word}: {text}", numbers.join(" "))
            };
            RyeOsDiffRowVm {
                kind: RyeOsDiffRowKind::Line,
                text,
                label,
                mark: Some(mark),
                old_no,
                new_no,
                tone,
                collapsed: None,
            }
        }
        RowRef::Fold { count, .. } => plain(
            RyeOsDiffRowKind::Fold,
            format!(
                "{count} {}",
                counted(count, "unchanged_line", "unchanged_lines", words)
            ),
            RyeOsTone::Neutral,
            Some(true),
        ),
        RowRef::MoreLines { count, .. } => plain(
            RyeOsDiffRowKind::More,
            format!(
                "{count} {}",
                counted(count, "more_line", "more_lines", words)
            ),
            RyeOsTone::Neutral,
            Some(true),
        ),
        RowRef::MoreFiles(count) => plain(
            RyeOsDiffRowKind::More,
            format!(
                "{count} {}",
                counted(count, "more_file", "more_files", words)
            ),
            RyeOsTone::Neutral,
            Some(true),
        ),
        RowRef::Binary(_) => plain(
            RyeOsDiffRowKind::NotShown,
            words.get("binary").to_string(),
            RyeOsTone::Neutral,
            None,
        ),
        RowRef::FileCut(index) => {
            let file = &set.files[index];
            let text = match &file.truncated {
                Some(cut) => {
                    let mut text = format!("{}: ", words.get("not_shown"));
                    if cut.omitted_lines > 0 {
                        text.push_str(&format!(
                            "{} {} ",
                            cut.omitted_lines,
                            counted(cut.omitted_lines, "line", "lines", words)
                        ));
                    }
                    if let (Some(old), Some(new)) = (cut.old_bytes, cut.new_bytes) {
                        text.push_str(&format!("{old} / {new} {} ", words.get("bytes")));
                    }
                    text.push_str(&format!("({})", words.get(reason_key(cut.reason))));
                    text
                }
                None => words.get("not_shown").to_string(),
            };
            plain(RyeOsDiffRowKind::NotShown, text, RyeOsTone::Warn, None)
        }
        RowRef::FilesCut(count) => plain(
            RyeOsDiffRowKind::NotShown,
            format!(
                "{}: {count} {} ({})",
                words.get("not_shown"),
                counted(count, "file", "files", words),
                words.get("reason.files")
            ),
            RyeOsTone::Warn,
            None,
        ),
    }
}

/// The diff's view model: every row planned, the rows of the window around
/// the cursor built. `applied` is false when the entry carrying a requested
/// change did not complete it.
pub(crate) fn diff_vm(
    set: &ChangeSet,
    applied: bool,
    policy: &DiffPolicy,
    state: &DiffLocalState,
    window: usize,
) -> RyeOsDiffVm {
    let plan = plan(set, policy, state);
    let total_rows = plan.len();
    let cursor = state.cursor.map(|cursor| cursor.min(total_rows - 1));
    let first_row = match cursor {
        Some(cursor) if total_rows > window => {
            cursor.saturating_sub(window / 2).min(total_rows - window)
        }
        _ => 0,
    };
    let rows: Vec<RyeOsDiffRowVm> = plan
        .iter()
        .skip(first_row)
        .take(window)
        .map(|row| row_vm(set, applied, policy, state, *row))
        .collect();
    let words = &policy.words;
    let left_out = |count: usize| {
        (count > 0).then(|| format!("{count} {}", counted(count, "more_row", "more_rows", words)))
    };
    RyeOsDiffVm {
        label: block_label(set, applied, words),
        total_rows,
        first_row,
        above: left_out(first_row),
        below: left_out(total_rows - first_row - rows.len()),
        rows,
        cursor,
    }
}

/// The diff of an entry's change block as a renderer draws it, under the
/// view's `projections` and with no operator choices applied. `None` when
/// the entry carries no change block this client knows.
pub fn diff_vm_for_entry(entry: &Value, projections: &Value) -> Option<RyeOsDiffVm> {
    Some(diff_vm(
        &entry_change_set(entry)?,
        entry_applied(entry),
        &DiffPolicy::from_projections(projections),
        &DiffLocalState::default(),
        DIFF_RENDER_WINDOW,
    ))
}

/// What the operator can do inside a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RyeOsDiffAction {
    /// Move the cursor by rows; moving past either end leaves the block.
    Move {
        delta: i32,
    },
    /// Open or close what the cursor is on: the block, a file, a fold, or
    /// rows held back by a threshold.
    Toggle,
    NextFile,
    PreviousFile,
    NextHunk,
    PreviousHunk,
    /// Put the cursor back on the entry that carries the block.
    Leave,
    /// Copy the file under the cursor as unified diff text.
    CopyFile,
    /// Copy the whole block as unified diff text.
    CopyBlock,
}

/// What an action did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct DiffOutcome {
    /// The local state changed.
    pub changed: bool,
    /// Text to put on the clipboard.
    pub copy: Option<String>,
}

/// The file a planned row belongs to.
fn row_file(row: RowRef) -> Option<usize> {
    match row {
        RowRef::File(file)
        | RowRef::Hunk { file, .. }
        | RowRef::Line { file, .. }
        | RowRef::Fold { file, .. }
        | RowRef::MoreLines { file, .. }
        | RowRef::Binary(file)
        | RowRef::FileCut(file) => Some(file),
        RowRef::Summary | RowRef::MoreFiles(_) | RowRef::FilesCut(_) => None,
    }
}

/// The unified diff text of the block (or of one file): exactly the bounded,
/// sanitised lines that are drawn, not the files themselves.
pub(crate) fn copy_text(set: &ChangeSet, file: Option<usize>) -> String {
    match file.and_then(|index| set.files.get(index)) {
        Some(file) => file.unified(),
        None => set.files.iter().map(FileChange::unified).collect(),
    }
}

/// Applies `action` to the diff's local state.
pub(crate) fn apply_action(
    set: &ChangeSet,
    policy: &DiffPolicy,
    state: &mut DiffLocalState,
    action: RyeOsDiffAction,
) -> DiffOutcome {
    let before = state.clone();
    let rows = plan(set, policy, state);
    let last = rows.len() - 1;
    let cursor = state.cursor.map(|cursor| cursor.min(last));
    let mut copy = None;
    // The first row at or after `from` (or at or before it, going back) that
    // starts a file or a hunk.
    let seek = |from: Option<usize>, forward: bool, hunks: bool| {
        let starts = |index: &usize| match rows[*index] {
            RowRef::File(_) => !hunks,
            RowRef::Hunk { .. } => hunks,
            // A first hunk with no separator row starts at its first line.
            RowRef::Line {
                line: 0, hunk: 0, ..
            } => hunks && !matches!(rows[index.saturating_sub(1)], RowRef::Hunk { .. }),
            _ => false,
        };
        if forward {
            (from.map_or(0, |at| at + 1)..=last).find(starts)
        } else {
            (0..from.unwrap_or(0)).rev().find(starts)
        }
    };
    match action {
        RyeOsDiffAction::Move { delta } => {
            state.cursor = match cursor {
                None if delta > 0 => Some(0),
                None => None,
                Some(at) => {
                    let to = at as i64 + i64::from(delta);
                    (0..=last as i64).contains(&to).then_some(to as usize)
                }
            };
        }
        RyeOsDiffAction::Leave => state.cursor = None,
        RyeOsDiffAction::NextFile => {
            state.cursor = seek(cursor, true, false).or(cursor);
        }
        RyeOsDiffAction::PreviousFile => {
            state.cursor = seek(cursor, false, false).or(cursor);
        }
        RyeOsDiffAction::NextHunk => {
            state.cursor = seek(cursor, true, true).or(cursor);
        }
        RyeOsDiffAction::PreviousHunk => {
            state.cursor = seek(cursor, false, true).or(cursor);
        }
        RyeOsDiffAction::Toggle => match rows[cursor.unwrap_or(0)] {
            RowRef::Summary => {
                state.open = Some(!block_open(set, policy, state));
                state.cursor = Some(0);
            }
            RowRef::File(file) => {
                if !state.closed_files.remove(&file) {
                    state.closed_files.insert(file);
                }
            }
            RowRef::Fold { file, fold, .. } => {
                state.open_folds.insert((file, fold));
            }
            RowRef::MoreLines { file, .. } => {
                state.all_lines.insert(file);
            }
            RowRef::MoreFiles(_) => state.all_files = true,
            // Other rows have nothing to open or close.
            _ => {}
        },
        RyeOsDiffAction::CopyFile => {
            copy = Some(copy_text(set, cursor.and_then(|at| row_file(rows[at]))));
        }
        RyeOsDiffAction::CopyBlock => copy = Some(copy_text(set, None)),
    }
    DiffOutcome {
        changed: *state != before,
        copy,
    }
}

/// The change set of an entry's first `changes` block, when it has one that
/// parses. Anything else (an unknown block kind, a shape this client does not
/// know) is not drawn.
pub(crate) fn entry_change_set(entry: &Value) -> Option<ChangeSet> {
    entry
        .get("blocks")?
        .as_array()?
        .iter()
        .find_map(block_change_set)
}

fn block_change_set(block: &Value) -> Option<ChangeSet> {
    let mut fields = block.as_object()?.clone();
    if fields.remove("block")?.as_str()? != "changes" {
        return None;
    }
    serde_json::from_value(Value::Object(fields)).ok()
}

/// The change set a `diff` view draws: the response fields its projections
/// name (`basis`, `files`, and optionally `omitted_files`), read as the one
/// change-set shape. Anything else is not drawn.
pub(crate) fn view_change_set(projections: &Value, response: &Value) -> Option<ChangeSet> {
    let field = |name: &str| {
        projections
            .get(name)
            .and_then(Value::as_str)
            .and_then(|path| super::content::field_path(response, path))
            .cloned()
    };
    serde_json::from_value(serde_json::json!({
        "basis": field("basis")?,
        "files": field("files")?,
        "omitted_files": field("omitted_files").unwrap_or_else(|| Value::from(0)),
    }))
    .ok()
}

/// What a `diff` view says when its source has no change to draw: the view's
/// `empty` sentence when it gives one, else the source's availability record
/// as it stands (`state: reason`).
pub(crate) fn unavailable_message(policy: &DiffPolicy, response: &Value) -> String {
    if let Some(sentence) = policy.words.sentence("empty") {
        return sentence.to_string();
    }
    let record = response.pointer("/availability/0");
    let part = |name: &str| {
        record
            .and_then(|record| record.get(name))
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    format!("{}: {}", part("state"), part("reason"))
}

/// Whether a record carries any typed block, so it can be opened to show one.
pub(crate) fn has_blocks(entry: &Value) -> bool {
    entry
        .get("blocks")
        .and_then(Value::as_array)
        .is_some_and(|blocks| !blocks.is_empty())
}

/// Whether the entry that carries a requested change completed it: any state
/// but a finished-well one means the change is only what was asked for.
pub(crate) fn entry_applied(entry: &Value) -> bool {
    entry.get("state").and_then(Value::as_str) == Some("done")
}

/// The open diff under the point of the focused view.
struct FocusedDiff {
    tile_id: crate::ids::TileId,
    /// The stable identity of the record carrying the diff: the key its local
    /// state is stored under.
    key: String,
    set: ChangeSet,
    policy: DiffPolicy,
    state: DiffLocalState,
    /// The diff sits under a timeline entry and keeps its own cursor. When
    /// false the view itself is the diff and the tile's cursor is its cursor.
    nested: bool,
}

impl RyeOsCore {
    /// The diff the keys act on: the focused view's record under the point,
    /// when it is expanded and carries a change set.
    fn focused_diff(&self) -> Option<FocusedDiff> {
        let view_set = self.active_view_set()?;
        let tile_id = view_set.focused_tile;
        let tile = view_set.tiles.get(&tile_id)?;
        let binding = self.binding_for_instance(&tile.instance_key, &tile.view.view_ref)?;
        let ViewLocalState::GenericList {
            cursor,
            expanded_rows,
            diffs,
            ..
        } = &tile.local
        else {
            return None;
        };
        if binding.widget == "diff" {
            let (channel, _) = binding.primary_source()?;
            let source_key = super::source_key::RyeOsSourceInstanceKey::named(
                tile.instance_key.clone(),
                channel,
            )
            .encode();
            let set = view_change_set(&binding.projections, self.data.sources.get(&source_key)?)
                .filter(|set| !set.files.is_empty())?;
            let mut state = diffs.get(VIEW_DIFF_KEY).cloned().unwrap_or_default();
            state.cursor = Some(*cursor);
            return Some(FocusedDiff {
                tile_id,
                key: VIEW_DIFF_KEY.to_string(),
                set,
                policy: DiffPolicy::from_projections(&binding.projections),
                state,
                nested: false,
            });
        }
        let (key, record) = self.focused_row_key_and_record(tile_id, binding)?;
        if !expanded_rows.contains(&key) {
            return None;
        }
        Some(FocusedDiff {
            tile_id,
            set: entry_change_set(&record)?,
            policy: DiffPolicy::from_projections(&binding.projections),
            state: diffs.get(&key).cloned().unwrap_or_default(),
            key,
            nested: true,
        })
    }

    pub(crate) fn focused_diff_key_context(&self) -> Option<RyeOsDiffKeyContext> {
        self.focused_diff().map(|focus| RyeOsDiffKeyContext {
            inside: focus.state.cursor.is_some(),
            nested: focus.nested,
        })
    }

    /// Applies a diff action to the open diff under the point. Copying puts
    /// the drawn text on the clipboard and says, in the view's words, that it
    /// is the bounded display.
    pub(crate) fn diff_action(&mut self, action: RyeOsDiffAction) -> Vec<RyeOsEffect> {
        let Some(mut focus) = self.focused_diff() else {
            return Vec::new();
        };
        let held = focus.state.cursor;
        let outcome = apply_action(&focus.set, &focus.policy, &mut focus.state, action);
        if outcome.changed {
            if let Some(tile) = self
                .active_view_set_mut()
                .and_then(|view_set| view_set.tiles.get_mut(&focus.tile_id))
                && let ViewLocalState::GenericList { cursor, diffs, .. } = &mut tile.local
            {
                if !focus.nested {
                    // The tile's cursor is this diff's cursor: it never
                    // leaves the diff, and it is stored on the tile.
                    *cursor = focus.state.cursor.take().or(held).unwrap_or(0);
                }
                if focus.state == DiffLocalState::default() {
                    diffs.remove(&focus.key);
                } else {
                    diffs.insert(focus.key.clone(), focus.state.clone());
                }
            }
            self.bump_generation();
        }
        let Some(text) = outcome.copy else {
            return Vec::new();
        };
        if let Some(notice) = focus.policy.words.sentence("copy_notice") {
            self.notice(notice.to_string(), RyeOsTone::Neutral);
        }
        vec![self.emit(RyeOsEffectKind::CopyToClipboard { text })]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_changeset::Caps;
    use serde_json::json;

    fn wide() -> Caps {
        Caps {
            files: 200,
            lines_per_file: 10_000,
            total_lines: 10_000,
            line_chars: 400,
        }
    }

    fn policy() -> DiffPolicy {
        DiffPolicy::from_projections(&json!({"diff": {
        "fold_after": 3, "collapse_lines": 400, "files_shown": 10, "lines_shown": 200,
        "words": {
            "basis.requested": "Requested", "basis.reported": "Reported",
            "basis.recorded": "Recorded", "basis.not_applied": "Requested, not applied",
            "op.add": "added file", "op.modify": "modified", "op.delete": "deleted",
            "op.rename": "renamed", "renamed_to": "renamed to", "change_to": "change to",
            "file": "file", "files": "files", "added": "added", "removed": "removed",
            "hunk": "hunk", "hunks": "hunks",
            "unchanged_line": "unchanged line", "unchanged_lines": "unchanged lines",
            "line.added": "added", "line.removed": "removed",
            "line.unchanged": "unchanged", "line.note": "note",
            "more_line": "more line", "more_lines": "more lines",
            "more_file": "more file", "more_files": "more files",
            "more_row": "more row", "more_rows": "more rows",
            "not_shown": "Not shown", "line": "line", "lines": "lines",
            "reason.lines": "file limit", "reason.files": "limit",
        }}}))
    }

    const TWO_HUNKS: &str = "\
--- a/src/main.rs
+++ b/src/main.rs
@@ -10,3 +10,4 @@ fn main() {
 fn main() {
-    old();
+    new();
+    more();
 }
@@ -40,2 +41,2 @@
 // tail
-x
+y
";

    fn set(patch: &str) -> ChangeSet {
        ChangeSet::from_patch(patch, Basis::Requested, &wide()).unwrap()
    }

    fn rows(vm: &RyeOsDiffVm) -> Vec<(Option<usize>, Option<usize>, Option<char>, &str)> {
        vm.rows
            .iter()
            .map(|row| (row.old_no, row.new_no, row.mark, row.text.as_str()))
            .collect()
    }

    #[test]
    fn lines_are_numbered_by_walking_each_hunk() {
        let vm = diff_vm(
            &set(TWO_HUNKS),
            true,
            &policy(),
            &DiffLocalState::default(),
            200,
        );
        assert_eq!(
            rows(&vm),
            [
                (None, None, None, "Requested · 1 file · +3 -2"),
                (None, None, None, "src/main.rs · modified · +3 -2"),
                (None, None, None, "9 unchanged lines · fn main() {"),
                (Some(10), Some(10), Some(' '), "fn main() {"),
                (Some(11), None, Some('-'), "    old();"),
                (None, Some(11), Some('+'), "    new();"),
                (None, Some(12), Some('+'), "    more();"),
                (Some(12), Some(13), Some(' '), "}"),
                (None, None, None, "27 unchanged lines"),
                (Some(40), Some(41), Some(' '), "// tail"),
                (Some(41), None, Some('-'), "x"),
                (None, Some(42), Some('+'), "y"),
            ]
        );
        assert_eq!(vm.total_rows, 12);
        assert_eq!((vm.above.as_deref(), vm.below.as_deref()), (None, None));
    }

    #[test]
    fn an_edit_without_line_numbers_draws_none_and_no_gap() {
        let set = ChangeSet::from_edit("f", "a\nb\n", "a\nc\n", Basis::Requested, &wide()).unwrap();
        let vm = diff_vm(&set, true, &policy(), &DiffLocalState::default(), 200);
        assert_eq!(
            rows(&vm)[2..],
            [
                (None, None, Some(' '), "a"),
                (None, None, Some('-'), "b"),
                (None, None, Some('+'), "c"),
            ]
        );
        assert_eq!(vm.rows[3].label, "removed: b");
    }

    /// A run of context folds only when it is longer than twice `fold_after`,
    /// keeps `fold_after` lines either side, and opens on a toggle.
    #[test]
    fn long_unchanged_runs_fold_at_the_views_threshold() {
        let context =
            |count: usize| -> String { (0..count).map(|index| format!(" c{index}\n")).collect() };
        let patch = |count: usize| {
            format!(
                "--- a/f\n+++ b/f\n@@ -1,{} +1,{} @@\n-x\n+y\n{}-p\n+q\n",
                count + 2,
                count + 2,
                context(count)
            )
        };
        let at_threshold = set(&patch(6));
        let vm = diff_vm(
            &at_threshold,
            true,
            &policy(),
            &DiffLocalState::default(),
            200,
        );
        assert!(vm.rows.iter().all(|row| row.kind != RyeOsDiffRowKind::Fold));
        assert_eq!(vm.total_rows, 2 + 10);

        let over = set(&patch(9));
        let mut state = DiffLocalState::default();
        let vm = diff_vm(&over, true, &policy(), &state, 200);
        let texts: Vec<&str> = vm.rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(
            texts[2..],
            [
                "x",
                "y",
                "c0",
                "c1",
                "c2",
                "3 unchanged lines",
                "c6",
                "c7",
                "c8",
                "p",
                "q"
            ]
        );
        let fold = &vm.rows[7];
        assert_eq!(
            (fold.kind, fold.collapsed),
            (RyeOsDiffRowKind::Fold, Some(true))
        );
        assert_eq!(fold.label, "3 unchanged lines");
        // Numbers continue across the fold.
        assert_eq!((vm.rows[8].old_no, vm.rows[8].new_no), (Some(8), Some(8)));

        state.cursor = Some(7);
        assert!(apply_action(&over, &policy(), &mut state, RyeOsDiffAction::Toggle).changed);
        assert_eq!(state.open_folds, BTreeSet::from([(0, 0)]));
        let vm = diff_vm(&over, true, &policy(), &state, 200);
        assert_eq!(vm.total_rows, 2 + 13);
        assert_eq!(vm.rows[7].text, "c3");

        // A lower threshold from the view folds the shorter run too.
        let mut tight = policy();
        tight.fold_after = 1;
        let vm = diff_vm(&at_threshold, true, &tight, &DiffLocalState::default(), 200);
        let texts: Vec<&str> = vm.rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(
            texts[2..],
            ["x", "y", "c0", "4 unchanged lines", "c5", "p", "q"]
        );
    }

    #[test]
    fn labels_use_the_views_words() {
        let patch = format!(
            "{TWO_HUNKS}diff --git a/old.rs b/new.rs\nrename from old.rs\nrename to new.rs\n"
        );
        let set = set(&patch);
        let vm = diff_vm(&set, true, &policy(), &DiffLocalState::default(), 200);
        assert_eq!(
            vm.label,
            "Requested change to 2 files: src/main.rs modified, 3 added, 2 removed; \
             new.rs renamed, 0 added, 0 removed"
        );
        assert_eq!(vm.rows[0].label, vm.label);
        assert_eq!(
            vm.rows[1].label,
            "src/main.rs, modified, 3 added, 2 removed, 2 hunks"
        );
        assert_eq!(vm.rows[4].label, "11 removed:     old();");
        assert_eq!(vm.rows[7].label, "12 13 unchanged: }");
        let renamed = vm.rows.last().unwrap();
        assert_eq!(renamed.text, "old.rs → new.rs · renamed · +0 -0");
        assert_eq!(
            renamed.label,
            "old.rs renamed to new.rs, renamed, 0 added, 0 removed, 0 hunks"
        );
        // A requested change the entry did not complete says so.
        let failed = diff_vm(&set, false, &policy(), &DiffLocalState::default(), 200);
        assert!(
            failed
                .label
                .starts_with("Requested, not applied change to 2 files")
        );
        assert_eq!(failed.rows[0].tone, RyeOsTone::Warn);
        // A view that gives no words still reads, by key.
        let bare = diff_vm(
            &set,
            true,
            &DiffPolicy::default(),
            &DiffLocalState::default(),
            200,
        );
        assert!(bare.label.starts_with("basis.requested change_to 2 files"));
    }

    #[test]
    fn copied_text_parses_back_to_what_is_drawn() {
        let patch = format!("{TWO_HUNKS}--- /dev/null\n+++ b/n.txt\n@@ -0,0 +1 @@\n+hello\n");
        let set = set(&patch);
        let text = copy_text(&set, None);
        assert_eq!(
            ChangeSet::from_patch(&text, Basis::Requested, &wide()).unwrap(),
            set
        );
        let one = copy_text(&set, Some(1));
        assert_eq!(one, "--- /dev/null\n+++ b/n.txt\n@@ -0,0 +1,1 @@\n+hello\n");

        // Copying the file under the cursor, and the whole block.
        let mut state = DiffLocalState {
            cursor: Some(4),
            ..DiffLocalState::default()
        };
        let outcome = apply_action(&set, &policy(), &mut state, RyeOsDiffAction::CopyFile);
        assert_eq!(
            outcome.copy.as_deref(),
            Some(set.files[0].unified().as_str())
        );
        assert!(!outcome.changed);
        let outcome = apply_action(&set, &policy(), &mut state, RyeOsDiffAction::CopyBlock);
        assert_eq!(outcome.copy, Some(text));
    }

    #[test]
    fn a_five_thousand_row_diff_builds_only_the_visible_window() {
        let mut patch = String::from("--- /dev/null\n+++ b/big.txt\n@@ -0,0 +1,5000 @@\n");
        for index in 0..5_000 {
            patch.push_str(&format!("+{index}\n"));
        }
        let set = set(&patch);
        assert_eq!(set.shown_lines(), 5_000);
        let mut state = DiffLocalState {
            open: Some(true),
            all_lines: BTreeSet::from([0]),
            ..DiffLocalState::default()
        };
        let vm = diff_vm(&set, true, &policy(), &state, 40);
        assert_eq!(vm.total_rows, 5_002);
        assert_eq!((vm.first_row, vm.rows.len()), (0, 40));
        assert_eq!(vm.below.as_deref(), Some("4962 more rows"));

        state.cursor = Some(3_000);
        let vm = diff_vm(&set, true, &policy(), &state, 40);
        assert_eq!((vm.first_row, vm.rows.len()), (2_980, 40));
        assert_eq!(vm.rows[20].text, "2998");
        assert_eq!(vm.rows[20].new_no, Some(2_999));
        assert_eq!(vm.above.as_deref(), Some("2980 more rows"));
        assert_eq!(vm.below.as_deref(), Some("1982 more rows"));

        // Without the operator's choices the view's thresholds hold it back:
        // collapsed to its header, then the head of the file and a count.
        let closed = diff_vm(&set, true, &policy(), &DiffLocalState::default(), 40);
        assert_eq!(closed.total_rows, 1);
        assert_eq!(closed.rows[0].collapsed, Some(true));
        let mut state = DiffLocalState::default();
        apply_action(&set, &policy(), &mut state, RyeOsDiffAction::Toggle);
        let head = diff_vm(&set, true, &policy(), &state, 400);
        assert_eq!(head.total_rows, 2 + 200 + 1);
        let more = head.rows.last().unwrap();
        assert_eq!(
            (more.kind, more.text.as_str()),
            (RyeOsDiffRowKind::More, "4800 more lines")
        );
    }

    #[test]
    fn keys_walk_files_and_hunks_and_leaving_returns_to_the_entry() {
        let patch = format!("{TWO_HUNKS}--- /dev/null\n+++ b/n.txt\n@@ -0,0 +1 @@\n+hello\n");
        let set = set(&patch);
        let policy = policy();
        let mut state = DiffLocalState::default();
        let act = |state: &mut DiffLocalState, action| {
            apply_action(&set, &policy, state, action);
            state.cursor
        };
        // Rows: 0 summary, 1 file, 2 hunk, 3..=7 lines, 8 hunk, 9..=11 lines,
        // 12 file, 13 line.
        assert_eq!(act(&mut state, RyeOsDiffAction::NextFile), Some(1));
        assert_eq!(act(&mut state, RyeOsDiffAction::NextHunk), Some(2));
        assert_eq!(act(&mut state, RyeOsDiffAction::NextHunk), Some(8));
        assert_eq!(act(&mut state, RyeOsDiffAction::NextFile), Some(12));
        assert_eq!(act(&mut state, RyeOsDiffAction::NextHunk), Some(13));
        assert_eq!(act(&mut state, RyeOsDiffAction::NextHunk), Some(13));
        assert_eq!(act(&mut state, RyeOsDiffAction::PreviousHunk), Some(8));
        assert_eq!(act(&mut state, RyeOsDiffAction::PreviousFile), Some(1));
        assert_eq!(act(&mut state, RyeOsDiffAction::PreviousFile), Some(1));

        // Closing the file under the cursor leaves its header.
        apply_action(&set, &policy, &mut state, RyeOsDiffAction::Toggle);
        assert_eq!(state.closed_files, BTreeSet::from([0]));
        let vm = diff_vm(&set, true, &policy, &state, 200);
        assert_eq!(vm.total_rows, 4);
        assert_eq!(vm.rows[1].collapsed, Some(true));
        apply_action(&set, &policy, &mut state, RyeOsDiffAction::Toggle);
        assert!(state.closed_files.is_empty());

        // Moving past either end leaves the block.
        assert_eq!(
            act(&mut state, RyeOsDiffAction::Move { delta: -1 }),
            Some(0)
        );
        assert_eq!(act(&mut state, RyeOsDiffAction::Move { delta: -1 }), None);
        assert_eq!(act(&mut state, RyeOsDiffAction::Move { delta: -1 }), None);
        assert_eq!(act(&mut state, RyeOsDiffAction::Move { delta: 1 }), Some(0));
        state.cursor = Some(13);
        assert_eq!(act(&mut state, RyeOsDiffAction::Move { delta: 1 }), None);
        state.cursor = Some(5);
        assert_eq!(act(&mut state, RyeOsDiffAction::Leave), None);
    }

    #[test]
    fn cut_and_binary_files_say_what_is_not_shown() {
        let mut patch = String::from("--- /dev/null\n+++ b/a\n@@ -0,0 +1,10 @@\n");
        for index in 0..10 {
            patch.push_str(&format!("+l{index}\n"));
        }
        patch.push_str("diff --git a/i.png b/i.png\nBinary files a/i.png and b/i.png differ\n");
        patch.push_str("--- /dev/null\n+++ b/c\n@@ -0,0 +1 @@\n+z\n");
        let caps = Caps {
            files: 2,
            lines_per_file: 4,
            total_lines: 100,
            line_chars: 400,
        };
        let set = ChangeSet::from_patch(&patch, Basis::Recorded, &caps).unwrap();
        let vm = diff_vm(&set, true, &policy(), &DiffLocalState::default(), 200);
        let texts: Vec<&str> = vm.rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "Recorded · 3 files · +10 -0",
                "a · added file · +10 -0",
                "l0",
                "l1",
                "l2",
                "l3",
                "Not shown: 6 lines (file limit)",
                "i.png · modified · +0 -0",
                "binary",
                "Not shown: 1 file (limit)",
            ]
        );
        assert_eq!(vm.rows[6].kind, RyeOsDiffRowKind::NotShown);
    }

    #[test]
    fn only_a_known_block_shape_is_read_from_an_entry() {
        let block = json!({"block": "changes", "basis": "requested", "omitted_files": 0,
            "files": [{"path": "f", "op": "add", "added": 1, "removed": 0, "binary": false,
                       "hunks": [{"lines": ["+x"]}]}]});
        let entry = json!({"state": "done", "blocks": [{"block": "form"}, block]});
        let set = entry_change_set(&entry).unwrap();
        assert_eq!(set.files[0].path, "f");
        assert!(entry_applied(&entry));
        assert!(!entry_applied(&json!({"state": "failed"})));
        assert!(entry_change_set(&json!({"blocks": [{"block": "form"}]})).is_none());
        assert!(entry_change_set(&json!({"slots": {}})).is_none());
        let mut unknown = entry.clone();
        unknown["blocks"][1]["files"][0]["link"] = json!("x");
        assert!(entry_change_set(&unknown).is_none());
    }
}
