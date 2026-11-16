//! How the adapter starts Claude Code: its configuration, argument list and
//! environment. Pure functions, so the strict-configuration flags are tested
//! without starting anything.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// Names the adapter copies from its own environment into Claude Code's.
/// Everything else is dropped: the child starts from an empty environment.
const PASSTHROUGH: [&str; 16] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "HOME",
    "PATH",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "TZ",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
];

/// Values that make Claude Code quiet and keep a long silent stream open.
/// `API_FORCE_IDLE_TIMEOUT=0` stops the CLI dropping a stream that is silent
/// for several minutes; the rest turn off telemetry, error reporting,
/// auto-update and other non-essential traffic.
const CONSTANT_ENVIRONMENT: [(&str, &str); 8] = [
    ("API_FORCE_IDLE_TIMEOUT", "0"),
    ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
    ("DISABLE_AUTOUPDATER", "1"),
    ("DISABLE_TELEMETRY", "1"),
    ("DISABLE_ERROR_REPORTING", "1"),
    ("MCP_TIMEOUT", "60000"),
    ("MCP_TOOL_TIMEOUT", "700000"),
    ("NODE_NO_WARNINGS", "1"),
];

/// Built-in tools that change files or run code. A read-only session may
/// allow none of them.
const WRITE_CAPABLE_TOOLS: [&str; 5] = ["Bash", "Edit", "MultiEdit", "NotebookEdit", "Write"];

/// What the environment config hands the adapter.
#[derive(Clone, Debug)]
pub struct Config {
    /// Claude Code's executable. Not the workload executable: that is the
    /// adapter itself.
    pub executable: PathBuf,
    /// Claude Code's configuration directory (the profile's workload home).
    pub home: PathBuf,
    /// The signed MCP server list, written to the adapter's own MCP config.
    pub mcp_config: Value,
    /// Gateway route: the key and base URL are delivered, so Claude Code runs
    /// in bare mode and never reads OAuth state or the keychain.
    pub bare: bool,
    pub environment: Vec<(String, String)>,
}

impl Config {
    pub fn from_environment(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let executable = get("CLAUDE_CODE_EXECUTABLE")
            .filter(|value| !value.is_empty())
            .ok_or("CLAUDE_CODE_EXECUTABLE is not set")?;
        let executable = PathBuf::from(executable);
        if !executable.is_absolute() {
            return Err("CLAUDE_CODE_EXECUTABLE must be an absolute path".to_owned());
        }
        if !executable.is_file() {
            return Err("CLAUDE_CODE_EXECUTABLE does not name a file".to_owned());
        }
        let home = get("CLAUDE_CONFIG_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or("CLAUDE_CONFIG_DIR must be an absolute path")?;
        let mcp_config = match get("CLAUDE_CODE_MCP_CONFIG") {
            None => json!({"mcpServers": {}}),
            Some(text) => {
                let value: Value = serde_json::from_str(&text)
                    .map_err(|_| "CLAUDE_CODE_MCP_CONFIG is not JSON".to_owned())?;
                let servers_are_objects = value
                    .get("mcpServers")
                    .and_then(Value::as_object)
                    .is_some_and(|servers| servers.values().all(Value::is_object));
                if !servers_are_objects {
                    return Err(
                        "CLAUDE_CODE_MCP_CONFIG must be {\"mcpServers\": {name: server}}"
                            .to_owned(),
                    );
                }
                value
            }
        };
        let environment: Vec<(String, String)> = PASSTHROUGH
            .iter()
            .filter_map(|name| get(name).map(|value| ((*name).to_owned(), value)))
            .collect();
        let present = |wanted: &str| {
            environment
                .iter()
                .any(|(name, value)| name == wanted && !value.is_empty())
        };
        // Exactly one way to authenticate: a model key (optionally through a
        // gateway), or a Claude subscription token. The subscription signs in
        // directly, so it never goes through a gateway.
        match (
            present("ANTHROPIC_API_KEY"),
            present("CLAUDE_CODE_OAUTH_TOKEN"),
        ) {
            (true, false) => {}
            (false, true) if !present("ANTHROPIC_BASE_URL") => {}
            (false, true) => {
                return Err(
                    "CLAUDE_CODE_OAUTH_TOKEN signs in directly and cannot use ANTHROPIC_BASE_URL"
                        .to_owned(),
                );
            }
            (true, true) => {
                return Err("set ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN, not both".to_owned());
            }
            (false, false) => {
                return Err(
                    "no credential: set ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN".to_owned(),
                );
            }
        }
        let bare = present("ANTHROPIC_BASE_URL");
        Ok(Self {
            executable,
            home,
            mcp_config,
            bare,
            environment,
        })
    }

    /// The child's whole environment.
    pub fn child_environment(&self) -> Vec<(String, String)> {
        let mut environment = self.environment.clone();
        environment.push((
            "CLAUDE_CONFIG_DIR".to_owned(),
            self.home.display().to_string(),
        ));
        environment.extend(
            CONSTANT_ENVIRONMENT
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
        );
        environment
    }

    pub fn adapter_directory(&self) -> PathBuf {
        self.home.join("adapter")
    }

    pub fn mcp_config_path(&self) -> PathBuf {
        self.adapter_directory().join("mcp.json")
    }

    pub fn session_map_path(&self) -> PathBuf {
        self.adapter_directory().join("sessions.json")
    }
}

/// What a session carries from `session/start` for every later turn.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSettings {
    pub read_only: bool,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub tools: Vec<String>,
    pub system_prompt: Option<String>,
}

impl SessionSettings {
    /// Refuse an allow-list that could widen what the item allows: only plain
    /// tool names, never rule syntax such as `Bash(rm *)`, and no write-capable
    /// built-in in a read-only session.
    pub fn validate(&self) -> Result<(), String> {
        for tool in &self.tools {
            let plain = !tool.is_empty()
                && tool.len() <= 128
                && tool
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '*');
            if !plain {
                return Err(format!("tool name `{tool}` is not admitted"));
            }
            if self.read_only && WRITE_CAPABLE_TOOLS.contains(&tool.as_str()) {
                return Err(format!(
                    "tool `{tool}` is not allowed in a read-only session"
                ));
            }
        }
        if let Some(effort) = &self.effort
            && !["low", "medium", "high", "xhigh", "max"].contains(&effort.as_str())
        {
            return Err("effort is not admitted".to_owned());
        }
        if let Some(model) = &self.model
            && (model.is_empty()
                || model.len() > 128
                || !model.chars().all(|c| {
                    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '[' | ']')
                }))
        {
            return Err("model is not admitted".to_owned());
        }
        Ok(())
    }
}

/// How a turn names Claude Code's session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionFlag {
    /// First turn: Claude Code creates the session under this id.
    Create(String),
    /// A later turn continues it.
    Resume(String),
}

#[derive(Clone, Debug)]
pub struct TurnSettings {
    pub system_prompt: Option<String>,
    pub output_schema: Option<Value>,
    pub max_budget_usd: Option<f64>,
    pub session: SessionFlag,
}

/// The argument list after the executable. The prompt travels on stdin.
pub fn build_argv(
    config: &Config,
    session: &SessionSettings,
    turn: &TurnSettings,
    mcp_config_path: &Path,
) -> Vec<OsString> {
    let mut argv: Vec<OsString> = Vec::new();
    let mut flag = |name: &str, value: Option<&str>| {
        argv.push(name.into());
        if let Some(value) = value {
            argv.push(value.into());
        }
    };
    if config.bare {
        flag("--bare", None);
    }
    flag("-p", None);
    flag("--output-format", Some("stream-json"));
    flag("--verbose", None);
    // Strict configuration. Only the user source counts, which is the
    // baseline settings file in the adapter's own config directory; project
    // and local settings, and their hooks, are never read. MCP servers come
    // only from the adapter-written file. Skills and slash commands from the
    // workspace are off.
    flag("--setting-sources", Some("user"));
    flag("--strict-mcp-config", None);
    flag("--mcp-config", Some(&mcp_config_path.display().to_string()));
    flag("--disable-slash-commands", None);
    // Unattended: a permission prompt is denied, and only the allow-list runs
    // without one.
    flag("--permission-mode", Some("dontAsk"));
    flag("--permission-prompts", Some("none"));
    // `--tools` selects built-ins; MCP tools are named only in the allow-list.
    let builtin: Vec<&str> = session
        .tools
        .iter()
        .map(String::as_str)
        .filter(|tool| !tool.starts_with("mcp__"))
        .collect();
    flag("--tools", Some(&builtin.join(",")));
    if !session.tools.is_empty() {
        flag("--allowedTools", Some(&session.tools.join(",")));
    }
    if session.read_only {
        flag("--disallowedTools", Some(&WRITE_CAPABLE_TOOLS.join(",")));
    }
    if let Some(model) = &session.model {
        flag("--model", Some(model));
    }
    if let Some(effort) = &session.effort {
        flag("--effort", Some(effort));
    }
    if let Some(prompt) = turn
        .system_prompt
        .as_ref()
        .or(session.system_prompt.as_ref())
    {
        flag("--append-system-prompt", Some(prompt));
    }
    match &turn.session {
        SessionFlag::Create(id) => flag("--session-id", Some(id)),
        SessionFlag::Resume(id) => flag("--resume", Some(id)),
    }
    if let Some(schema) = &turn.output_schema {
        flag("--json-schema", Some(&schema.to_string()));
    }
    if let Some(budget) = turn.max_budget_usd {
        flag(
            "--max-budget-usd",
            Some(&format!("{:.2}", budget.max(0.01))),
        );
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(base_url: Option<&str>) -> Config {
        let mut environment = vec![("ANTHROPIC_API_KEY".to_owned(), "sk-x".to_owned())];
        if let Some(url) = base_url {
            environment.push(("ANTHROPIC_BASE_URL".to_owned(), url.to_owned()));
        }
        Config {
            executable: "/opt/claude-code/bin/claude".into(),
            home: "/home/w/.claude".into(),
            mcp_config: json!({"mcpServers": {}}),
            bare: base_url.is_some(),
            environment,
        }
    }

    fn session() -> SessionSettings {
        SessionSettings {
            read_only: false,
            model: Some("claude-opus-5-5".into()),
            effort: Some("max".into()),
            tools: vec!["Agent".into(), "mcp__task__*".into()],
            system_prompt: Some("be exact".into()),
        }
    }

    fn argv(config: &Config, session: &SessionSettings, turn: &TurnSettings) -> Vec<String> {
        build_argv(
            config,
            session,
            turn,
            Path::new("/home/w/.claude/adapter/mcp.json"),
        )
        .into_iter()
        .map(|arg| arg.into_string().unwrap())
        .collect()
    }

    fn turn(session: SessionFlag) -> TurnSettings {
        TurnSettings {
            system_prompt: None,
            output_schema: None,
            max_budget_usd: None,
            session,
        }
    }

    fn value_of<'a>(argv: &'a [String], flag: &str) -> &'a str {
        let at = argv.iter().position(|arg| arg == flag).unwrap();
        &argv[at + 1]
    }

    #[test]
    fn the_argv_carries_the_strict_configuration_flags() {
        let argv = argv(
            &config(Some("http://gw:8080")),
            &session(),
            &turn(SessionFlag::Create("u-1".into())),
        );
        assert_eq!(argv[0], "--bare");
        assert!(argv.contains(&"-p".to_owned()));
        assert_eq!(value_of(&argv, "--output-format"), "stream-json");
        assert_eq!(value_of(&argv, "--setting-sources"), "user");
        assert!(argv.contains(&"--strict-mcp-config".to_owned()));
        assert_eq!(
            value_of(&argv, "--mcp-config"),
            "/home/w/.claude/adapter/mcp.json"
        );
        assert!(argv.contains(&"--disable-slash-commands".to_owned()));
        assert_eq!(value_of(&argv, "--permission-mode"), "dontAsk");
        assert_eq!(value_of(&argv, "--permission-prompts"), "none");
        // Built-ins only in --tools; the MCP pattern only in the allow-list.
        assert_eq!(value_of(&argv, "--tools"), "Agent");
        assert_eq!(value_of(&argv, "--allowedTools"), "Agent,mcp__task__*");
        assert_eq!(value_of(&argv, "--model"), "claude-opus-5-5");
        assert_eq!(value_of(&argv, "--effort"), "max");
        assert_eq!(value_of(&argv, "--append-system-prompt"), "be exact");
        assert_eq!(value_of(&argv, "--session-id"), "u-1");
        assert!(!argv.contains(&"--resume".to_owned()));
        assert!(!argv.contains(&"--dangerously-skip-permissions".to_owned()));
        assert!(!argv.contains(&"--no-session-persistence".to_owned()));
    }

    #[test]
    fn no_tools_means_no_built_ins_and_no_allow_list() {
        let mut bare_session = session();
        bare_session.tools.clear();
        let argv = argv(
            &config(None),
            &bare_session,
            &turn(SessionFlag::Resume("c-9".into())),
        );
        assert_eq!(argv[0], "-p", "no gateway, so no --bare");
        assert_eq!(value_of(&argv, "--tools"), "");
        assert!(!argv.contains(&"--allowedTools".to_owned()));
        assert_eq!(value_of(&argv, "--resume"), "c-9");
    }

    #[test]
    fn a_read_only_session_denies_the_write_tools_and_refuses_them_in_the_allow_list() {
        let mut read_only = session();
        read_only.read_only = true;
        assert!(read_only.validate().is_ok());
        let argv = argv(
            &config(None),
            &read_only,
            &turn(SessionFlag::Create("u".into())),
        );
        assert_eq!(
            value_of(&argv, "--disallowedTools"),
            "Bash,Edit,MultiEdit,NotebookEdit,Write"
        );
        read_only.tools.push("Write".into());
        assert!(read_only.validate().unwrap_err().contains("read-only"));
    }

    #[test]
    fn rule_syntax_cannot_ride_in_through_the_allow_list() {
        let mut widened = session();
        widened.tools = vec!["Bash(rm *)".into()];
        assert!(widened.validate().is_err());
        widened.tools = vec!["Read,Write".into()];
        assert!(widened.validate().is_err());
        widened.tools = vec!["Read".into()];
        widened.effort = Some("godlike".into());
        assert!(widened.validate().is_err());
    }

    #[test]
    fn schema_budget_and_turn_system_prompt_are_passed() {
        let mut turn = turn(SessionFlag::Create("u".into()));
        turn.system_prompt = Some("turn prompt".into());
        turn.output_schema = Some(json!({"type": "object"}));
        turn.max_budget_usd = Some(0.001);
        let argv = argv(&config(None), &session(), &turn);
        assert_eq!(value_of(&argv, "--append-system-prompt"), "turn prompt");
        assert_eq!(value_of(&argv, "--json-schema"), r#"{"type":"object"}"#);
        assert_eq!(value_of(&argv, "--max-budget-usd"), "0.01");
    }

    #[test]
    fn the_child_environment_holds_the_reference_constants_and_only_listed_passthrough() {
        let environment = config(Some("http://gw")).child_environment();
        let get = |name: &str| {
            environment
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(get("API_FORCE_IDLE_TIMEOUT"), Some("0"));
        assert_eq!(get("CLAUDE_CONFIG_DIR"), Some("/home/w/.claude"));
        assert_eq!(get("DISABLE_TELEMETRY"), Some("1"));
        assert_eq!(get("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"), Some("1"));
        assert_eq!(get("ANTHROPIC_API_KEY"), Some("sk-x"));
    }

    #[test]
    fn configuration_is_read_from_the_environment_and_refused_when_unusable() {
        let exe = std::env::current_exe().unwrap().display().to_string();
        let env = |pairs: Vec<(&'static str, String)>| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.clone())
            }
        };
        let ok = Config::from_environment(env(vec![
            ("CLAUDE_CODE_EXECUTABLE", exe.clone()),
            ("CLAUDE_CONFIG_DIR", "/home/w/.claude".into()),
            ("ANTHROPIC_API_KEY", "sk-x".into()),
            ("ANTHROPIC_BASE_URL", "http://gw".into()),
            (
                "CLAUDE_CODE_MCP_CONFIG",
                r#"{"mcpServers":{"task":{"type":"http","url":"http://127.0.0.1:9"}}}"#.into(),
            ),
        ]))
        .unwrap();
        assert!(ok.bare);
        assert_eq!(ok.mcp_config["mcpServers"]["task"]["type"], "http");
        // A subscription token alone is a complete credential, and never bare.
        let subscription = Config::from_environment(env(vec![
            ("CLAUDE_CODE_EXECUTABLE", exe.clone()),
            ("CLAUDE_CONFIG_DIR", "/home/w/.claude".into()),
            ("CLAUDE_CODE_OAUTH_TOKEN", "oauth-x".into()),
        ]))
        .unwrap();
        assert!(!subscription.bare);
        assert!(
            subscription
                .child_environment()
                .contains(&("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), "oauth-x".to_owned()))
        );
        let base = || {
            vec![
                ("CLAUDE_CODE_EXECUTABLE", exe.clone()),
                ("CLAUDE_CONFIG_DIR", "/h".to_owned()),
            ]
        };
        let with = |extra: Vec<(&'static str, &str)>| {
            let mut pairs = base();
            pairs.extend(
                extra
                    .into_iter()
                    .map(|(key, value)| (key, value.to_owned())),
            );
            pairs
        };
        for (pairs, needle) in [
            (base(), "no credential"),
            (
                with(vec![
                    ("ANTHROPIC_API_KEY", "sk"),
                    ("CLAUDE_CODE_OAUTH_TOKEN", "o"),
                ]),
                "not both",
            ),
            (
                with(vec![
                    ("CLAUDE_CODE_OAUTH_TOKEN", "o"),
                    ("ANTHROPIC_BASE_URL", "http://gw"),
                ]),
                "cannot use ANTHROPIC_BASE_URL",
            ),
        ] {
            let error = Config::from_environment(env(pairs)).unwrap_err();
            assert!(error.contains(needle), "{error}");
        }
        for (pairs, needle) in [
            (vec![("CLAUDE_CONFIG_DIR", "/h".to_owned())], "not set"),
            (
                vec![
                    ("CLAUDE_CODE_EXECUTABLE", "claude".to_owned()),
                    ("CLAUDE_CONFIG_DIR", "/h".to_owned()),
                ],
                "absolute",
            ),
            (
                vec![
                    ("CLAUDE_CODE_EXECUTABLE", "/no/such/claude".to_owned()),
                    ("CLAUDE_CONFIG_DIR", "/h".to_owned()),
                ],
                "does not name a file",
            ),
            (
                vec![("CLAUDE_CODE_EXECUTABLE", exe.clone())],
                "CLAUDE_CONFIG_DIR",
            ),
            (
                vec![
                    ("CLAUDE_CODE_EXECUTABLE", exe.clone()),
                    ("CLAUDE_CONFIG_DIR", "/h".to_owned()),
                    ("ANTHROPIC_API_KEY", "sk".to_owned()),
                    (
                        "CLAUDE_CODE_MCP_CONFIG",
                        r#"{"mcpServers":{"a":1}}"#.to_owned(),
                    ),
                ],
                "mcpServers",
            ),
        ] {
            let error = Config::from_environment(env(pairs)).unwrap_err();
            assert!(error.contains(needle), "{error}");
        }
    }
}
