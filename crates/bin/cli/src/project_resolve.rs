//! Client-side project-path resolution.
//!
//! Remote project-bearing verbs need a project root, and the daemon
//! CANNOT do auto-discovery — its cwd is irrelevant to the caller.
//! So the CLI is responsible for:
//!
//! 1. Detecting an explicit `--project <path>` (or `-p <path>`) in the
//!    tail and canonicalizing the path against the *CLI's* cwd.
//! 2. Detecting `--no-project` in the tail and passing it through.
//! 3. If neither is present, walking up from cwd with
//!    `discover_project_root`. If nothing is found, hard-erroring with
//!    a message pointing at `--project` and `--no-project`.
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

/// Verbs whose tail must be processed by this resolver. These are
/// the only commands today that need a project root; other verbs
/// (status, vault, bundle, fetch, …) ignore project entirely so we
/// don't touch their tail.
pub fn verb_needs_project_resolution(tokens: &[String]) -> bool {
    matches!(
        tokens
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice(),
        ["remote", "execute", ..]
            | ["remote", "push", ..]
            | ["remote", "bind-project", ..]
            | ["remote", "sync-project-ai", ..]
            | ["remote", "project-status", ..]
    )
}

/// Process the tail of a project-bearing verb, returning a rewritten
/// tail where `--project` and `--no-project` are in their canonical
/// daemon-friendly form (absolute paths, no `-p`/`--project=foo`
/// syntactic noise).
///
/// Returns an error if both `--no-project` and `--project` are present,
/// or if discovery fails to find a project root and the operator did
/// not opt out via `--no-project`.
pub fn rewrite_project_tail(tail: &[String]) -> Result<Vec<String>, CliError> {
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

    let resolved = resolve_spec(no_project, explicit_path)?;
    match resolved {
        ResolvedProjectSpec::NoProject => out.push("--no-project".into()),
        ResolvedProjectSpec::Explicit(p) => {
            out.push("--project".into());
            out.push(p.to_string_lossy().into_owned());
        }
    }
    Ok(out)
}

/// Combine the parsed flags with auto-discovery to produce a single
/// concrete spec. Pure — no IO except `current_dir` and the discovery
/// walk.
fn resolve_spec(
    no_project: bool,
    explicit: Option<PathBuf>,
) -> Result<ResolvedProjectSpec, CliError> {
    match (no_project, explicit) {
        (true, Some(_)) => Err(CliError::ProjectResolution(
            "cannot pass both --no-project and --project: choose one".into(),
        )),
        (true, None) => Ok(ResolvedProjectSpec::NoProject),
        (false, Some(p)) => {
            let cwd = std::env::current_dir()
                .map_err(|e| CliError::ProjectResolution(format!("cwd: {e}")))?;
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
            // Auto-discover from cwd.
            let cwd = std::env::current_dir()
                .map_err(|e| CliError::ProjectResolution(format!("cwd: {e}")))?;
            let discovered = ryeos_state::project_discovery::discover_project_root(&cwd)
                .map_err(|e| CliError::ProjectResolution(format!("project discovery: {e}")))?;
            match discovered {
                Some(root) => {
                    let canonical = root.canonicalize().map_err(|e| {
                        CliError::ProjectResolution(format!(
                            "cannot canonicalize discovered project path '{}': {e}",
                            root.display()
                        ))
                    })?;
                    Ok(ResolvedProjectSpec::Explicit(canonical))
                }
                None => Err(CliError::ProjectResolution(format!(
                    "not in a project. No project marker (.ai/, .ryeos-project, \
                     .git) found walking up from '{}'. Pass --project <path> \
                     explicitly, or --no-project to skip project ingest.",
                    cwd.display()
                ))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verb_needs_project_resolution_matches_remote_execute_push() {
        assert!(verb_needs_project_resolution(&[
            "remote".into(),
            "execute".into(),
        ]));
        assert!(verb_needs_project_resolution(&[
            "remote".into(),
            "push".into(),
            "--remote".into(),
            "default".into(),
        ]));
        assert!(verb_needs_project_resolution(&[
            "remote".into(),
            "bind-project".into(),
        ]));
        assert!(verb_needs_project_resolution(&[
            "remote".into(),
            "sync-project-ai".into(),
        ]));
        assert!(verb_needs_project_resolution(&[
            "remote".into(),
            "project-status".into(),
        ]));
        assert!(!verb_needs_project_resolution(&["status".into()]));
        assert!(!verb_needs_project_resolution(&[
            "remote".into(),
            "configure".into(),
        ]));
    }

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
}
