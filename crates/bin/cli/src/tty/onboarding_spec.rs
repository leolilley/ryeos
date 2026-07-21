use std::collections::BTreeSet;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

const SPEC_SOURCE: &str = include_str!("../../assets/onboarding/v1.yaml");
pub(crate) const PRISM_COMPACT: &str = include_str!("../../assets/brand/prism-compact.txt");
pub(crate) const PRISM_WIDE: &str = include_str!("../../assets/brand/prism-wide.txt");

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OnboardingSpec {
    pub schema: String,
    pub pages: Vec<PageSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PageSpec {
    pub id: String,
    pub kind: PageKind,
    #[serde(default)]
    pub art: Option<ArtVariant>,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub fields: Vec<FieldId>,
    pub action: ActionId,
    #[serde(default)]
    pub next: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PageKind {
    Introduction,
    Document,
    Form,
    Progress,
    Choice,
    Completion,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ArtVariant {
    PrismCompact,
    PrismWide,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum FieldId {
    DisplayName,
    IdentityStatement,
    EntropyContribution,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ActionId {
    Continue,
    InitializeOperator,
    InitializeCore,
    ConfigureProvider,
    Finish,
}

pub(crate) fn load() -> Result<OnboardingSpec> {
    let spec: OnboardingSpec =
        serde_yaml::from_str(SPEC_SOURCE).context("parse embedded onboarding v1 spec")?;
    validate(&spec)?;
    Ok(spec)
}

fn validate(spec: &OnboardingSpec) -> Result<()> {
    if spec.schema != "ryeos/onboarding/v1" {
        bail!("unsupported onboarding schema '{}'", spec.schema);
    }
    if spec.pages.is_empty() {
        bail!("onboarding spec has no pages");
    }
    let mut ids = BTreeSet::new();
    for page in &spec.pages {
        if page.id.trim().is_empty() || !ids.insert(page.id.as_str()) {
            bail!("onboarding page IDs must be non-empty and unique");
        }
        if page.title.trim().is_empty() || page.body.trim().is_empty() {
            bail!("onboarding page '{}' has empty copy", page.id);
        }
        if page.kind != PageKind::Form && !page.fields.is_empty() {
            bail!("non-form onboarding page '{}' declares fields", page.id);
        }
    }
    for page in &spec.pages {
        if let Some(next) = &page.next {
            if !ids.contains(next.as_str()) {
                bail!("onboarding page '{}' targets unknown page '{next}'", page.id);
            }
        } else if page.action != ActionId::Finish {
            bail!("non-terminal onboarding page '{}' has no next page", page.id);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_spec_is_strict_and_connected() {
        let spec = load().unwrap();
        assert_eq!(spec.pages.first().unwrap().id, "welcome");
        assert_eq!(spec.pages.last().unwrap().action, ActionId::Finish);
    }

    #[test]
    fn unknown_action_is_rejected() {
        let source = SPEC_SOURCE.replace("action: finish", "action: shell-command");
        assert!(serde_yaml::from_str::<OnboardingSpec>(&source).is_err());
    }
}
