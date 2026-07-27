use super::error::SourceSpan;
use super::value::Numeric;

#[derive(Debug, Clone)]
pub(crate) struct Expr {
    pub kind: ExprKind,
    pub span: SourceSpan,
}

#[derive(Debug, Clone)]
pub(crate) enum ExprKind {
    Literal(Literal),
    Variable(String),
    Member {
        target: Box<Expr>,
        key: String,
    },
    Index {
        target: Box<Expr>,
        index: Box<Expr>,
    },
    Array(Vec<Expr>),
    Object(Vec<(String, Expr)>),
    Unary {
        operator: UnaryOperator,
        operand: Box<Expr>,
    },
    Binary {
        operator: BinaryOperator,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Conditional {
        condition: Box<Expr>,
        then_branch: Box<Expr>,
        else_branch: Box<Expr>,
    },
    Call {
        function: BuiltinFunction,
        arguments: Vec<Expr>,
    },
    Group(Box<Expr>),
}

#[derive(Debug, Clone)]
pub(crate) enum Literal {
    Null,
    Bool(bool),
    String(String),
    Number(Numeric),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnaryOperator {
    Not,
    Plus,
    Minus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinaryOperator {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    In,
    And,
    Or,
    Coalesce,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuiltinFunction {
    Length,
    Contains,
    Keys,
    Upper,
    Lower,
    Json,
    FromJson,
    Type,
    Exists,
    Matches,
    String,
    Number,
}

impl BuiltinFunction {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "length" => Self::Length,
            "contains" => Self::Contains,
            "keys" => Self::Keys,
            "upper" => Self::Upper,
            "lower" => Self::Lower,
            "json" => Self::Json,
            "from_json" => Self::FromJson,
            "type" => Self::Type,
            "exists" => Self::Exists,
            "matches" => Self::Matches,
            "string" => Self::String,
            "number" => Self::Number,
            _ => return None,
        })
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Length => "length",
            Self::Contains => "contains",
            Self::Keys => "keys",
            Self::Upper => "upper",
            Self::Lower => "lower",
            Self::Json => "json",
            Self::FromJson => "from_json",
            Self::Type => "type",
            Self::Exists => "exists",
            Self::Matches => "matches",
            Self::String => "string",
            Self::Number => "number",
        }
    }

    pub(crate) fn arity(self) -> usize {
        match self {
            Self::Contains | Self::Matches => 2,
            _ => 1,
        }
    }
}
