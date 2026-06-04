//! Typed extension state bag for composition-root state injection.
//!
//! `AppState::extensions` holds an `Arc<ExtensionState>` that can store
//! typed `Arc<T>` values keyed by `TypeId`. The core crate does not
//! define any specific extension types — those come from service layers.
//!
//! # Usage
//!
//! ```ignore
//! use ryeos_app::extension_state::ExtensionState;
//! use std::sync::Arc;
//!
//! let mut ext = ExtensionState::new();
//! ext.insert(Arc::new(MyState::new()));
//!
//! let retrieved: Option<Arc<MyState>> = ext.get();
//! ```

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Default, Clone)]
pub struct ExtensionState {
    entries: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl ExtensionState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a typed `Arc<T>` value.
    pub fn insert<T: Any + Send + Sync + 'static>(&mut self, value: Arc<T>) {
        self.entries.insert(TypeId::of::<T>(), value);
    }

    /// Retrieve a typed `Arc<T>` if it was previously inserted.
    pub fn get<T: Any + Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.entries
            .get(&TypeId::of::<T>())
            .and_then(|arc| arc.clone().downcast::<T>().ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestExt {
        value: u32,
    }

    #[test]
    fn round_trip() {
        let mut ext = ExtensionState::new();
        ext.insert(Arc::new(TestExt { value: 42 }));
        let got: Arc<TestExt> = ext.get().expect("should be present");
        assert_eq!(got.value, 42);
    }

    #[test]
    fn missing_returns_none() {
        let ext = ExtensionState::new();
        let got: Option<Arc<TestExt>> = ext.get();
        assert!(got.is_none());
    }

    #[test]
    fn overwrite_replaces() {
        let mut ext = ExtensionState::new();
        ext.insert(Arc::new(TestExt { value: 1 }));
        ext.insert(Arc::new(TestExt { value: 2 }));
        let got: Arc<TestExt> = ext.get().unwrap();
        assert_eq!(got.value, 2);
    }
}
