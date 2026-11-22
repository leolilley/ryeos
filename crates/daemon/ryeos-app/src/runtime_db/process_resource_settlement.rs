//! Contract checks for the existing process-resource owner transaction.
//!
//! These checks neither allocate a resource nor observe a process or driver.
//! Callers must obtain observations from the selected admitted cleanup owner;
//! a worker message, timeout or absent PID is not an observation by itself.
//! Durable reservations now distinguish both authorities. Trusted executable
//! admission remains disabled pending the retained node/product and actual
//! driver-retirement joins. These checks cannot mint that evidence.

/// Opaque digests join authorities without interpreting resource vocabulary.
pub type ContractDigest = [u8; 32];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceSettlementAuthority {
    LocalProcessScope,
    TrustedProcessGroup { cleanup_contract: ContractDigest },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceSettlementCeiling {
    LocalProcessScopeOnly,
    /// Explicit node-policy opt-in to this exact trusted cleanup contract.
    LocalScopeOrTrustedGroup {
        cleanup_contract: ContractDigest,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementRefusal {
    TrustedAuthorityNotAdmitted,
    CleanupContractMismatch,
    ProcessRetirementUnknown,
    ProcessAuthorityMismatch,
    DriverRetirementUnknown,
    DriverOwnerMismatch,
    DriverResourceSetMismatch,
    LocalResourceScopeMissing,
}

/// Agreement of independently admitted node and product cleanup contracts.
/// Resource access (including deployment visibility) is deliberately absent:
/// suitability/access cannot select or downgrade settlement authority.
pub fn admit_settlement_authority(
    node: ResourceSettlementCeiling,
    product: ResourceSettlementAuthority,
) -> Result<ResourceSettlementAuthority, SettlementRefusal> {
    match (node, product) {
        (_, ResourceSettlementAuthority::LocalProcessScope) => Ok(product),
        (
            ResourceSettlementCeiling::LocalScopeOrTrustedGroup {
                cleanup_contract: node,
            },
            ResourceSettlementAuthority::TrustedProcessGroup {
                cleanup_contract: product,
            },
        ) if node == product => Ok(ResourceSettlementAuthority::TrustedProcessGroup {
            cleanup_contract: product,
        }),
        (ResourceSettlementCeiling::LocalProcessScopeOnly, _) => {
            Err(SettlementRefusal::TrustedAuthorityNotAdmitted)
        }
        _ => Err(SettlementRefusal::CleanupContractMismatch),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessRetirementObservation {
    Unknown,
    /// Obtained through the exact retained Lillux scope recovery authority.
    LocalScopeRetired,
    /// Obtained through the exact trusted group cleanup authority. This does
    /// not assert hostile descendant death or independent placement death.
    TrustedGroupRetired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustedDriverRetirement {
    pub cleanup_contract: ContractDigest,
    pub owner_incarnation: ContractDigest,
    pub resource_set: ContractDigest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverRetirementObservation {
    Unknown,
    /// The admitted trusted cleanup contract observed completion/retirement
    /// of device use for this exact process occurrence and resource set.
    Retired(TrustedDriverRetirement),
}

/// Check evidence before the existing transaction can mark an owner cleaned.
/// Request completion, an empty pool and provider replacement are not inputs.
/// Unknown evidence leaves the existing reservation/owner in contention.
pub fn require_resource_retirement(
    authority: ResourceSettlementAuthority,
    owner_incarnation: ContractDigest,
    resource_set: ContractDigest,
    process: ProcessRetirementObservation,
    driver: DriverRetirementObservation,
) -> Result<(), SettlementRefusal> {
    if process == ProcessRetirementObservation::Unknown {
        return Err(SettlementRefusal::ProcessRetirementUnknown);
    }
    match authority {
        ResourceSettlementAuthority::LocalProcessScope => {
            if process != ProcessRetirementObservation::LocalScopeRetired {
                return Err(SettlementRefusal::ProcessAuthorityMismatch);
            }
            // Preserve the existing admitted local-scope cleanup mechanism.
            // A trusted group observation cannot substitute for its scope.
            Ok(())
        }
        ResourceSettlementAuthority::TrustedProcessGroup { cleanup_contract } => {
            if process != ProcessRetirementObservation::TrustedGroupRetired {
                return Err(SettlementRefusal::ProcessAuthorityMismatch);
            }
            let DriverRetirementObservation::Retired(retired) = driver else {
                return Err(SettlementRefusal::DriverRetirementUnknown);
            };
            if retired.cleanup_contract != cleanup_contract {
                return Err(SettlementRefusal::CleanupContractMismatch);
            }
            if retired.owner_incarnation != owner_incarnation {
                return Err(SettlementRefusal::DriverOwnerMismatch);
            }
            if retired.resource_set != resource_set {
                return Err(SettlementRefusal::DriverResourceSetMismatch);
            }
            Ok(())
        }
    }
}

/// Current serialized resource owners have no explicit trusted authority.
/// Missing scope must therefore refuse cleanup, never select the new variant.
/// Device-free trusted sessions (including Q) retain their existing behavior.
pub fn require_current_local_resource_scope(
    has_resources: bool,
    has_scope: bool,
) -> Result<(), SettlementRefusal> {
    if has_resources && !has_scope {
        return Err(SettlementRefusal::LocalResourceScopeMissing);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTRACT: ContractDigest = [1; 32];
    const OWNER: ContractDigest = [2; 32];
    const RESOURCES: ContractDigest = [3; 32];

    fn trusted() -> ResourceSettlementAuthority {
        ResourceSettlementAuthority::TrustedProcessGroup {
            cleanup_contract: CONTRACT,
        }
    }

    fn retirement() -> TrustedDriverRetirement {
        TrustedDriverRetirement {
            cleanup_contract: CONTRACT,
            owner_incarnation: OWNER,
            resource_set: RESOURCES,
        }
    }

    fn check(
        process: ProcessRetirementObservation,
        driver: DriverRetirementObservation,
    ) -> Result<(), SettlementRefusal> {
        require_resource_retirement(trusted(), OWNER, RESOURCES, process, driver)
    }

    #[test]
    fn trusted_requires_exact_node_and_product_agreement() {
        assert_eq!(
            admit_settlement_authority(ResourceSettlementCeiling::LocalProcessScopeOnly, trusted()),
            Err(SettlementRefusal::TrustedAuthorityNotAdmitted)
        );
        assert_eq!(
            admit_settlement_authority(
                ResourceSettlementCeiling::LocalScopeOrTrustedGroup {
                    cleanup_contract: [4; 32]
                },
                trusted()
            ),
            Err(SettlementRefusal::CleanupContractMismatch)
        );
        assert_eq!(
            admit_settlement_authority(
                ResourceSettlementCeiling::LocalScopeOrTrustedGroup {
                    cleanup_contract: CONTRACT
                },
                trusted()
            ),
            Ok(trusted())
        );
    }

    #[test]
    fn trusted_opt_in_does_not_downgrade_local_scope_product() {
        for ceiling in [
            ResourceSettlementCeiling::LocalProcessScopeOnly,
            ResourceSettlementCeiling::LocalScopeOrTrustedGroup {
                cleanup_contract: CONTRACT,
            },
        ] {
            assert_eq!(
                admit_settlement_authority(ceiling, ResourceSettlementAuthority::LocalProcessScope),
                Ok(ResourceSettlementAuthority::LocalProcessScope)
            );
        }
    }

    #[test]
    fn group_death_with_unknown_driver_keeps_capacity_quarantined() {
        assert_eq!(
            check(
                ProcessRetirementObservation::TrustedGroupRetired,
                DriverRetirementObservation::Unknown
            ),
            Err(SettlementRefusal::DriverRetirementUnknown)
        );
    }

    #[test]
    fn driver_retirement_without_group_retirement_is_insufficient() {
        assert_eq!(
            check(
                ProcessRetirementObservation::Unknown,
                DriverRetirementObservation::Retired(retirement())
            ),
            Err(SettlementRefusal::ProcessRetirementUnknown)
        );
    }

    #[test]
    fn driver_evidence_cannot_cross_owner_resource_or_contract() {
        for (retired, expected) in [
            (
                TrustedDriverRetirement {
                    owner_incarnation: [4; 32],
                    ..retirement()
                },
                SettlementRefusal::DriverOwnerMismatch,
            ),
            (
                TrustedDriverRetirement {
                    resource_set: [4; 32],
                    ..retirement()
                },
                SettlementRefusal::DriverResourceSetMismatch,
            ),
            (
                TrustedDriverRetirement {
                    cleanup_contract: [4; 32],
                    ..retirement()
                },
                SettlementRefusal::CleanupContractMismatch,
            ),
        ] {
            assert_eq!(
                check(
                    ProcessRetirementObservation::TrustedGroupRetired,
                    DriverRetirementObservation::Retired(retired)
                ),
                Err(expected)
            );
        }
    }

    #[test]
    fn exact_trusted_process_and_driver_evidence_join() {
        assert_eq!(
            check(
                ProcessRetirementObservation::TrustedGroupRetired,
                DriverRetirementObservation::Retired(retirement())
            ),
            Ok(())
        );
    }

    #[test]
    fn retirement_authorities_cannot_substitute_for_each_other() {
        assert_eq!(
            require_resource_retirement(
                ResourceSettlementAuthority::LocalProcessScope,
                OWNER,
                RESOURCES,
                ProcessRetirementObservation::TrustedGroupRetired,
                DriverRetirementObservation::Retired(retirement())
            ),
            Err(SettlementRefusal::ProcessAuthorityMismatch)
        );
        assert_eq!(
            check(
                ProcessRetirementObservation::LocalScopeRetired,
                DriverRetirementObservation::Retired(retirement())
            ),
            Err(SettlementRefusal::ProcessAuthorityMismatch)
        );
    }

    #[test]
    fn current_device_free_q_group_remains_supported_but_resources_need_scope() {
        assert_eq!(require_current_local_resource_scope(false, false), Ok(()));
        assert_eq!(require_current_local_resource_scope(false, true), Ok(()));
        assert_eq!(require_current_local_resource_scope(true, true), Ok(()));
        assert_eq!(
            require_current_local_resource_scope(true, false),
            Err(SettlementRefusal::LocalResourceScopeMissing)
        );
    }
}
