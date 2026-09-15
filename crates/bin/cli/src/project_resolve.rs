//! Client-side project selector boundary.
//!
//! The command descriptor's project policy is applied to typed JSON by
//! `dispatcher::apply_project_policy`, shared by live and offline dispatch.
//! Do not add an argv round-trip or independent project-discovery/default
//! resolver here: it can erase malformed selectors and change authority.
//! This module only refuses user flags that impersonate runtime-bound fields.

use crate::error::CliError;

/// A command's `project.bind_parameter` is an internal service-field mapping,
/// not a second user-facing project selector. Accepting both it and the
/// canonical `--project` flag makes command resolution and payload binding
/// disagree about which project owns the target item.
pub fn reject_bound_project_parameter_flag(
    tail: &[String],
    bind_parameter: Option<&str>,
) -> Result<(), CliError> {
    let mut forbidden = vec!["--project-path".to_owned()];
    if let Some(bind_parameter) = bind_parameter {
        let flag = format!("--{}", bind_parameter.replace('_', "-"));
        if flag != "--project" && !forbidden.contains(&flag) {
            forbidden.push(flag);
        }
    }
    if let Some(flag) = forbidden.iter().find(|flag| {
        tail.iter()
            .any(|token| token == *flag || token.starts_with(&format!("{flag}=")))
    }) {
        return Err(CliError::ProjectResolution(format!(
            "{flag} is a runtime-bound service field, not a project selector; use --project <path>"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_bound_project_path_is_not_a_second_selector() {
        let error = reject_bound_project_parameter_flag(
            &["--project-path=/tmp/project".to_string()],
            Some("project_path"),
        )
        .unwrap_err();
        assert!(format!("{error}").contains("use --project <path>"));
    }

    #[test]
    fn canonical_project_flag_is_allowed_when_it_is_the_bind_name() {
        let tail = vec!["--project".into(), "/project".into()];
        reject_bound_project_parameter_flag(&tail, Some("project"))
            .expect("canonical selector must remain user-facing");
    }

    #[test]
    fn project_path_alias_is_rejected_even_when_the_bind_name_is_project() {
        let tail = vec!["--project-path=/project".into()];
        let error = reject_bound_project_parameter_flag(&tail, Some("project"))
            .expect_err("a second selector must be refused");
        assert!(format!("{error}").contains("use --project <path>"));
    }
}
