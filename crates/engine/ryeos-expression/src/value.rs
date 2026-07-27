use std::cmp::Ordering;

use serde_json::{Number, Value};

#[derive(Debug, Clone, Copy)]
pub(crate) enum Numeric {
    Signed(i64),
    Unsigned(u64),
    Decimal(f64),
}

impl Numeric {
    pub(crate) fn parse_unsigned_token(token: &str) -> Result<Self, &'static str> {
        if token.bytes().any(|byte| matches!(byte, b'.' | b'e' | b'E')) {
            let value = token.parse::<f64>().map_err(|_| "invalid decimal number")?;
            return Self::finite_decimal(value);
        }
        if let Ok(value) = token.parse::<i64>() {
            return Ok(Self::Signed(value));
        }
        token
            .parse::<u64>()
            .map(Self::Unsigned)
            .map_err(|_| "integer literal exceeds the u64 range")
    }

    pub(crate) fn parse_string(value: &str) -> Result<Self, &'static str> {
        if value.is_empty() || value.trim() != value {
            return Err("number(string) requires a numeric string without whitespace");
        }
        let (negative, unsigned) = match value.as_bytes().first() {
            Some(b'-') => (true, &value[1..]),
            Some(b'+') => (false, &value[1..]),
            _ => (false, value),
        };
        if unsigned.is_empty() || !valid_unsigned_number_token(unsigned) {
            return Err("number(string) requires the expression numeric grammar");
        }
        let parsed = Self::parse_unsigned_token(unsigned)?;
        if negative {
            parsed.negate()
        } else {
            Ok(parsed)
        }
    }

    pub(crate) fn from_json(number: &Number) -> Result<Self, &'static str> {
        if let Some(value) = number.as_i64() {
            Ok(Self::Signed(value))
        } else if let Some(value) = number.as_u64() {
            Ok(Self::Unsigned(value))
        } else {
            Self::finite_decimal(number.as_f64().ok_or("unrepresentable JSON number")?)
        }
    }

    pub(crate) fn to_json(self) -> Result<Value, &'static str> {
        Ok(Value::Number(match self {
            Self::Signed(value) => Number::from(value),
            Self::Unsigned(value) => Number::from(value),
            Self::Decimal(value) => {
                Number::from_f64(normalize_zero(value)).ok_or("numeric result is not finite")?
            }
        }))
    }

    pub(crate) fn canonical(self) -> Result<String, &'static str> {
        match self {
            Self::Signed(value) => Ok(value.to_string()),
            Self::Unsigned(value) => Ok(value.to_string()),
            Self::Decimal(value) => Number::from_f64(normalize_zero(value))
                .map(|number| number.to_string())
                .ok_or("numeric result is not finite"),
        }
    }

    pub(crate) fn is_zero(self) -> bool {
        match self {
            Self::Signed(value) => value == 0,
            Self::Unsigned(value) => value == 0,
            Self::Decimal(value) => value == 0.0,
        }
    }

    pub(crate) fn as_array_index(self) -> Result<Option<usize>, &'static str> {
        match self {
            Self::Signed(value) if value < 0 => Err("array index cannot be negative"),
            Self::Signed(value) => Ok(usize::try_from(value).ok()),
            Self::Unsigned(value) => Ok(usize::try_from(value).ok()),
            Self::Decimal(_) => Err("array index must be a non-negative integer"),
        }
    }

    pub(crate) fn add(self, other: Self) -> Result<Self, &'static str> {
        self.integer_or_decimal(other, "addition")
    }

    pub(crate) fn subtract(self, other: Self) -> Result<Self, &'static str> {
        match (self, other) {
            (
                left @ (Self::Signed(_) | Self::Unsigned(_)),
                right @ (Self::Signed(_) | Self::Unsigned(_)),
            ) => from_i128(
                integer_i128(left) - integer_i128(right),
                "integer subtraction overflow",
            ),
            _ => Self::finite_decimal(self.as_f64() - other.as_f64())
                .map_err(|_| "decimal subtraction produced a non-finite result"),
        }
    }

    pub(crate) fn multiply(self, other: Self) -> Result<Self, &'static str> {
        self.integer_or_decimal(other, "multiplication")
    }

    pub(crate) fn divide(self, other: Self) -> Result<Self, &'static str> {
        if other.is_zero() {
            return Err("division by zero");
        }
        Self::finite_decimal(self.as_f64() / other.as_f64())
            .map_err(|_| "division produced a non-finite result")
    }

    pub(crate) fn remainder(self, other: Self) -> Result<Self, &'static str> {
        if other.is_zero() {
            return Err("remainder by zero");
        }
        match (self, other) {
            (
                left @ (Self::Signed(_) | Self::Unsigned(_)),
                right @ (Self::Signed(_) | Self::Unsigned(_)),
            ) => from_i128(
                integer_i128(left) % integer_i128(right),
                "integer remainder overflow",
            ),
            _ => Self::finite_decimal(self.as_f64() % other.as_f64())
                .map_err(|_| "decimal remainder produced a non-finite result"),
        }
    }

    pub(crate) fn positive(self) -> Result<Self, &'static str> {
        match self {
            Self::Decimal(value) => Self::finite_decimal(value),
            value => Ok(value),
        }
    }

    pub(crate) fn negate(self) -> Result<Self, &'static str> {
        match self {
            value @ (Self::Signed(_) | Self::Unsigned(_)) => {
                from_i128(-integer_i128(value), "integer negation overflow")
            }
            Self::Decimal(value) => Self::finite_decimal(normalize_zero(-value))
                .map_err(|_| "decimal negation produced a non-finite result"),
        }
    }

    pub(crate) fn compare(self, other: Self) -> Ordering {
        match (self, other) {
            (Self::Signed(left), Self::Signed(right)) => left.cmp(&right),
            (Self::Unsigned(left), Self::Unsigned(right)) => left.cmp(&right),
            (Self::Signed(left), Self::Unsigned(right)) => {
                if left < 0 {
                    Ordering::Less
                } else {
                    (left as u64).cmp(&right)
                }
            }
            (Self::Unsigned(left), Self::Signed(right)) => {
                if right < 0 {
                    Ordering::Greater
                } else {
                    left.cmp(&(right as u64))
                }
            }
            (Self::Signed(integer), Self::Decimal(decimal)) => compare_i64_f64(integer, decimal),
            (Self::Decimal(decimal), Self::Signed(integer)) => {
                compare_i64_f64(integer, decimal).reverse()
            }
            (Self::Unsigned(integer), Self::Decimal(decimal)) => compare_u64_f64(integer, decimal),
            (Self::Decimal(decimal), Self::Unsigned(integer)) => {
                compare_u64_f64(integer, decimal).reverse()
            }
            (Self::Decimal(left), Self::Decimal(right)) => left.partial_cmp(&right).unwrap(),
        }
    }

    fn finite_decimal(value: f64) -> Result<Self, &'static str> {
        value
            .is_finite()
            .then_some(Self::Decimal(normalize_zero(value)))
            .ok_or("decimal number must be finite")
    }

    fn as_f64(self) -> f64 {
        match self {
            Self::Signed(value) => value as f64,
            Self::Unsigned(value) => value as f64,
            Self::Decimal(value) => value,
        }
    }

    fn integer_or_decimal(
        self,
        other: Self,
        operation: &'static str,
    ) -> Result<Self, &'static str> {
        match (self, other) {
            (
                left @ (Self::Signed(_) | Self::Unsigned(_)),
                right @ (Self::Signed(_) | Self::Unsigned(_)),
            ) => {
                let overflow = match operation {
                    "addition" => "integer addition overflow",
                    _ => "integer multiplication overflow",
                };
                let value = match operation {
                    "addition" => integer_i128(left).checked_add(integer_i128(right)),
                    _ => integer_i128(left).checked_mul(integer_i128(right)),
                };
                value
                    .ok_or(overflow)
                    .and_then(|value| from_i128(value, overflow))
            }
            _ => Self::finite_decimal(match operation {
                "addition" => self.as_f64() + other.as_f64(),
                _ => self.as_f64() * other.as_f64(),
            })
            .map_err(|_| "decimal arithmetic produced a non-finite result"),
        }
    }
}

fn integer_i128(value: Numeric) -> i128 {
    match value {
        Numeric::Signed(value) => value as i128,
        Numeric::Unsigned(value) => value as i128,
        Numeric::Decimal(_) => unreachable!(),
    }
}

fn from_i128(value: i128, overflow: &'static str) -> Result<Numeric, &'static str> {
    if let Ok(value) = i64::try_from(value) {
        Ok(Numeric::Signed(value))
    } else if let Ok(value) = u64::try_from(value) {
        Ok(Numeric::Unsigned(value))
    } else {
        Err(overflow)
    }
}

fn normalize_zero(value: f64) -> f64 {
    if value == 0.0 {
        0.0
    } else {
        value
    }
}

fn compare_i64_f64(integer: i64, decimal: f64) -> Ordering {
    const I64_MIN_F64: f64 = -9_223_372_036_854_775_808.0;
    const I64_UPPER_F64: f64 = 9_223_372_036_854_775_808.0;
    if decimal < I64_MIN_F64 {
        return Ordering::Greater;
    }
    if decimal >= I64_UPPER_F64 {
        return Ordering::Less;
    }
    let truncated = decimal.trunc() as i64;
    match integer.cmp(&truncated) {
        Ordering::Equal if decimal.fract() > 0.0 => Ordering::Less,
        Ordering::Equal if decimal.fract() < 0.0 => Ordering::Greater,
        ordering => ordering,
    }
}

fn compare_u64_f64(integer: u64, decimal: f64) -> Ordering {
    const U64_UPPER_F64: f64 = 18_446_744_073_709_551_616.0;
    if decimal < 0.0 {
        return Ordering::Greater;
    }
    if decimal >= U64_UPPER_F64 {
        return Ordering::Less;
    }
    let truncated = decimal.trunc() as u64;
    match integer.cmp(&truncated) {
        Ordering::Equal if decimal.fract() > 0.0 => Ordering::Less,
        ordering => ordering,
    }
}

fn valid_unsigned_number_token(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let mut cursor = if bytes[0] == b'0' {
        if bytes.get(1).is_some_and(u8::is_ascii_digit) {
            return false;
        }
        1
    } else if bytes[0].is_ascii_digit() {
        let mut cursor = 1;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        cursor
    } else {
        return false;
    };
    if bytes.get(cursor) == Some(&b'.') {
        cursor += 1;
        let start = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        if cursor == start {
            return false;
        }
    }
    if bytes
        .get(cursor)
        .is_some_and(|byte| matches!(byte, b'e' | b'E'))
    {
        cursor += 1;
        if bytes
            .get(cursor)
            .is_some_and(|byte| matches!(byte, b'+' | b'-'))
        {
            cursor += 1;
        }
        let start = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        if cursor == start {
            return false;
        }
    }
    cursor == bytes.len()
}
