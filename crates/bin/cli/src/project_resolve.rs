//! Client-side project-path resolution.
//!
//! Remote project-bearing commands need a project root, and the daemon
//! CANNOT do auto-discovery — its cwd is irrelevant to the caller.
//! So the CLI is responsible for:
//!
//! 1. Detecting an explicit `--project <path>` (or `-p <path>`) in the
//!    tail and canonicalizing the path against the *CLI's* cwd.
//! 2. Detecting `--no-project` in the tail and passing it through.
//! 3. If neither is present, walking upward from cwd to find a directory
//!    containing `.ai/`. Otherwise, falling back to `--no-project`.
//!
//! The resolved spec is then re-injected into the tail as either
//! `--project <abs>` or `--no-project`, in canonical form. The daemon's
//! `arg_binder` flattens those into the handler's `Request` fields.

use std::path::PathBuf;

use crate::error::CliError;

/// Resolved spec after CLI-side processing. Either an absolute
/// canonical project root, or the explicit `--no-project` choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedProjectSpec {
    NoProject,
    Explicit(PathBuf),
}

/// Process the tail of a project-bearing command, returning a rewritten
/// tail where `--project` and `--no-project` are in their canonical
/// daemon-friendly form (absolute paths, no `-p`/`--project=foo`
/// syntactic noise).
///
/// Returns an error if both `--no-project` and `--project` are present,
/// or if the explicit project path cannot be canonicalized.
#[cfg(test)]
fn rewrite_project_tail(tail: &[String]) -> Result<Vec<String>, CliError> {
    rewrite_project_tail_with_default(tail, None)
}

/// Rewrite a project-bearing tail, using `default_project` when the tail
/// does not contain either `--project` or `--no-project`. This is how
/// the CLI preserves global `-p/--project` support while still accepting
/// service-field `--project` after the command.
pub fn rewrite_project_tail_with_default(
    tail: &[String],
    default_project: Option<&std::path::Path>,
) -> Result<Vec<String>, CliError> {
    let mut out: Vec<String> = Vec::with_capacity(tail.len() + 2);
    let mut explicit_path: Option<PathBuf> = None;
    let mut no_project = false;
    let mut i = 0;
    while i < tail.len() {
        let tok = &tail[i];
        // --no-project bare flag
        if tok == "--no-project" {
            no_project = true;
            i += 1;
            continue;
        }
        // --project=<path>, -p=<path>, --project <path>, -p <path>
        if let Some(rest) = tok
            .strip_prefix("--project=")
            .or_else(|| tok.strip_prefix("-p="))
        {
            explicit_path = Some(PathBuf::from(rest));
            i += 1;
            continue;
        }
        if tok == "--project" || tok == "-p" {
            if i + 1 >= tail.len() {
                return Err(CliError::ProjectResolution(format!(
                    "{tok} requires a value (path to the project root)"
                )));
            }
            explicit_path = Some(PathBuf::from(&tail[i + 1]));
            i += 2;
            continue;
        }
        // Pass everything else through unchanged.
        out.push(tok.clone());
        i += 1;
    }

    let cwd =
        std::env::current_dir().map_err(|e| CliError::ProjectResolution(format!("cwd: {e}")))?;
    if explicit_path.is_none() && !no_project {
        explicit_path = default_project.map(PathBuf::from);
    }
    let resolved = resolve_spec_from_cwd(no_project, explicit_path, &cwd)?;
    match resolved {
        ResolvedProjectSpec::NoProject => out.push("--no-project".into()),
        ResolvedProjectSpec::Explicit(p) => {
            out.push("--project".into());
            out.push(p.to_string_lossy().into_owned());
        }
    }
    Ok(out)
}

/// Combine the parsed flags with cwd into a single concrete spec.
fn resolve_spec_from_cwd(
    no_project: bool,
    explicit: Option<PathBuf>,
    cwd: &std::path::Path,
) -> Result<ResolvedProjectSpec, CliError> {
    match (no_project, explicit) {
        (true, Some(_)) => Err(CliError::ProjectResolution(
            "cannot pass both --no-project and --project: choose one".into(),
        )),
        (true, None) => Ok(ResolvedProjectSpec::NoProject),
        (false, Some(p)) => {
            let abs = if p.is_absolute() { p } else { cwd.join(p) };
            let canonical = abs.canonicalize().map_err(|e| {
                CliError::ProjectResolution(format!(
                    "cannot canonicalize project path '{}': {e}. \
                     Ensure the path exists and is accessible.",
                    abs.display()
                ))
            })?;
            Ok(ResolvedProjectSpec::Explicit(canonical))
        }
        (false, None) => {
            let canonical_cwd = cwd.canonicalize().map_err(|e| {
                CliError::ProjectResolution(format!(
                    "cannot canonicalize current directory '{}': {e}",
                    cwd.display()
                ))
            })?;

            for ancestor in canonical_cwd.ancestors() {
                if ancestor.join(ryeos_engine::AI_DIR).is_dir() {
                    return Ok(ResolvedProjectSpec::Explicit(ancestor.to_path_buf()));
                }
            }
            Ok(ResolvedProjectSpec::NoProject)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_passes_no_project_through() {
        let tail = vec!["--no-project".into(), "tool:foo/bar".into()];
        let out = rewrite_project_tail(&tail).unwrap();
        assert_eq!(out, vec!["tool:foo/bar".to_string(), "--no-project".into()]);
    }

    #[test]
    fn rewrite_rejects_both_flags() {
        let tail = vec!["--no-project".into(), "--project".into(), "/tmp".into()];
        let err = rewrite_project_tail(&tail).unwrap_err();
        assert!(format!("{err}").contains("cannot pass both"));
    }

    #[test]
    fn rewrite_canonicalizes_explicit_path() {
        let tmp = tempfile::tempdir().unwrap();
        let tail = vec!["--project".into(), tmp.path().to_string_lossy().into()];
        let out = rewrite_project_tail(&tail).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], "--project");
        assert!(PathBuf::from(&out[1]).is_absolute());
    }

    #[test]
    fn rewrite_accepts_short_p_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let tail = vec!["-p".into(), tmp.path().to_string_lossy().into()];
        let out = rewrite_project_tail(&tail).unwrap();
        assert_eq!(out[0], "--project");
    }

    #[test]
    fn rewrite_accepts_equals_form() {
        let tmp = tempfile::tempdir().unwrap();
        let tail = vec![format!("--project={}", tmp.path().display())];
        let out = rewrite_project_tail(&tail).unwrap();
        assert_eq!(out[0], "--project");
    }

    #[test]
    fn rewrite_uses_default_project_when_tail_has_no_project_choice() {
        let tmp = tempfile::tempdir().unwrap();
        let tail = vec!["--remote".into(), "prod".into()];
        let out = rewrite_project_tail_with_default(&tail, Some(tmp.path())).unwrap();
        assert_eq!(out[0..2], ["--remote", "prod"]);
        assert!(out.windows(2).any(|w| {
            w[0] == "--project" && w[1] == tmp.path().canonicalize().unwrap().to_string_lossy()
        }));
    }

    #[test]
    fn explicit_tail_project_overrides_default_project() {
        let default = tempfile::tempdir().unwrap();
        let explicit = tempfile::tempdir().unwrap();
        let tail = vec!["--project".into(), explicit.path().to_string_lossy().into()];
        let out = rewrite_project_tail_with_default(&tail, Some(default.path())).unwrap();
        assert_eq!(out[0], "--project");
        assert_eq!(
            PathBuf::from(&out[1]),
            explicit.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn default_uses_cwd_when_cwd_contains_dot_ai() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join(ryeos_engine::AI_DIR)).unwrap();
        let resolved = resolve_spec_from_cwd(false, None, project.path()).unwrap();
        assert_eq!(
            resolved,
            ResolvedProjectSpec::Explicit(project.path().canonicalize().unwrap())
        );
    }

    #[test]
    fn default_uses_no_project_when_cwd_has_no_dot_ai() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = resolve_spec_from_cwd(false, None, dir.path()).unwrap();
        assert_eq!(resolved, ResolvedProjectSpec::NoProject);
    }
}
