//! Typed extension state bag for composition-root state injection.
//!
//! `AppState::extensions` holds an `Arc<ExtensionState>` that can store
//! typed `Arc<T>` values keyed by `TypeId`. The core crate does not
//! define any specific extension types — those come from service layers.
//!
//! # Usage
//!
//! ```
//! use ryeos_app::extension_state::ExtensionState;
//! use std::sync::Arc;
//!
//! struct MyState;
//!
//! impl MyState {
//!     fn new() -> Self {
//!         Self
//!     }
//! }
//!
//! let ext = ExtensionState::new();
//! ext.insert(Arc::new(MyState::new()));
//!
//! let retrieved: Option<Arc<MyState>> = ext.get();
//! assert!(retrieved.is_some());
//! ```

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

#[derive(Default)]
pub struct ExtensionState {
    entries: RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

impl Clone for ExtensionState {
    fn clone(&self) -> Self {
        let entries = self
            .entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self {
            entries: RwLock::new(entries.clone()),
        }
    }
}

impl ExtensionState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a typed `Arc<T>` value.
    pub fn insert<T: Any + Send + Sync + 'static>(&self, value: Arc<T>) {
        self.entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(TypeId::of::<T>(), value);
    }

    /// Retrieve a typed `Arc<T>` if it was previously inserted.
    pub fn get<T: Any + Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&TypeId::of::<T>())
            .and_then(|arc| arc.clone().downcast::<T>().ok())
    }

    /// Retrieve the typed `Arc<T>`, creating it with `create` when absent. The
    /// check and the insertion happen under one write lock, so concurrent
    /// callers share exactly one instance.
    pub fn get_or_insert_with<T: Any + Send + Sync + 'static>(
        &self,
        create: impl FnOnce() -> T,
    ) -> Arc<T> {
        let mut entries = self
            .entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = entries
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Arc::new(create()) as Arc<dyn Any + Send + Sync>);
        entry
            .clone()
            .downcast::<T>()
            .expect("an extension entry is keyed by its own type")
    }

    /// Remove one typed owner after its users have drained. Composition roots
    /// use this to transfer a stopped extension into its finalization owner;
    /// the returned `Arc` keeps the exact registered allocation alive until
    /// that transfer succeeds or the caller restores it.
    pub fn remove<T: Any + Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&TypeId::of::<T>())
            .and_then(|arc| arc.downcast::<T>().ok())
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
        let ext = ExtensionState::new();
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
        let ext = ExtensionState::new();
        ext.insert(Arc::new(TestExt { value: 1 }));
        ext.insert(Arc::new(TestExt { value: 2 }));
        let got: Arc<TestExt> = ext.get().unwrap();
        assert_eq!(got.value, 2);
    }

    #[test]
    fn get_or_insert_with_creates_once_and_shares() {
        let ext = ExtensionState::new();
        let first = ext.get_or_insert_with(|| TestExt { value: 1 });
        let second = ext.get_or_insert_with(|| TestExt { value: 2 });
        assert_eq!(second.value, 1);
        assert!(Arc::ptr_eq(&first, &second));
        assert!(Arc::ptr_eq(&first, &ext.get::<TestExt>().unwrap()));
    }

    #[test]
    fn remove_transfers_the_registered_owner() {
        let ext = ExtensionState::new();
        ext.insert(Arc::new(TestExt { value: 42 }));
        let ext = Arc::new(ext);
        let alias = Arc::clone(&ext);
        let removed = alias.remove::<TestExt>().expect("registered extension");
        assert_eq!(removed.value, 42);
        assert!(ext.get::<TestExt>().is_none());
    }
}
