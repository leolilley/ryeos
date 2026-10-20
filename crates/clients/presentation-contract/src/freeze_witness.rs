//! Canonical preparation identity shared by typed producer and receipt reader.
use serde::Serialize;

pub fn preparation_identity(
    schema: u32,
    producer: &impl Serialize,
    prepared_engine_seq: u64,
    root_binding: &impl Serialize,
    request_bounds: &impl Serialize,
    normalized_body: &serde_json::Value,
) -> Result<String, &'static str> {
    #[derive(Serialize)]
    struct Identity<'a, P, R, B> {
        domain: &'static str,
        schema: u32,
        producer: &'a P,
        prepared_engine_seq: u64,
        root_binding: &'a R,
        request_bounds: &'a B,
        body: &'a serde_json::Value,
    }
    crate::identity::identity_from_serializable(&Identity {
        domain: "ryeos.ui.presentation-operation.v2",
        schema,
        producer,
        prepared_engine_seq,
        root_binding,
        request_bounds,
        body: normalized_body,
    })
}
