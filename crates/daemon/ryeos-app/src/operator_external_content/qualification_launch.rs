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
                anyhow::ensure!(
                    source.policy.policy.consumer_execution_context.is_none(),
                    "content qualification consumer context has no authenticated preparation"
                );
                Ok(PreparedQualificationLaunch::ActivatedContent {
                    launch_id: request.launch_id.clone(),
                    owner_fingerprint: context.fingerprint.clone(),
                    acquisition_mode,
                    source: Box::new(source),
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
    },
}

impl PreparedQualificationLaunch {
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
                ..
            } => {
                anyhow::ensure!(
                    owner_fingerprint == &context.fingerprint,
                    "content preparation differs from authenticated launch owner"
                );
                source.require_current(state, context, *acquisition_mode)
            }
        }
    }
}
