//! Type-level custody for the selected composition's native SQLite source
//! slots.
//!
//! The accounting and scheduler slots below each own native connection parts,
//! unopened inputs, finalization owners or acknowledged terminals. They sit in
//! one `Held` under a fixed kind, so any end of the composition that leaves a
//! slot occupied retains and counts the whole group instead of dropping it.
//! The composition's own fail-closed exit still runs first; this is the
//! backstop beneath it.
//!
//! `CompositionNativeParts` is private to this module, so nothing outside can
//! build or replace the whole group. Each slot is reached only through its own
//! accessor pair.

use lillux::custody::Held;

/// The kind an occupied group is counted under when its composition ends.
pub(crate) const COMPOSITION_NATIVE_SOURCES_KIND: &str = "ryeosd::composition-native-sources";

struct CompositionNativeParts {
    accounting_native_connection_parts: Option<lillux_sqlite::OriginalNativeOutsideConnectionParts>,
    accounting_native_reference_parts: Option<lillux_sqlite::OriginalNativeOutsideConnectionParts>,
    accounting_native_unused_reference: Option<lillux_sqlite::OriginalNativeTransientInputs>,
    #[cfg(feature = "scheduling")]
    scheduler_native_finalization_owner:
        Option<ryeosd::scheduler::db::SchedulerNativeFinalizationOwner>,
    #[cfg(feature = "scheduling")]
    scheduler_native_outside_sources:
        Option<ryeos_scheduler_contract::SchedulerNativeOutsideSources>,
    #[cfg(feature = "scheduling")]
    scheduler_native_connection_parts: Option<lillux_sqlite::OriginalNativeOutsideConnectionParts>,
    #[cfg(feature = "scheduling")]
    scheduler_native_unopened_inputs: Option<lillux_sqlite::OriginalNativeBoundInputs>,
    #[cfg(feature = "scheduling")]
    scheduler_native_schema_parts: Option<lillux_sqlite::OriginalNativeOutsideConnectionParts>,
    #[cfg(feature = "scheduling")]
    scheduler_native_schema_transient: Option<lillux_sqlite::OriginalNativeTransientInputs>,
    #[cfg(feature = "scheduling")]
    scheduler_native_terminals: Option<Vec<lillux_sqlite::OriginalNativeConnectionTerminal>>,
}

impl CompositionNativeParts {
    /// Whether every slot is empty, the only state in which the group may drop.
    fn all_empty(&self) -> bool {
        let empty = self.accounting_native_connection_parts.is_none()
            && self.accounting_native_reference_parts.is_none()
            && self.accounting_native_unused_reference.is_none();
        #[cfg(feature = "scheduling")]
        let empty = empty
            && self.scheduler_native_finalization_owner.is_none()
            && self.scheduler_native_outside_sources.is_none()
            && self.scheduler_native_connection_parts.is_none()
            && self.scheduler_native_unopened_inputs.is_none()
            && self.scheduler_native_schema_parts.is_none()
            && self.scheduler_native_schema_transient.is_none()
            && self.scheduler_native_terminals.is_none();
        empty
    }
}

/// The composition's native source slots, held until every one is empty.
pub(crate) struct CompositionNativeSources(Held<CompositionNativeParts>);

macro_rules! slots {
    ($($(#[$meta:meta])* $slot:ident / $slot_mut:ident: $ty:ty;)*) => {
        impl CompositionNativeSources {
            $(
                $(#[$meta])*
                pub(crate) fn $slot(&self) -> &Option<$ty> {
                    &self.0.$slot
                }
                $(#[$meta])*
                pub(crate) fn $slot_mut(&mut self) -> &mut Option<$ty> {
                    &mut self.0.get_mut().$slot
                }
            )*
        }
    };
}

slots! {
    accounting_native_connection_parts / accounting_native_connection_parts_mut:
        lillux_sqlite::OriginalNativeOutsideConnectionParts;
    accounting_native_reference_parts / accounting_native_reference_parts_mut:
        lillux_sqlite::OriginalNativeOutsideConnectionParts;
    accounting_native_unused_reference / accounting_native_unused_reference_mut:
        lillux_sqlite::OriginalNativeTransientInputs;
    #[cfg(feature = "scheduling")]
    scheduler_native_finalization_owner / scheduler_native_finalization_owner_mut:
        ryeosd::scheduler::db::SchedulerNativeFinalizationOwner;
    #[cfg(feature = "scheduling")]
    scheduler_native_outside_sources / scheduler_native_outside_sources_mut:
        ryeos_scheduler_contract::SchedulerNativeOutsideSources;
    #[cfg(feature = "scheduling")]
    scheduler_native_connection_parts / scheduler_native_connection_parts_mut:
        lillux_sqlite::OriginalNativeOutsideConnectionParts;
    #[cfg(feature = "scheduling")]
    scheduler_native_unopened_inputs / scheduler_native_unopened_inputs_mut:
        lillux_sqlite::OriginalNativeBoundInputs;
    #[cfg(feature = "scheduling")]
    scheduler_native_schema_parts / scheduler_native_schema_parts_mut:
        lillux_sqlite::OriginalNativeOutsideConnectionParts;
    #[cfg(feature = "scheduling")]
    scheduler_native_schema_transient / scheduler_native_schema_transient_mut:
        lillux_sqlite::OriginalNativeTransientInputs;
    #[cfg(feature = "scheduling")]
    scheduler_native_terminals / scheduler_native_terminals_mut:
        Vec<lillux_sqlite::OriginalNativeConnectionTerminal>;
}

impl CompositionNativeSources {
    /// An empty group, settled from the start.
    pub(crate) fn new() -> Self {
        Self(Held::new(
            COMPOSITION_NATIVE_SOURCES_KIND,
            CompositionNativeParts {
                accounting_native_connection_parts: None,
                accounting_native_reference_parts: None,
                accounting_native_unused_reference: None,
                #[cfg(feature = "scheduling")]
                scheduler_native_finalization_owner: None,
                #[cfg(feature = "scheduling")]
                scheduler_native_outside_sources: None,
                #[cfg(feature = "scheduling")]
                scheduler_native_connection_parts: None,
                #[cfg(feature = "scheduling")]
                scheduler_native_unopened_inputs: None,
                #[cfg(feature = "scheduling")]
                scheduler_native_schema_parts: None,
                #[cfg(feature = "scheduling")]
                scheduler_native_schema_transient: None,
                #[cfg(feature = "scheduling")]
                scheduler_native_terminals: None,
            },
            CompositionNativeParts::all_empty,
        ))
    }
}

impl Default for CompositionNativeSources {
    fn default() -> Self {
        Self::new()
    }
}
