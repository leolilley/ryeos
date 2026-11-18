//! One closed shape for a change to files, and the only code that builds it.
//!
//! A [`ChangeSet`] is what a reader is shown of a patch or an edit: per file a
//! path, an operation, full-change line counts and bounded hunks of marked
//! lines. It is built from untrusted text in three ways (a unified patch, a
//! before/after pair, two whole file contents) and every way sanitises the
//! text and applies the same caps, so whoever draws a change set draws text
//! that is already safe and bounded.
//!
//! The crate is pure: no I/O, no clock, and the same input always gives the
//! same change set.

use serde::{Deserialize, Serialize};

mod clean;
mod diff;
mod patch;

pub use clean::{MAX_PATH_CHARS, clean_line, clean_path};
pub use patch::MAX_PATCH_BYTES;

/// A serialised change set never exceeds this, whatever the caps say.
pub const MAX_BLOCK_BYTES: usize = 64 * 1024;
/// A before or after text longer than this is not diffed.
pub const MAX_EDIT_BYTES: usize = 256 * 1024;
/// The line diff gives up searching past this many edits and reports the
/// differing region as one whole replacement.
pub const MAX_EDIT_DISTANCE: usize = 1_000;

/// Ceilings the caps are clamped to.
const MAX_FILES: usize = 200;
const MAX_LINES: usize = 10_000;
const MAX_LINE_CHARS: usize = 2_000;

/// Bytes held back for the change set's own envelope and for one file's
/// truncation record, so the byte ceiling holds after they are added.
const ENVELOPE_RESERVE: usize = 192;
const TRUNCATED_RESERVE: usize = 160;

/// How far a change set may grow. Deployment policy: the values come from
/// signed data and are clamped to the crate's ceilings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Caps {
    /// Files drawn; the rest are counted in `omitted_files`.
    pub files: usize,
    /// Diff lines drawn per file.
    pub lines_per_file: usize,
    /// Diff lines drawn over the whole change set.
    pub total_lines: usize,
    /// Characters kept of one line before an elision mark.
    pub line_chars: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            files: 20,
            lines_per_file: 400,
            total_lines: 2_000,
            line_chars: 400,
        }
    }
}

impl Caps {
    fn clamped(&self) -> Self {
        Self {
            files: self.files.min(MAX_FILES),
            lines_per_file: self.lines_per_file.min(MAX_LINES),
            total_lines: self.total_lines.min(MAX_LINES),
            line_chars: self.line_chars.min(MAX_LINE_CHARS),
        }
    }
}

/// What the change is evidence of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    /// What was asked for; not proof it was applied.
    Requested,
    /// What the party that made the change says it did.
    Reported,
    /// Derived from retained before and after content.
    Recorded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Add,
    Modify,
    Delete,
    Rename,
}

/// Why lines of a file are not shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TruncatedReason {
    /// The per-file line cap.
    Lines,
    /// The change set's total line cap.
    Total,
    /// The change set's byte ceiling.
    Bytes,
    /// A response carrying many change sets ran out of room for this one.
    Window,
    /// The content was too large to read.
    TooLarge,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Truncated {
    pub reason: TruncatedReason,
    /// Diff lines of this file that are not shown.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted_lines: usize,
    /// Sizes of the two contents when one was too large to read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_bytes: Option<u64>,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// A run of diff lines. Each line is one mark character (space, `-`, `+`, or
/// `\` for a no-newline note) followed by the text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hunk {
    /// Line number of the hunk's first line in the old and new text. Absent
    /// when the change was derived without the surrounding file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_start: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_start: Option<usize>,
    /// The text after a patch's hunk header, naming where the hunk sits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileChange {
    pub path: String,
    pub op: Op,
    /// The path a renamed file had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Lines added and removed over the whole change, shown or not.
    pub added: usize,
    pub removed: usize,
    /// The content is not text; there are no hunks.
    #[serde(default)]
    pub binary: bool,
    pub hunks: Vec<Hunk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<Truncated>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeSet {
    pub basis: Basis,
    pub files: Vec<FileChange>,
    /// Files of the change that are not listed at all.
    #[serde(default)]
    pub omitted_files: usize,
}

/// A parsed or computed file change before sanitising and caps.
pub(crate) struct RawFile {
    /// Already sanitised and checked.
    pub path: String,
    pub op: Op,
    pub from: Option<String>,
    pub binary: bool,
    pub hunks: Vec<RawHunk>,
}

pub(crate) struct RawHunk {
    pub old_start: Option<usize>,
    pub new_start: Option<usize>,
    pub section: Option<String>,
    pub lines: Vec<String>,
}

impl ChangeSet {
    /// The change set of unified diff `text`, or `None` when the text is not
    /// a well-formed patch, is longer than [`MAX_PATCH_BYTES`], or names an
    /// absolute path or one with a `..` segment.
    pub fn from_patch(text: &str, basis: Basis, caps: &Caps) -> Option<Self> {
        Some(bound(patch::parse(text)?, basis, caps))
    }

    /// The change that turns `before` into `after` at `path`, where both are
    /// fragments of a file: the hunks carry no line numbers. `None` when the
    /// path is unsafe to show or either text is longer than
    /// [`MAX_EDIT_BYTES`].
    pub fn from_edit(
        path: &str,
        before: &str,
        after: &str,
        basis: Basis,
        caps: &Caps,
    ) -> Option<Self> {
        Self::diffed(path, Op::Modify, before, after, false, basis, caps)
    }

    /// The change between two whole file contents at `path`; an absent side
    /// means the file did not exist there. Hunks carry line numbers. `None`
    /// when the path is unsafe to show, both sides are absent, or either text
    /// is longer than [`MAX_EDIT_BYTES`].
    pub fn from_contents(
        path: &str,
        before: Option<&str>,
        after: Option<&str>,
        basis: Basis,
        caps: &Caps,
    ) -> Option<Self> {
        let op = match (before, after) {
            (None, None) => return None,
            (None, Some(_)) => Op::Add,
            (Some(_), None) => Op::Delete,
            (Some(_), Some(_)) => Op::Modify,
        };
        Self::diffed(
            path,
            op,
            before.unwrap_or_default(),
            after.unwrap_or_default(),
            true,
            basis,
            caps,
        )
    }

    /// One file whose content cannot be shown: it is binary, or too large to
    /// read (`truncated` then says so, with the sizes). `None` when the path
    /// is unsafe to show.
    pub fn opaque(
        path: &str,
        op: Op,
        basis: Basis,
        binary: bool,
        truncated: Option<Truncated>,
    ) -> Option<Self> {
        Some(Self {
            basis,
            files: vec![FileChange {
                path: clean_path(path)?,
                op,
                from: None,
                added: 0,
                removed: 0,
                binary,
                hunks: Vec::new(),
                truncated,
            }],
            omitted_files: 0,
        })
    }

    fn diffed(
        path: &str,
        op: Op,
        before: &str,
        after: &str,
        numbered: bool,
        basis: Basis,
        caps: &Caps,
    ) -> Option<Self> {
        if before.len() > MAX_EDIT_BYTES || after.len() > MAX_EDIT_BYTES {
            return None;
        }
        let file = RawFile {
            path: clean_path(path)?,
            op,
            from: None,
            binary: false,
            hunks: diff::diff_lines(before, after, numbered, MAX_EDIT_DISTANCE),
        };
        Some(bound(vec![file], basis, caps))
    }

    /// Drops every hunk, keeping each file's path, operation and counts, and
    /// records `reason` with the number of lines no longer shown.
    pub fn degrade(&mut self, reason: TruncatedReason) {
        for file in &mut self.files {
            let shown: usize = file.hunks.iter().map(|hunk| hunk.lines.len()).sum();
            if shown == 0 {
                continue;
            }
            file.hunks.clear();
            let already = file.truncated.as_ref().map_or(0, |cut| cut.omitted_lines);
            file.truncated = Some(Truncated {
                reason,
                omitted_lines: already + shown,
                old_bytes: None,
                new_bytes: None,
            });
        }
    }

    /// Diff lines shown over all files.
    pub fn shown_lines(&self) -> usize {
        self.files.iter().map(FileChange::shown_lines).sum()
    }
}

impl FileChange {
    /// Diff lines shown for this file.
    pub fn shown_lines(&self) -> usize {
        self.hunks.iter().map(|hunk| hunk.lines.len()).sum()
    }

    /// The file's change as unified diff text, built from what is shown: the
    /// bounded, sanitised lines, not the file. A hunk without line numbers
    /// gets a bare `@@` header.
    pub fn unified(&self) -> String {
        let old = match self.op {
            Op::Add => "/dev/null".to_string(),
            _ => format!("a/{}", self.from.as_deref().unwrap_or(&self.path)),
        };
        let new = match self.op {
            Op::Delete => "/dev/null".to_string(),
            _ => format!("b/{}", self.path),
        };
        let mut out = format!("--- {old}\n+++ {new}\n");
        for hunk in &self.hunks {
            match (hunk.old_start, hunk.new_start) {
                (Some(old_start), Some(new_start)) => {
                    let count = |marks: [char; 2]| {
                        hunk.lines
                            .iter()
                            .filter(|line| line.starts_with(marks))
                            .count()
                    };
                    out.push_str(&format!(
                        "@@ -{old_start},{} +{new_start},{} @@",
                        count([' ', '-']),
                        count([' ', '+'])
                    ));
                }
                _ => out.push_str("@@"),
            }
            if let Some(section) = &hunk.section {
                out.push(' ');
                out.push_str(section);
            }
            out.push('\n');
            for line in &hunk.lines {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }
}

fn json_len<T: Serialize>(value: &T) -> usize {
    // Serialising these plain data shapes cannot fail; a failure would only
    // make the value count as too large to admit.
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}

/// One diff line with its text sanitised and cut; the mark is kept as is.
fn bounded_line(raw: &str, line_chars: usize) -> String {
    let mut chars = raw.chars();
    match chars.next() {
        Some(mark @ (' ' | '-' | '+')) => {
            let mut line = String::from(mark);
            line.push_str(&clean_line(chars.as_str(), line_chars));
            line
        }
        _ => clean_line(raw, line_chars),
    }
}

/// Sanitises and caps raw files into a change set.
///
/// Cuts apply in order: whole files past the file cap, then a file's lines
/// past the per-file cap, then lines past the total cap, and throughout, the
/// byte ceiling. Every cut is recorded on the file or in `omitted_files`, and
/// the added and removed counts always describe the whole change.
fn bound(files: Vec<RawFile>, basis: Basis, caps: &Caps) -> ChangeSet {
    let caps = caps.clamped();
    let mut budget = MAX_BLOCK_BYTES - ENVELOPE_RESERVE;
    let mut total_left = caps.total_lines;
    let mut full = false;
    let mut out: Vec<FileChange> = Vec::new();
    let mut omitted_files = 0usize;
    for raw in files {
        if full || out.len() >= caps.files {
            omitted_files += 1;
            continue;
        }
        let count = |mark: char| {
            raw.hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .filter(|line| line.starts_with(mark))
                .count()
        };
        let mut file = FileChange {
            added: count('+'),
            removed: count('-'),
            path: raw.path,
            op: raw.op,
            from: raw.from,
            binary: raw.binary,
            hunks: Vec::new(),
            truncated: None,
        };
        let header = json_len(&file) + TRUNCATED_RESERVE;
        if header > budget {
            full = true;
            omitted_files += 1;
            continue;
        }
        budget -= header;
        let mut file_left = caps.lines_per_file;
        let mut omitted = 0usize;
        let mut reason: Option<TruncatedReason> = None;
        for raw_hunk in raw.hunks {
            let mut hunk = Hunk {
                old_start: raw_hunk.old_start,
                new_start: raw_hunk.new_start,
                section: raw_hunk
                    .section
                    .map(|text| clean_line(&text, caps.line_chars))
                    .filter(|text| !text.trim().is_empty()),
                lines: Vec::new(),
            };
            let opening = json_len(&hunk) + 1;
            for raw_line in &raw_hunk.lines {
                if reason.is_none() {
                    if file_left == 0 {
                        reason = Some(TruncatedReason::Lines);
                    } else if total_left == 0 {
                        reason = Some(TruncatedReason::Total);
                    } else {
                        let line = bounded_line(raw_line, caps.line_chars);
                        let cost =
                            json_len(&line) + 1 + if hunk.lines.is_empty() { opening } else { 0 };
                        if cost > budget {
                            reason = Some(TruncatedReason::Bytes);
                        } else {
                            budget -= cost;
                            file_left -= 1;
                            total_left -= 1;
                            hunk.lines.push(line);
                            continue;
                        }
                    }
                }
                omitted += 1;
            }
            if !hunk.lines.is_empty() {
                file.hunks.push(hunk);
            }
        }
        if let Some(reason) = reason {
            file.truncated = Some(Truncated {
                reason,
                omitted_lines: omitted,
                old_bytes: None,
                new_bytes: None,
            });
        }
        out.push(file);
    }
    ChangeSet {
        basis,
        files: out,
        omitted_files,
    }
}
