//! Deterministic lexical (BM25) retrieval over a verified knowledge
//! corpus. Semantic/vector retrieval is deliberately deferred — this is
//! the read side that works offline with no embedding model.
//!
//! Scoring covers the document body (frontmatter stripped) plus selected
//! frontmatter fields (title, tags, category) so a query can match on
//! metadata as well as prose.
//!
//! NOTE: BM25 statistics (df, avgdl) are computed over the *filtered*
//! candidate set, so this is "search within this scope". Scores are
//! comparable within one filter set but NOT across different filters.

use std::collections::HashMap;

use crate::frontmatter::{parse_frontmatter, strip_frontmatter};
use crate::types::{KnowledgeError, QueryFilters, QueryMatch, QueryOutput, QueryPayload};

/// BM25 term-frequency saturation.
const K1: f64 = 1.5;
/// BM25 length-normalization.
const B: f64 = 0.75;
/// Excerpt window length, in characters.
const EXCERPT_CHARS: usize = 240;
/// Characters of lead-in before the first matched term in an excerpt.
const EXCERPT_LEAD: usize = 40;

struct Doc {
    item_ref: String,
    title: Option<String>,
    body: String,
    /// term → frequency over the searchable text.
    tokens: HashMap<String, usize>,
    len: usize,
    metadata: serde_json::Value,
    digest: String,
    raw_content: String,
}

pub fn query(payload: &QueryPayload) -> Result<QueryOutput, KnowledgeError> {
    let q = payload.inputs.query.trim();
    if q.is_empty() {
        return Err(KnowledgeError::InvalidInput {
            op: "query".into(),
            reason: "query string is empty".into(),
        });
    }
    let query_terms = unique_tokens(q);
    if query_terms.is_empty() {
        return Ok(QueryOutput {
            query: q.to_string(),
            matches: Vec::new(),
        });
    }

    // 1. Build candidate docs with filters applied. BM25 statistics (df,
    //    avgdl) are computed over the filtered candidate set so scores are
    //    relative to what the caller actually searched.
    let mut docs: Vec<Doc> = Vec::new();
    for (item_ref, item) in &payload.items_by_ref {
        if !ref_prefix_ok(item_ref, &payload.inputs.filters) {
            continue;
        }
        let fm = parse_frontmatter(&item.raw_content);
        if !meta_filters_ok(&fm, &payload.inputs.filters) {
            continue;
        }
        let body = strip_frontmatter(&item.raw_content, item_ref)
            .unwrap_or_else(|_| item.raw_content.clone());
        let title = fm
            .get("title")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        // Searchable text: title + tags/category + body. Metadata terms
        // are folded in so a tag or title word contributes to relevance.
        let mut searchable = String::new();
        if let Some(t) = &title {
            searchable.push_str(t);
            searchable.push(' ');
        }
        searchable.push_str(&meta_text(&fm));
        searchable.push(' ');
        searchable.push_str(&body);

        let tokens = term_freq(&searchable);
        let len = tokens.values().sum();
        docs.push(Doc {
            item_ref: item_ref.clone(),
            title,
            body,
            tokens,
            len,
            metadata: item.metadata.clone(),
            digest: item.raw_content_digest.clone(),
            raw_content: item.raw_content.clone(),
        });
    }

    if docs.is_empty() {
        return Ok(QueryOutput {
            query: q.to_string(),
            matches: Vec::new(),
        });
    }

    // 2. Document frequency per query term over the candidate set.
    let n = docs.len() as f64;
    let total_len: usize = docs.iter().map(|d| d.len).sum();
    let avgdl = (total_len as f64 / n).max(1.0);
    let mut df: HashMap<&str, usize> = HashMap::new();
    for term in &query_terms {
        let count = docs.iter().filter(|d| d.tokens.contains_key(term)).count();
        df.insert(term.as_str(), count);
    }

    // 3. Score and rank.
    let mut scored: Vec<(f64, &Doc)> = docs
        .iter()
        .map(|d| (bm25(d, &query_terms, &df, n, avgdl), d))
        .filter(|(s, _)| *s > 0.0)
        .collect();

    // Highest score first; ties broken by ref for deterministic output.
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.item_ref.cmp(&b.1.item_ref))
    });
    scored.truncate(payload.inputs.limit);

    let matches = scored
        .into_iter()
        .map(|(score, d)| QueryMatch {
            item_ref: d.item_ref.clone(),
            score,
            title: d.title.clone(),
            excerpt: make_excerpt(&d.body, &query_terms),
            metadata: d.metadata.clone(),
            raw_content_digest: d.digest.clone(),
            content: payload
                .inputs
                .include_content
                .then(|| d.raw_content.clone()),
        })
        .collect();

    Ok(QueryOutput {
        query: q.to_string(),
        matches,
    })
}

fn bm25(d: &Doc, query_terms: &[String], df: &HashMap<&str, usize>, n: f64, avgdl: f64) -> f64 {
    let mut score = 0.0;
    for term in query_terms {
        let tf = *d.tokens.get(term).unwrap_or(&0) as f64;
        if tf == 0.0 {
            continue;
        }
        let dfi = *df.get(term.as_str()).unwrap_or(&0) as f64;
        // idf with the +1 form so it stays non-negative even when a term
        // appears in every candidate document.
        let idf = ((n - dfi + 0.5) / (dfi + 0.5) + 1.0).ln();
        let denom = tf + K1 * (1.0 - B + B * (d.len as f64 / avgdl));
        score += idf * (tf * (K1 + 1.0)) / denom;
    }
    score
}

/// Lowercase alphanumeric word tokens, in order.
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

/// Unique query tokens (so a repeated query word is not double-counted).
fn unique_tokens(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    tokenize(text)
        .into_iter()
        .filter(|t| seen.insert(t.clone()))
        .collect()
}

fn term_freq(text: &str) -> HashMap<String, usize> {
    let mut map = HashMap::new();
    for tok in tokenize(text) {
        *map.entry(tok).or_insert(0) += 1;
    }
    map
}

fn ref_prefix_ok(item_ref: &str, filters: &QueryFilters) -> bool {
    filters.ref_prefixes.is_empty()
        || filters
            .ref_prefixes
            .iter()
            .any(|p| item_ref.starts_with(p))
}

/// Tags/categories filters are intersection tests against the item's
/// frontmatter. An item with no tags fails a non-empty `tags` filter.
fn meta_filters_ok(fm: &serde_json::Value, filters: &QueryFilters) -> bool {
    if !filters.tags.is_empty() {
        let item_tags = string_set(fm.get("tags"));
        if !filters.tags.iter().any(|t| item_tags.contains(t)) {
            return false;
        }
    }
    if !filters.categories.is_empty() {
        let mut cats = string_set(fm.get("categories"));
        if let Some(c) = fm.get("category").and_then(|v| v.as_str()) {
            cats.insert(c.to_string());
        }
        if !filters.categories.iter().any(|c| cats.contains(c)) {
            return false;
        }
    }
    true
}

/// Flatten a frontmatter value that may be a string or array-of-strings
/// into a set of strings.
fn string_set(v: Option<&serde_json::Value>) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    match v {
        Some(serde_json::Value::Array(arr)) => {
            for item in arr {
                if let Some(s) = item.as_str() {
                    set.insert(s.to_string());
                }
            }
        }
        Some(serde_json::Value::String(s)) => {
            set.insert(s.clone());
        }
        _ => {}
    }
    set
}

/// Concatenate searchable metadata text (tags + category) for scoring.
fn meta_text(fm: &serde_json::Value) -> String {
    let mut parts: Vec<String> = string_set(fm.get("tags")).into_iter().collect();
    parts.extend(string_set(fm.get("categories")));
    if let Some(c) = fm.get("category").and_then(|v| v.as_str()) {
        parts.push(c.to_string());
    }
    parts.join(" ")
}

/// A character-windowed excerpt around the earliest matched query term,
/// or the document lead when no term occurs in the body.
fn make_excerpt(body: &str, terms: &[String]) -> String {
    let lower = body.to_lowercase();
    let mut earliest: Option<usize> = None;
    for term in terms {
        if let Some(byte_idx) = lower.find(term.as_str()) {
            let char_idx = lower[..byte_idx].chars().count();
            earliest = Some(earliest.map_or(char_idx, |e| e.min(char_idx)));
        }
    }

    let chars: Vec<char> = body.chars().collect();
    let start = earliest.map(|i| i.saturating_sub(EXCERPT_LEAD)).unwrap_or(0);
    let end = (start + EXCERPT_CHARS).min(chars.len());
    let core: String = chars[start..end].iter().collect();
    let core = core.trim();

    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(core);
    if end < chars.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_runtime::op_wire::{TrustClass, VerifiedItem};
    use std::collections::BTreeMap;

    use crate::types::QueryInputs;

    fn item(body: &str) -> VerifiedItem {
        VerifiedItem {
            raw_content: body.to_string(),
            raw_content_digest: format!("sha256:{}", body.len()),
            metadata: serde_json::json!({"source_path": "x"}),
            trust_class: TrustClass::TrustedBundle,
        }
    }

    fn payload(items: &[(&str, &str)], inputs: QueryInputs) -> QueryPayload {
        let mut map = BTreeMap::new();
        for (r, body) in items {
            map.insert(r.to_string(), item(body));
        }
        QueryPayload {
            items_by_ref: map,
            edges: Vec::new(),
            inputs,
        }
    }

    fn inputs(q: &str) -> QueryInputs {
        QueryInputs {
            query: q.to_string(),
            limit: 10,
            include_content: false,
            filters: Default::default(),
        }
    }

    #[test]
    fn returns_relevant_item_first() {
        let p = payload(
            &[
                ("k/cats", "---\ntitle: Cats\ntags: [animals]\n---\nCats purr and chase mice."),
                ("k/cars", "---\ntitle: Cars\ntags: [vehicles]\n---\nCars have engines and wheels."),
            ],
            inputs("purring cats"),
        );
        let out = query(&p).unwrap();
        assert!(!out.matches.is_empty());
        assert_eq!(out.matches[0].item_ref, "k/cats");
        assert_eq!(out.matches[0].title.as_deref(), Some("Cats"));
    }

    #[test]
    fn excerpt_includes_matched_text() {
        let p = payload(
            &[("k/doc", "---\ntitle: Doc\n---\nThe quick brown fox jumps over the lazy dog.")],
            inputs("fox"),
        );
        let out = query(&p).unwrap();
        assert!(out.matches[0].excerpt.to_lowercase().contains("fox"));
    }

    #[test]
    fn limit_caps_results() {
        let mut inp = inputs("alpha");
        inp.limit = 1;
        let p = payload(
            &[
                ("k/a", "alpha alpha alpha"),
                ("k/b", "alpha beta"),
                ("k/c", "alpha gamma"),
            ],
            inp,
        );
        let out = query(&p).unwrap();
        assert_eq!(out.matches.len(), 1);
    }

    #[test]
    fn tag_filter_excludes_non_matching() {
        let p = payload(
            &[
                ("k/a", "---\ntitle: A\ntags: [keep]\n---\nshared term here"),
                ("k/b", "---\ntitle: B\ntags: [drop]\n---\nshared term here"),
            ],
            QueryInputs {
                query: "shared".into(),
                limit: 10,
                include_content: false,
                filters: QueryFilters {
                    tags: vec!["keep".into()],
                    ..Default::default()
                },
            },
        );
        let out = query(&p).unwrap();
        assert_eq!(out.matches.len(), 1);
        assert_eq!(out.matches[0].item_ref, "k/a");
    }

    #[test]
    fn ref_prefix_filter_restricts_corpus() {
        let p = payload(
            &[
                ("memory/a", "needle"),
                ("notes/b", "needle"),
            ],
            QueryInputs {
                query: "needle".into(),
                limit: 10,
                include_content: false,
                filters: QueryFilters {
                    ref_prefixes: vec!["memory/".into()],
                    ..Default::default()
                },
            },
        );
        let out = query(&p).unwrap();
        assert_eq!(out.matches.len(), 1);
        assert_eq!(out.matches[0].item_ref, "memory/a");
    }

    #[test]
    fn include_content_returns_raw() {
        let mut inp = inputs("hello");
        inp.include_content = true;
        let p = payload(&[("k/a", "hello world")], inp);
        let out = query(&p).unwrap();
        assert_eq!(out.matches[0].content.as_deref(), Some("hello world"));
    }

    #[test]
    fn html_signed_markdown_excerpt_excludes_signature_and_frontmatter() {
        // A signed `.md` knowledge doc: HTML signature comment + `---`
        // frontmatter + body. BM25/excerpt must operate on the BODY only —
        // the excerpt for a body term must not leak the signature or YAML.
        let doc = "<!-- ryeos:signed:2026-01-01T00:00:00Z:h:s:fp -->\n---\ntitle: Felines\ntags: [animals]\n---\nWhiskers help cats sense their surroundings.";
        let p = payload(&[("k/cats", doc)], inputs("whiskers"));
        let out = query(&p).unwrap();
        assert_eq!(out.matches.len(), 1);
        let ex = &out.matches[0].excerpt;
        assert!(ex.to_lowercase().contains("whiskers"), "excerpt: {ex}");
        assert!(!ex.contains("ryeos:signed"), "signature leaked into excerpt: {ex}");
        assert!(!ex.contains("title:"), "frontmatter leaked into excerpt: {ex}");
        // Title still surfaced from frontmatter.
        assert_eq!(out.matches[0].title.as_deref(), Some("Felines"));
    }

    #[test]
    fn empty_query_is_invalid() {
        let p = payload(&[("k/a", "x")], inputs("   "));
        assert!(query(&p).is_err());
    }

    #[test]
    fn no_match_returns_empty() {
        let p = payload(&[("k/a", "completely unrelated")], inputs("zzzznotpresent"));
        assert!(query(&p).unwrap().matches.is_empty());
    }
}
