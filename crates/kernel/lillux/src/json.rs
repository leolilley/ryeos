use serde::de::DeserializeOwned;

/// Deserialize JSON while growing the parser stack on demand.
///
/// This keeps deeply nested, producer-authored JSON from exhausting a bounded
/// runtime worker stack while retaining serde_json's normal syntax and trailing
/// input validation.
pub fn deserialize_json_str_stack_safe<T>(raw: &str) -> Result<T, serde_json::Error>
where
    T: DeserializeOwned,
{
    let mut deserializer = serde_json::Deserializer::from_str(raw);
    let value = T::deserialize(serde_stacker::Deserializer::new(&mut deserializer))?;
    deserializer.end()?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_json_does_not_exhaust_a_bounded_worker_stack() {
        let mut value = serde_json::json!("leaf");
        for _ in 0..32 {
            value = serde_json::json!({"nested": value});
        }
        let raw = serde_json::to_string(&value).unwrap();

        let result = std::thread::Builder::new()
            .name("stack-safe-json".to_string())
            .stack_size(256 * 1024)
            .spawn(move || deserialize_json_str_stack_safe::<serde_json::Value>(&raw))
            .unwrap()
            .join();

        result
            .expect("stack-safe JSON decoding must not overflow the worker stack")
            .expect("nested JSON must decode");
    }
}
