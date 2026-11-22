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
    let mut growing = serde_stacker::Deserializer::new(&mut deserializer);
    // The growth check runs once per nesting level, and between two checks
    // the decoder descends through several serde frames. In a debug build a
    // single derived-struct frame here is around 100 KB, so the adapter's
    // 64 KB default red zone let the decode run off a 4 MiB worker stack
    // before it ever moved to a heap segment. The red zone must cover the
    // frames between checks; the segment size keeps regrowth rare.
    growing.red_zone = JSON_STACK_RED_ZONE_BYTES;
    growing.stack_size = JSON_STACK_SEGMENT_BYTES;
    let value = T::deserialize(growing)?;
    deserializer.end()?;
    Ok(value)
}

const JSON_STACK_RED_ZONE_BYTES: usize = 1024 * 1024;
const JSON_STACK_SEGMENT_BYTES: usize = 8 * 1024 * 1024;

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

        // Smaller than the red zone: every level must decode on heap segments.
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
