//! Authorized secret environment for one process launch.
//!
//! Values exist only between the authority reader and the final process
//! request. The type is deliberately neither `Clone` nor serde-capable, and its
//! `Debug` form lists names only.

use zeroize::Zeroizing;

pub struct DeliveredSecretEnv {
    /// Sorted by name.
    entries: Vec<(String, Zeroizing<String>)>,
}

impl DeliveredSecretEnv {
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Accept values that already crossed the admitted environment authority.
    pub fn from_authorized(entries: impl IntoIterator<Item = (String, Zeroizing<String>)>) -> Self {
        let mut entries: Vec<_> = entries.into_iter().collect();
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        Self { entries }
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(name, _)| name.as_str())
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .binary_search_by(|(candidate, _)| candidate.as_str().cmp(name))
            .ok()
            .map(|index| self.entries[index].1.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Append every value to a final process environment. A name already
    /// present (or repeated here) is refused before anything is appended; the
    /// error carries that name only.
    pub fn append_to(&self, envs: &mut Vec<(String, String)>) -> Result<(), String> {
        let mut previous: Option<&str> = None;
        for (name, _) in &self.entries {
            if previous == Some(name.as_str()) || envs.iter().any(|(existing, _)| existing == name)
            {
                return Err(name.clone());
            }
            previous = Some(name.as_str());
        }
        envs.extend(
            self.entries
                .iter()
                .map(|(name, value)| (name.clone(), value.as_str().to_owned())),
        );
        Ok(())
    }
}

impl std::fmt::Debug for DeliveredSecretEnv {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeliveredSecretEnv")
            .field("names", &self.names().collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> DeliveredSecretEnv {
        DeliveredSecretEnv::from_authorized([
            (
                "ZED_TOKEN".to_owned(),
                Zeroizing::new("zed-secret-value".to_owned()),
            ),
            (
                "API_KEY".to_owned(),
                Zeroizing::new("api-secret-value".to_owned()),
            ),
        ])
    }

    #[test]
    fn debug_lists_names_only() {
        let rendered = format!("{:?}", fixture());
        assert!(rendered.contains("API_KEY") && rendered.contains("ZED_TOKEN"));
        assert!(!rendered.contains("secret-value"), "{rendered}");
        let pretty = format!("{:#?}", fixture());
        assert!(!pretty.contains("secret-value"), "{pretty}");
    }

    #[test]
    fn lookup_is_by_exact_name() {
        let env = fixture();
        assert_eq!(env.names().collect::<Vec<_>>(), ["API_KEY", "ZED_TOKEN"]);
        assert_eq!(env.get("API_KEY"), Some("api-secret-value"));
        assert_eq!(env.get("api_key"), None);
        assert!(DeliveredSecretEnv::empty().is_empty());
    }

    #[test]
    fn append_refuses_collision_without_partial_delivery() {
        let env = fixture();
        let mut envs = vec![("ZED_TOKEN".to_owned(), "plan".to_owned())];
        assert_eq!(env.append_to(&mut envs), Err("ZED_TOKEN".to_owned()));
        assert_eq!(envs, vec![("ZED_TOKEN".to_owned(), "plan".to_owned())]);

        let mut envs = vec![("PATH".to_owned(), "/bin".to_owned())];
        env.append_to(&mut envs).unwrap();
        assert_eq!(envs.len(), 3);
        assert!(envs.contains(&("API_KEY".to_owned(), "api-secret-value".to_owned())));
    }

    #[test]
    fn repeated_name_is_refused() {
        let env = DeliveredSecretEnv::from_authorized([
            ("TOKEN".to_owned(), Zeroizing::new("a".to_owned())),
            ("TOKEN".to_owned(), Zeroizing::new("b".to_owned())),
        ]);
        let mut envs = Vec::new();
        assert_eq!(env.append_to(&mut envs), Err("TOKEN".to_owned()));
        assert!(envs.is_empty());
    }
}
