const THREAD_TAIL_COMMAND: &str = "ryeos thread tail";

pub(crate) fn tail_command(thread_id: &str) -> String {
    format!("{THREAD_TAIL_COMMAND} {thread_id}")
}

pub(crate) fn watch_progress_hint(thread_id: &str) -> String {
    format!("run `{}` to watch progress", tail_command(thread_id))
}

pub(crate) fn child_diagnostic(summary: &str, thread_id: &str) -> String {
    format!(
        "{summary}; full child diagnostic: `{}`",
        tail_command(thread_id)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_tail_instructions_share_the_canonical_command() {
        assert_eq!(tail_command("T-child"), "ryeos thread tail T-child");
        assert_eq!(
            watch_progress_hint("T-child"),
            "run `ryeos thread tail T-child` to watch progress"
        );
        assert_eq!(
            child_diagnostic("child failed", "T-child"),
            "child failed; full child diagnostic: `ryeos thread tail T-child`"
        );
    }
}
