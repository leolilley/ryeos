//! Input-line classification and CLI-style tokenization.
//!
//! The input has exactly two modes: slash (explicit grammar — tokens
//! resolved against the command registry) and plain (the ground verb —
//! text fills the pinned invocation template whole, never shell-split).
//! Tokenization here is for slash mode and completion only.
//!
//! Rules (decided in input-design):
//! - leading `/` enters slash mode; the rest of the line is tokenized;
//! - bare `/` is a prompt for tokens (opens completion), not an error;
//! - leading `//` escapes a literal slash: plain text starting with `/`;
//! - quoting is shell-style: single quotes literal, double quotes with
//!   backslash escapes, no globbing, unterminated quote is an error.

/// How a submitted line is interpreted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputLine {
    /// Plain text for the pinned route (ground verb). Carries the text
    /// with any `//` escape already collapsed to a single `/`.
    Plain(String),
    /// Bare `/`: open grammar completion, nothing to submit.
    SlashEmpty,
    /// `/tokens…`: explicit grammar.
    Slash(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TokenizeError {
    #[error("unterminated quote starting at byte {at}")]
    UnterminatedQuote { at: usize },
    #[error("trailing escape character")]
    TrailingEscape,
}

/// Classify a submitted line per the input-mode rules.
pub fn classify_line(line: &str) -> Result<InputLine, TokenizeError> {
    if let Some(escaped) = line.strip_prefix("//") {
        return Ok(InputLine::Plain(format!("/{escaped}")));
    }
    let Some(rest) = line.strip_prefix('/') else {
        return Ok(InputLine::Plain(line.to_string()));
    };
    let tokens = tokenize(rest)?;
    if tokens.is_empty() {
        return Ok(InputLine::SlashEmpty);
    }
    Ok(InputLine::Slash(tokens))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionCandidate {
    pub token: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCompletion {
    pub candidates: Vec<CompletionCandidate>,
    pub exact_hint: Option<String>,
}

pub fn slash_completion(records: &[serde_json::Value], text: &str) -> Option<SlashCompletion> {
    let rest = text.strip_prefix('/')?;
    if text.starts_with("//") {
        return None;
    }
    let typed: Vec<&str> = rest.split_whitespace().collect();
    let trailing_space = rest.ends_with(' ') || rest.is_empty();
    let (complete, partial) = if trailing_space {
        (typed.as_slice(), "")
    } else {
        match typed.split_last() {
            Some((last, head)) => (head, *last),
            None => (typed.as_slice(), ""),
        }
    };
    let mut candidates: Vec<CompletionCandidate> = Vec::new();
    let mut exact_hint: Option<String> = None;
    for record in records {
        if record.get("invocable").and_then(serde_json::Value::as_bool) == Some(false) {
            continue;
        }
        let Some(tokens) = record.get("tokens").and_then(serde_json::Value::as_array) else {
            continue;
        };
        let tokens: Vec<&str> = tokens
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        if typed.len() >= tokens.len() && tokens.iter().zip(typed.iter()).all(|(a, b)| a == b) {
            exact_hint = Some(argument_hint(
                record,
                typed.len().saturating_sub(tokens.len()),
            ));
            continue;
        }
        if tokens.len() < complete.len() || !tokens.iter().zip(complete.iter()).all(|(a, b)| a == b)
        {
            continue;
        }
        if tokens.len() == complete.len() {
            if partial.is_empty() {
                exact_hint = Some(argument_hint(
                    record,
                    typed.len().saturating_sub(tokens.len()),
                ));
            }
            continue;
        }
        let next = tokens[complete.len()];
        if next.starts_with(partial) && !candidates.iter().any(|c| c.token == next) {
            candidates.push(CompletionCandidate {
                token: next.to_string(),
                description: record
                    .get("description")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
            });
        }
    }
    candidates.sort_by(|a, b| a.token.cmp(&b.token));
    Some(SlashCompletion {
        candidates,
        exact_hint,
    })
}

pub fn slash_completion_hint(records: &[serde_json::Value], text: &str) -> Option<String> {
    let completion = slash_completion(records, text)?;
    if let Some(exact) = completion.exact_hint {
        return Some(exact);
    }
    if completion.candidates.is_empty() {
        return Some("no matching commands".to_string());
    }
    Some(
        completion
            .candidates
            .into_iter()
            .take(6)
            .map(|candidate| candidate.token)
            .collect::<Vec<_>>()
            .join(" · "),
    )
}

pub fn accept_slash_completion(
    records: &[serde_json::Value],
    text: &str,
    cursor: usize,
) -> Option<(String, usize)> {
    if cursor != text.len() || !text.starts_with('/') || text.starts_with("//") {
        return None;
    }
    let candidate = slash_completion(records, text)?
        .candidates
        .into_iter()
        .next()?;
    let rest = text.strip_prefix('/').unwrap_or_default();
    let trailing_space = rest.ends_with(' ') || rest.is_empty();
    let mut tokens: Vec<&str> = rest.split_whitespace().collect();
    if trailing_space {
        tokens.push(candidate.token.as_str());
    } else if let Some(last) = tokens.last_mut() {
        *last = candidate.token.as_str();
    } else {
        tokens.push(candidate.token.as_str());
    }
    let mut completed = format!("/{}", tokens.join(" "));
    completed.push(' ');
    let cursor = completed.len();
    Some((completed, cursor))
}

fn argument_hint(record: &serde_json::Value, current_arg: usize) -> String {
    let args = record
        .get("arguments")
        .and_then(serde_json::Value::as_array)
        .map(|args| {
            args.iter()
                .enumerate()
                .filter_map(|(index, arg)| {
                    let name = arg.get("name").and_then(serde_json::Value::as_str)?;
                    Some(if index == current_arg {
                        format!("[{name}]")
                    } else {
                        name.to_string()
                    })
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let desc = record
        .get("description")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if args.is_empty() {
        format!("⏎ {desc}")
    } else {
        format!("args: {args} — {desc}")
    }
}

/// Shell-style tokenization: whitespace-separated; `'…'` literal;
/// `"…"` with `\"` and `\\` escapes; backslash outside quotes escapes
/// the next character. No globbing, no expansion.
pub fn tokenize(input: &str) -> Result<Vec<String>, TokenizeError> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut chars = input.char_indices().peekable();

    while let Some((at, ch)) = chars.next() {
        match ch {
            c if c.is_whitespace() => {
                if in_token {
                    tokens.push(std::mem::take(&mut current));
                    in_token = false;
                }
            }
            '\'' => {
                in_token = true;
                loop {
                    match chars.next() {
                        Some((_, '\'')) => break,
                        Some((_, c)) => current.push(c),
                        None => return Err(TokenizeError::UnterminatedQuote { at }),
                    }
                }
            }
            '"' => {
                in_token = true;
                loop {
                    match chars.next() {
                        Some((_, '"')) => break,
                        Some((_, '\\')) => match chars.next() {
                            Some((_, escaped @ ('"' | '\\'))) => current.push(escaped),
                            Some((_, other)) => {
                                current.push('\\');
                                current.push(other);
                            }
                            None => return Err(TokenizeError::TrailingEscape),
                        },
                        Some((_, c)) => current.push(c),
                        None => return Err(TokenizeError::UnterminatedQuote { at }),
                    }
                }
            }
            '\\' => {
                in_token = true;
                match chars.next() {
                    Some((_, escaped)) => current.push(escaped),
                    None => return Err(TokenizeError::TrailingEscape),
                }
            }
            c => {
                in_token = true;
                current.push(c);
            }
        }
    }
    if in_token {
        tokens.push(current);
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_stays_whole() {
        assert_eq!(
            classify_line("summarize my week").unwrap(),
            InputLine::Plain("summarize my week".to_string())
        );
    }

    #[test]
    fn double_slash_escapes_literal_slash() {
        assert_eq!(
            classify_line("//etc/hosts looks odd").unwrap(),
            InputLine::Plain("/etc/hosts looks odd".to_string())
        );
    }

    #[test]
    fn bare_slash_opens_completion() {
        assert_eq!(classify_line("/").unwrap(), InputLine::SlashEmpty);
        assert_eq!(classify_line("/   ").unwrap(), InputLine::SlashEmpty);
    }

    #[test]
    fn slash_tokenizes_with_quoting() {
        assert_eq!(
            classify_line(r#"/scheduler register job-1 '{"a": 1}'"#).unwrap(),
            InputLine::Slash(vec![
                "scheduler".into(),
                "register".into(),
                "job-1".into(),
                r#"{"a": 1}"#.into(),
            ])
        );
    }

    #[test]
    fn double_quotes_allow_escapes() {
        assert_eq!(
            tokenize(r#"say "she said \"hi\" twice" done"#).unwrap(),
            vec!["say", r#"she said "hi" twice"#, "done"]
        );
    }

    #[test]
    fn single_quotes_are_literal() {
        assert_eq!(
            tokenize(r#"a 'b \" c' d"#).unwrap(),
            vec!["a", r#"b \" c"#, "d"]
        );
    }

    #[test]
    fn adjacent_quoted_segments_join_one_token() {
        assert_eq!(tokenize(r#"pre'mid'"post""#).unwrap(), vec!["premidpost"]);
    }

    #[test]
    fn empty_quotes_make_empty_token() {
        assert_eq!(tokenize(r#"a '' b"#).unwrap(), vec!["a", "", "b"]);
    }

    #[test]
    fn unterminated_quote_errors() {
        assert!(matches!(
            tokenize("a 'oops"),
            Err(TokenizeError::UnterminatedQuote { .. })
        ));
        assert!(matches!(
            tokenize(r#"a "oops"#),
            Err(TokenizeError::UnterminatedQuote { .. })
        ));
    }

    #[test]
    fn trailing_escape_errors() {
        assert!(matches!(
            tokenize("oops\\"),
            Err(TokenizeError::TrailingEscape)
        ));
    }

    fn command(tokens: &[&str], description: &str, arguments: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "invocable": true,
            "tokens": tokens,
            "description": description,
            "arguments": arguments.iter().map(|name| serde_json::json!({ "name": name })).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn slash_completion_lists_next_tokens() {
        let records = vec![
            command(&["thread", "list"], "List threads", &[]),
            command(&["thread", "get"], "Get thread", &["thread_id"]),
            command(&["bundle", "list"], "List bundles", &[]),
        ];
        assert_eq!(
            slash_completion_hint(&records, "/thr"),
            Some("thread".to_string())
        );
        assert_eq!(
            slash_completion_hint(&records, "/thread "),
            Some("get · list".to_string())
        );
    }

    #[test]
    fn slash_completion_highlights_current_argument() {
        let records = vec![command(
            &["thread", "get"],
            "Get thread",
            &["thread_id", "format"],
        )];
        assert_eq!(
            slash_completion_hint(&records, "/thread get "),
            Some("args: [thread_id] format — Get thread".to_string())
        );
        assert_eq!(
            slash_completion_hint(&records, "/thread get T-1"),
            Some("args: thread_id [format] — Get thread".to_string())
        );
    }

    #[test]
    fn accept_slash_completion_replaces_partial_token() {
        let records = vec![
            command(&["thread", "list"], "List threads", &[]),
            command(&["thread", "get"], "Get thread", &["thread_id"]),
        ];
        assert_eq!(
            accept_slash_completion(&records, "/thr", 4),
            Some(("/thread ".to_string(), 8))
        );
        assert_eq!(
            accept_slash_completion(&records, "/thread g", 9),
            Some(("/thread get ".to_string(), 12))
        );
    }
}
