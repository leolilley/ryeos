//! Subject proofs for Surface publication.
//!
//! A proof is constructed by the owner that re-checked the selected subject
//! (a registered project entry, a saved record) and is compared against the
//! candidate attachment under the session lock. Browser request data never
//! supplies one, and a proof grants no session capability by itself.

use std::sync::Arc;

use super::{
    AttachmentStoreError, BindingAttachmentCandidate, ParticularSurfaceReceipt,
    SavedSurfaceRequestIdentity,
};
use ryeos_client_base::surface::saved_surfaces::SavedSurfaceOrigin;

/// The retained registered-project entry the selected subject resolved to,
/// together with the exact pinned directory authority its candidate must carry.
pub struct RegisteredProjectSurfaceAdmission {
    pub principal_id: String,
    pub local_id: String,
    pub root: String,
    pub authority: Arc<lillux::PinnedDirectory>,
}

impl RegisteredProjectSurfaceAdmission {
    pub fn matches_candidate(
        &self,
        principal_id: &str,
        candidate: &BindingAttachmentCandidate,
    ) -> bool {
        self.principal_id == principal_id
            && candidate.registered_project_id.as_deref() == Some(self.local_id.as_str())
            && candidate.compiled_binding.binding.project_root.as_deref()
                == Some(self.root.as_str())
            && candidate
                .project_authority
                .as_ref()
                .is_some_and(|authority| Arc::ptr_eq(authority, &self.authority))
            && self.authority.path() == std::path::Path::new(&self.root)
            && self.authority.ensure_path_binding().is_ok()
    }
}

/// One particular saved view-set resumed as a Surface.
pub struct ParticularSurfaceAdmission {
    pub principal_id: String,
    pub project: Option<RegisteredProjectSurfaceAdmission>,
    pub receipt: ParticularSurfaceReceipt,
    pub max_receipt_bytes: usize,
}

impl ParticularSurfaceAdmission {
    pub fn receipt(&self) -> &ParticularSurfaceReceipt {
        &self.receipt
    }

    pub fn max_receipt_bytes(&self) -> usize {
        self.max_receipt_bytes
    }

    pub fn matches_candidate(
        &self,
        principal_id: &str,
        candidate: &BindingAttachmentCandidate,
    ) -> bool {
        if principal_id != self.principal_id
            || candidate.compiled_binding.binding.surface.canonical_ref != self.receipt.surface_ref
        {
            return false;
        }
        subject_matches(self.project.as_ref(), principal_id, candidate)
    }
}

/// One saved Surface (its origin and ordered tabs) admitted as a whole.
pub struct SavedSurfaceAdmission {
    principal_id: String,
    origin_project: Option<RegisteredProjectSurfaceAdmission>,
    receipt: SavedSurfaceRequestIdentity,
    tabs: Vec<SavedSurfaceTabAdmission>,
}

enum SavedSurfaceTabAdmission {
    Particular(ParticularSurfaceAdmission),
    Reusable {
        principal_id: String,
        origin: SavedSurfaceOrigin,
        project: Option<RegisteredProjectSurfaceAdmission>,
    },
}

impl SavedSurfaceTabAdmission {
    fn matches_candidate(&self, principal: &str, candidate: &BindingAttachmentCandidate) -> bool {
        match self {
            Self::Particular(proof) => proof.matches_candidate(principal, candidate),
            Self::Reusable {
                principal_id,
                origin,
                project,
            } => {
                principal_id == principal
                    && candidate.compiled_binding.binding.surface.canonical_ref
                        == origin.surface_ref
                    && match project {
                        Some(proof) => {
                            origin
                                .project
                                .as_ref()
                                .is_some_and(|reference| reference.local_id == proof.local_id)
                                && proof.matches_candidate(principal, candidate)
                        }
                        None => origin.project.is_none() && projectless(candidate),
                    }
            }
        }
    }
}

impl SavedSurfaceAdmission {
    /// Admission of an ordered tab collection of particular saved view-sets.
    pub fn particular(
        principal_id: String,
        receipt: SavedSurfaceRequestIdentity,
        origin_project: Option<RegisteredProjectSurfaceAdmission>,
        tabs: Vec<ParticularSurfaceAdmission>,
    ) -> Result<Self, AttachmentStoreError> {
        let super::SavedSurfaceRequestBody::Particular { tabs: saved } = &receipt.body else {
            return Err(AttachmentStoreError::PresentationInvalid);
        };
        if saved.len() != tabs.len()
            || saved
                .iter()
                .zip(&tabs)
                .any(|(record, proof)| !record.same_request(proof.receipt()))
        {
            return Err(AttachmentStoreError::PresentationInvalid);
        }
        Ok(Self {
            principal_id,
            origin_project,
            receipt,
            tabs: tabs
                .into_iter()
                .map(SavedSurfaceTabAdmission::Particular)
                .collect(),
        })
    }

    #[cfg(test)]
    pub(crate) fn projectless_fixture(
        principal: &str,
        receipt: SavedSurfaceRequestIdentity,
    ) -> Self {
        let super::SavedSurfaceRequestBody::Particular { tabs: saved } = &receipt.body else {
            panic!("particular fixture requires particular receipt")
        };
        let tabs = saved
            .iter()
            .cloned()
            .map(|receipt| {
                SavedSurfaceTabAdmission::Particular(ParticularSurfaceAdmission {
                    principal_id: principal.into(),
                    project: None,
                    receipt,
                    max_receipt_bytes:
                        ryeos_client_base::surface::view_sets::MAX_PARTICULAR_VIEW_SET_LIBRARY_BYTES,
                })
            })
            .collect();
        Self {
            principal_id: principal.into(),
            origin_project: None,
            receipt,
            tabs,
        }
    }

    /// Admission of an ordered tab collection of reusable (or composed)
    /// saved view-sets, one optional project proof per tab.
    pub fn reusable(
        principal: String,
        receipt: SavedSurfaceRequestIdentity,
        origin_project: Option<RegisteredProjectSurfaceAdmission>,
        tab_projects: Vec<Option<RegisteredProjectSurfaceAdmission>>,
    ) -> Result<Self, AttachmentStoreError> {
        let (super::SavedSurfaceRequestBody::Reusable { tabs }
        | super::SavedSurfaceRequestBody::Composed { tabs }) = &receipt.body
        else {
            return Err(AttachmentStoreError::PresentationInvalid);
        };
        if tabs.len() != tab_projects.len() {
            return Err(AttachmentStoreError::PresentationInvalid);
        }
        let tabs = tabs
            .iter()
            .zip(tab_projects)
            .map(|(tab, project)| SavedSurfaceTabAdmission::Reusable {
                principal_id: principal.clone(),
                origin: tab.origin.clone(),
                project,
            })
            .collect();
        Ok(Self {
            principal_id: principal,
            origin_project,
            receipt,
            tabs,
        })
    }

    pub fn receipt(&self) -> &SavedSurfaceRequestIdentity {
        &self.receipt
    }

    pub fn matches_origin(&self, principal: &str, candidate: &BindingAttachmentCandidate) -> bool {
        principal == self.principal_id
            && candidate.compiled_binding.binding.surface.canonical_ref
                == self.receipt.origin.surface_ref
            && subject_matches(self.origin_project.as_ref(), principal, candidate)
    }

    pub fn matches_tab(
        &self,
        index: usize,
        principal: &str,
        candidate: &BindingAttachmentCandidate,
    ) -> bool {
        self.tabs
            .get(index)
            .is_some_and(|tab| tab.matches_candidate(principal, candidate))
    }
}

fn projectless(candidate: &BindingAttachmentCandidate) -> bool {
    candidate.registered_project_id.is_none()
        && candidate.project_authority.is_none()
        && candidate.compiled_binding.binding.project_root.is_none()
}

fn subject_matches(
    project: Option<&RegisteredProjectSurfaceAdmission>,
    principal_id: &str,
    candidate: &BindingAttachmentCandidate,
) -> bool {
    match project {
        Some(project) => project.matches_candidate(principal_id, candidate),
        None => projectless(candidate),
    }
}
