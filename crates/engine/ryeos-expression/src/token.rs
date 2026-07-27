use super::error::SourceSpan;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Token {
    pub kind: TokenKind,
    pub span: SourceSpan,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TokenKind {
    Null,
    True,
    False,
    In,
    Identifier(String),
    Number(String),
    String(String),
    LeftParen,
    RightParen,
    LeftBracket,
    RightBracket,
    LeftBrace,
    RightBrace,
    Comma,
    Colon,
    Dot,
    Question,
    Coalesce,
    Or,
    And,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Bang,
    Eof,
}

impl TokenKind {
    pub(crate) fn description(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::True => "true",
            Self::False => "false",
            Self::In => "in",
            Self::Identifier(_) => "identifier",
            Self::Number(_) => "number",
            Self::String(_) => "string",
            Self::LeftParen => "`(`",
            Self::RightParen => "`)`",
            Self::LeftBracket => "`[`",
            Self::RightBracket => "`]`",
            Self::LeftBrace => "`{`",
            Self::RightBrace => "`}`",
            Self::Comma => "`,`",
            Self::Colon => "`:`",
            Self::Dot => "`.`",
            Self::Question => "`?`",
            Self::Coalesce => "`??`",
            Self::Or => "`||`",
            Self::And => "`&&`",
            Self::Equal => "`==`",
            Self::NotEqual => "`!=`",
            Self::Less => "`<`",
            Self::LessEqual => "`<=`",
            Self::Greater => "`>`",
            Self::GreaterEqual => "`>=`",
            Self::Plus => "`+`",
            Self::Minus => "`-`",
            Self::Star => "`*`",
            Self::Slash => "`/`",
            Self::Percent => "`%`",
            Self::Bang => "`!`",
            Self::Eof => "end of expression",
        }
    }
}
