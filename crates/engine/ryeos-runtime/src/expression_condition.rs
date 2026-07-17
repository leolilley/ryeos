use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

/// Authored condition syntax shared by graph edges and runtime hooks.
///
/// `Absent` is deliberately distinct from every authored value. Graph edges
/// use it for the default branch and hooks use it for an unconditional hook.
/// An explicit `null` must therefore fail deserialization instead of silently
/// becoming `None` through Serde's `Option` handling.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ExpressionCondition {
    #[default]
    Absent,
    Boolean(bool),
    Expression(String),
}

impl ExpressionCondition {
    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }

    pub fn as_expression(&self) -> Option<&str> {
        match self {
            Self::Expression(source) => Some(source),
            Self::Absent | Self::Boolean(_) => None,
        }
    }
}

impl Serialize for ExpressionCondition {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Absent => serializer.serialize_none(),
            Self::Boolean(value) => serializer.serialize_bool(*value),
            Self::Expression(source) => serializer.serialize_str(source),
        }
    }
}

impl<'de> Deserialize<'de> for ExpressionCondition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ConditionVisitor;

        impl<'de> de::Visitor<'de> for ConditionVisitor {
            type Value = ExpressionCondition;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a boolean or a non-empty rye-expr/1 expression string")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(ExpressionCondition::Boolean(value))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_string(value.to_owned())
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value.trim().is_empty() {
                    return Err(E::custom(
                        "condition expression must not be empty or whitespace-only",
                    ));
                }
                Ok(ExpressionCondition::Expression(value))
            }

            fn visit_none<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Err(E::custom(
                    "condition cannot be null; omit the field for an unconditional branch or hook",
                ))
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_none()
            }

            fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Err(E::invalid_type(de::Unexpected::Signed(_value), &self))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Err(E::invalid_type(de::Unexpected::Unsigned(value), &self))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Err(E::invalid_type(de::Unexpected::Float(value), &self))
            }

            fn visit_seq<A>(self, _sequence: A) -> Result<Self::Value, A::Error>
            where
                A: de::SeqAccess<'de>,
            {
                Err(de::Error::custom(
                    "condition arrays are not valid rye-expr/1 conditions",
                ))
            }

            fn visit_map<A>(self, _map: A) -> Result<Self::Value, A::Error>
            where
                A: de::MapAccess<'de>,
            {
                Err(de::Error::custom(
                    "structured path/op/value conditions are not supported; write one rye-expr/1 expression string",
                ))
            }
        }

        deserializer.deserialize_any(ConditionVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct Wrapper {
        #[serde(default)]
        condition: ExpressionCondition,
    }

    #[test]
    fn omitted_condition_is_distinct_from_explicit_null() {
        let omitted: Wrapper = serde_yaml::from_str("{}").unwrap();
        assert_eq!(omitted.condition, ExpressionCondition::Absent);

        let error = serde_yaml::from_str::<Wrapper>("condition: null")
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot be null"));
    }

    #[test]
    fn accepts_boolean_and_non_empty_expression() {
        let boolean: Wrapper = serde_yaml::from_str("condition: true").unwrap();
        assert_eq!(boolean.condition, ExpressionCondition::Boolean(true));

        let expression: Wrapper =
            serde_yaml::from_str("condition: 'state.ready && result.ok'").unwrap();
        assert_eq!(
            expression.condition,
            ExpressionCondition::Expression("state.ready && result.ok".to_string())
        );
    }

    #[test]
    fn rejects_empty_and_structured_legacy_conditions() {
        assert!(serde_yaml::from_str::<Wrapper>("condition: '   '").is_err());
        let error = serde_yaml::from_str::<Wrapper>(
            "condition:\n  path: state.ready\n  op: eq\n  value: true\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("structured path/op/value"));
    }
}
