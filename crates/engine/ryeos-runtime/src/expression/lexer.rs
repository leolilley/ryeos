use std::sync::Arc;

use super::error::{ErrorPhase, ExpressionError, SourceSpan};
use super::limits::CompilationLimits;
use super::token::{Token, TokenKind};

pub(crate) fn lex(
    source: Arc<str>,
    field: Option<Arc<str>>,
    limits: &CompilationLimits,
) -> Result<Vec<Token>, ExpressionError> {
    if source.len() > limits.max_source_bytes {
        return Err(error(
            &source,
            &field,
            ErrorPhase::Limit,
            SourceSpan::new(0, source.len()),
            format!(
                "expression source is {} bytes; limit is {}",
                source.len(),
                limits.max_source_bytes
            ),
        ));
    }
    let mut lexer = Lexer {
        source,
        field,
        limits,
        cursor: 0,
        tokens: Vec::new(),
    };
    lexer.run()?;
    Ok(lexer.tokens)
}

struct Lexer<'a> {
    source: Arc<str>,
    field: Option<Arc<str>>,
    limits: &'a CompilationLimits,
    cursor: usize,
    tokens: Vec<Token>,
}

impl Lexer<'_> {
    fn run(&mut self) -> Result<(), ExpressionError> {
        while self.cursor < self.source.len() {
            let byte = self.source.as_bytes()[self.cursor];
            if byte.is_ascii_whitespace() {
                self.cursor += 1;
                continue;
            }
            let start = self.cursor;
            let kind = match byte {
                b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.identifier(),
                b'0'..=b'9' => self.number()?,
                b'\'' | b'"' => self.string(byte)?,
                b'(' => self.one(TokenKind::LeftParen),
                b')' => self.one(TokenKind::RightParen),
                b'[' => self.one(TokenKind::LeftBracket),
                b']' => self.one(TokenKind::RightBracket),
                b'{' => self.one(TokenKind::LeftBrace),
                b'}' => self.one(TokenKind::RightBrace),
                b',' => self.one(TokenKind::Comma),
                b':' => self.one(TokenKind::Colon),
                b'.' => self.one(TokenKind::Dot),
                b'+' => self.one(TokenKind::Plus),
                b'-' => self.one(TokenKind::Minus),
                b'*' => self.one(TokenKind::Star),
                b'/' => self.one(TokenKind::Slash),
                b'%' => self.one(TokenKind::Percent),
                b'?' if self.peek_byte(1) == Some(b'?') => self.two(TokenKind::Coalesce),
                b'?' => self.one(TokenKind::Question),
                b'|' if self.peek_byte(1) == Some(b'|') => self.two(TokenKind::Or),
                b'|' => {
                    return Err(self
                        .error(
                            ErrorPhase::Lex,
                            SourceSpan::new(start, start + 1),
                            "pipe-filter syntax was removed in rye-expr/1",
                        )
                        .correction(
                            "call the filter as a function, for example `length(value)`; use `??` for fallback",
                        ));
                }
                b'&' if self.peek_byte(1) == Some(b'&') => self.two(TokenKind::And),
                b'=' if self.peek_byte(1) == Some(b'=') => self.two(TokenKind::Equal),
                b'!' if self.peek_byte(1) == Some(b'=') => self.two(TokenKind::NotEqual),
                b'!' => self.one(TokenKind::Bang),
                b'<' if self.peek_byte(1) == Some(b'=') => self.two(TokenKind::LessEqual),
                b'<' => self.one(TokenKind::Less),
                b'>' if self.peek_byte(1) == Some(b'=') => self.two(TokenKind::GreaterEqual),
                b'>' => self.one(TokenKind::Greater),
                _ => {
                    let character = self.source[self.cursor..].chars().next().unwrap();
                    return Err(self.error(
                        ErrorPhase::Lex,
                        SourceSpan::new(start, start + character.len_utf8()),
                        format!("unexpected character `{character}`"),
                    ));
                }
            };
            self.push(kind, SourceSpan::new(start, self.cursor))?;
        }
        self.push(TokenKind::Eof, SourceSpan::at(self.source.len()))?;
        Ok(())
    }

    fn push(&mut self, kind: TokenKind, span: SourceSpan) -> Result<(), ExpressionError> {
        if self.tokens.len() >= self.limits.max_tokens {
            return Err(self.error(
                ErrorPhase::Limit,
                span,
                format!(
                    "expression exceeds token limit of {}",
                    self.limits.max_tokens
                ),
            ));
        }
        self.tokens.push(Token { kind, span });
        Ok(())
    }

    fn identifier(&mut self) -> TokenKind {
        let start = self.cursor;
        self.cursor += 1;
        while self
            .source
            .as_bytes()
            .get(self.cursor)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            self.cursor += 1;
        }
        match &self.source[start..self.cursor] {
            "null" => TokenKind::Null,
            "true" => TokenKind::True,
            "false" => TokenKind::False,
            "in" => TokenKind::In,
            value => TokenKind::Identifier(value.to_string()),
        }
    }

    fn number(&mut self) -> Result<TokenKind, ExpressionError> {
        let start = self.cursor;
        if self.source.as_bytes()[self.cursor] == b'0' {
            self.cursor += 1;
            if self
                .source
                .as_bytes()
                .get(self.cursor)
                .is_some_and(u8::is_ascii_digit)
            {
                return Err(self.error(
                    ErrorPhase::Lex,
                    SourceSpan::new(start, self.cursor + 1),
                    "numeric literals cannot contain leading zeroes",
                ));
            }
        } else {
            self.take_digits();
        }
        if self.source.as_bytes().get(self.cursor) == Some(&b'.') {
            self.cursor += 1;
            let fraction = self.cursor;
            self.take_digits();
            if fraction == self.cursor {
                return Err(self.error(
                    ErrorPhase::Lex,
                    SourceSpan::new(start, self.cursor),
                    "a decimal point must be followed by a digit",
                ));
            }
        }
        if self
            .source
            .as_bytes()
            .get(self.cursor)
            .is_some_and(|byte| matches!(byte, b'e' | b'E'))
        {
            self.cursor += 1;
            if self
                .source
                .as_bytes()
                .get(self.cursor)
                .is_some_and(|byte| matches!(byte, b'+' | b'-'))
            {
                self.cursor += 1;
            }
            let exponent = self.cursor;
            self.take_digits();
            if exponent == self.cursor {
                return Err(self.error(
                    ErrorPhase::Lex,
                    SourceSpan::new(start, self.cursor),
                    "an exponent must contain at least one digit",
                ));
            }
        }
        Ok(TokenKind::Number(
            self.source[start..self.cursor].to_string(),
        ))
    }

    fn take_digits(&mut self) {
        while self
            .source
            .as_bytes()
            .get(self.cursor)
            .is_some_and(u8::is_ascii_digit)
        {
            self.cursor += 1;
        }
    }

    fn string(&mut self, quote: u8) -> Result<TokenKind, ExpressionError> {
        let start = self.cursor;
        self.cursor += 1;
        let mut output = String::new();
        while self.cursor < self.source.len() {
            let byte = self.source.as_bytes()[self.cursor];
            if byte == quote {
                self.cursor += 1;
                return Ok(TokenKind::String(output));
            }
            if byte == b'\\' {
                self.cursor += 1;
                self.decode_escape(quote, start, &mut output)?;
                continue;
            }
            if byte < 0x20 {
                return Err(self.error(
                    ErrorPhase::Lex,
                    SourceSpan::new(self.cursor, self.cursor + 1),
                    "unescaped control character in string literal",
                ));
            }
            let character = self.source[self.cursor..].chars().next().unwrap();
            output.push(character);
            self.cursor += character.len_utf8();
        }
        Err(self.error(
            ErrorPhase::Lex,
            SourceSpan::new(start, self.source.len()),
            "unterminated string literal",
        ))
    }

    fn decode_escape(
        &mut self,
        quote: u8,
        string_start: usize,
        output: &mut String,
    ) -> Result<(), ExpressionError> {
        let escape_start = self.cursor.saturating_sub(1);
        let Some(&escaped) = self.source.as_bytes().get(self.cursor) else {
            return Err(self.error(
                ErrorPhase::Lex,
                SourceSpan::new(string_start, self.source.len()),
                "unterminated string escape",
            ));
        };
        self.cursor += 1;
        match escaped {
            b'"' => output.push('"'),
            b'\'' if quote == b'\'' => output.push('\''),
            b'\\' => output.push('\\'),
            b'/' => output.push('/'),
            b'b' => output.push('\u{0008}'),
            b'f' => output.push('\u{000c}'),
            b'n' => output.push('\n'),
            b'r' => output.push('\r'),
            b't' => output.push('\t'),
            b'u' => {
                let first = self.read_hex_escape(escape_start)?;
                let scalar = if (0xd800..=0xdbff).contains(&first) {
                    if self.source.as_bytes().get(self.cursor..self.cursor + 2) != Some(b"\\u") {
                        return Err(self.error(
                            ErrorPhase::Lex,
                            SourceSpan::new(escape_start, self.cursor),
                            "high Unicode surrogate must be followed by a low surrogate",
                        ));
                    }
                    self.cursor += 2;
                    let second = self.read_hex_escape(escape_start)?;
                    if !(0xdc00..=0xdfff).contains(&second) {
                        return Err(self.error(
                            ErrorPhase::Lex,
                            SourceSpan::new(escape_start, self.cursor),
                            "invalid Unicode surrogate pair",
                        ));
                    }
                    0x10000 + (((first - 0xd800) as u32) << 10) + (second - 0xdc00) as u32
                } else if (0xdc00..=0xdfff).contains(&first) {
                    return Err(self.error(
                        ErrorPhase::Lex,
                        SourceSpan::new(escape_start, self.cursor),
                        "unpaired low Unicode surrogate",
                    ));
                } else {
                    first as u32
                };
                output.push(char::from_u32(scalar).ok_or_else(|| {
                    self.error(
                        ErrorPhase::Lex,
                        SourceSpan::new(escape_start, self.cursor),
                        "invalid Unicode scalar value",
                    )
                })?);
            }
            _ => {
                return Err(self.error(
                    ErrorPhase::Lex,
                    SourceSpan::new(escape_start, self.cursor),
                    format!("unknown string escape `\\{}`", escaped as char),
                ));
            }
        }
        Ok(())
    }

    fn read_hex_escape(&mut self, escape_start: usize) -> Result<u16, ExpressionError> {
        let end = self.cursor.saturating_add(4);
        let Some(hex) = self.source.get(self.cursor..end) else {
            return Err(self.error(
                ErrorPhase::Lex,
                SourceSpan::new(escape_start, self.source.len()),
                "Unicode escape must contain four hexadecimal digits",
            ));
        };
        if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(self.error(
                ErrorPhase::Lex,
                SourceSpan::new(escape_start, end),
                "Unicode escape must contain four hexadecimal digits",
            ));
        }
        self.cursor = end;
        Ok(u16::from_str_radix(hex, 16).unwrap())
    }

    fn one(&mut self, kind: TokenKind) -> TokenKind {
        self.cursor += 1;
        kind
    }

    fn two(&mut self, kind: TokenKind) -> TokenKind {
        self.cursor += 2;
        kind
    }

    fn peek_byte(&self, offset: usize) -> Option<u8> {
        self.source.as_bytes().get(self.cursor + offset).copied()
    }

    fn error(
        &self,
        phase: ErrorPhase,
        span: SourceSpan,
        message: impl Into<String>,
    ) -> ExpressionError {
        error(&self.source, &self.field, phase, span, message)
    }
}

fn error(
    source: &Arc<str>,
    field: &Option<Arc<str>>,
    phase: ErrorPhase,
    span: SourceSpan,
    message: impl Into<String>,
) -> ExpressionError {
    ExpressionError::new(phase, field.clone(), source.clone(), span, message)
}
