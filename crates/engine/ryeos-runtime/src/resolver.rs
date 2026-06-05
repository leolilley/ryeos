//! Shared command resolution — resolves command tokens into an executable target.

use serde::Serialize;

/// The result of resolving a token sequence through the command registry.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedCommand {
    /// The command name the token sequence resolved to.
    pub command: String,
    /// The canonical ref to execute.
    pub execute_ref: String,
    /// How many tokens were consumed by the command match.
    pub consumed: usize,
    /// Remaining tokens after the command match.
    pub tail: Vec<String>,
    /// Parameters parsed from the tail (--key value, --flag, positional forms).
    pub parameters: serde_json::Value,
    /// Whether the matched command alias is deprecated.
    pub deprecated: bool,
    /// If deprecated, the suggested replacement token sequence.
    pub replacement_tokens: Option<Vec<String>>,
    /// If deprecated, the version in which this alias will be removed.
    pub removed_in: Option<String>,
}

/// Errors from command resolution.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("no command matches tokens {tokens:?}")]
    NoMatch { tokens: Vec<String> },
    #[error("command '{command}' does not dispatch to an executable item ref")]
    NotExecutable { command: String },
    #[error("command '{command}' requires argument '{argument}'")]
    MissingItemRefArgument { command: String, argument: String },
    #[error("failed to bind arguments for command {tokens:?}: {detail}")]
    Bind { tokens: Vec<String>, detail: String },
}

/// Resolve a token sequence into a fully-bound command.
pub fn resolve_command(
    tokens: &[String],
    command_registry: &crate::CommandRegistry,
) -> Result<ResolvedCommand, ResolveError> {
    let matched = command_registry
        .resolve(tokens)
        .map_err(|_| ResolveError::NoMatch {
            tokens: tokens.to_vec(),
        })?;

    let mut parameters =
        crate::arg_binder::bind_argv_with_command(&matched.tail, Some(&matched.command)).map_err(
            |e| ResolveError::Bind {
                tokens: matched.matched_tokens.clone(),
                detail: e,
            },
        )?;

    let execute_ref = match &matched.command.dispatch {
        crate::CommandDispatch::ExecuteRef { execute, .. } => execute.clone(),
        crate::CommandDispatch::DirectExecuteItemRef { item_ref_arg, .. } => {
            let item_ref = parameters
                .get(item_ref_arg)
                .and_then(|v| v.as_str())
                .map(ToString::to_string)
                .ok_or_else(|| ResolveError::MissingItemRefArgument {
                    command: matched.command.name.clone(),
                    argument: item_ref_arg.clone(),
                })?;
            if let Some(obj) = parameters.as_object_mut() {
                obj.remove(item_ref_arg);
            }
            item_ref
        }
        crate::CommandDispatch::Group | crate::CommandDispatch::LocalHandler { .. } => {
            return Err(ResolveError::NotExecutable {
                command: matched.command.name.clone(),
            });
        }
    };

    let deprecated = matched
        .alias
        .as_ref()
        .and_then(|d| d.deprecated)
        .unwrap_or(false);
    let replacement_tokens = matched
        .alias
        .as_ref()
        .and_then(|d| d.replacement_tokens.clone());
    let removed_in = matched.alias.as_ref().and_then(|d| d.removed_in.clone());

    Ok(ResolvedCommand {
        command: matched.command.name,
        execute_ref,
        consumed: matched.consumed,
        tail: matched.tail,
        parameters,
        deprecated,
        replacement_tokens,
        removed_in,
    })
}
