use serde_json::Value;

pub fn extract_child_hashes(obj: &Value) -> Vec<String> {
    let mut hashes = Vec::new();
    collect_hash_refs(obj, &mut hashes);
    hashes
}

/// Recursively walk a JSON object, extracting CAS reference hashes.
///
/// Convention: `*_hash` (string) and `*_hashes` (array of strings or map
/// whose values are strings) are reserved as CAS reference fields.
fn collect_hash_refs(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if k.ends_with("_hash") {
                    if let Some(h) = v.as_str() {
                        out.push(h.to_string());
                    }
                } else if k.ends_with("_hashes") {
                    if let Some(arr) = v.as_array() {
                        for x in arr {
                            if let Some(h) = x.as_str() {
                                out.push(h.to_string());
                            }
                        }
                    } else if let Some(obj) = v.as_object() {
                        for val in obj.values() {
                            if let Some(h) = val.as_str() {
                                out.push(h.to_string());
                            }
                        }
                    }
                }
                collect_hash_refs(v, out);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collect_hash_refs(v, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_single_hash_fields() {
        let obj = json!({
            "kind": "item_source",
            "content_blob_hash": "abc123",
            "other_field": "ignored"
        });
        assert_eq!(extract_child_hashes(&obj), vec!["abc123"]);
    }

    #[test]
    fn extracts_hash_array_fields() {
        let obj = json!({
            "kind": "project_snapshot",
            "project_manifest_hash": "aaa",
            "parent_hashes": ["bbb", "ccc"],
        });
        let mut hashes = extract_child_hashes(&obj);
        hashes.sort();
        assert_eq!(hashes, vec!["aaa", "bbb", "ccc"]);
    }

    #[test]
    fn recurses_into_nested_objects() {
        let obj = json!({
            "kind": "bundle",
            "nested": {
                "deep_hash": "deep123"
            },
            "items_hashes": ["h1", "h2"]
        });
        let mut hashes = extract_child_hashes(&obj);
        hashes.sort();
        assert_eq!(hashes, vec!["deep123", "h1", "h2"]);
    }

    #[test]
    fn recurses_into_nested_arrays() {
        let obj = json!({
            "artifacts": [
                { "blob_hash": "b1" },
                { "blob_hash": "b2" }
            ]
        });
        assert_eq!(extract_child_hashes(&obj), vec!["b1", "b2"]);
    }

    #[test]
    fn ignores_non_hash_fields() {
        let obj = json!({
            "kind": "something",
            "name": "test",
            "count": 42,
            "data": { "foo": "bar" }
        });
        assert!(extract_child_hashes(&obj).is_empty());
    }

    #[test]
    fn extracts_source_manifest_item_source_hashes() {
        let obj = json!({
            "kind": "source_manifest",
            "item_source_hashes": {
                "src/main.rs": "item_hash_1",
                "src/lib.rs": "item_hash_2"
            }
        });
        let mut hashes = extract_child_hashes(&obj);
        hashes.sort();
        assert_eq!(hashes, vec!["item_hash_1", "item_hash_2"]);
    }

    #[test]
    fn extracts_hashes_from_map_values() {
        let obj = json!({
            "kind": "source_manifest",
            "item_source_hashes": {
                "src/main.rs": "hash1",
                "src/lib.rs": "hash2"
            }
        });
        let mut hashes = extract_child_hashes(&obj);
        hashes.sort();
        assert_eq!(hashes, vec!["hash1", "hash2"]);
    }

    #[test]
    fn extracts_execution_snapshot_hashes() {
        let obj = json!({
            "kind": "execution_snapshot",
            "thread_id": "t1",
            "project_manifest_hash": "manifest1",
            "user_manifest_hash": "manifest2",
        });
        let mut hashes = extract_child_hashes(&obj);
        hashes.sort();
        assert_eq!(hashes, vec!["manifest1", "manifest2"]);
    }

    #[test]
    fn extracts_runtime_outputs_bundle() {
        let obj = json!({
            "kind": "runtime_outputs_bundle",
            "execution_snapshot_hash": "snap1",
            "output_manifest_hash": "out1",
            "artifacts": [
                { "blob_hash": "blob1" },
                { "blob_hash": "blob2" }
            ]
        });
        let mut hashes = extract_child_hashes(&obj);
        hashes.sort();
        assert_eq!(hashes, vec!["blob1", "blob2", "out1", "snap1"]);
    }

    #[test]
    fn new_kind_zero_gc_changes() {
        let obj = json!({
            "kind": "custom_report",
            "report_hash": "r1",
            "dependency_hashes": ["d1", "d2"],
            "metadata": {
                "thumbnail_hash": "thumb1"
            }
        });
        let mut hashes = extract_child_hashes(&obj);
        hashes.sort();
        assert_eq!(hashes, vec!["d1", "d2", "r1", "thumb1"]);
    }
}
