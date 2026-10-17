//! Shared direct-only qualification root admission; no spawn authority.
use anyhow::{Context, Result, bail};
use ryeos_app::{callback_token::AdmittedRemoteConsumerGrant, state::AppState};
use ryeos_engine::protocol_vocabulary::CallbackChannel;
use serde_json::Value;
pub(super) fn admitted_qualification_root(
    state: &AppState,
    metadata: &ryeos_app::launch_metadata::RuntimeLaunchMetadata,
    thread_id: &str,
    launch_owner: &str,
    protocol: &ryeos_engine::protocols::VerifiedProtocol,
) -> Result<
    Option<(
        ryeos_state::external_content::qualification_purpose::QualificationLaunchPurpose,
        ryeos_state::objects::AdmittedLaunchCapsule,
    )>,
> {
    let Some(sealed) = metadata.sealed_root_request.as_ref() else {
        return Ok(None);
    };
    let Some(purpose) = sealed.qualification_purpose() else {
        return Ok(None);
    };
    purpose.validate()?;
    if metadata.launch_driver
        != Some(ryeos_state::objects::ExecutionLaunchDriver::DirectItemExecutor)
        || protocol.descriptor.callback_channel != CallbackChannel::Http
    {
        bail!("qualification verifier requires a direct callback-capable Tool");
    }
    let capsule = metadata
        .admitted_launch_capsule()?
        .context("qualification root has no admitted capsule")?;
    let realized = capsule
        .exact_program
        .get("effective_definition_digest")
        .and_then(Value::as_str)
        .context("qualification capsule has no realized definition")?;
    // Both the capsule and sealed invocation are finalized definitions. The
    // admitted pre-realization digest was checked at root admission instead.
    if realized != purpose.verifier_realized_definition_digest
        || sealed.effective_definition_digest().as_str()
            != purpose.verifier_realized_definition_digest
    {
        bail!("qualification verifier definition changed before callback grant");
    }
    let planning = state
        .state_store
        .launch_planning_record_for_owner(&purpose.launch_id, &purpose.owner_fingerprint)?
        .context("qualification launch has no owner-bound planning row")?;
    if planning.state != "bound"
        || planning.reserved_thread_id != thread_id
        || planning.bound_thread_id.as_deref() != Some(thread_id)
    {
        bail!("qualification launch planning row does not bind this root");
    }
    state
        .state_store
        .assert_launch_owner(thread_id, launch_owner)?;
    let root = state
        .state_store
        .get_authoritative_root_thread_snapshot(thread_id)?
        .context("qualification callback root is absent")?;
    if root.chain_root_id != thread_id
        || root.admitted_launch_capsule_hash.as_deref() != Some(capsule.content_hash()?.as_str())
    {
        bail!("qualification callback capsule differs from its born root");
    }
    Ok(Some((purpose.clone(), capsule)))
}

pub(super) fn admitted_remote_consumer_grant(
    state: &AppState,
    metadata: &ryeos_app::launch_metadata::RuntimeLaunchMetadata,
    thread_id: &str,
    launch_owner: &str,
    protocol: &ryeos_engine::protocols::VerifiedProtocol,
    root_raw_content_digest: &str,
) -> Result<Option<AdmittedRemoteConsumerGrant>> {
    if protocol.descriptor.qualification_callback
        != Some(ryeos_engine::protocols::QualificationCallbackAuthority::RemoteConsumer)
    {
        return Ok(None);
    }
    let (purpose, capsule) =
        admitted_qualification_root(state, metadata, thread_id, launch_owner, protocol)?
            .context("remote consumer callback has no admitted qualification root")?;
    let subject = metadata
        .sealed_root_request
        .as_ref()
        .context("remote consumer root has no sealed subject")?
        .admitted_program_subject()?;
    if subject.canonical_ref != purpose.verifier_ref
        || subject.raw_content_digest != root_raw_content_digest
    {
        bail!("remote consumer source differs from its admitted verifier executable closure");
    }
    let grant = AdmittedRemoteConsumerGrant {
        purpose,
        root_thread_id: thread_id.to_owned(),
        launch_owner: launch_owner.to_owned(),
        admitted_capsule_hash: capsule.content_hash()?,
        root_raw_content_digest: root_raw_content_digest.into(),
    };
    grant.validate()?;
    Ok(Some(grant))
}
