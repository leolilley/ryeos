//! Operator-owned binding between retained external bytes and one consumer.
//!
//! Importing bytes proves only that the node captured them. This object is the
//! separate target-local grant that permits one exact consumer authority to
//! use the retained manifest. A signed generic head selects the current grant
//! for a stable subject coordinate. The selected immutable binding identity
//! additionally commits to the local authorizer and its node-signed grant.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::EffectiveSourceClosureProjection;

pub const EXTERNAL_CONTENT_BINDING_KIND: &str = "external_content_binding";
pub const EXTERNAL_CONTENT_BINDING_SCHEMA: &str = "ryeos.external_content_binding.v3";
pub const EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE: &str = "external-content-bindings";
pub const EXTERNAL_CONTENT_BINDING_SCHEMA_EPOCH: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalContentBindingState {
    Active,
    Released,
}

/// Exact authority of the item for which retained bytes are authorized.
///
/// Bundle consumers retain the installed-publisher boundary used by managed
/// activation. Project consumers are deliberately generation-scoped and
/// commit to the effective item after exact source admission but before
/// external realization admission; using the post-realization digest would
/// make binding authority circular.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExternalContentConsumerAuthority {
    InstalledBundle {
        consumer_ref: String,
        publisher_fingerprint: String,
    },
    PinnedProject {
        consumer_ref: String,
        publisher_fingerprint: String,
        project_snapshot_hash: String,
        effective_consumer_digest: String,
        // Explicit null means the admitted program has no separately executed
        // source tree (for example a declarative exact-realization command).
        // The pinned generation and effective digest still bind its definition.
        // Missing fields are invalid; this is not a source-admission fallback.
        #[serde(deserialize_with = "super::deserialize_required_nullable")]
        source_closure: Option<EffectiveSourceClosureProjection>,
    },
}

impl ExternalContentConsumerAuthority {
    pub fn installed_bundle(
        consumer_ref: String,
        publisher_fingerprint: String,
    ) -> anyhow::Result<Self> {
        let authority = Self::InstalledBundle {
            consumer_ref,
            publisher_fingerprint,
        };
        authority.validate()?;
        Ok(authority)
    }

    pub fn pinned_project(
        consumer_ref: String,
        publisher_fingerprint: String,
        project_snapshot_hash: String,
        effective_consumer_digest: String,
        source_closure: Option<EffectiveSourceClosureProjection>,
    ) -> anyhow::Result<Self> {
        let authority = Self::PinnedProject {
            consumer_ref,
            publisher_fingerprint,
            project_snapshot_hash,
            effective_consumer_digest,
            source_closure,
        };
        authority.validate()?;
        Ok(authority)
    }

    pub fn consumer_ref(&self) -> &str {
        match self {
            Self::InstalledBundle { consumer_ref, .. }
            | Self::PinnedProject { consumer_ref, .. } => consumer_ref,
        }
    }

    pub fn publisher_fingerprint(&self) -> &str {
        match self {
            Self::InstalledBundle {
                publisher_fingerprint,
                ..
            }
            | Self::PinnedProject {
                publisher_fingerprint,
                ..
            } => publisher_fingerprint,
        }
    }

    pub fn source_closure(&self) -> Option<&EffectiveSourceClosureProjection> {
        match self {
            Self::InstalledBundle { .. } => None,
            Self::PinnedProject { source_closure, .. } => source_closure.as_ref(),
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        validate_bounded_identity("external-content consumer ref", self.consumer_ref())?;
        validate_hash(
            "external-content consumer publisher fingerprint",
            self.publisher_fingerprint(),
        )?;
        if let Self::PinnedProject {
            project_snapshot_hash,
            effective_consumer_digest,
            source_closure,
            ..
        } = self
        {
            validate_hash(
                "external-content consumer project snapshot",
                project_snapshot_hash,
            )?;
            validate_hash(
                "external-content effective consumer digest",
                effective_consumer_digest,
            )?;
            if let Some(source_closure) = source_closure {
                source_closure.validate()?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentBinding {
    pub schema: String,
    pub kind: String,
    /// Stable head coordinate for manifest + consumer + target node.
    pub binding_subject_id: String,
    /// Immutable identity of this exact authorization event.
    pub binding_id: String,
    pub manifest_hash: String,
    pub manifest_kind: String,
    pub consumer: ExternalContentConsumerAuthority,
    pub target_node_fingerprint: String,
    pub state: ExternalContentBindingState,
    pub authorized_by: String,
    pub authorizer_grant_digest: String,
    pub recorded_at: String,
}

impl ExternalContentBinding {
    pub fn active(
        manifest_hash: String,
        manifest_kind: String,
        consumer: ExternalContentConsumerAuthority,
        target_node_fingerprint: String,
        authorized_by: String,
        authorizer_grant_digest: String,
    ) -> anyhow::Result<Self> {
        let recorded_at = lillux::time::iso8601_now();
        let binding_subject_id = Self::derive_binding_subject_id(
            &manifest_hash,
            &manifest_kind,
            &consumer,
            &target_node_fingerprint,
        )?;
        let binding_id = Self::derive_binding_id(
            &binding_subject_id,
            ExternalContentBindingState::Active,
            &authorized_by,
            &authorizer_grant_digest,
            &recorded_at,
        )?;
        let value = Self {
            schema: EXTERNAL_CONTENT_BINDING_SCHEMA.to_owned(),
            kind: EXTERNAL_CONTENT_BINDING_KIND.to_owned(),
            binding_subject_id,
            binding_id,
            manifest_hash,
            manifest_kind,
            consumer,
            target_node_fingerprint,
            state: ExternalContentBindingState::Active,
            authorized_by,
            authorizer_grant_digest,
            recorded_at,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn released_from(
        active: &Self,
        released_by: String,
        authorizer_grant_digest: String,
    ) -> anyhow::Result<Self> {
        active.validate()?;
        if active.state != ExternalContentBindingState::Active {
            anyhow::bail!("only an active external-content binding can be released");
        }
        let mut value = Self {
            state: ExternalContentBindingState::Released,
            authorized_by: released_by,
            authorizer_grant_digest,
            recorded_at: lillux::time::iso8601_now(),
            ..active.clone()
        };
        value.binding_id = Self::derive_binding_id(
            &value.binding_subject_id,
            value.state,
            &value.authorized_by,
            &value.authorizer_grant_digest,
            &value.recorded_at,
        )?;
        value.validate()?;
        Ok(value)
    }

    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        if value.get("schema").and_then(Value::as_str) != Some(EXTERNAL_CONTENT_BINDING_SCHEMA) {
            anyhow::bail!("unsupported external-content binding schema");
        }
        let binding: Self = serde_json::from_value(value.clone())?;
        binding.validate()?;
        Ok(binding)
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn derive_binding_subject_id(
        manifest_hash: &str,
        manifest_kind: &str,
        consumer: &ExternalContentConsumerAuthority,
        target_node_fingerprint: &str,
    ) -> anyhow::Result<String> {
        validate_hash("external-content binding manifest", manifest_hash)?;
        validate_manifest_kind(manifest_kind)?;
        consumer.validate()?;
        validate_hash(
            "external-content binding target node",
            target_node_fingerprint,
        )?;
        let canonical = lillux::canonical_json(&serde_json::json!({
            "schema": "ryeos.external_content_binding_subject.v3",
            "manifest_hash": manifest_hash,
            "manifest_kind": manifest_kind,
            "consumer": consumer,
            "target_node_fingerprint": target_node_fingerprint,
        }))?;
        Ok(lillux::sha256_hex(canonical.as_bytes()))
    }

    pub fn derive_binding_id(
        binding_subject_id: &str,
        state: ExternalContentBindingState,
        authorized_by: &str,
        authorizer_grant_digest: &str,
        recorded_at: &str,
    ) -> anyhow::Result<String> {
        validate_hash("external-content binding subject", binding_subject_id)?;
        validate_hash("external-content binding authorizer", authorized_by)?;
        validate_hash(
            "external-content binding authorizer grant",
            authorizer_grant_digest,
        )?;
        super::parse_canonical_timestamp(recorded_at)?;
        let canonical = lillux::canonical_json(&serde_json::json!({
            "schema": EXTERNAL_CONTENT_BINDING_SCHEMA,
            "binding_subject_id": binding_subject_id,
            "state": state,
            "authorized_by": authorized_by,
            "authorizer_grant_digest": authorizer_grant_digest,
            "recorded_at": recorded_at,
        }))?;
        Ok(lillux::sha256_hex(canonical.as_bytes()))
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != EXTERNAL_CONTENT_BINDING_SCHEMA {
            anyhow::bail!("external-content binding schema is not current");
        }
        if self.kind != EXTERNAL_CONTENT_BINDING_KIND {
            anyhow::bail!("external-content binding kind is invalid");
        }
        validate_hash("external-content binding subject", &self.binding_subject_id)?;
        validate_hash("external-content binding id", &self.binding_id)?;
        validate_hash("external-content binding manifest", &self.manifest_hash)?;
        validate_manifest_kind(&self.manifest_kind)?;
        self.consumer.validate()?;
        validate_hash(
            "external-content binding target node",
            &self.target_node_fingerprint,
        )?;
        validate_hash("external-content binding authorizer", &self.authorized_by)?;
        validate_hash(
            "external-content binding authorizer grant",
            &self.authorizer_grant_digest,
        )?;
        super::parse_canonical_timestamp(&self.recorded_at)?;
        let expected_subject = Self::derive_binding_subject_id(
            &self.manifest_hash,
            &self.manifest_kind,
            &self.consumer,
            &self.target_node_fingerprint,
        )?;
        if self.binding_subject_id != expected_subject {
            anyhow::bail!("external-content binding subject contradicts its authority tuple");
        }
        let expected_binding = Self::derive_binding_id(
            &self.binding_subject_id,
            self.state,
            &self.authorized_by,
            &self.authorizer_grant_digest,
            &self.recorded_at,
        )?;
        if self.binding_id != expected_binding {
            anyhow::bail!("external-content binding id contradicts its authorization tuple");
        }
        Ok(())
    }
}

fn validate_manifest_kind(value: &str) -> anyhow::Result<()> {
    if !matches!(
        value,
        super::EXTERNAL_CONTENT_MANIFEST_KIND | super::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND
    ) {
        anyhow::bail!("external-content binding names an unsupported manifest kind");
    }
    Ok(())
}

fn validate_hash(label: &str, value: &str) -> anyhow::Result<()> {
    super::thread_snapshot::validate_canonical_hash(label, value)
}

fn validate_bounded_identity(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 512
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        anyhow::bail!("{label} is empty, unbounded, or non-canonical");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarative_consumer_requires_explicit_absence_and_preserves_exact_identity() {
        let consumer = ExternalContentConsumerAuthority::pinned_project(
            "tool:project/build".to_owned(),
            "b".repeat(64),
            "c".repeat(64),
            "d".repeat(64),
            None,
        )
        .unwrap();
        let mut value = serde_json::to_value(&consumer).unwrap();
        assert!(value.get("source_closure").unwrap().is_null());
        assert_eq!(
            serde_json::from_value::<ExternalContentConsumerAuthority>(value.clone()).unwrap(),
            consumer
        );
        value.as_object_mut().unwrap().remove("source_closure");
        assert!(serde_json::from_value::<ExternalContentConsumerAuthority>(value).is_err());

        let subject = |consumer: &ExternalContentConsumerAuthority| {
            ExternalContentBinding::derive_binding_subject_id(
                &"a".repeat(64),
                super::super::EXTERNAL_CONTENT_MANIFEST_KIND,
                consumer,
                &"2".repeat(64),
            )
            .unwrap()
        };
        let mut source_owning = consumer.clone();
        let ExternalContentConsumerAuthority::PinnedProject { source_closure, .. } =
            &mut source_owning
        else {
            unreachable!()
        };
        *source_closure = Some(EffectiveSourceClosureProjection {
            schema: super::super::EFFECTIVE_SOURCE_BINDING_SCHEMA,
            binding_hash: "e".repeat(64),
            content_manifest_hash: "f".repeat(64),
            owner_key: "1".repeat(64),
            file_count: 1,
            total_bytes: 1,
        });
        assert_ne!(subject(&consumer), subject(&source_owning));

        let binding = ExternalContentBinding::active(
            "a".repeat(64),
            super::super::EXTERNAL_CONTENT_MANIFEST_KIND.to_owned(),
            consumer,
            "2".repeat(64),
            "3".repeat(64),
            "4".repeat(64),
        )
        .unwrap();
        let mut predecessor = binding.to_value().unwrap();
        predecessor["schema"] = serde_json::json!("ryeos.external_content_binding.v2");
        assert!(ExternalContentBinding::from_value(&predecessor).is_err());
    }

    #[test]
    fn project_binding_identity_includes_generation_source_node_and_authorizer() {
        let consumer = ExternalContentConsumerAuthority::pinned_project(
            "tool:project/build".to_owned(),
            "b".repeat(64),
            "c".repeat(64),
            "d".repeat(64),
            Some(EffectiveSourceClosureProjection {
                schema: super::super::EFFECTIVE_SOURCE_BINDING_SCHEMA,
                binding_hash: "e".repeat(64),
                content_manifest_hash: "f".repeat(64),
                owner_key: "1".repeat(64),
                file_count: 1,
                total_bytes: 1,
            }),
        )
        .unwrap();
        let active = ExternalContentBinding::active(
            "a".repeat(64),
            super::super::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.to_owned(),
            consumer,
            "2".repeat(64),
            "3".repeat(64),
            "4".repeat(64),
        )
        .unwrap();
        let active_value = active.to_value().unwrap();
        assert_eq!(
            ExternalContentBinding::from_value(&active_value).unwrap(),
            active
        );
        assert!(active_value.get("project_path").is_none());
        let foreign_subject = ExternalContentBinding::derive_binding_subject_id(
            &active.manifest_hash,
            &active.manifest_kind,
            &active.consumer,
            &"7".repeat(64),
        )
        .unwrap();
        assert_ne!(foreign_subject, active.binding_subject_id);
        let released =
            ExternalContentBinding::released_from(&active, "5".repeat(64), "6".repeat(64)).unwrap();
        assert_eq!(released.binding_subject_id, active.binding_subject_id);
        assert_ne!(released.binding_id, active.binding_id);
        assert_eq!(released.state, ExternalContentBindingState::Released);
    }
}
