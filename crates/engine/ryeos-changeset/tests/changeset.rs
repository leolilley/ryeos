//! The change set built from patches, edits and whole contents: what parses,
//! what is refused, and what hostile text turns into.

use ryeos_changeset::{
    Basis, Caps, ChangeSet, FileChange, MAX_BLOCK_BYTES, MAX_EDIT_BYTES, MAX_PATCH_BYTES, Op,
    TruncatedReason,
};
use serde_json::{Value, json};

fn caps() -> Caps {
    Caps::default()
}

fn wide() -> Caps {
    Caps {
        files: 200,
        lines_per_file: 10_000,
        total_lines: 10_000,
        line_chars: 2_000,
    }
}

fn patch(text: &str) -> Option<Value> {
    ChangeSet::from_patch(text, Basis::Requested, &caps())
        .map(|set| serde_json::to_value(set).unwrap())
}

const MODIFY: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -10,3 +10,3 @@ fn main() {
 fn main() {
-    old();
+    new();
 }
";

const ADD: &str = "\
diff --git a/new.txt b/new.txt
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+one
+two
";

const DELETE: &str = "\
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 3333333..0000000
--- a/gone.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-one
-two
";

const RENAME: &str = "\
diff --git a/old/name.rs b/new/name.rs
similarity index 100%
rename from old/name.rs
rename to new/name.rs
";

fn modify_file() -> Value {
    json!({"path": "src/main.rs", "op": "modify", "added": 1, "removed": 1, "binary": false,
           "hunks": [{"old_start": 10, "new_start": 10, "section": "fn main() {",
                      "lines": [" fn main() {", "-    old();", "+    new();", " }"]}]})
}

fn add_file() -> Value {
    json!({"path": "new.txt", "op": "add", "added": 2, "removed": 0, "binary": false,
           "hunks": [{"old_start": 0, "new_start": 1, "lines": ["+one", "+two"]}]})
}

fn delete_file() -> Value {
    json!({"path": "gone.txt", "op": "delete", "added": 0, "removed": 2, "binary": false,
           "hunks": [{"old_start": 1, "new_start": 0, "lines": ["-one", "-two"]}]})
}

fn rename_file() -> Value {
    json!({"path": "new/name.rs", "op": "rename", "from": "old/name.rs",
           "added": 0, "removed": 0, "binary": false, "hunks": []})
}

fn set_of(files: Vec<Value>) -> Value {
    json!({"basis": "requested", "files": files, "omitted_files": 0})
}

#[test]
fn a_modification_parses_with_its_numbers_and_section() {
    assert_eq!(patch(MODIFY), Some(set_of(vec![modify_file()])));
}

#[test]
fn an_added_file_parses() {
    assert_eq!(patch(ADD), Some(set_of(vec![add_file()])));
}

#[test]
fn a_deleted_file_parses() {
    assert_eq!(patch(DELETE), Some(set_of(vec![delete_file()])));
}

#[test]
fn a_pure_rename_parses_with_its_source() {
    assert_eq!(patch(RENAME), Some(set_of(vec![rename_file()])));
}

#[test]
fn a_rename_with_changes_keeps_both_paths() {
    let text = "\
diff --git a/a.rs b/b.rs
similarity index 90%
rename from a.rs
rename to b.rs
--- a/a.rs
+++ b/b.rs
@@ -1 +1 @@
-x
+y
";
    assert_eq!(
        patch(text),
        Some(set_of(vec![json!({
            "path": "b.rs", "op": "rename", "from": "a.rs", "added": 1, "removed": 1,
            "binary": false,
            "hunks": [{"old_start": 1, "new_start": 1, "lines": ["-x", "+y"]}]})]))
    );
}

#[test]
fn several_files_parse_in_order() {
    let text = format!("{MODIFY}{ADD}{DELETE}{RENAME}");
    assert_eq!(
        patch(&text),
        Some(set_of(vec![
            modify_file(),
            add_file(),
            delete_file(),
            rename_file()
        ]))
    );
}

#[test]
fn a_plain_unified_diff_without_a_git_header_parses() {
    let text = "--- a/f.txt\t2026-01-01 00:00:00\n+++ b/f.txt\t2026-01-02 00:00:00\n@@ -1,2 +1,2 @@\n a\n-b\n+c\n";
    assert_eq!(
        patch(text),
        Some(set_of(vec![json!({
            "path": "f.txt", "op": "modify", "added": 1, "removed": 1, "binary": false,
            "hunks": [{"old_start": 1, "new_start": 1, "lines": [" a", "-b", "+c"]}]})]))
    );
}

#[test]
fn a_no_newline_note_is_kept_as_a_line() {
    let text = "\
--- a/f
+++ b/f
@@ -1 +1 @@
-old
\\ No newline at end of file
+new
\\ No newline at end of file
";
    assert_eq!(
        patch(text),
        Some(set_of(vec![json!({
            "path": "f", "op": "modify", "added": 1, "removed": 1, "binary": false,
            "hunks": [{"old_start": 1, "new_start": 1, "lines": [
                "-old", "\\ No newline at end of file",
                "+new", "\\ No newline at end of file"]}]})]))
    );
}

#[test]
fn a_binary_file_parses_with_no_hunks() {
    let text = "\
diff --git a/img.png b/img.png
index 1111111..2222222 100644
Binary files a/img.png and b/img.png differ
";
    assert_eq!(
        patch(text),
        Some(set_of(vec![json!({
            "path": "img.png", "op": "modify", "added": 0, "removed": 0, "binary": true,
            "hunks": []})]))
    );
    let encoded = "\
diff --git a/img.png b/img.png
new file mode 100644
index 0000000..2222222
GIT binary patch
literal 12
Tcmb2|=3oE;Vqj!s

literal 0
HcmV?d00001

";
    let set = patch(&format!("{encoded}{MODIFY}")).unwrap();
    assert_eq!(set["files"][0]["binary"], json!(true));
    assert_eq!(set["files"][0]["op"], json!("add"));
    assert_eq!(set["files"][1], modify_file());
}

#[test]
fn carriage_returns_in_a_patch_do_not_break_parsing() {
    let text = MODIFY.replace('\n', "\r\n");
    assert_eq!(patch(&text), Some(set_of(vec![modify_file()])));
}

#[test]
fn malformed_text_gives_no_change_set() {
    let cases: &[(&str, &str)] = &[
        ("prose", "this is not a patch\n"),
        ("empty", ""),
        ("blank", "\n\n"),
        (
            "another patch format",
            "*** Begin Patch\n*** Update File: a\n-x\n+y\n*** End Patch\n",
        ),
        ("header without hunks", "--- a/f\n+++ b/f\n"),
        ("old header alone", "--- a/f\n@@ -1 +1 @@\n-x\n+y\n"),
        (
            "too few lines for the count",
            "--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n-b\n+c\n",
        ),
        (
            "too many lines for the count",
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-b\n+c\n+d\n",
        ),
        (
            "unknown mark",
            "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n a\n*b\n",
        ),
        (
            "text after the last hunk",
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-b\n+c\ntrailing words\n",
        ),
        (
            "bad hunk header",
            "--- a/f\n+++ b/f\n@@ -x,1 +1 @@\n-b\n+c\n",
        ),
        (
            "negative count",
            "--- a/f\n+++ b/f\n@@ -1,-1 +1 @@\n-b\n+c\n",
        ),
        (
            "quoted path",
            "--- \"a/f\"\n+++ \"b/f\"\n@@ -1 +1 @@\n-b\n+c\n",
        ),
        (
            "both sides absent",
            "--- /dev/null\n+++ /dev/null\n@@ -1 +1 @@\n-b\n+c\n",
        ),
        ("git header alone", "diff --git a/f b/f\n"),
        (
            "a good file then a bad one",
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-b\n+c\n--- a/g\n+++ b/g\n@@ nonsense @@\n",
        ),
    ];
    for (name, text) in cases {
        assert_eq!(patch(text), None, "{name}");
    }
}

#[test]
fn an_unsafe_path_gives_no_change_set() {
    for path in ["../secret", "a/../../secret", "sub\\..\\..\\secret"] {
        let text = format!("--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-b\n+c\n");
        assert_eq!(patch(&text), None, "{path}");
        assert_eq!(
            ChangeSet::from_edit(path, "a\n", "b\n", Basis::Requested, &caps()),
            None,
            "{path}"
        );
    }
    for path in [
        "/etc/passwd",
        "C:\\Windows\\system.ini",
        "\\\\host\\share\\f",
    ] {
        let text = format!("--- {path}\n+++ {path}\n@@ -1 +1 @@\n-b\n+c\n");
        assert_eq!(patch(&text), None, "{path}");
        assert_eq!(
            ChangeSet::from_contents(path, Some("a\n"), Some("b\n"), Basis::Recorded, &caps()),
            None,
            "{path}"
        );
        assert_eq!(
            ChangeSet::opaque(path, Op::Modify, Basis::Recorded, true, None),
            None,
            "{path}"
        );
    }
    // A rename may not smuggle one in as its source either.
    let text = "diff --git a/x b/y\nrename from ../x\nrename to y\n";
    assert_eq!(patch(text), None);
    // Hidden characters cannot disguise a parent segment.
    let text = "--- a/.\u{202e}./x\n+++ b/.\u{202e}./x\n@@ -1 +1 @@\n-b\n+c\n";
    assert_eq!(patch(text), None);
}

#[test]
fn escapes_bidi_controls_and_nul_are_stripped_from_lines_paths_and_sections() {
    let text = "--- a/s\u{1b}[31mrc/m\u{202e}ain.rs\n+++ b/s\u{1b}[31mrc/m\u{202e}ain.rs\n\
@@ -1,2 +1,2 @@ fn \u{1b}]0;owned\u{07}main\u{2066}()\n \
keep\u{0}\tme\n-\u{1b}[2Jold\u{202e}\n+\u{1b}[1;31mnew\u{1b}[0m\u{9b}\r\n";
    let set = ChangeSet::from_patch(text, Basis::Requested, &caps()).unwrap();
    let file = &set.files[0];
    assert_eq!(file.path, "src/main.rs");
    assert_eq!(file.hunks[0].section.as_deref(), Some("fn main()"));
    assert_eq!(file.hunks[0].lines, [" keep\tme", "-old", "+new"]);
    let drawn = serde_json::to_string(&set).unwrap();
    for hostile in [
        "\\u001b", "\\u0000", "\u{202e}", "\u{2066}", "\u{9b}", "\\r",
    ] {
        assert!(!drawn.contains(hostile), "{hostile:?} survived");
    }
}

#[test]
fn the_mark_of_a_line_is_never_taken_from_its_text() {
    // Text that tries to pass for another mark, or to erase its own.
    let text = "--- a/f\n+++ b/f\n@@ -1 +1,2 @@\n-\u{8}+not added\n+\u{1b}[1D-not removed\n+ \n";
    let set = ChangeSet::from_patch(text, Basis::Requested, &caps()).unwrap();
    let file = &set.files[0];
    assert_eq!(file.hunks[0].lines, ["-+not added", "+-not removed", "+ "]);
    assert_eq!((file.added, file.removed), (2, 1));
}

#[test]
fn a_megabyte_line_is_cut_and_the_block_stays_small() {
    let long = "x".repeat(1024 * 1024);
    let text = format!("--- a/f\n+++ b/f\n@@ -1 +1 @@\n-short\n+{long}\n");
    let set = ChangeSet::from_patch(&text, Basis::Requested, &caps()).unwrap();
    let line = &set.files[0].hunks[0].lines[1];
    assert_eq!(line.chars().count(), 1 + 400 + 1);
    assert!(line.starts_with("+xxx") && line.ends_with('…'));
    assert_eq!(set.files[0].added, 1);
    assert!(serde_json::to_vec(&set).unwrap().len() < 2_048);

    let edited = ChangeSet::from_edit(
        "f",
        "short\n",
        &long[..MAX_EDIT_BYTES],
        Basis::Requested,
        &caps(),
    )
    .unwrap();
    assert!(serde_json::to_vec(&edited).unwrap().len() < 2_048);
}

#[test]
fn input_past_the_ceilings_is_not_read() {
    let filler = format!("+{}\n", "y".repeat(1023));
    let count = MAX_PATCH_BYTES / filler.len() + 1;
    let text = format!(
        "--- /dev/null\n+++ b/f\n@@ -0,0 +1,{count} @@\n{}",
        filler.repeat(count)
    );
    assert!(text.len() > MAX_PATCH_BYTES);
    assert_eq!(patch(&text), None);

    let big = "z".repeat(MAX_EDIT_BYTES + 1);
    assert_eq!(
        ChangeSet::from_edit("f", &big, "", Basis::Requested, &caps()),
        None
    );
    assert_eq!(
        ChangeSet::from_contents("f", Some(""), Some(&big), Basis::Recorded, &caps()),
        None
    );
}

fn added_file(path: &str, lines: usize) -> String {
    let mut text = format!("--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{lines} @@\n");
    for index in 0..lines {
        text.push_str(&format!("+line {index}\n"));
    }
    text
}

#[test]
fn cuts_are_recorded_and_counts_stay_whole_change_totals() {
    let text = [
        added_file("a", 10),
        added_file("b", 10),
        added_file("c", 10),
    ]
    .concat();
    let caps = Caps {
        files: 2,
        lines_per_file: 6,
        total_lines: 8,
        line_chars: 400,
    };
    let set = ChangeSet::from_patch(&text, Basis::Reported, &caps).unwrap();
    assert_eq!(set.basis, Basis::Reported);
    assert_eq!(set.omitted_files, 1);
    assert_eq!(set.files.len(), 2);

    let first = &set.files[0];
    assert_eq!((first.added, first.removed), (10, 0));
    assert_eq!(first.shown_lines(), 6);
    let cut = first.truncated.as_ref().unwrap();
    assert_eq!((cut.reason, cut.omitted_lines), (TruncatedReason::Lines, 4));

    let second = &set.files[1];
    assert_eq!((second.added, second.removed), (10, 0));
    assert_eq!(second.shown_lines(), 2);
    let cut = second.truncated.as_ref().unwrap();
    assert_eq!((cut.reason, cut.omitted_lines), (TruncatedReason::Total, 8));
    assert_eq!(set.shown_lines(), 8);
}

#[test]
fn an_uncut_file_has_no_truncation_record() {
    let set = ChangeSet::from_patch(&added_file("a", 10), Basis::Requested, &caps()).unwrap();
    assert_eq!(set.files[0].truncated, None);
    assert_eq!(set.omitted_files, 0);
}

#[test]
fn the_byte_ceiling_holds_whatever_the_caps_say() {
    // Quotes double when serialised, the worst case for the byte count.
    let quotes = "\"".repeat(300);
    let mut text = String::from("--- /dev/null\n+++ b/f\n@@ -0,0 +1,5000 @@\n");
    for _ in 0..5_000 {
        text.push_str(&format!("+{quotes}\n"));
    }
    let huge = Caps {
        files: usize::MAX,
        lines_per_file: usize::MAX,
        total_lines: usize::MAX,
        line_chars: usize::MAX,
    };
    let set = ChangeSet::from_patch(&text, Basis::Requested, &huge).unwrap();
    assert!(serde_json::to_vec(&set).unwrap().len() <= MAX_BLOCK_BYTES);
    let file = &set.files[0];
    let cut = file.truncated.as_ref().unwrap();
    assert_eq!(cut.reason, TruncatedReason::Bytes);
    assert_eq!(cut.omitted_lines + file.shown_lines(), 5_000);
    assert_eq!(file.added, 5_000);
    assert!(file.shown_lines() > 50);

    // Many files with long paths: whole files are left out, and counted.
    let mut text = String::new();
    for index in 0..150 {
        text.push_str(&added_file(&format!("{}/{index}", "d".repeat(400)), 1));
    }
    let set = ChangeSet::from_patch(&text, Basis::Requested, &huge).unwrap();
    assert!(serde_json::to_vec(&set).unwrap().len() <= MAX_BLOCK_BYTES);
    assert!(set.omitted_files > 0);
    assert_eq!(set.files.len() + set.omitted_files, 150);
}

#[test]
fn degrading_keeps_counts_and_says_why() {
    let text = [MODIFY, RENAME].concat();
    let mut set = ChangeSet::from_patch(&text, Basis::Requested, &caps()).unwrap();
    set.degrade(TruncatedReason::Window);
    let file = &set.files[0];
    assert!(file.hunks.is_empty());
    assert_eq!((file.added, file.removed), (1, 1));
    let cut = file.truncated.as_ref().unwrap();
    assert_eq!(
        (cut.reason, cut.omitted_lines),
        (TruncatedReason::Window, 4)
    );
    // A file that showed nothing has nothing to record.
    assert_eq!(set.files[1].truncated, None);
    assert_eq!(set.shown_lines(), 0);
}

/// Applies a numbered file change to `before`.
fn apply(before: &str, file: &FileChange) -> String {
    let old: Vec<&str> = before.split_inclusive('\n').collect();
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut at = 0usize;
    for hunk in &file.hunks {
        let has_old = hunk.lines.iter().any(|line| line.starts_with([' ', '-']));
        let start = hunk.old_start.unwrap() - usize::from(has_old);
        while at < start {
            out.push((
                old[at].trim_end_matches('\n').to_string(),
                old[at].ends_with('\n'),
            ));
            at += 1;
        }
        let mut last_was_new_side = false;
        for line in &hunk.lines {
            let (mark, text) = line.split_at(1);
            match mark {
                " " => {
                    assert_eq!(old[at].trim_end_matches('\n'), text);
                    out.push((text.to_string(), true));
                    at += 1;
                    last_was_new_side = true;
                }
                "-" => {
                    assert_eq!(old[at].trim_end_matches('\n'), text);
                    at += 1;
                    last_was_new_side = false;
                }
                "+" => {
                    out.push((text.to_string(), true));
                    last_was_new_side = true;
                }
                "\\" => {
                    if last_was_new_side {
                        out.last_mut().unwrap().1 = false;
                    }
                }
                other => panic!("unknown mark {other:?}"),
            }
        }
    }
    while at < old.len() {
        out.push((
            old[at].trim_end_matches('\n').to_string(),
            old[at].ends_with('\n'),
        ));
        at += 1;
    }
    out.into_iter()
        .map(|(text, terminated)| if terminated { text + "\n" } else { text })
        .collect()
}

/// A small deterministic generator, so the property runs the same every time.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, below: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % below
    }

    fn text(&mut self, lines: u64, alphabet: u64) -> String {
        let mut text = String::new();
        for _ in 0..self.next(lines + 1) {
            text.push_str(&format!("line {}\n", self.next(alphabet)));
        }
        if self.next(3) == 0 {
            text.pop();
        }
        text
    }
}

#[test]
fn applying_the_diff_to_before_reproduces_after() {
    let mut random = Lcg(7);
    for case in 0..400 {
        let before = random.text(40, 6);
        let after = if case % 4 == 0 {
            random.text(40, 6)
        } else {
            // A few local edits of `before`.
            let mut lines: Vec<String> = before.split_inclusive('\n').map(String::from).collect();
            for _ in 0..random.next(4) {
                let at = random.next(lines.len() as u64 + 1) as usize;
                if random.next(2) == 0 || at >= lines.len() {
                    lines.insert(at.min(lines.len()), format!("new {}\n", random.next(9)));
                } else {
                    lines.remove(at);
                }
            }
            lines.concat()
        };
        let set =
            ChangeSet::from_contents("f", Some(&before), Some(&after), Basis::Recorded, &wide())
                .unwrap();
        let file = &set.files[0];
        assert_eq!(
            apply(&before, file),
            after,
            "case {case}: {before:?} to {after:?}"
        );
        if before == after {
            assert!(file.hunks.is_empty(), "case {case}");
        }
        // The shown text parses back to the same file change.
        if !file.hunks.is_empty() {
            let again = ChangeSet::from_patch(&file.unified(), Basis::Recorded, &wide()).unwrap();
            assert_eq!(&again.files[0], file, "case {case}");
        }
    }
}

#[test]
fn a_change_past_the_work_cap_is_one_whole_replacement_and_still_applies() {
    let before: String = (0..1_200).map(|index| format!("a{index}\n")).collect();
    let after: String = (0..1_200).map(|index| format!("b{index}\n")).collect();
    let both = format!("head\n{before}tail\n");
    let then = format!("head\n{after}tail\n");
    let set =
        ChangeSet::from_contents("f", Some(&both), Some(&then), Basis::Recorded, &wide()).unwrap();
    let file = &set.files[0];
    assert_eq!((file.added, file.removed), (1_200, 1_200));
    assert_eq!(file.hunks.len(), 1);
    let lines = &file.hunks[0].lines;
    assert_eq!(lines.len(), 2_402);
    assert_eq!(lines[0], " head");
    assert!(lines[1..1_201].iter().all(|line| line.starts_with('-')));
    assert!(lines[1_201..2_401].iter().all(|line| line.starts_with('+')));
    assert_eq!(lines[2_401], " tail");
    assert_eq!(apply(&both, file), then);
}

#[test]
fn an_edit_has_context_and_no_line_numbers() {
    let before = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\n";
    let after = "one\ntwo\nthree\nfour\nFIVE\nsix\nseven\neight\nnine\n";
    let set = ChangeSet::from_edit("src/lib.rs", before, after, Basis::Requested, &caps()).unwrap();
    assert_eq!(
        serde_json::to_value(&set).unwrap(),
        json!({"basis": "requested", "omitted_files": 0, "files": [{
            "path": "src/lib.rs", "op": "modify", "added": 1, "removed": 1, "binary": false,
            "hunks": [{"lines": [" two", " three", " four", "-five", "+FIVE",
                                 " six", " seven", " eight"]}]}]})
    );
    assert_eq!(
        set.files[0].unified(),
        "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@\n two\n three\n four\n-five\n+FIVE\n six\n seven\n eight\n"
    );
}

#[test]
fn whole_contents_give_numbered_hunks_and_the_operation() {
    let before = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\n";
    let after = "one\ntwo\nthree\nfour\nFIVE\nsix\nseven\neight\nnine";
    let set =
        ChangeSet::from_contents("f", Some(before), Some(after), Basis::Recorded, &caps()).unwrap();
    assert_eq!(
        serde_json::to_value(&set.files[0].hunks).unwrap(),
        json!([{"old_start": 2, "new_start": 2, "lines": [
            " two", " three", " four", "-five", "+FIVE", " six", " seven", " eight",
            "-nine", "+nine", "\\ No newline at end of file"]}])
    );
    let added = ChangeSet::from_contents("f", None, Some("a\nb\n"), Basis::Recorded, &caps());
    let added = added.unwrap();
    assert_eq!(added.files[0].op, Op::Add);
    assert_eq!(
        serde_json::to_value(&added.files[0].hunks).unwrap(),
        json!([{"old_start": 0, "new_start": 1, "lines": ["+a", "+b"]}])
    );
    let deleted = ChangeSet::from_contents("f", Some("a\n"), None, Basis::Recorded, &caps());
    assert_eq!(deleted.unwrap().files[0].op, Op::Delete);
    assert_eq!(
        ChangeSet::from_contents("f", None, None, Basis::Recorded, &caps()),
        None
    );
}

#[test]
fn the_shape_refuses_fields_it_does_not_know() {
    let good = json!({"basis": "recorded", "files": [{"path": "f", "op": "add", "added": 0,
        "removed": 0, "hunks": []}]});
    assert!(serde_json::from_value::<ChangeSet>(good.clone()).is_ok());
    let mut extra = good.clone();
    extra["files"][0]["link"] = json!("https://example.com");
    assert!(serde_json::from_value::<ChangeSet>(extra).is_err());
    let mut op = good;
    op["files"][0]["op"] = json!("chmod");
    assert!(serde_json::from_value::<ChangeSet>(op).is_err());
}
