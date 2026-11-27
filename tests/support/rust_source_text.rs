//! The production text of a Rust source file, for the source guards.
//!
//! Included by path (`#[path = ".../tests/support/rust_source_text.rs"] mod
//! rust_source_text;`) from every guard that scans source text, so they all
//! cut test code the same way instead of each guessing at it.

/// `source` with every `#[cfg(test)]` item blanked: a `;`-terminated item
/// (`mod x;`, `use ..;`, `const ..;`) ends at its `;`, a braced item
/// (`mod tests { .. }`, `fn f() { .. }`) ends where its braces balance, and
/// attributes between the `#[cfg(test)]` line and the item belong to it. Every
/// blanked line is kept as an empty line, so line numbers in the result are the
/// line numbers in `source`. A `#[cfg(test)]` that is not alone on its line
/// (`#[cfg(all(test, ..))]`, a trailing attribute) is not recognised and its
/// item stays; a guard that depends on one must not pass silently, so it pairs
/// this with a floor on how many files lose text (see the callers guard).
#[allow(dead_code)]
pub fn production_text(source: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let mut blanked = vec![false; lines.len()];
    let mut index = 0;
    while index < lines.len() {
        if lines[index].trim() != "#[cfg(test)]" {
            index += 1;
            continue;
        }
        let start = index;
        let mut depth = 0_i64;
        let mut opened = false;
        let mut end = index;
        'item: for (offset, line) in lines[index + 1..].iter().enumerate() {
            end = index + 1 + offset;
            if !opened && line.trim_start().starts_with("#[") {
                continue;
            }
            for character in line.chars() {
                match character {
                    '{' => {
                        opened = true;
                        depth += 1;
                    }
                    '}' => depth -= 1,
                    ';' if !opened => break 'item,
                    _ => {}
                }
                if opened && depth == 0 {
                    break 'item;
                }
            }
        }
        for flag in &mut blanked[start..=end] {
            *flag = true;
        }
        index = end + 1;
    }
    let mut out = String::with_capacity(source.len());
    for (line, blank) in lines.iter().zip(&blanked) {
        if !blank {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::production_text;

    #[test]
    fn a_braced_test_module_and_a_declaration_and_a_fn_are_blanked_with_their_lines_kept() {
        let source = "fn a() {}\n#[cfg(test)]\nmod x;\nfn b() {}\n#[cfg(test)]\n#[allow(dead_code)]\nfn t() {\n    let s = 1;\n}\nfn c() {}\n#[cfg(test)]\nmod tests {\n    fn d() {}\n}\nfn e() {}\n";
        let text = production_text(source);
        assert_eq!(text.lines().count(), source.lines().count());
        assert_eq!(
            text.lines().collect::<Vec<_>>(),
            vec![
                "fn a() {}",
                "",
                "",
                "fn b() {}",
                "",
                "",
                "",
                "",
                "",
                "fn c() {}",
                "",
                "",
                "",
                "",
                "fn e() {}"
            ]
        );
    }

    #[test]
    fn production_code_after_a_test_module_stays() {
        let text = production_text("#[cfg(test)]\nmod tests {\n}\nfn after() {}\n");
        assert!(text.contains("fn after() {}"));
    }

    #[test]
    fn a_file_without_test_items_is_unchanged() {
        let source = "fn a() {}\nfn b() {}\n";
        assert_eq!(production_text(source), source);
    }
}
