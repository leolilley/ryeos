//! Strict parser for unified diff text.
//!
//! The parser accepts what `diff -u` and `git diff` write and nothing else.
//! Text it cannot account for line by line yields `None`: a reader is shown a
//! whole patch or none, never a guess at part of one.

use crate::clean::clean_path;
use crate::{Op, RawFile, RawHunk};

/// Patch text longer than this is not parsed.
pub const MAX_PATCH_BYTES: usize = 4 * 1024 * 1024;

/// One side of a file header.
enum Side {
    /// `/dev/null`: the file does not exist on this side.
    Absent,
    Path(String),
}

/// Parses a whole patch into its files, or `None` when any line of it is not
/// part of a well-formed unified diff or any path is unsafe to show.
pub(crate) fn parse(text: &str) -> Option<Vec<RawFile>> {
    if text.len() > MAX_PATCH_BYTES {
        return None;
    }
    // A final newline ends the last line; it does not start an empty one.
    let text = text.strip_suffix('\n').unwrap_or(text);
    let lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let mut files = Vec::new();
    let mut at = 0usize;
    while at < lines.len() {
        let line = lines[at];
        if line.is_empty() {
            at += 1;
        } else if line.starts_with("diff ") || line.starts_with("--- ") {
            files.push(file(&lines, &mut at)?);
        } else {
            return None;
        }
    }
    (!files.is_empty()).then_some(files)
}

/// Parses one file's section starting at `lines[*at]`, leaving `at` after it.
fn file(lines: &[&str], at: &mut usize) -> Option<RawFile> {
    let mut header_paths: Option<(String, String)> = None;
    let (mut added, mut deleted, mut binary, mut mode) = (false, false, false, false);
    let (mut rename_from, mut rename_to) = (None, None);

    if let Some(rest) = lines[*at].strip_prefix("diff ") {
        if let Some(names) = rest.strip_prefix("--git ") {
            header_paths = git_header_paths(names);
        }
        *at += 1;
        while let Some(line) = lines.get(*at) {
            if line.starts_with("new file mode ") {
                added = true;
            } else if line.starts_with("deleted file mode ") {
                deleted = true;
            } else if let Some(path) = line.strip_prefix("rename from ") {
                rename_from = Some(path.to_string());
            } else if let Some(path) = line.strip_prefix("rename to ") {
                rename_to = Some(path.to_string());
            } else if line.starts_with("Binary files ") && line.ends_with(" differ") {
                binary = true;
            } else if *line == "GIT binary patch" {
                // The encoded payload runs to the next file; none of it is drawn.
                binary = true;
                while lines
                    .get(*at + 1)
                    .is_some_and(|next| !next.starts_with("diff "))
                {
                    *at += 1;
                }
            } else if line.starts_with("old mode ") || line.starts_with("new mode ") {
                mode = true;
            } else if ![
                "index ",
                "similarity index ",
                "dissimilarity index ",
                "copy from ",
                "copy to ",
            ]
            .iter()
            .any(|prefix| line.starts_with(prefix))
            {
                break;
            }
            *at += 1;
        }
    }

    let mut sides: Option<(Side, Side)> = None;
    let mut hunks = Vec::new();
    if let Some(old) = lines.get(*at).and_then(|line| line.strip_prefix("--- ")) {
        let new = lines.get(*at + 1)?.strip_prefix("+++ ")?;
        sides = Some((side(old)?, side(new)?));
        *at += 2;
        while lines.get(*at).is_some_and(|line| line.starts_with("@@ ")) {
            hunks.push(hunk(lines, at)?);
        }
        if hunks.is_empty() {
            return None;
        }
    } else if !(added || deleted || binary || mode || rename_from.is_some()) {
        // A header that changes nothing is not a file change.
        return None;
    }

    let (old, new) = match sides {
        Some((old, new)) => strip_prefixes(old, new),
        None => {
            let (old, new) = match (rename_from.clone(), rename_to.clone()) {
                (Some(from), Some(to)) => (from, to),
                (None, None) => header_paths?,
                _ => return None,
            };
            (Side::Path(old), Side::Path(new))
        }
    };
    let (path, op, from) = match (old, new) {
        (Side::Absent, Side::Path(new)) => (new, Op::Add, None),
        (Side::Path(old), Side::Absent) => (old, Op::Delete, None),
        (Side::Path(old), Side::Path(new)) => {
            if rename_from.is_some() || old != new {
                (new, Op::Rename, Some(rename_from.unwrap_or(old)))
            } else if added {
                (new, Op::Add, None)
            } else if deleted {
                (new, Op::Delete, None)
            } else {
                (new, Op::Modify, None)
            }
        }
        (Side::Absent, Side::Absent) => return None,
    };
    let from = match from {
        Some(from) => Some(clean_path(&from)?),
        None => None,
    };
    Some(RawFile {
        path: clean_path(&path)?,
        op,
        from,
        binary,
        hunks,
    })
}

/// The two paths of `a/<old> b/<new>` after `diff --git`. The split is only
/// unambiguous when both names are the same or ` b/` occurs once; otherwise
/// the header gives no paths and the file needs them from elsewhere.
fn git_header_paths(names: &str) -> Option<(String, String)> {
    let rest = names.strip_prefix("a/")?;
    let mut splits = rest.match_indices(" b/");
    let (index, _) = splits.next()?;
    if splits.next().is_some() {
        let half = rest.len().checked_sub(3)? / 2;
        let (old, new) = (rest.get(..half)?, rest.get(half + 3..)?);
        return (old == new && rest.get(half..half + 3) == Some(" b/"))
            .then(|| (old.to_string(), new.to_string()));
    }
    Some((rest[..index].to_string(), rest[index + 3..].to_string()))
}

/// A `---` or `+++` header's path: the text up to a tab (a timestamp may
/// follow). A quoted path is not accepted.
fn side(text: &str) -> Option<Side> {
    let path = text.split('\t').next().unwrap_or_default().trim_end();
    if path.is_empty() || path.starts_with('"') {
        return None;
    }
    Some(if path == "/dev/null" {
        Side::Absent
    } else {
        Side::Path(path.to_string())
    })
}

/// Drops the conventional `a/` and `b/` prefixes when the header uses them on
/// every side that names a file.
fn strip_prefixes(old: Side, new: Side) -> (Side, Side) {
    let stripped = |side: &Side, prefix: &str| match side {
        Side::Absent => Some(Side::Absent),
        Side::Path(path) => path
            .strip_prefix(prefix)
            .map(|rest| Side::Path(rest.to_string())),
    };
    match (stripped(&old, "a/"), stripped(&new, "b/")) {
        (Some(old), Some(new)) => (old, new),
        _ => (old, new),
    }
}

/// `start[,count]`: digits only; an absent count is one.
fn range(text: &str) -> Option<(usize, usize)> {
    let number = |digits: &str| {
        (!digits.is_empty() && digits.len() <= 9 && digits.bytes().all(|b| b.is_ascii_digit()))
            .then(|| digits.parse::<usize>().ok())
            .flatten()
    };
    match text.split_once(',') {
        Some((start, count)) => Some((number(start)?, number(count)?)),
        None => Some((number(text)?, 1)),
    }
}

/// Parses the hunk whose `@@` header is `lines[*at]`: exactly as many old and
/// new lines as the header counts, each with a known mark.
fn hunk(lines: &[&str], at: &mut usize) -> Option<RawHunk> {
    let rest = lines[*at].strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, rest) = rest.split_once(" @@")?;
    let section = match rest {
        "" => None,
        text => Some(text.strip_prefix(' ')?.to_string()),
    };
    let (old_start, mut old_left) = range(old)?;
    let (new_start, mut new_left) = range(new)?;
    *at += 1;
    let mut body: Vec<String> = Vec::new();
    while old_left > 0 || new_left > 0 {
        let line = *lines.get(*at)?;
        match line.chars().next() {
            // A context line whose trailing blank was trimmed arrives empty.
            None | Some(' ') => {
                old_left = old_left.checked_sub(1)?;
                new_left = new_left.checked_sub(1)?;
                body.push(if line.is_empty() {
                    " ".to_string()
                } else {
                    line.to_string()
                });
            }
            Some('-') => {
                old_left = old_left.checked_sub(1)?;
                body.push(line.to_string());
            }
            Some('+') => {
                new_left = new_left.checked_sub(1)?;
                body.push(line.to_string());
            }
            Some('\\') if !body.is_empty() => body.push(line.to_string()),
            _ => return None,
        }
        *at += 1;
    }
    if !body.is_empty()
        && let Some(line) = lines.get(*at)
        && line.starts_with('\\')
    {
        body.push(line.to_string());
        *at += 1;
    }
    Some(RawHunk {
        old_start: Some(old_start),
        new_start: Some(new_start),
        section: section.filter(|text| !text.trim().is_empty()),
        lines: body,
    })
}
