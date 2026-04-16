use std::collections::HashSet;

use anyhow::Result;

use crate::cas::CasStore;
use crate::refs::RefStore;

pub struct MarkCacheEntry {
    pub root_hashes: Vec<String>,
    pub reachable: HashSet<String>,
}

pub fn save_mark_cache(
    cas: &CasStore,
    refs: &RefStore,
    roots: &[String],
    reachable: &HashSet<String>,
) -> Result<()> {
    let mut sorted_reachable: Vec<&String> = reachable.iter().collect();
    sorted_reachable.sort();
    let data = serde_json::json!({
        "kind": "gc_mark_cache",
        "root_hashes": roots,
        "reachable": sorted_reachable,
    });
    let hash = cas.store_object(&data)?;
    refs.write_ref("internal/last_mark_cache", &hash)?;
    Ok(())
}

pub fn load_mark_cache(cas: &CasStore, refs: &RefStore) -> Option<MarkCacheEntry> {
    let hash = refs.read_ref("internal/last_mark_cache").ok()??;
    let obj = cas.get_object(&hash).ok()??;

    let root_hashes: Vec<String> = obj
        .get("root_hashes")?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    let reachable: HashSet<String> = obj
        .get("reachable")?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    Some(MarkCacheEntry {
        root_hashes,
        reachable,
    })
}
