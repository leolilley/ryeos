use std::fmt;

use crate::error::EngineError;

/// Suffix modifiers on canonical refs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RefSuffix {
    /// `@cap:<signature>:<fingerprint>:<constraints_hash>`
    Capability {
        signature: String,
        fingerprint: String,
        constraints_hash: String,
    },
    /// `@sig:<content_hash>:<signature>`
    Signed {
        content_hash: String,
        signature: String,
    },
    /// `@t:<iso_timestamp>`
    Temporal { at: String },
}

/// A parsed canonical item reference.
///
/// Grammar: `<kind>:<bare_id>[@<suffix>]`
///
/// The parser accepts any kind string — kind validation against the
/// registry happens during resolution, not during parsing. This keeps
/// the parser kind-agnostic: adding a new kind requires only adding
/// an extractor schema, not changing engine code.
///
/// Rejects bare refs, legacy formats, and anything without an explicit
/// `kind:bare_id` structure.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CanonicalRef {
    /// The kind string, e.g. `"tool"`, `"directive"`, `"graph"`
    pub kind: String,
    /// The bare item ID, e.g. `"ryeos/bash/bash"`
    pub bare_id: String,
    /// Optional suffix modifier
    pub suffix: Option<RefSuffix>,
}

impl CanonicalRef {
    /// Parse a canonical ref string.
    ///
    /// Rejects bare refs, legacy formats, and anything without
    /// an explicit `kind:bare_id` structure. Does NOT validate the
    /// kind string against a registry — that happens at resolution time.
    pub fn parse(input: &str) -> Result<Self, EngineError> {
        if input.is_empty() {
            return Err(EngineError::MalformedRef {
                input: input.to_owned(),
                reason: "empty ref string".to_owned(),
            });
        }

        // Split kind from the rest at the first ':'
        let colon_pos = input.find(':').ok_or_else(|| EngineError::BareRefRejected {
            input: input.to_owned(),
        })?;

        let kind_str = &input[..colon_pos];
        let remainder = &input[colon_pos + 1..];

        if kind_str.is_empty() {
            return Err(EngineError::MalformedRef {
                input: input.to_owned(),
                reason: "empty kind prefix".to_owned(),
            });
        }

        // Validate kind characters: lowercase alphanumeric, '_', and '-'
        if !kind_str
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        {
            return Err(EngineError::MalformedRef {
                input: input.to_owned(),
                reason: format!("kind contains invalid characters: {kind_str}"),
            });
        }

        if remainder.is_empty() {
            return Err(EngineError::MalformedRef {
                input: input.to_owned(),
                reason: "empty bare_id after kind".to_owned(),
            });
        }

        // Split bare_id from optional suffix at first '@'
        let (bare_id, suffix) = if let Some(at_pos) = remainder.find('@') {
            let bare = &remainder[..at_pos];
            let suffix_str = &remainder[at_pos + 1..];
            (bare.to_owned(), Some(parse_suffix(input, suffix_str)?))
        } else {
            (remainder.to_owned(), None)
        };

        if bare_id.is_empty() {
            return Err(EngineError::MalformedRef {
                input: input.to_owned(),
                reason: "empty bare_id".to_owned(),
            });
        }

        // Validate bare_id characters: alphanumeric, '/', '-', '_', '.'
        if !bare_id
            .chars()
            .all(|c| c.is_alphanumeric() || c == '/' || c == '-' || c == '_' || c == '.')
        {
            return Err(EngineError::MalformedRef {
                input: input.to_owned(),
                reason: format!("bare_id contains invalid characters: {bare_id}"),
            });
        }

        // Reject path traversal and malformed segments
        if bare_id.starts_with('/') {
            return Err(EngineError::MalformedRef {
                input: input.to_owned(),
                reason: "bare_id must not start with '/' (absolute path)".to_owned(),
            });
        }
        if bare_id.ends_with('/') {
            return Err(EngineError::MalformedRef {
                input: input.to_owned(),
                reason: "bare_id must not end with '/'".to_owned(),
            });
        }
        for segment in bare_id.split('/') {
            if segment.is_empty() {
                return Err(EngineError::MalformedRef {
                    input: input.to_owned(),
                    reason: "bare_id contains empty segment (double slash)".to_owned(),
                });
            }
            if segment == ".." {
                return Err(EngineError::MalformedRef {
                    input: input.to_owned(),
                    reason: "bare_id contains '..' path traversal segment".to_owned(),
                });
            }
            if segment == "." {
                return Err(EngineError::MalformedRef {
                    input: input.to_owned(),
                    reason: "bare_id contains '.' segment".to_owned(),
                });
            }
        }

        let parsed = Self {
            kind: kind_str.to_owned(),
            bare_id,
            suffix,
        };
        tracing::trace!(input = %input, kind = %parsed.kind, id = %parsed.bare_id, "parsed canonical ref");
        Ok(parsed)
    }
}

impl fmt::Display for CanonicalRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind, self.bare_id)?;
        if let Some(suffix) = &self.suffix {
            match suffix {
                RefSuffix::Capability {
                    signature,
                    fingerprint,
                    constraints_hash,
                } => write!(f, "@cap:{signature}:{fingerprint}:{constraints_hash}")?,
                RefSuffix::Signed {
                    content_hash,
                    signature,
                } => write!(f, "@sig:{content_hash}:{signature}")?,
                RefSuffix::Temporal { at } => write!(f, "@t:{at}")?,
            }
        }
        Ok(())
    }
}

fn parse_suffix(full_input: &str, suffix_str: &str) -> Result<RefSuffix, EngineError> {
    tracing::trace!(suffix = %suffix_str, "parsing canonical ref suffix");
    if let Some(rest) = suffix_str.strip_prefix("cap:") {
        let parts: Vec<&str> = rest.splitn(3, ':').collect();
        if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
            return Err(EngineError::InvalidSuffix {
                input: full_input.to_owned(),
                reason: "cap suffix requires cap:<signature>:<fingerprint>:<constraints_hash>"
                    .to_owned(),
            });
        }
        Ok(RefSuffix::Capability {
            signature: parts[0].to_owned(),
            fingerprint: parts[1].to_owned(),
            constraints_hash: parts[2].to_owned(),
        })
    } else if let Some(rest) = suffix_str.strip_prefix("sig:") {
        let parts: Vec<&str> = rest.splitn(2, ':').collect();
        if parts.len() != 2 || parts.iter().any(|p| p.is_empty()) {
            return Err(EngineError::InvalidSuffix {
                input: full_input.to_owned(),
                reason: "sig suffix requires sig:<content_hash>:<signature>".to_owned(),
            });
        }
        Ok(RefSuffix::Signed {
            content_hash: parts[0].to_owned(),
            signature: parts[1].to_owned(),
        })
    } else if let Some(rest) = suffix_str.strip_prefix("t:") {
        if rest.is_empty() {
            return Err(EngineError::InvalidSuffix {
                input: full_input.to_owned(),
                reason: "t suffix requires t:<timestamp>".to_owned(),
            });
        }
        Ok(RefSuffix::Temporal {
            at: rest.to_owned(),
        })
    } else {
        Err(EngineError::InvalidSuffix {
            input: full_input.to_owned(),
            reason: format!("unknown suffix type: {suffix_str}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_ref() {
        let r = CanonicalRef::parse("tool:ryeos/bash/bash").unwrap();
        assert_eq!(r.kind, "tool");
        assert_eq!(r.bare_id, "ryeos/bash/bash");
        assert!(r.suffix.is_none());
    }

    #[test]
    fn parse_various_kinds() {
        for (input, expected_kind) in [
            ("tool:x", "tool"),
            ("directive:x", "directive"),
            ("graph:x", "graph"),
            ("knowledge:x", "knowledge"),
            ("node:x", "node"),
            ("custom_kind:x", "custom_kind"),
        ] {
            let r = CanonicalRef::parse(input).unwrap();
            assert_eq!(r.kind, expected_kind);
        }
    }

    #[test]
    fn parse_capability_suffix() {
        let r = CanonicalRef::parse("tool:ryeos/email/send@cap:sig123:fp456:ch789").unwrap();
        assert_eq!(
            r.suffix,
            Some(RefSuffix::Capability {
                signature: "sig123".into(),
                fingerprint: "fp456".into(),
                constraints_hash: "ch789".into(),
            })
        );
    }

    #[test]
    fn parse_signed_suffix() {
        let r = CanonicalRef::parse("directive:agent/report@sig:sha256abc:sigb64").unwrap();
        assert_eq!(
            r.suffix,
            Some(RefSuffix::Signed {
                content_hash: "sha256abc".into(),
                signature: "sigb64".into(),
            })
        );
    }

    #[test]
    fn parse_temporal_suffix() {
        let r = CanonicalRef::parse("knowledge:docs/readme@t:2026-04-10T08:00:00Z").unwrap();
        assert_eq!(
            r.suffix,
            Some(RefSuffix::Temporal {
                at: "2026-04-10T08:00:00Z".into(),
            })
        );
    }

    #[test]
    fn reject_bare_ref() {
        let err = CanonicalRef::parse("ryeos/bash/bash").unwrap_err();
        assert!(
            matches!(err, EngineError::BareRefRejected { .. }),
            "expected BareRefRejected, got: {err:?}"
        );
    }

    #[test]
    fn reject_empty() {
        let err = CanonicalRef::parse("").unwrap_err();
        assert!(matches!(err, EngineError::MalformedRef { .. }));
    }

    #[test]
    fn reject_empty_bare_id() {
        let err = CanonicalRef::parse("tool:").unwrap_err();
        assert!(matches!(err, EngineError::MalformedRef { .. }));
    }

    #[test]
    fn accept_hyphenated_kind() {
        let r = CanonicalRef::parse("my-kind:foo/bar").unwrap();
        assert_eq!(r.kind, "my-kind");
        assert_eq!(r.bare_id, "foo/bar");
    }

    #[test]
    fn reject_uppercase_kind() {
        let err = CanonicalRef::parse("Tool:x").unwrap_err();
        assert!(matches!(err, EngineError::MalformedRef { .. }));
    }

    #[test]
    fn roundtrip_display() {
        let input = "tool:ryeos/bash/bash";
        let r = CanonicalRef::parse(input).unwrap();
        assert_eq!(r.to_string(), input);
    }

    #[test]
    fn roundtrip_display_with_suffix() {
        let input = "tool:ryeos/email/send@cap:sig:fp:ch";
        let r = CanonicalRef::parse(input).unwrap();
        assert_eq!(r.to_string(), input);
    }

    #[test]
    fn reject_absolute_path() {
        let err = CanonicalRef::parse("tool:/etc/passwd").unwrap_err();
        assert!(matches!(err, EngineError::MalformedRef { .. }));
    }

    #[test]
    fn reject_path_traversal() {
        let err = CanonicalRef::parse("tool:../../../etc/passwd").unwrap_err();
        assert!(matches!(err, EngineError::MalformedRef { .. }));
    }

    #[test]
    fn reject_dot_segment() {
        let err = CanonicalRef::parse("tool:./sneaky").unwrap_err();
        assert!(matches!(err, EngineError::MalformedRef { .. }));
    }

    #[test]
    fn reject_empty_segment() {
        let err = CanonicalRef::parse("tool:foo//bar").unwrap_err();
        assert!(matches!(err, EngineError::MalformedRef { .. }));
    }

    #[test]
    fn reject_trailing_slash() {
        let err = CanonicalRef::parse("tool:foo/").unwrap_err();
        assert!(matches!(err, EngineError::MalformedRef { .. }));
    }

    #[test]
    fn accept_valid_nested() {
        let r = CanonicalRef::parse("tool:ryeos/bash/bash").unwrap();
        assert_eq!(r.kind, "tool");
        assert_eq!(r.bare_id, "ryeos/bash/bash");
    }

    #[test]
    fn accept_hyphens_underscores() {
        let r = CanonicalRef::parse("tool:my-tool_v2").unwrap();
        assert_eq!(r.kind, "tool");
        assert_eq!(r.bare_id, "my-tool_v2");
    }

    #[test]
    fn parse_service_ref() {
        let r = CanonicalRef::parse("service:commands/submit").unwrap();
        assert_eq!(r.kind, "service");
        assert_eq!(r.bare_id, "commands/submit");
        assert!(r.suffix.is_none());
        assert_eq!(r.to_string(), "service:commands/submit");
    }

    #[test]
    fn parse_service_ref_with_suffix() {
        let r =
            CanonicalRef::parse("service:system/status@t:2026-04-26T00:00:00Z").unwrap();
        assert_eq!(r.kind, "service");
        assert_eq!(r.bare_id, "system/status");
        assert!(r.suffix.is_some());
    }

    #[test]
    fn canonical_ref_is_hashable_consistently_with_eq() {
        // Two refs that parse equal must hash to the same bucket — this is
        // the std::derive contract, but pin it here so we catch any future
        // hand-rolled Hash impl that breaks the invariant.
        use std::collections::HashSet;

        let a = CanonicalRef::parse("tool:ryeos/bash/bash").unwrap();
        let b = CanonicalRef::parse("tool:ryeos/bash/bash").unwrap();
        assert_eq!(a, b);

        let mut set: HashSet<CanonicalRef> = HashSet::new();
        assert!(set.insert(a.clone()));
        // Inserting the equal-but-distinct b must collide and return false.
        assert!(!set.insert(b.clone()));
        assert_eq!(set.len(), 1);

        // Suffix difference must split into distinct keys.
        let c = CanonicalRef::parse("service:system/status@t:2026-04-26T00:00:00Z").unwrap();
        assert!(set.insert(c));
        assert_eq!(set.len(), 2);
    }
}
