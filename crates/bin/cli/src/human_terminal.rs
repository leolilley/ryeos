//! Verbs that run only from a human's terminal.
//!
//! The MCP server starts `ryeos` with `RYEOS_CALLER=agent`. A verb that changes
//! who or what the node trusts, or approves something for a human (see
//! `CommandDef::human_terminal_only` and [`HUMAN_TERMINAL_BUILTINS`]), is then
//! refused with `verb_needs_human_terminal`. The marker is read where the verb
//! is exactly known, after the CLI has resolved it, so flag order and aliases
//! cannot get around it.
//!
//! This guards against an honest agent steered by injected text into a trust
//! change through a sanctioned tool. It does not contain a hostile run: on a
//! node that does not enclose its runs, a run can read the keys directly, and
//! the enclosure is the control for that.

use std::collections::BTreeSet;

use ryeos_runtime::{CommandDef, CommandDispatch};

use crate::error::CliError;

/// The environment variable a caller sets to say what it is.
pub const CALLER_ENV: &str = "RYEOS_CALLER";
/// The one value that restricts the CLI. Unset means a human's terminal.
const AGENT: &str = "agent";

/// Hardcoded lifecycle commands that need a human's terminal, as token
/// prefixes of `lifecycle_commands`' own table. Signed command items declare
/// the same fact on themselves.
pub const HUMAN_TERMINAL_BUILTINS: &[&[&str]] = &[
    &["init"],
    &["setup"],
    &["trust", "pin"],
    &["trust", "scope"],
    &["trust", "retire"],
    &["trust", "revoke"],
    &["trust", "succeed"],
    &["project", "register"],
    &["project", "unregister"],
    &["node", "operator", "rotate"],
    &["node", "authoring", "rotate"],
    &["node", "policy-apply"],
    &["node", "reset"],
    &["node", "authorization", "revoke"],
    &["node", "host-access", "grant"],
    &["node", "host-access", "revoke"],
    &["node", "host", "setup"],
    &["node", "host", "retire"],
    &["node", "bundle-state", "import"],
    &["node", "bundle-state", "remove"],
    &["node", "bundle-set", "update"],
    &["node", "capacity", "reissue"],
    &["node", "capacity", "repair-account"],
    &["node", "capacity", "reanchor"],
    &["node", "external-execution"],
    &["node", "offline-operation", "sign"],
    &["node", "offline-operation", "submit"],
    &["node", "large-objects", "purge-quarantine"],
    &["node", "allow-restart"],
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// A human at a terminal: every verb is available.
    Terminal,
    /// An agent acting through a tool: human-terminal-only verbs are refused.
    Agent,
}

/// What a `RYEOS_CALLER` value means. A value this CLI does not know is
/// refused rather than ignored, so a misspelling cannot switch the guard off.
pub fn caller_from(value: Option<&std::ffi::OsStr>) -> Result<Caller, CliError> {
    match value {
        None => Ok(Caller::Terminal),
        Some(value) if value == AGENT => Ok(Caller::Agent),
        Some(value) => Err(CliError::Local {
            detail: format!(
                "caller_marker_unknown: {CALLER_ENV} is {value:?}; it must be unset (a human's \
                 terminal) or `{AGENT}`"
            ),
        }),
    }
}

/// The caller this process was started as.
pub fn caller() -> Result<Caller, CliError> {
    caller_from(std::env::var_os(CALLER_ENV).as_deref())
}

fn refusal(verb: &str) -> CliError {
    CliError::Local {
        detail: format!(
            "verb_needs_human_terminal: `ryeos {verb}` changes what the node trusts or approves \
             something for a person, so it runs only from a human's terminal ({CALLER_ENV}=\
             {AGENT} is set). Ask the operator to run it themselves"
        ),
    }
}

/// Refuse `command` for an agent caller when its signed item says it needs a
/// human's terminal. `tokens` are the words the caller typed, for the message.
pub fn guard_command(command: &CommandDef, tokens: &[String]) -> Result<(), CliError> {
    guard_command_for(caller()?, command, tokens)
}

fn guard_command_for(
    caller: Caller,
    command: &CommandDef,
    tokens: &[String],
) -> Result<(), CliError> {
    if !command.human_terminal_only || caller == Caller::Terminal {
        return Ok(());
    }
    Err(refusal(
        &tokens[..tokens.len().min(command.tokens.len())].join(" "),
    ))
}

/// The item refs the human-terminal-only verbs dispatch to, derived from the
/// verified command registry. A verb that takes a raw ref (`execute`, a
/// validate verb) or an unmarked verb that reaches the same service would
/// otherwise get an agent to the same effect as the marked verb.
pub fn marked_targets(commands: &[CommandDef]) -> BTreeSet<&str> {
    commands
        .iter()
        .filter(|command| command.human_terminal_only)
        .filter_map(|command| match &command.dispatch {
            CommandDispatch::ExecuteRef { execute, .. } => Some(execute.as_str()),
            _ => None,
        })
        .collect()
}

/// Refuse, for an agent caller, any resolved item ref that a human-terminal-only
/// verb dispatches to, whichever verb reached it.
pub fn guard_target(item_ref: &str, commands: &[CommandDef]) -> Result<(), CliError> {
    guard_target_for(caller()?, item_ref, commands)
}

fn guard_target_for(
    caller: Caller,
    item_ref: &str,
    commands: &[CommandDef],
) -> Result<(), CliError> {
    if caller == Caller::Terminal || !marked_targets(commands).contains(item_ref) {
        return Ok(());
    }
    Err(CliError::Local {
        detail: format!(
            "verb_needs_human_terminal: `{item_ref}` changes what the node trusts or approves \
             something for a person, so it runs only from a human's terminal ({CALLER_ENV}=\
             {AGENT} is set). Ask the operator to run it themselves"
        ),
    })
}

/// Whether a hardcoded lifecycle command needs a human's terminal.
pub fn builtin_needs_human_terminal(argv: &[String]) -> bool {
    HUMAN_TERMINAL_BUILTINS.iter().any(|prefix| {
        argv.len() >= prefix.len() && prefix.iter().zip(argv).all(|(want, got)| want == got)
    })
}

/// Refuse a hardcoded lifecycle command for an agent caller.
pub fn guard_builtin(argv: &[String]) -> Result<(), CliError> {
    guard_builtin_for(caller()?, argv)
}

fn guard_builtin_for(caller: Caller, argv: &[String]) -> Result<(), CliError> {
    if caller == Caller::Terminal || !builtin_needs_human_terminal(argv) {
        return Ok(());
    }
    let words = HUMAN_TERMINAL_BUILTINS
        .iter()
        .filter(|prefix| argv.len() >= prefix.len() && prefix.iter().zip(argv).all(|(w, g)| w == g))
        .map(|prefix| prefix.len())
        .max()
        .unwrap_or(1);
    Err(refusal(&argv[..words.min(argv.len())].join(" ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn the_caller_marker_is_unset_agent_or_refused() {
        assert_eq!(caller_from(None).unwrap(), Caller::Terminal);
        assert_eq!(
            caller_from(Some(OsStr::new("agent"))).unwrap(),
            Caller::Agent
        );
        for misspelt in ["Agent", "agents", "", "human", "0"] {
            let error = caller_from(Some(OsStr::new(misspelt))).unwrap_err();
            assert!(
                error.to_string().starts_with("caller_marker_unknown"),
                "{misspelt}: {error}"
            );
        }
    }

    #[test]
    fn a_builtin_is_matched_by_token_prefix() {
        assert!(builtin_needs_human_terminal(&argv(&["trust", "pin", "x"])));
        assert!(builtin_needs_human_terminal(&argv(&[
            "node",
            "reset",
            "authorization"
        ])));
        assert!(builtin_needs_human_terminal(&argv(&[
            "node",
            "external-execution",
            "recompose"
        ])));
        assert!(builtin_needs_human_terminal(&argv(&["init"])));
        assert!(!builtin_needs_human_terminal(&argv(&["node", "status"])));
        assert!(!builtin_needs_human_terminal(&argv(&["trust", "list"])));
        assert!(!builtin_needs_human_terminal(&argv(&["node"])));
        assert!(!builtin_needs_human_terminal(&argv(&[])));
    }

    fn command(tokens: &[&str], execute: &str, marked: bool) -> CommandDef {
        serde_json::from_value(serde_json::json!({
            "tokens": tokens,
            "description": "test",
            "human_terminal_only": marked,
            "dispatch": { "kind": "execute_ref", "execute": execute },
        }))
        .expect("a command item")
    }

    #[test]
    fn a_marked_verb_is_refused_for_an_agent_only() {
        let vault = command(&["vault", "set"], "service:vault/set", true);
        let list = command(&["vault", "list"], "service:vault/list", false);
        let typed = argv(&["vault", "set", "k"]);
        assert_eq!(
            guard_command_for(Caller::Agent, &vault, &typed)
                .unwrap_err()
                .to_string()
                .split(':')
                .next(),
            Some("verb_needs_human_terminal")
        );
        guard_command_for(Caller::Terminal, &vault, &typed).unwrap();
        guard_command_for(Caller::Agent, &list, &argv(&["vault", "list"])).unwrap();
    }

    #[test]
    fn the_generic_verbs_cannot_reach_a_marked_verbs_service() {
        let commands = vec![
            command(&["vault", "set"], "service:vault/set", true),
            command(&["vault", "list"], "service:vault/list", false),
            // An unmarked verb that dispatches to the same service is the
            // same act.
            command(&["secret", "put"], "service:vault/set", false),
        ];
        assert_eq!(
            marked_targets(&commands).into_iter().collect::<Vec<_>>(),
            ["service:vault/set"]
        );
        // `ryeos execute service:vault/set ...` resolves to this item ref.
        let refused = guard_target_for(Caller::Agent, "service:vault/set", &commands).unwrap_err();
        assert!(
            refused.to_string().starts_with("verb_needs_human_terminal"),
            "{refused}"
        );
        guard_target_for(Caller::Terminal, "service:vault/set", &commands).unwrap();
        guard_target_for(Caller::Agent, "service:vault/list", &commands).unwrap();
    }

    #[test]
    fn a_builtin_is_refused_for_an_agent_only() {
        let pin = argv(&["trust", "pin", "x"]);
        let refused = guard_builtin_for(Caller::Agent, &pin)
            .unwrap_err()
            .to_string();
        assert!(
            refused.starts_with("verb_needs_human_terminal"),
            "{refused}"
        );
        assert!(refused.contains("ryeos trust pin"), "{refused}");
        guard_builtin_for(Caller::Terminal, &pin).unwrap();
        guard_builtin_for(Caller::Agent, &argv(&["node", "status"])).unwrap();
    }

    #[test]
    fn every_builtin_entry_names_a_real_lifecycle_command() {
        let descriptors = crate::lifecycle_commands::local_command_descriptors();
        for prefix in HUMAN_TERMINAL_BUILTINS {
            assert!(
                descriptors.iter().any(|descriptor| {
                    descriptor.tokens.len() >= prefix.len()
                        && descriptor.tokens[..prefix.len()] == **prefix
                }),
                "{prefix:?} names no lifecycle command"
            );
        }
    }

    /// The shipped core command items, read the way the CLI reads them.
    fn shipped(name: &str) -> CommandDef {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../bundles/core/.ai/node/commands")
            .join(format!("{name}.yaml"));
        let text = std::fs::read_to_string(&path).expect("a shipped command item");
        let body: String = text
            .lines()
            .filter(|line| !line.starts_with("# ryeos:signed"))
            .collect::<Vec<_>>()
            .join("\n");
        serde_yaml::from_str(&body).expect("a command item")
    }

    #[test]
    fn the_raw_ref_verb_cannot_reach_what_a_shipped_marked_verb_guards() {
        let vault_set = shipped("vault-set");
        let execute = shipped("execute");
        assert!(vault_set.human_terminal_only);
        assert!(!execute.human_terminal_only, "execute is generic");
        assert!(shipped("endorse").human_terminal_only);
        assert!(shipped("sign").human_terminal_only);
        assert!(!shipped("vault-list").human_terminal_only);
        let commands = vec![vault_set, execute, shipped("vault-list")];
        // `ryeos execute service:vault/set ...` compiles to this ref.
        assert!(guard_target_for(Caller::Agent, "service:vault/set", &commands).is_err());
        assert!(guard_target_for(Caller::Agent, "service:vault/list", &commands).is_ok());
    }

    #[test]
    fn the_refusal_names_the_verb_and_its_code() {
        let error = refusal("trust pin").to_string();
        assert!(error.starts_with("verb_needs_human_terminal"), "{error}");
        assert!(error.contains("ryeos trust pin"), "{error}");
    }
}
