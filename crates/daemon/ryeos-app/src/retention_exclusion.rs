//! Retention exclusion: one App-owned gate plus a sealed set of pin sources.
//!
//! Anything that deletes or fences thread chain state must first hold this
//! exclusion, so that references to those threads held elsewhere in the
//! composition cannot appear between the pin count and the deletion. A
//! composition part that may hold durable thread references registers a
//! [`ThreadPinSource`] while the composition is being assembled; sealing the
//! set proves which parts exist. The mechanism names no part and no meaning of
//! a reference: it only orders locks and sums counts.
//!
//! Lock order for every path: this gate, then each source's own gate in sealed
//! order, then the store-level locks below.

use anyhow::{Result, bail};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::RwLock;

pub type PinFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// Lock mode of a gate. Mechanical only: it says nothing about the work done
/// under the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExclusionMode {
    Shared,
    Exclusive,
}

/// Identity of a registered source: the address of the shared allocation, so
/// a source and the guards it hands out agree on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PinSourceId(usize);

impl PinSourceId {
    /// Identity of the value behind an `Arc`, or a reference into it.
    pub fn of<T: ?Sized>(source: &T) -> Self {
        Self(std::ptr::from_ref(source).cast::<()>() as usize)
    }
}

/// A composition part that may hold durable references to threads.
pub trait ThreadPinSource: Send + Sync {
    /// Hold this source's own mutation gate in `mode`, verify that its pin
    /// index is current, or refuse. A non-current index is an error, never
    /// "zero pins".
    fn acquire_pins(&self, mode: ExclusionMode) -> PinFuture<'_, Box<dyn ThreadPinGuard>>;
}

/// A source's held gate with a verified-current pin index.
pub trait ThreadPinGuard: Send + Sync {
    /// Identity of the source that issued this guard.
    fn source(&self) -> PinSourceId;
    /// Number of references this source holds to any of `thread_ids`. Called
    /// under the store's inner lock: short access only, no re-entry.
    fn count_thread_pins(&self, thread_ids: &[String]) -> Result<u64>;
}

enum PinSources {
    Composing(Vec<Arc<dyn ThreadPinSource>>),
    Sealed(Arc<[Arc<dyn ThreadPinSource>]>),
    Withdrawn,
}

struct Inner {
    gate: Arc<RwLock<()>>,
    sources: std::sync::RwLock<PinSources>,
}

/// Owner of the retention gate and the pin-source set.
#[derive(Clone)]
pub struct RetentionCoordinator {
    inner: Arc<Inner>,
}

impl Default for RetentionCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl RetentionCoordinator {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                gate: Arc::new(RwLock::new(())),
                sources: std::sync::RwLock::new(PinSources::Composing(Vec::new())),
            }),
        }
    }

    fn sources_write(&self) -> std::sync::RwLockWriteGuard<'_, PinSources> {
        self.inner
            .sources
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Register a source. Allowed only while the composition is assembling.
    pub fn register(&self, source: Arc<dyn ThreadPinSource>) -> Result<()> {
        let mut sources = self.sources_write();
        match &mut *sources {
            PinSources::Composing(list) => {
                let id = PinSourceId::of(&*source);
                if list.iter().any(|known| PinSourceId::of(&**known) == id) {
                    bail!("retention pin source is already registered");
                }
                list.push(source);
                Ok(())
            }
            PinSources::Sealed(_) => {
                bail!("retention pin sources are sealed; no source may register")
            }
            PinSources::Withdrawn => {
                bail!("retention pin sources were withdrawn for shutdown")
            }
        }
    }

    /// Close the set. Called exactly once, before any recovery and before
    /// readiness.
    pub fn seal(&self) -> Result<()> {
        let mut sources = self.sources_write();
        match std::mem::replace(&mut *sources, PinSources::Withdrawn) {
            PinSources::Composing(list) => {
                *sources = PinSources::Sealed(list.into());
                Ok(())
            }
            previous @ PinSources::Sealed(_) => {
                *sources = previous;
                bail!("retention pin sources are already sealed")
            }
            PinSources::Withdrawn => {
                bail!("retention pin sources were withdrawn for shutdown")
            }
        }
    }

    /// End the composition's retention exclusions for shutdown. Idempotent.
    pub fn withdraw(&self) {
        *self.sources_write() = PinSources::Withdrawn;
    }

    /// Identities of the sealed sources, in sealed order.
    pub fn sealed_source_ids(&self) -> Result<Vec<PinSourceId>> {
        Ok(self
            .sealed_sources()?
            .iter()
            .map(|source| PinSourceId::of(&**source))
            .collect())
    }

    fn sealed_sources(&self) -> Result<Arc<[Arc<dyn ThreadPinSource>]>> {
        match &*self
            .inner
            .sources
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            PinSources::Sealed(sources) => Ok(Arc::clone(sources)),
            PinSources::Composing(_) => {
                bail!("composition has not sealed its retention pin sources")
            }
            PinSources::Withdrawn => bail!("retention pin sources withdrawn for shutdown"),
        }
    }

    /// Take the App gate in `mode`. Refuses unless the sources are sealed.
    pub async fn acquire_retention_gate(&self, mode: ExclusionMode) -> Result<RetentionGate> {
        self.sealed_sources()?;
        let held: Box<dyn std::any::Any + Send + Sync> = match mode {
            ExclusionMode::Shared => Box::new(Arc::clone(&self.inner.gate).read_owned().await),
            ExclusionMode::Exclusive => Box::new(Arc::clone(&self.inner.gate).write_owned().await),
        };
        // Shutdown may have withdrawn the set while this call waited.
        let sources = self.sealed_sources()?;
        Ok(RetentionGate {
            owner: Arc::clone(&self.inner),
            sources,
            mode,
            _held: held,
        })
    }
}

/// The App gate held in one mode. Complete it with the sealed sources' guards
/// to obtain a [`RetentionExclusion`].
pub struct RetentionGate {
    owner: Arc<Inner>,
    sources: Arc<[Arc<dyn ThreadPinSource>]>,
    mode: ExclusionMode,
    _held: Box<dyn std::any::Any + Send + Sync>,
}

impl RetentionGate {
    /// Acquire every sealed source's pins in sealed order, in this gate's mode.
    pub async fn acquire_sealed_pins(self) -> Result<RetentionExclusion<'static>> {
        let mut guards = Vec::with_capacity(self.sources.len());
        for source in self.sources.iter() {
            let guard = source.acquire_pins(self.mode).await?;
            if guard.source() != PinSourceId::of(&**source) {
                bail!("pin guard was issued by a different source than the one acquired");
            }
            guards.push(guard);
        }
        Ok(RetentionExclusion {
            guards: PinGuards::Owned(guards),
            owner: self.owner,
            _gate: self._held,
        })
    }

    /// Complete the exclusion with guards the caller already holds, for a
    /// caller that needs a source's richer guard. The held guards must come
    /// from exactly the sealed sources: a missing, extra or repeated guard
    /// refuses.
    pub fn with_held_pins<'a>(
        self,
        held: Vec<&'a dyn ThreadPinGuard>,
    ) -> Result<RetentionExclusion<'a>> {
        let mut sealed: Vec<PinSourceId> = self
            .sources
            .iter()
            .map(|source| PinSourceId::of(&**source))
            .collect();
        let mut offered: Vec<PinSourceId> = held.iter().map(|guard| guard.source()).collect();
        sealed.sort_unstable();
        offered.sort_unstable();
        if sealed != offered {
            bail!("held pin guards do not match the sealed composition");
        }
        Ok(RetentionExclusion {
            guards: PinGuards::Held(held),
            owner: self.owner,
            _gate: self._held,
        })
    }
}

enum PinGuards<'a> {
    Owned(Vec<Box<dyn ThreadPinGuard>>),
    Held(Vec<&'a dyn ThreadPinGuard>),
}

/// The App gate alone, held until dropped.
pub struct RetentionGateHold {
    _owner: Arc<Inner>,
    _gate: Box<dyn std::any::Any + Send + Sync>,
}

/// Proof that the App gate and every sealed source's pins are held. Pin
/// counts taken through it stay true until it is dropped.
pub struct RetentionExclusion<'a> {
    // Source guards drop before the App gate.
    guards: PinGuards<'a>,
    owner: Arc<Inner>,
    _gate: Box<dyn std::any::Any + Send + Sync>,
}

impl RetentionExclusion<'_> {
    /// Total references to any of `thread_ids` across the sealed sources. An
    /// empty sealed set is exactly zero: the composition has no pin holder.
    pub fn count_thread_pins(&self, thread_ids: &[String]) -> Result<u64> {
        let mut total = 0u64;
        let mut add = |guard: &dyn ThreadPinGuard| -> Result<()> {
            let count = guard.count_thread_pins(thread_ids)?;
            total = total
                .checked_add(count)
                .ok_or_else(|| anyhow::anyhow!("thread pin count overflowed"))?;
            Ok(())
        };
        match &self.guards {
            PinGuards::Owned(guards) => guards.iter().try_for_each(|guard| add(&**guard))?,
            PinGuards::Held(guards) => guards.iter().try_for_each(|guard| add(*guard))?,
        }
        Ok(total)
    }

    /// End the pin checks but keep the App gate held until the returned hold
    /// drops: for a caller whose later work must stay excluded from other
    /// retention paths after the borrowed or per-source guards are released.
    pub fn into_gate_hold(self) -> RetentionGateHold {
        RetentionGateHold {
            _owner: self.owner,
            _gate: self._gate,
        }
    }

    /// Refuse unless this exclusion was taken from `coordinator`.
    pub fn validate_owner(&self, coordinator: &RetentionCoordinator) -> Result<()> {
        if !Arc::ptr_eq(&self.owner, &coordinator.inner) {
            bail!("retention exclusion belongs to a different composition");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Source {
        gate: Arc<RwLock<()>>,
        pins: u64,
        current: bool,
        order: Arc<Mutex<Vec<&'static str>>>,
        name: &'static str,
    }

    struct SourceGuard {
        id: PinSourceId,
        pins: u64,
        _held: Box<dyn std::any::Any + Send + Sync>,
    }

    impl ThreadPinSource for Source {
        fn acquire_pins(&self, mode: ExclusionMode) -> PinFuture<'_, Box<dyn ThreadPinGuard>> {
            Box::pin(async move {
                self.order.lock().unwrap().push(self.name);
                let held: Box<dyn std::any::Any + Send + Sync> = match mode {
                    ExclusionMode::Shared => Box::new(Arc::clone(&self.gate).read_owned().await),
                    ExclusionMode::Exclusive => {
                        Box::new(Arc::clone(&self.gate).write_owned().await)
                    }
                };
                if !self.current {
                    bail!("{} pin index is not current", self.name);
                }
                Ok(Box::new(SourceGuard {
                    id: PinSourceId::of(self),
                    pins: self.pins,
                    _held: held,
                }) as Box<dyn ThreadPinGuard>)
            })
        }
    }

    impl ThreadPinGuard for SourceGuard {
        fn source(&self) -> PinSourceId {
            self.id
        }
        fn count_thread_pins(&self, _thread_ids: &[String]) -> Result<u64> {
            Ok(self.pins)
        }
    }

    fn source(name: &'static str, pins: u64, order: &Arc<Mutex<Vec<&'static str>>>) -> Arc<Source> {
        Arc::new(Source {
            gate: Arc::new(RwLock::new(())),
            pins,
            current: true,
            order: Arc::clone(order),
            name,
        })
    }

    fn order() -> Arc<Mutex<Vec<&'static str>>> {
        Arc::new(Mutex::new(Vec::new()))
    }

    fn ids() -> Vec<String> {
        vec!["T-1".to_owned()]
    }

    #[tokio::test]
    async fn refuses_before_seal_and_after_withdrawal() {
        let coordinator = RetentionCoordinator::new();
        let error = coordinator
            .acquire_retention_gate(ExclusionMode::Shared)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("has not sealed"), "{error}");
        coordinator.seal().unwrap();
        coordinator
            .acquire_retention_gate(ExclusionMode::Shared)
            .await
            .unwrap();
        coordinator.withdraw();
        let error = coordinator
            .acquire_retention_gate(ExclusionMode::Exclusive)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("withdrawn"), "{error}");
        assert!(coordinator.register(source("a", 0, &order())).is_err());
        assert!(coordinator.seal().is_err());
    }

    #[test]
    fn registration_closes_at_seal_and_rejects_duplicates() {
        let coordinator = RetentionCoordinator::new();
        let a = source("a", 0, &order());
        coordinator.register(a.clone()).unwrap();
        assert!(coordinator.register(a).is_err());
        coordinator.seal().unwrap();
        assert!(coordinator.register(source("b", 0, &order())).is_err());
        assert!(coordinator.seal().is_err());
        assert_eq!(coordinator.sealed_source_ids().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn empty_sealed_set_counts_exactly_zero() {
        let coordinator = RetentionCoordinator::new();
        coordinator.seal().unwrap();
        let exclusion = coordinator
            .acquire_retention_gate(ExclusionMode::Exclusive)
            .await
            .unwrap()
            .acquire_sealed_pins()
            .await
            .unwrap();
        assert_eq!(exclusion.count_thread_pins(&ids()).unwrap(), 0);
        exclusion.validate_owner(&coordinator).unwrap();
        assert!(
            exclusion
                .validate_owner(&RetentionCoordinator::new())
                .is_err()
        );
    }

    #[tokio::test]
    async fn pins_sum_across_sources_acquired_in_sealed_order() {
        let log = order();
        let coordinator = RetentionCoordinator::new();
        coordinator.register(source("first", 2, &log)).unwrap();
        coordinator.register(source("second", 3, &log)).unwrap();
        coordinator.seal().unwrap();
        let exclusion = coordinator
            .acquire_retention_gate(ExclusionMode::Shared)
            .await
            .unwrap()
            .acquire_sealed_pins()
            .await
            .unwrap();
        assert_eq!(exclusion.count_thread_pins(&ids()).unwrap(), 5);
        assert_eq!(*log.lock().unwrap(), vec!["first", "second"]);
    }

    #[tokio::test]
    async fn pin_count_overflow_refuses() {
        let log = order();
        let coordinator = RetentionCoordinator::new();
        coordinator.register(source("a", u64::MAX, &log)).unwrap();
        coordinator.register(source("b", 1, &log)).unwrap();
        coordinator.seal().unwrap();
        let exclusion = coordinator
            .acquire_retention_gate(ExclusionMode::Shared)
            .await
            .unwrap()
            .acquire_sealed_pins()
            .await
            .unwrap();
        let error = exclusion.count_thread_pins(&ids()).unwrap_err();
        assert!(error.to_string().contains("overflowed"), "{error}");
    }

    #[tokio::test]
    async fn non_current_source_refuses_and_releases_the_gate() {
        let log = order();
        let coordinator = RetentionCoordinator::new();
        let stale = Arc::new(Source {
            gate: Arc::new(RwLock::new(())),
            pins: 0,
            current: false,
            order: Arc::clone(&log),
            name: "stale",
        });
        coordinator.register(stale).unwrap();
        coordinator.seal().unwrap();
        let error = coordinator
            .acquire_retention_gate(ExclusionMode::Exclusive)
            .await
            .unwrap()
            .acquire_sealed_pins()
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("not current"), "{error}");
        assert!(coordinator.inner.gate.try_write().is_ok());
    }

    #[tokio::test]
    async fn exclusive_exclusion_blocks_other_acquirers_until_dropped() {
        let coordinator = RetentionCoordinator::new();
        coordinator.seal().unwrap();
        let exclusion = coordinator
            .acquire_retention_gate(ExclusionMode::Exclusive)
            .await
            .unwrap()
            .acquire_sealed_pins()
            .await
            .unwrap();
        let blocked = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            coordinator.acquire_retention_gate(ExclusionMode::Shared),
        )
        .await;
        assert!(blocked.is_err());
        drop(exclusion);
        coordinator
            .acquire_retention_gate(ExclusionMode::Shared)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn shared_exclusions_coexist_but_exclude_exclusive() {
        let coordinator = RetentionCoordinator::new();
        coordinator.seal().unwrap();
        let first = coordinator
            .acquire_retention_gate(ExclusionMode::Shared)
            .await
            .unwrap();
        let second = coordinator
            .acquire_retention_gate(ExclusionMode::Shared)
            .await
            .unwrap();
        assert!(coordinator.inner.gate.try_write().is_err());
        drop((first, second));
        assert!(coordinator.inner.gate.try_write().is_ok());
    }

    #[tokio::test]
    async fn held_pins_must_match_the_sealed_set_exactly() {
        let log = order();
        let a = source("a", 4, &log);
        let b = source("b", 1, &log);
        let coordinator = RetentionCoordinator::new();
        coordinator.register(a.clone()).unwrap();
        coordinator.register(b.clone()).unwrap();
        coordinator.seal().unwrap();
        let guard_a = a.acquire_pins(ExclusionMode::Exclusive).await.unwrap();
        let guard_b = b.acquire_pins(ExclusionMode::Exclusive).await.unwrap();

        let gate = || coordinator.acquire_retention_gate(ExclusionMode::Exclusive);
        let exclusion = gate()
            .await
            .unwrap()
            .with_held_pins(vec![&*guard_b, &*guard_a])
            .unwrap();
        assert_eq!(exclusion.count_thread_pins(&ids()).unwrap(), 5);
        drop(exclusion);

        for wrong in [
            vec![&*guard_a],
            vec![&*guard_a, &*guard_a],
            vec![&*guard_a, &*guard_b, &*guard_a],
            vec![],
        ] {
            let error = gate().await.unwrap().with_held_pins(wrong).err().unwrap();
            assert!(
                error.to_string().contains("do not match the sealed"),
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn held_pins_with_an_empty_sealed_set_accept_only_none() {
        let log = order();
        let outsider = source("outsider", 0, &log);
        let coordinator = RetentionCoordinator::new();
        coordinator.seal().unwrap();
        let guard = outsider.acquire_pins(ExclusionMode::Shared).await.unwrap();
        let gate = coordinator
            .acquire_retention_gate(ExclusionMode::Shared)
            .await
            .unwrap();
        assert!(gate.with_held_pins(vec![&*guard]).is_err());
        let gate = coordinator
            .acquire_retention_gate(ExclusionMode::Shared)
            .await
            .unwrap();
        assert_eq!(
            gate.with_held_pins(vec![])
                .unwrap()
                .count_thread_pins(&ids())
                .unwrap(),
            0
        );
    }

    struct ForgingSource {
        impostor: PinSourceId,
    }
    impl ThreadPinSource for ForgingSource {
        fn acquire_pins(&self, _mode: ExclusionMode) -> PinFuture<'_, Box<dyn ThreadPinGuard>> {
            let id = self.impostor;
            Box::pin(async move {
                Ok(Box::new(SourceGuard {
                    id,
                    pins: 0,
                    _held: Box::new(()),
                }) as Box<dyn ThreadPinGuard>)
            })
        }
    }

    #[tokio::test]
    async fn guard_from_another_source_is_refused() {
        let coordinator = RetentionCoordinator::new();
        coordinator
            .register(Arc::new(ForgingSource {
                impostor: PinSourceId::of(&0u8),
            }))
            .unwrap();
        coordinator.seal().unwrap();
        let error = coordinator
            .acquire_retention_gate(ExclusionMode::Shared)
            .await
            .unwrap()
            .acquire_sealed_pins()
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("different source"), "{error}");
    }

    #[tokio::test]
    async fn app_state_seals_once_and_withdraws_with_the_scheduler_slot() {
        let root = tempfile::TempDir::new().unwrap();
        let (_state_owner, state) = crate::state::test_support::build(root.path()).unwrap();
        assert!(
            state
                .acquire_retention_gate(ExclusionMode::Exclusive)
                .await
                .is_err()
        );
        state
            .register_thread_pin_source(source("a", 7, &order()))
            .unwrap();
        state.seal_thread_pin_sources().unwrap();
        assert!(state.seal_thread_pin_sources().is_err());
        assert!(
            state
                .register_thread_pin_source(source("b", 0, &order()))
                .is_err()
        );
        let exclusion = state
            .acquire_retention_gate(ExclusionMode::Exclusive)
            .await
            .unwrap()
            .acquire_sealed_pins()
            .await
            .unwrap();
        assert_eq!(exclusion.count_thread_pins(&ids()).unwrap(), 7);
        exclusion.validate_owner(state.threads.retention()).unwrap();
        drop(exclusion);
        assert!(state.take_scheduler_participant_for_shutdown().is_none());
        assert!(
            state
                .acquire_retention_gate(ExclusionMode::Shared)
                .await
                .is_err()
        );
    }
}
