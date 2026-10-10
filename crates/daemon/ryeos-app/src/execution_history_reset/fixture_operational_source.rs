//! Test source establishment through the real State namespace owner.
//! No invented schema, marker, recovery absence, or unsigned quota issuer.
use super::*;

#[derive(Default)]
struct FixtureRepairs(std::sync::Mutex<Vec<ryeos_state::ProjectionRepairRequest>>);
impl ryeos_state::ProjectionRepairSink for FixtureRepairs {
    fn request_repair(&self, request: ryeos_state::ProjectionRepairRequest) {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(request);
    }
}
struct RefuseUnknownLiveness;
impl ryeos_state::RuntimeLivenessInspector for RefuseUnknownLiveness {
    fn chain_has_live_recovery_state(&self, _chain_root_id: &str) -> Result<bool> {
        anyhow::bail!("inspection fixture cannot authenticate runtime recovery liveness")
    }
}
pub(super) fn establish(
    directory: &lillux::PinnedDirectory,
    namespace: &lillux::PinnedDirectoryLock,
    identity: &ryeos_node_runtime::identity::NodeIdentity,
) -> Result<()> {
    namespace.ensure_protects(directory)?;
    let mut trust = ryeos_state::refs::TrustStore::new();
    trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
    let repairs = Arc::new(FixtureRepairs::default());
    let state = ryeos_state::StateDb::open_with_projection_repair_sink_runtime_liveness_and_namespace_authority(
        directory.path(), directory.try_clone()?, namespace.clone(), repairs.clone(),
        Arc::new(trust), &RefuseUnknownLiveness,
    )?;
    // Release fixture setup before capturing the original signed source facts.
    drop(state);
    anyhow::ensure!(
        repairs
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_empty(),
        "inspection fixture source setup left projection repair obligations"
    );
    Ok(())
}
