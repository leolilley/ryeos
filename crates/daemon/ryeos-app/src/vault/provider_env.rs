//! Provider-delivered environment values taken at daemon startup.
//!
//! The node's signed policy declares the names. Startup takes exactly those
//! names out of the daemon's own environment, before any thread exists, and
//! this snapshot is then the only place their values live. Values leave it
//! only through authorized secret delivery; diagnostics see names only.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Result, bail};
use zeroize::Zeroizing;

/// Declared provider-environment names and the values present at startup.
pub struct ProviderEnvironment {
    declared_names: Vec<String>,
    values: BTreeMap<String, Zeroizing<String>>,
}

impl ProviderEnvironment {
    /// A node that declares no provider-delivered names.
    pub const fn none() -> Self {
        Self {
            declared_names: Vec::new(),
            values: BTreeMap::new(),
        }
    }

    /// Bind the values taken at startup to the declaration they were taken
    /// under. Every value must belong to a declared name.
    pub fn captured(
        declared_names: Vec<String>,
        values: Vec<(String, Zeroizing<String>)>,
    ) -> Result<Self> {
        let mut previous: Option<&str> = None;
        for name in &declared_names {
            if previous.is_some_and(|candidate| candidate >= name.as_str()) {
                bail!("provider-environment declaration must be sorted and unique");
            }
            previous = Some(name);
        }
        let mut retained = BTreeMap::new();
        for (name, value) in values {
            if declared_names.binary_search(&name).is_err() {
                bail!("provider-environment value `{name}` is outside the declaration");
            }
            if retained.insert(name.clone(), value).is_some() {
                bail!("provider-environment value `{name}` was taken twice");
            }
        }
        Ok(Self {
            declared_names,
            values: retained,
        })
    }

    pub fn declared_names(&self) -> &[String] {
        &self.declared_names
    }

    pub fn declares(&self, name: &str) -> bool {
        self.declared_names
            .binary_search_by(|candidate| candidate.as_str().cmp(name))
            .is_ok()
    }

    /// Declared names that had a value at startup, in sorted order.
    pub fn present_names(&self) -> Vec<String> {
        self.values.keys().cloned().collect()
    }

    /// Declared names that had no value at startup, in sorted order.
    pub fn absent_names(&self) -> Vec<String> {
        self.declared_names
            .iter()
            .filter(|name| !self.values.contains_key(name.as_str()))
            .cloned()
            .collect()
    }

    /// Copy the values of the requested declared names that are present.
    /// The caller owns zeroizing every returned value.
    pub(super) fn read_declared(&self, requested: &[String]) -> HashMap<String, String> {
        requested
            .iter()
            .filter(|name| self.declares(name))
            .filter_map(|name| {
                self.values
                    .get(name.as_str())
                    .map(|value| (name.clone(), value.as_str().to_owned()))
            })
            .collect()
    }
}

impl std::fmt::Debug for ProviderEnvironment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderEnvironment")
            .field("declared_names", &self.declared_names)
            .field("present_names", &self.values.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn captured(declared: &[&str], values: &[(&str, &str)]) -> Result<ProviderEnvironment> {
        ProviderEnvironment::captured(
            declared.iter().map(|name| (*name).to_owned()).collect(),
            values
                .iter()
                .map(|(name, value)| ((*name).to_owned(), Zeroizing::new((*value).to_owned())))
                .collect(),
        )
    }

    #[test]
    fn values_are_bound_to_the_declaration_and_never_debug_printed() {
        let environment = captured(&["A_KEY", "B_KEY"], &[("A_KEY", "value-a")]).unwrap();
        assert_eq!(environment.present_names(), ["A_KEY"]);
        assert_eq!(environment.absent_names(), ["B_KEY"]);
        assert!(!format!("{environment:?}").contains("value-a"));
        assert!(captured(&["B_KEY", "A_KEY"], &[]).is_err());
        assert!(captured(&["A_KEY"], &[("OTHER", "value")]).is_err());
        assert!(captured(&["A_KEY"], &[("A_KEY", "1"), ("A_KEY", "2")]).is_err());
    }

    #[test]
    fn reads_return_only_requested_declared_present_values() {
        let environment = captured(&["A_KEY", "B_KEY"], &[("A_KEY", "value-a")]).unwrap();
        let read = environment.read_declared(&["A_KEY".to_owned(), "C_KEY".to_owned()]);
        assert_eq!(read.len(), 1);
        assert_eq!(read.get("A_KEY").map(String::as_str), Some("value-a"));
        assert!(ProviderEnvironment::none().read_declared(&["A_KEY".to_owned()]).is_empty());
    }
}
