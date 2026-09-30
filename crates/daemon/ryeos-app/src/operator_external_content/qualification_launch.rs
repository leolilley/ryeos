//! Source adapters for the single accepted qualification launch owner.
//! No reservation, spawn, scheduler or publication machinery lives here.

use super::{content_qualification, product_qualification::launch};
use crate::managed_external_content_operation::AcquisitionMode;
use crate::{handler_context::HandlerContext, state::AppState};
use anyhow::Result;
use ryeos_state::external_content::products::{
    composition::ProductSelectionInputs, qualification::ProductQualificationPolicySource,
};

#[derive(Clone)]
pub enum QualificationLaunchSourceRequest {
    CapturedProduct(launch::ProductQualificationLaunchRequest),
    ActivatedContent(content_qualification::ContentQualificationLaunchRequest),
}

impl QualificationLaunchSourceRequest {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::CapturedProduct(request) => request.validate(),
            Self::ActivatedContent(request) => request.validate(),
        }
    }

    pub fn launch_id(&self) -> &str {
        match self {
            Self::CapturedProduct(request) => &request.launch_id,
            Self::ActivatedContent(request) => &request.launch_id,
        }
    }

    pub fn prepare_after_reservation(
        &self,
        state: &AppState,
        context: &HandlerContext,
    ) -> Result<PreparedQualificationLaunch> {
        match self {
            Self::CapturedProduct(request) => Ok(PreparedQualificationLaunch::CapturedProduct(
                Box::new(launch::prepare_after_reservation(state, context, request)?),
            )),
            Self::ActivatedContent(request) => {
                // Read-only qualification never requests online acquisition.
                let acquisition_mode = AcquisitionMode::Offline;
                let source = content_qualification::prepare_after_reservation(
                    state,
                    context,
                    request,
                    acquisition_mode,
                )?;
                let consumer_content = source
                    .policy
                    .policy
                    .consumer_execution_context
                    .as_ref()
                    .map(|_| {
                        content_qualification::PreparedContentQualificationConsumer::prepare(
                            state,
                            context,
                            &source,
                            acquisition_mode,
                        )
                    })
                    .transpose()?
                    .map(Box::new);
                Ok(PreparedQualificationLaunch::ActivatedContent {
                    launch_id: request.launch_id.clone(),
                    owner_fingerprint: context.fingerprint.clone(),
                    acquisition_mode,
                    source: Box::new(source),
                    consumer_content,
                })
            }
        }
    }
}

pub enum PreparedQualificationLaunch {
    CapturedProduct(Box<launch::PreparedProductQualificationLaunch>),
    ActivatedContent {
        launch_id: String,
        owner_fingerprint: String,
        acquisition_mode: AcquisitionMode,
        source: Box<content_qualification::PreparedContentQualificationSource>,
        consumer_content: Option<Box<content_qualification::PreparedContentQualificationConsumer>>,
    },
}

impl PreparedQualificationLaunch {
    /// Extend the source-owned lease without transferring or publishing it.
    pub fn stage_remote_verifier_sources(
        &mut self,
        state: &AppState,
        producer_sources: &std::collections::BTreeMap<String, ryeos_state::external_content::products::qualification::ProductProducerRecipeSourceIdentity>,
    ) -> Result<std::collections::BTreeMap<String, ryeos_state::external_content::products::qualification::remote_verifier_source::QualificationRemoteVerifierSource>>{
        let policy = self.policy_source().clone();
        let subject = self.subject_manifest_hash().to_owned();
        let mut retained = std::collections::BTreeMap::new();
        for (scenario_id, scenario) in &policy.policy.producer_scenarios {
            if scenario.remote_verifier.is_none() {
                continue;
            }
            let verifier = super::product_qualification::prepare_remote_consumer_verifier_from_admitted_sources(
                state, &policy, producer_sources, &subject, scenario_id,
            )?;
            let publication = match self {
                Self::CapturedProduct(source) => source.consumer_publication_mut()?,
                Self::ActivatedContent {
                    consumer_content, ..
                } => consumer_content
                    .as_mut()
                    .ok_or_else(|| {
                        anyhow::anyhow!("remote verifier requires admitted consumer content")
                    })?
                    .publication_mut()?,
            };
            retained.insert(
                scenario_id.clone(),
                verifier.stage_in_publication(
                    state,
                    &policy,
                    scenario_id,
                    &subject,
                    publication,
                )?,
            );
        }
        Ok(retained)
    }

    pub fn consumer_content_identity(&self) -> Result<Option<ryeos_state::external_content::products::qualification::ProductQualificationConsumerContentIdentity>>{
        match self {
            Self::CapturedProduct(source) => source.consumer_content_identity(),
            Self::ActivatedContent {
                consumer_content, ..
            } => consumer_content
                .as_ref()
                .map(|content| content.retained_identity())
                .transpose(),
        }
    }

    pub fn take_consumer_content_publication(
        &mut self,
    ) -> Result<Option<ryeos_state::PendingCasPublication>> {
        match self {
            Self::CapturedProduct(source) => source.take_consumer_content_publication(),
            Self::ActivatedContent {
                consumer_content, ..
            } => consumer_content
                .take()
                .map(|content| (*content).into_publication())
                .transpose(),
        }
    }
    pub fn launch_id(&self) -> &str {
        match self {
            Self::CapturedProduct(source) => &source.launch_id,
            Self::ActivatedContent { launch_id, .. } => launch_id,
        }
    }

    pub fn owner_fingerprint(&self) -> &str {
        match self {
            Self::CapturedProduct(source) => &source.owner_fingerprint,
            Self::ActivatedContent {
                owner_fingerprint, ..
            } => owner_fingerprint,
        }
    }

    pub fn policy_source(&self) -> &ProductQualificationPolicySource {
        match self {
            Self::CapturedProduct(source) => &source.policy_source,
            Self::ActivatedContent { source, .. } => &source.policy,
        }
    }

    pub fn subject_manifest_hash(&self) -> &str {
        match self {
            Self::CapturedProduct(source) => &source.subject_manifest_hash,
            Self::ActivatedContent { source, .. } => &source.subject.manifest_hash,
        }
    }

    pub fn verifier_admitted_definition_digest(&self) -> Option<&str> {
        match self {
            Self::CapturedProduct(source) => source.verifier_admitted_definition_digest.as_deref(),
            Self::ActivatedContent { source, .. } => {
                Some(&source.verifier_admitted_definition_digest)
            }
        }
    }

    pub fn verifier_realized_definition_digest(&self) -> Option<&str> {
        match self {
            Self::CapturedProduct(source) => source.verifier_realized_definition_digest.as_deref(),
            Self::ActivatedContent { source, .. } => {
                Some(&source.verifier_realized_definition_digest)
            }
        }
    }

    pub fn pinned_snapshot_hash(&self) -> Option<&str> {
        match self {
            Self::CapturedProduct(source) => source.pinned_snapshot_hash.as_deref(),
            Self::ActivatedContent { .. } => None,
        }
    }

    pub fn product_selections(&self) -> ProductSelectionInputs {
        match self {
            Self::CapturedProduct(source) => source.product_selections.clone(),
            Self::ActivatedContent { .. } => Vec::new(),
        }
    }

    pub fn require_current(&self, state: &AppState, context: &HandlerContext) -> Result<()> {
        match self {
            Self::CapturedProduct(source) => {
                launch::require_current_policy_matches_prepared(state, source)
            }
            Self::ActivatedContent {
                source,
                acquisition_mode,
                owner_fingerprint,
                consumer_content,
                ..
            } => {
                anyhow::ensure!(
                    owner_fingerprint == &context.fingerprint,
                    "content preparation differs from authenticated launch owner"
                );
                source.require_current(state, context, *acquisition_mode)?;
                if let Some(consumer) = consumer_content {
                    consumer.require_current(state, context, source, *acquisition_mode)?;
                }
                Ok(())
            }
        }
    }
}
