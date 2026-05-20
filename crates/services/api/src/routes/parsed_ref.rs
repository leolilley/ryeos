//! `ParsedItemRef` — a canonical ref parsed exactly once at the
//! route-system boundary, then carried through the launch helper
//! without re-parsing.

use ryeos_engine::canonical_ref::CanonicalRef;

#[derive(Debug, Clone)]
pub struct ParsedItemRef {
    raw: String,
    kind: String,
}

impl ParsedItemRef {
    pub fn parse(s: &str) -> Result<Self, String> {
        let canonical = CanonicalRef::parse(s).map_err(|e| e.to_string())?;
        Ok(Self {
            raw: s.to_string(),
            kind: canonical.kind.to_string(),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_valid_ref() {
        let r = ParsedItemRef::parse("directive:my/agent").unwrap();
        assert_eq!(r.as_str(), "directive:my/agent");
        assert_eq!(r.kind(), "directive");
    }

    #[test]
    fn parse_accepts_any_kind() {
        for s in &["tool:foo/bar", "graph:a/b", "service:x", "knowledge:y/z"] {
            let r = ParsedItemRef::parse(s).unwrap();
            let kind = s.split(':').next().unwrap();
            assert_eq!(r.kind(), kind);
        }
    }

    #[test]
    fn parse_rejects_malformed() {
        let err = ParsedItemRef::parse("no-colon").unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn kind_returns_kind_portion() {
        assert_eq!(ParsedItemRef::parse("tool:hello").unwrap().kind(), "tool");
        assert_eq!(
            ParsedItemRef::parse("fictional_kind:any/path").unwrap().kind(),
            "fictional_kind"
        );
    }
}
