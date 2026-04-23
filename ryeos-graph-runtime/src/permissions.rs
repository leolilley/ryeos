pub fn check_permission(
    capabilities: &[String],
    item_id: &str,
) -> Result<(), String> {
    let primary = "execute";
    let (kind, bare_id) = parse_canonical_ref(item_id);

    if let Some(ref bare) = bare_id {
        if bare.starts_with("rye/agent/threads/internal/") {
            return Ok(());
        }
    }

    let kind = kind.ok_or_else(|| {
        format!(
            "Canonical ref required for permissioned actions \
             (e.g. 'tool:{item_id}' or 'directive:{item_id}'). \
             Cannot generate capability string for bare item_id: '{item_id}'"
        )
    })?;

    let bare_id = bare_id.ok_or_else(|| format!("Cannot parse bare_id from: '{item_id}'"))?;

    if capabilities.is_empty() {
        return Err(format!(
            "Permission denied: no capabilities. Cannot {primary} '{item_id}'"
        ));
    }

    let bare_id_dotted = bare_id.replace('/', ".");
    let required = format!("rye.{primary}.{kind}.{bare_id_dotted}");

    for cap in capabilities {
        if ryeos_runtime::cap_matches(cap, &required) {
            return Ok(());
        }
    }

    Err(format!(
        "Permission denied: '{required}' not covered by capabilities"
    ))
}

fn parse_canonical_ref(item_id: &str) -> (Option<String>, Option<String>) {
    if let Some(idx) = item_id.find(':') {
        let kind = &item_id[..idx];
        let bare = &item_id[idx + 1..];
        (Some(kind.to_string()), Some(bare.to_string()))
    } else {
        (None, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_capability_grants_access() {
        let caps = vec!["rye.execute.tool.rye.echo".to_string()];
        assert!(check_permission(&caps, "tool:rye/echo").is_ok());
    }

    #[test]
    fn wildcard_capability_grants_access() {
        let caps = vec!["rye.execute.*".to_string()];
        assert!(check_permission(&caps, "tool:rye/echo").is_ok());
    }

    #[test]
    fn no_capabilities_denies() {
        let caps: Vec<String> = vec![];
        assert!(check_permission(&caps, "tool:rye/echo").is_err());
    }

    #[test]
    fn internal_thread_tools_always_allowed() {
        let caps: Vec<String> = vec![];
        assert!(check_permission(&caps, "tool:rye/agent/threads/internal/list").is_ok());
    }

    #[test]
    fn bare_item_id_requires_canonical_ref() {
        let caps = vec!["rye.execute.*".to_string()];
        assert!(check_permission(&caps, "rye/echo").is_err());
    }

    #[test]
    fn wrong_capability_denies() {
        let caps = vec!["rye.fetch.*".to_string()];
        assert!(check_permission(&caps, "tool:rye/echo").is_err());
    }
}
