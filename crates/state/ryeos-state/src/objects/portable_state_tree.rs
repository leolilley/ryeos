//! Canonical confidential state-tree attachment for generic restore manifests.
//!
//! This is a typed blob, not an outer CAS object. `StateManifest` remains the
//! authoritative restore object. The workload-declared selector contract
//! supplies the limits and classifications used to validate these bytes.

use std::collections::BTreeSet;
use std::path::{Component, Path};

use anyhow::{Context, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use super::{
    PortableSessionStateClass, PortableSessionStateContract, PortableSessionStateSelector,
};

pub const PORTABLE_STATE_TREE_KIND: &str = "portable_state_tree";
pub const PORTABLE_STATE_TREE_SCHEMA: u32 = 1;
pub const PORTABLE_STATE_TREE_MEDIA_TYPE: &str = "application/vnd.ryeos.portable-state-tree+json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortableStateTreeFile {
    /// Exact admitted selector pattern which classified this entry.
    pub selector: String,
    /// Canonical profile-home-relative file path.
    pub path: String,
    pub size_bytes: u64,
    pub content_sha256: String,
    pub content_base64: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortableStateTree {
    pub schema: u32,
    pub kind: String,
    pub restore_contract: String,
    pub upstream_session_id_sha256: String,
    pub files: Vec<PortableStateTreeFile>,
}

impl PortableStateTree {
    pub fn new(
        contract: &PortableSessionStateContract,
        upstream_session_id: &str,
        files: Vec<PortableStateTreeFile>,
    ) -> anyhow::Result<Self> {
        let tree = Self {
            schema: PORTABLE_STATE_TREE_SCHEMA,
            kind: PORTABLE_STATE_TREE_KIND.to_string(),
            restore_contract: contract.restore_contract.clone(),
            upstream_session_id_sha256: lillux::sha256_hex(upstream_session_id.as_bytes()),
            files,
        };
        tree.validate(contract, upstream_session_id)?;
        Ok(tree)
    }

    pub fn from_canonical_bytes(
        bytes: &[u8],
        contract: &PortableSessionStateContract,
        upstream_session_id: &str,
    ) -> anyhow::Result<Self> {
        let tree: Self = serde_json::from_slice(bytes).context("decode portable state tree")?;
        tree.validate(contract, upstream_session_id)?;
        if tree.canonical_bytes()? != bytes {
            bail!("portable state tree bytes are not canonical JSON");
        }
        Ok(tree)
    }

    pub fn canonical_bytes(&self) -> anyhow::Result<Vec<u8>> {
        Ok(lillux::canonical_json(&serde_json::to_value(self)?)?.into_bytes())
    }

    pub fn content_bytes(file: &PortableStateTreeFile) -> anyhow::Result<Vec<u8>> {
        base64::engine::general_purpose::STANDARD
            .decode(&file.content_base64)
            .context("decode portable state-tree file")
    }

    pub fn validate(
        &self,
        contract: &PortableSessionStateContract,
        upstream_session_id: &str,
    ) -> anyhow::Result<()> {
        contract.validate()?;
        if self.schema != PORTABLE_STATE_TREE_SCHEMA
            || self.kind != PORTABLE_STATE_TREE_KIND
            || self.restore_contract != contract.restore_contract
        {
            bail!("portable state tree is not the exact admitted contract");
        }
        super::thread_snapshot::validate_canonical_hash(
            "portable state-tree upstream session identity",
            &self.upstream_session_id_sha256,
        )?;
        if self.upstream_session_id_sha256 != lillux::sha256_hex(upstream_session_id.as_bytes()) {
            bail!("portable state tree belongs to another upstream session");
        }
        if self.files.is_empty() || self.files.len() > contract.max_entries as usize {
            bail!("portable state tree has an invalid file count");
        }

        let mut paths = BTreeSet::new();
        let mut selector_counts = vec![0_u32; contract.selectors.len()];
        let mut prior: Option<(&str, &str)> = None;
        let mut total_bytes = 0_u64;
        for file in &self.files {
            validate_relative_file_path(&file.path, contract.max_depth)?;
            if !paths.insert(file.path.as_str()) {
                bail!(
                    "portable state tree contains duplicate path {:?}",
                    file.path
                );
            }
            let ordering_key = (file.selector.as_str(), file.path.as_str());
            if prior.is_some_and(|prior| prior >= ordering_key) {
                bail!("portable state-tree files are not canonically ordered");
            }
            prior = Some(ordering_key);

            let selector_index = contract
                .selectors
                .binary_search_by(|selector| selector.pattern.as_str().cmp(&file.selector))
                .map_err(|_| anyhow::anyhow!("portable state tree uses an unadmitted selector"))?;
            let selector = &contract.selectors[selector_index];
            if selector.class != PortableSessionStateClass::PortableSessionState
                || !selector_matches(selector, &file.path, upstream_session_id)?
            {
                bail!("portable state-tree path is not selected as portable session state");
            }
            selector_counts[selector_index] = selector_counts[selector_index]
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("portable state selector count overflow"))?;
            if selector_counts[selector_index] > selector.max_matches {
                bail!("portable state selector exceeds its admitted match ceiling");
            }

            if file.size_bytes > contract.max_file_bytes {
                bail!("portable state-tree file exceeds its admitted byte ceiling");
            }
            let bytes = Self::content_bytes(file)?;
            if bytes.len() as u64 != file.size_bytes {
                bail!("portable state-tree file size contradicts its content");
            }
            super::thread_snapshot::validate_canonical_hash(
                "portable state-tree content digest",
                &file.content_sha256,
            )?;
            if lillux::sha256_hex(&bytes) != file.content_sha256 {
                bail!("portable state-tree content digest mismatch");
            }
            total_bytes = total_bytes
                .checked_add(file.size_bytes)
                .ok_or_else(|| anyhow::anyhow!("portable state-tree byte total overflow"))?;
            if total_bytes > contract.max_total_bytes {
                bail!("portable state tree exceeds its admitted aggregate byte ceiling");
            }
        }

        for (index, selector) in contract.selectors.iter().enumerate() {
            if selector.class == PortableSessionStateClass::PortableSessionState
                && selector_counts[index] != 1
            {
                bail!("portable state selector did not resolve exactly one session file");
            }
        }
        Ok(())
    }
}

pub fn selector_matches(
    selector: &PortableSessionStateSelector,
    path: &str,
    upstream_session_id: &str,
) -> anyhow::Result<bool> {
    if upstream_session_id.is_empty()
        || upstream_session_id.len() > 4096
        || upstream_session_id.chars().any(char::is_control)
        || upstream_session_id.contains('/')
    {
        bail!("upstream session identity is not safe for portable-state selection");
    }
    let pattern = selector
        .pattern
        .replace("{session_id}", upstream_session_id);
    let pattern_segments = pattern.split('/').collect::<Vec<_>>();
    let path_segments = path.split('/').collect::<Vec<_>>();
    Ok(glob_path_matches(&pattern_segments, &path_segments))
}

/// Select the one most-specific admitted classifier for a relative path.
/// Overlap is useful for a broad forbidden/cache subtree plus one exact
/// session-bound portable file. Equal-specificity overlap is ambiguous and
/// fails closed.
pub fn classify_portable_state_path<'a>(
    contract: &'a PortableSessionStateContract,
    path: &str,
    upstream_session_id: &str,
) -> anyhow::Result<&'a PortableSessionStateSelector> {
    let mut selected: Option<(&PortableSessionStateSelector, (usize, usize, usize))> = None;
    for selector in &contract.selectors {
        if !selector_matches(selector, path, upstream_session_id)? {
            continue;
        }
        let wildcards = selector.pattern.matches('*').count();
        let literal_bytes = selector.pattern.len().saturating_sub(wildcards);
        let score = (
            literal_bytes,
            usize::MAX - wildcards,
            selector.pattern.matches('/').count(),
        );
        match selected {
            None => selected = Some((selector, score)),
            Some((_, prior)) if score > prior => selected = Some((selector, score)),
            Some((prior_selector, prior)) if score == prior => {
                bail!(
                    "portable-state path {path:?} ambiguously matches {:?} and {:?}",
                    prior_selector.pattern,
                    selector.pattern
                );
            }
            Some(_) => {}
        }
    }
    selected
        .map(|(selector, _)| selector)
        .ok_or_else(|| anyhow::anyhow!("portable-state path {path:?} has no admitted classifier"))
}

fn glob_path_matches(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((segment, rest)) if *segment == "**" => {
            glob_path_matches(rest, path)
                || path
                    .split_first()
                    .is_some_and(|(_, path_rest)| glob_path_matches(pattern, path_rest))
        }
        Some((segment, rest)) => path.split_first().is_some_and(|(value, path_rest)| {
            glob_segment_matches(segment, value) && glob_path_matches(rest, path_rest)
        }),
    }
}

fn glob_segment_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let mut reachable = vec![false; value.len() + 1];
    reachable[0] = true;
    for byte in pattern {
        if *byte == b'*' {
            for index in 1..=value.len() {
                reachable[index] = reachable[index] || reachable[index - 1];
            }
        } else {
            for index in (1..=value.len()).rev() {
                reachable[index] = reachable[index - 1] && value[index - 1] == *byte;
            }
            reachable[0] = false;
        }
    }
    reachable[value.len()]
}

fn validate_relative_file_path(path: &str, max_depth: u16) -> anyhow::Result<()> {
    if path.is_empty()
        || path.len() > 4096
        || path.chars().any(char::is_control)
        || Path::new(path).is_absolute()
    {
        bail!("portable state-tree path is not bounded and relative");
    }
    let components = Path::new(path).components().collect::<Vec<_>>();
    if components.is_empty()
        || components.len() > usize::from(max_depth)
        || components
            .iter()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("portable state-tree path is not canonical");
    }
    if components.iter().any(|component| {
        component.as_os_str().to_str().is_none_or(|segment| {
            segment.is_empty() || segment.len() > 255 || segment.contains(['/', '\0'])
        })
    }) {
        bail!("portable state-tree path contains an invalid component");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn contract() -> PortableSessionStateContract {
        PortableSessionStateContract {
            schema: 1,
            restore_contract: "ryeos.worker_session.restore.v1".to_string(),
            max_depth: 8,
            max_entries: 4,
            max_file_bytes: 1024,
            max_total_bytes: 2048,
            selectors: vec![PortableSessionStateSelector {
                pattern: "sessions/*/rollout-*-{session_id}.jsonl".to_string(),
                class: PortableSessionStateClass::PortableSessionState,
                max_matches: 1,
            }],
        }
    }

    #[test]
    fn exact_session_file_round_trips_canonically() {
        let bytes = b"one session";
        let tree = PortableStateTree::new(
            &contract(),
            "thread-1",
            vec![PortableStateTreeFile {
                selector: "sessions/*/rollout-*-{session_id}.jsonl".to_string(),
                path: "sessions/day/rollout-a-thread-1.jsonl".to_string(),
                size_bytes: bytes.len() as u64,
                content_sha256: lillux::sha256_hex(bytes),
                content_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            }],
        )
        .unwrap();
        let encoded = tree.canonical_bytes().unwrap();
        assert_eq!(
            PortableStateTree::from_canonical_bytes(&encoded, &contract(), "thread-1").unwrap(),
            tree
        );
    }

    #[test]
    fn another_session_and_noncanonical_bytes_are_refused() {
        let bytes = b"wrong session";
        let file = PortableStateTreeFile {
            selector: "sessions/*/rollout-*-{session_id}.jsonl".to_string(),
            path: "sessions/day/rollout-a-thread-2.jsonl".to_string(),
            size_bytes: bytes.len() as u64,
            content_sha256: lillux::sha256_hex(bytes),
            content_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        };
        assert!(PortableStateTree::new(&contract(), "thread-1", vec![file]).is_err());

        let tree = PortableStateTree::new(
            &contract(),
            "thread-1",
            vec![PortableStateTreeFile {
                selector: "sessions/*/rollout-*-{session_id}.jsonl".to_string(),
                path: "sessions/day/rollout-a-thread-1.jsonl".to_string(),
                size_bytes: 1,
                content_sha256: lillux::sha256_hex(b"x"),
                content_base64: base64::engine::general_purpose::STANDARD.encode(b"x"),
            }],
        )
        .unwrap();
        let mut noncanonical = serde_json::to_vec_pretty(&tree).unwrap();
        noncanonical.push(b'\n');
        assert!(
            PortableStateTree::from_canonical_bytes(&noncanonical, &contract(), "thread-1")
                .is_err()
        );
    }

    #[test]
    fn exact_session_classifier_overrides_broad_forbidden_tree() {
        let mut contract = contract();
        contract.selectors.insert(
            0,
            PortableSessionStateSelector {
                pattern: "sessions/**".to_string(),
                class: PortableSessionStateClass::ForbiddenOrUnknown,
                max_matches: 4,
            },
        );
        contract
            .selectors
            .sort_by(|left, right| left.pattern.cmp(&right.pattern));
        contract.validate().unwrap();
        let selected = classify_portable_state_path(
            &contract,
            "sessions/day/rollout-a-thread-1.jsonl",
            "thread-1",
        )
        .unwrap();
        assert_eq!(
            selected.class,
            PortableSessionStateClass::PortableSessionState
        );
        assert_eq!(
            classify_portable_state_path(&contract, "sessions/day/unrelated", "thread-1")
                .unwrap()
                .class,
            PortableSessionStateClass::ForbiddenOrUnknown
        );
    }
}
