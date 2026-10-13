//! Test source establishment through the native gate: the State roots and
//! current-schema Operational database a live node's first boot creates,
//! built on a test host released before return. No invented schema, marker,
//! recovery absence, or unsigned quota issuer.
use super::*;

pub(super) fn establish(
    directory: &lillux::PinnedDirectory,
    namespace: &lillux::PinnedDirectoryLock,
) -> Result<()> {
    namespace.ensure_protects(directory)?;
    let host = lillux_sqlite::test_host::TestNativeHost::acquire();
    ryeos_state::test_support::seed_node_state(&host, directory.path())?;
    let (issued, recorded) = host.ticket_census();
    anyhow::ensure!(issued as usize == recorded, "every seeding ticket reaches the census");
    drop(host.release());
    Ok(())
}
