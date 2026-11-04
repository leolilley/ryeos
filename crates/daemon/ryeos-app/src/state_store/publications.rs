//! State anchor, observation and artifact publication.
use super::*;

impl StateStore {
    /// Append or exactly fold one source-scoped project observation. The
    /// rebuildable projection is the idempotency index, so the identity lives
    /// for exactly as long as its chain evidence.
    pub fn publish_project_observation(
        &self,
        params: &ryeos_runtime::ProjectObservationPublishParams,
        source_definition_ref: &str,
        source_effective_definition_digest: &str,
    ) -> Result<ProjectObservationPublication> {
        params.observation.validate()?;
        let occurrence = ryeos_state::ProjectObservationOccurrence {
            graph_run_id: params.graph_run_id.clone(),
            node: params.node.clone(),
            step: params.step,
        };
        occurrence.validate()?;
        let payload_canonical = lillux::canonical_json(&params.observation.payload)
            .context("canonicalize project observation payload")?;
        let payload_fingerprint = lillux::sha256_hex(payload_canonical.as_bytes());

        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let thread = g
            .state_db
            .get_thread(&params.thread_id)?
            .ok_or_else(|| anyhow!("project-observation thread not found: {}", params.thread_id))?;
        if thread.status != ThreadStatus::Running.as_str() {
            bail!(
                "project-observation publication requires a running thread; {} is '{}'",
                params.thread_id,
                thread.status
            );
        }
        let runtime = g
            .runtime_db
            .get_runtime_info(&params.thread_id)?
            .ok_or_else(|| anyhow!("runtime row missing while publishing project observation"))?;
        if runtime.stop_intent.is_some()
            || !self
                .process_attachment_admission_open
                .load(Ordering::Acquire)
        {
            bail!(
                "project-observation publication is closed for thread {}",
                params.thread_id
            );
        }
        if !self.projection_health.is_current() {
            bail!("project-observation admission requires a current thread projection");
        }

        let observation_id = ryeos_state::project_observation_id(
            &thread.chain_root_id,
            source_definition_ref,
            source_effective_definition_digest,
            &params.observation.namespace,
            &params.observation.stable_id,
        )?;
        let payload = ryeos_state::ProjectObservationRecordedPayload {
            schema_version: ryeos_state::PROJECT_OBSERVATION_SCHEMA.to_string(),
            observation_id: observation_id.clone(),
            namespace: params.observation.namespace.clone(),
            stable_id: params.observation.stable_id.clone(),
            source_definition_ref: source_definition_ref.to_string(),
            source_effective_definition_digest: source_effective_definition_digest.to_string(),
            occurrence,
            payload_fingerprint,
            payload: params.observation.payload.clone(),
        };
        payload.validate_for_chain(&thread.chain_root_id)?;
        let event_payload = serde_json::to_value(&payload)?;

        if let Some(existing) = ryeos_state::queries::get_project_observation_identity(
            g.state_db.projection(),
            &observation_id,
        )? {
            if existing.chain_root_id != thread.chain_root_id
                || existing.source_definition_ref != source_definition_ref
                || existing.source_effective_definition_digest != source_effective_definition_digest
                || existing.namespace != params.observation.namespace
                || existing.stable_id != params.observation.stable_id
                || existing.payload_fingerprint != payload.payload_fingerprint
            {
                bail!(
                    "project observation {} contradicts its durable identity",
                    observation_id
                );
            }
            let row = ryeos_state::queries::get_event_by_hash(
                g.state_db.projection(),
                &existing.event_hash,
            )?
            .ok_or_else(|| {
                anyhow!(
                    "project observation {} references missing projected event {}",
                    observation_id,
                    existing.event_hash
                )
            })?;
            let event = persisted_event_from_projection_row(row)?;
            if event.event_type != ryeos_state::event_types::PROJECT_OBSERVATION_RECORDED
                || event.payload != event_payload
            {
                bail!(
                    "project observation {} has divergent durable event content",
                    observation_id
                );
            }
            return Ok(ProjectObservationPublication {
                event,
                inserted: false,
            });
        }

        let event = NewEventRecord {
            event_type: ryeos_state::event_types::PROJECT_OBSERVATION_RECORDED.to_string(),
            storage_class: "indexed".to_string(),
            payload: event_payload,
        };
        let mut persisted = append_events_locked(
            &g,
            Some(permit.cas_guard()),
            &thread.chain_root_id,
            &params.thread_id,
            std::slice::from_ref(&event),
        )?;
        let event = persisted
            .pop()
            .ok_or_else(|| anyhow!("project observation append returned no event"))?;
        Ok(ProjectObservationPublication {
            event,
            inserted: true,
        })
    }

    /// Append or exactly fold a daemon-authored provider publication/replay
    /// observation after the provider record has crossed its proof boundary.
    pub fn publish_provider_call_observation(
        &self,
        thread_id: &str,
        draft: &ryeos_state::ProviderCallObservationDraft,
    ) -> Result<ProviderCallObservationPublication> {
        draft.validate()?;
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let thread = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("provider-call observation thread not found: {thread_id}"))?;
        if thread.status != ThreadStatus::Running.as_str() {
            bail!(
                "provider-call observation requires a running thread; {thread_id} is '{}'",
                thread.status
            );
        }
        // This event completes evidence for an already-issued provider effect.
        // The callback admission layer therefore classifies it with settlement
        // as a stop-completion mutation. Re-applying the ordinary authoring
        // gate here would make cancellation after contact strand a financially
        // terminal call without its provider observation.
        if !self.projection_health.is_current() {
            bail!("provider-call observation requires a current thread projection");
        }

        let observation_id = ryeos_state::provider_call_observation_id(
            &thread.chain_root_id,
            thread_id,
            draft.turn,
            draft.attempt_number,
            &draft.effect_coordinate_digest,
            draft.source,
            &draft.answer_digest,
            &draft.record_hash,
            draft.publication,
            draft.replayed_from.as_ref(),
        )?;
        let payload = ryeos_state::ProviderCallObservationRecordedPayload {
            schema_version: ryeos_state::PROVIDER_CALL_OBSERVATION_SCHEMA.to_string(),
            observation_id: observation_id.clone(),
            turn: draft.turn,
            attempt_number: draft.attempt_number,
            effect_coordinate_digest: draft.effect_coordinate_digest.clone(),
            source: draft.source,
            answer_digest: draft.answer_digest.clone(),
            record_hash: draft.record_hash.clone(),
            publication: draft.publication,
            replayed_from: draft.replayed_from.clone(),
        };
        payload.validate_for_subject(&thread.chain_root_id, thread_id)?;
        let event_payload = serde_json::to_value(&payload)?;
        let source = serde_json::to_value(payload.source)?
            .as_str()
            .expect("provider source serializes as a string")
            .to_string();
        let publication = serde_json::to_value(payload.publication)?
            .as_str()
            .expect("provider publication serializes as a string")
            .to_string();

        if let Some(existing) = ryeos_state::queries::get_provider_call_observation_identity(
            g.state_db.projection(),
            &observation_id,
        )? {
            if existing.chain_root_id != thread.chain_root_id
                || existing.thread_id != thread_id
                || existing.turn != draft.turn
                || existing.attempt_number != draft.attempt_number
                || existing.effect_coordinate_digest != draft.effect_coordinate_digest
                || existing.source != source
                || existing.answer_digest != draft.answer_digest
                || existing.record_hash != draft.record_hash
                || existing.publication != publication
            {
                bail!(
                    "provider-call observation {observation_id} contradicts its durable identity"
                );
            }
            let row = ryeos_state::queries::get_event_by_hash(
                g.state_db.projection(),
                &existing.event_hash,
            )?
            .ok_or_else(|| {
                anyhow!(
                    "provider-call observation {observation_id} references missing event {}",
                    existing.event_hash
                )
            })?;
            let event = persisted_event_from_projection_row(row)?;
            if event.event_type != ryeos_state::event_types::PROVIDER_CALL_OBSERVATION_RECORDED
                || event.payload != event_payload
            {
                bail!(
                    "provider-call observation {observation_id} has divergent durable event content"
                );
            }
            return Ok(ProviderCallObservationPublication {
                event,
                inserted: false,
            });
        }

        let event = NewEventRecord {
            event_type: ryeos_state::event_types::PROVIDER_CALL_OBSERVATION_RECORDED.to_string(),
            storage_class: "indexed".to_string(),
            payload: event_payload,
        };
        let mut persisted = append_events_locked(
            &g,
            Some(permit.cas_guard()),
            &thread.chain_root_id,
            thread_id,
            std::slice::from_ref(&event),
        )?;
        let event = persisted
            .pop()
            .ok_or_else(|| anyhow!("provider-call observation append returned no event"))?;
        Ok(ProviderCallObservationPublication {
            event,
            inserted: true,
        })
    }

    #[tracing::instrument(
        name = "state:publish_artifact",
        skip(self, artifact),
        fields(thread_id = %thread_id, artifact_type = %artifact.artifact_type)
    )]
    pub fn publish_artifact(
        &self,
        thread_id: &str,
        artifact: &NewArtifactRecord,
    ) -> Result<(ThreadArtifactRecord, PersistedEventRecord)> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let thread_row = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found: {thread_id}"))?;

        // Artifact projection is derived from the artifact_published event
        // during project_event (see projection.rs). No direct write needed.

        let event = NewEventRecord {
            event_type: "artifact_published".to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "artifact_type": artifact.artifact_type,
                "uri": artifact.uri,
                "content_hash": artifact.content_hash,
                "metadata": artifact.metadata,
            }),
        };
        if !self.projection_health.is_current() {
            bail!("artifact admission requires a current thread projection");
        }
        validate_artifact_event_admission(&g, thread_id, std::slice::from_ref(&event))?;

        let te = convert_events(
            std::slice::from_ref(&event),
            &thread_row.chain_root_id,
            thread_id,
        );
        let result = committed_value(g.state_db.append_events_admitted(
            &thread_row.chain_root_id,
            thread_id,
            te,
            vec![],
            g.signer.as_ref(),
            &g.runtime_db,
            permit.cas_guard(),
        )?);

        let persisted = persisted_from_append(&result, &[event])?;

        let persisted_event = persisted.into_iter().next().ok_or_else(|| {
            anyhow!("artifact_published event was not persisted for thread {thread_id}")
        })?;

        let artifact_id = persisted_event.event_id;

        let record = ThreadArtifactRecord {
            artifact_id,
            artifact_type: artifact.artifact_type.clone(),
            uri: artifact.uri.clone(),
            content_hash: artifact.content_hash.clone(),
            metadata: artifact.metadata.clone(),
        };

        Ok((record, persisted_event))
    }
}
