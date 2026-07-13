use anyhow::{bail, Result};

use crate::kind_profiles::KindProfileRegistry;

pub(super) fn normalize_terminal_status(status: &str) -> Result<&str> {
    match status {
        "completed" | "failed" | "cancelled" | "killed" | "timed_out" | "continued" => Ok(status),
        other => bail!("invalid terminal status: {other}"),
    }
}

pub(super) fn validate_kind(kind: &str, profiles: &KindProfileRegistry) -> Result<()> {
    if profiles.is_valid(kind) {
        Ok(())
    } else {
        bail!("invalid thread kind: {kind}")
    }
}

pub(super) fn validate_launch_mode(launch_mode: &str) -> Result<()> {
    match launch_mode {
        "inline" | "detached" => Ok(()),
        other => bail!("invalid launch mode: {other}"),
    }
}

pub(super) fn validate_thread_id_format(id: &str) -> Result<()> {
    if !id.starts_with("T-") {
        bail!("thread_id must start with `T-`: got `{id}`");
    }
    let suffix = &id[2..];
    let segments: Vec<&str> = suffix.split('-').collect();
    if segments.len() != 5 {
        bail!("thread_id suffix must have 5 dash-separated hex groups: got `{suffix}`");
    }
    let expected_lengths: &[usize] = &[8, 4, 4, 4, 12];
    for (seg, &expected) in segments.iter().zip(expected_lengths.iter()) {
        if seg.len() != expected || !seg.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!(
                "thread_id suffix hex groups must have lengths {expected_lengths:?}: got `{suffix}`"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_status_accepts_only_existing_terminal_vocabulary() {
        for status in [
            "completed",
            "failed",
            "cancelled",
            "killed",
            "timed_out",
            "continued",
        ] {
            assert_eq!(normalize_terminal_status(status).unwrap(), status);
        }
        assert_eq!(
            normalize_terminal_status("running").unwrap_err().to_string(),
            "invalid terminal status: running"
        );
    }

    #[test]
    fn launch_mode_preserves_accepted_values_and_error_text() {
        assert!(validate_launch_mode("inline").is_ok());
        assert!(validate_launch_mode("detached").is_ok());
        assert_eq!(
            validate_launch_mode("managed").unwrap_err().to_string(),
            "invalid launch mode: managed"
        );
    }

    #[test]
    fn thread_id_accepts_valid_format_and_generated_ids() {
        assert!(validate_thread_id_format("T-01234567-abcd-ef01-2345-6789abcdef01").is_ok());
        assert!(validate_thread_id_format(&super::super::new_thread_id()).is_ok());
    }

    #[test]
    fn thread_id_rejects_missing_prefix() {
        assert_eq!(
            validate_thread_id_format("foo-123").unwrap_err().to_string(),
            "thread_id must start with `T-`: got `foo-123`"
        );
    }

    #[test]
    fn thread_id_rejects_non_uuid_suffix() {
        let error = validate_thread_id_format("T-not-a-uuid").unwrap_err();
        assert!(error.to_string().contains("hex groups"));
    }

    #[test]
    fn thread_id_rejects_wrong_segment_lengths() {
        let error =
            validate_thread_id_format("T-01234567-ab-cdef-0123-456789abcdef01").unwrap_err();
        assert!(error.to_string().contains("hex groups"));
    }

    #[test]
    fn thread_id_rejects_non_hex_chars() {
        let error =
            validate_thread_id_format("T-ghijklmn-abcd-ef01-2345-6789abcdef01").unwrap_err();
        assert!(error.to_string().contains("hex groups"));
    }
}
