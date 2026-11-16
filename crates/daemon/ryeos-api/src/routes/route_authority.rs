//! Authority a signed route lends the principal of a launch that has no human
//! caller.
//!
//! An `auth: none` or `auth: hmac` request is authenticated, at most, as the
//! route itself. A thread it births still never holds authority its caller did
//! not lend, so the route is the caller: it states the capabilities it lends in
//! its signed `authority.capabilities`, and the route bundle's signed manifest
//! bounds them (`runtime_authority.route_capabilities`, or
//! `runtime_authority.operator_vault` for the operator-vault capability).
//! Strict root admission then requires those capabilities to cover everything
//! the launched item declares. A route without a declaration lends nothing.

use crate::route_error::{RouteConfigError, RouteDispatchError};
use crate::routes::compile::CompiledRoute;
use crate::routes::invocation::RoutePrincipal;
use ryeos_app::route_raw::RawRouteSpec;

/// Stable public code of a route whose declared authority its bundle's signed
/// manifest does not back.
pub const ROUTE_AUTHORITY_NOT_BACKED: &str = "route_authority_not_backed";

/// Check a route's authority declaration when the route table is built.
pub(crate) fn validate_route_authority(raw: &RawRouteSpec) -> Result<(), RouteConfigError> {
    let Some(authority) = &raw.authority else {
        return Ok(());
    };
    let invalid = |reason: String| RouteConfigError::InvalidRouteAuthority {
        id: raw.id.clone(),
        reason,
    };
    if !matches!(raw.auth.as_str(), "none" | "hmac") {
        return Err(invalid(format!(
            "`authority` is lent only to a route with no human caller (`auth: none` or \
             `auth: hmac`); a route with `auth: {}` already runs with its caller's authority. \
             Remove `authority`",
            raw.auth
        )));
    }
    if raw.response.mode == "handler" {
        return Err(invalid(
            "a handler route runs a tool under its fixed handler principal, and a tool cannot \
             declare capabilities, so there is nothing for `authority` to cover; remove \
             `authority` (operator-vault access is `response.source_config.authority`)"
                .to_owned(),
        ));
    }
    if authority.capabilities.is_empty() {
        return Err(invalid(
            "`authority.capabilities` is empty; list the capabilities the route lends or remove \
             `authority`"
                .to_owned(),
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for capability in &authority.capabilities {
        if !seen.insert(capability.as_str()) {
            return Err(invalid(format!(
                "`authority.capabilities` lists `{capability}` more than once"
            )));
        }
        ryeos_runtime::authorizer::validate_scope_pattern(capability).map_err(|reason| {
            invalid(format!(
                "`authority.capabilities` entry `{capability}` is not a canonical capability: \
                 {reason}"
            ))
        })?;
        if capability.as_str() != ryeos_engine::execution_contract::OPERATOR_VAULT_CAPABILITY
            && ryeos_bundle::runtime_authority::composed_grant_overlaps_manifest_runtime_authority(
                capability,
            )
        {
            return Err(invalid(format!(
                "`authority.capabilities` entry `{capability}` is runtime authority minted from \
                 the bundle manifest's own families; a route lends only caller-covered \
                 capabilities (plus the operator-vault capability, which the manifest's \
                 `operator_vault` bounds)"
            )));
        }
    }
    Ok(())
}

/// Give a request's principal the authority its route declared, after proving
/// the route bundle's signed manifest backs every declared capability. A route
/// that declares nothing leaves the principal unchanged, with no capability.
pub(crate) fn lend_route_authority(
    route: &CompiledRoute,
    state: &ryeos_app::state::AppState,
    principal: &mut RoutePrincipal,
) -> Result<(), RouteDispatchError> {
    let Some(authority) = &route.raw_response.authority else {
        return Ok(());
    };
    let refuse = |detail: String| RouteDispatchError::Structured {
        code: ROUTE_AUTHORITY_NOT_BACKED.to_owned(),
        status: 403,
        body: serde_json::json!({
            "code": ROUTE_AUTHORITY_NOT_BACKED,
            "error": format!("route `{}`: {detail}", route.id),
        }),
        thread_id: None,
    };
    let bundle_root = crate::routes::response_modes::handler_mode::bundle_root_from_route_source(
        &route.source_file,
    )
    .ok_or_else(|| {
        refuse(format!(
            "its source file {} is not under a bundle's `.ai/node/routes`, so no signed \
             manifest can back its authority",
            route.source_file.display()
        ))
    })?;
    let canonical_root = bundle_root.canonicalize().map_err(|error| {
        RouteDispatchError::Internal(format!(
            "canonicalize route bundle root {}: {error}",
            bundle_root.display()
        ))
    })?;
    let bundle = state
        .engine
        .registered_bundle_name_at(&canonical_root)
        .ok_or_else(|| {
            refuse(format!(
                "its bundle root {} is not a registered bundle",
                canonical_root.display()
            ))
        })?
        .to_owned();
    let verified = ryeos_bundle::manifest::load_verified_manifest(
        &canonical_root.join(ryeos_engine::AI_DIR),
        &bundle,
        &state.engine.node_trust_store,
    )
    .map_err(|error| {
        refuse(format!(
            "bundle `{bundle}`'s signed manifest could not be verified: {error:#}"
        ))
    })?;
    let not_backed = verified
        .manifest
        .runtime_authority
        .route_capabilities_not_backed(&authority.capabilities);
    if !not_backed.is_empty() {
        let caps = not_backed
            .iter()
            .map(|cap| format!("`{cap}`"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(refuse(format!(
            "declares authority {caps} that bundle `{bundle}`'s signed manifest does not back; \
             list them under `runtime_authority.route_capabilities` in the bundle manifest \
             (`runtime_authority.operator_vault: true` for the operator-vault capability), or \
             remove them from the route's `authority.capabilities`"
        )));
    }
    principal.scopes = authority.capabilities.clone();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_app::route_raw::{RawRouteAuthority, RawRouteSpec};

    fn route(auth: &str, mode: &str, capabilities: Option<Vec<&str>>) -> RawRouteSpec {
        let mut value = serde_json::json!({
            "id": "fixture/hook",
            "path": "/fixture/hook",
            "methods": ["POST"],
            "auth": auth,
            "response": { "mode": mode },
        });
        if let Some(capabilities) = capabilities {
            value["authority"] = serde_json::json!({ "capabilities": capabilities });
        }
        serde_json::from_value(value).expect("route fixture")
    }

    #[test]
    fn a_route_without_a_declaration_is_valid_and_lends_nothing() {
        assert!(validate_route_authority(&route("hmac", "launch", None)).is_ok());
    }

    #[test]
    fn a_declaration_is_valid_on_a_route_with_no_human_caller() {
        for auth in ["none", "hmac"] {
            let raw = route(auth, "launch", Some(vec!["ryeos.execute.tool.echo"]));
            assert_eq!(
                raw.authority,
                Some(RawRouteAuthority {
                    capabilities: vec!["ryeos.execute.tool.echo".to_owned()]
                })
            );
            assert!(validate_route_authority(&raw).is_ok(), "auth {auth}");
        }
    }

    #[test]
    fn a_declaration_is_refused_where_a_human_caller_already_lends_authority() {
        let error = validate_route_authority(&route(
            "ryeos_signed",
            "launch",
            Some(vec!["ryeos.execute.tool.echo"]),
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains("ryeos_signed") && error.contains("Remove `authority`"));
    }

    #[test]
    fn a_declaration_is_refused_on_a_handler_route() {
        let error = validate_route_authority(&route(
            "none",
            "handler",
            Some(vec!["ryeos.execute.tool.echo"]),
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains("handler route"));
    }

    #[test]
    fn empty_duplicate_malformed_and_reserved_capabilities_are_refused() {
        for capabilities in [
            vec![],
            vec!["ryeos.execute.tool.echo", "ryeos.execute.tool.echo"],
            vec!["ryeos.p*.tool.echo"],
            vec!["ryeos.append.bundle-events.arc/event"],
            vec!["*"],
        ] {
            assert!(
                validate_route_authority(&route("hmac", "launch", Some(capabilities.clone())))
                    .is_err(),
                "must refuse {capabilities:?}"
            );
        }
    }

    #[test]
    fn the_operator_vault_capability_is_declarable_and_bounded_by_the_manifest_flag() {
        let raw = route(
            "hmac",
            "launch",
            Some(vec![
                ryeos_engine::execution_contract::OPERATOR_VAULT_CAPABILITY,
            ]),
        );
        assert!(validate_route_authority(&raw).is_ok());
    }

    #[test]
    fn an_unknown_authority_field_is_refused_when_the_route_is_parsed() {
        let parsed = serde_json::from_value::<RawRouteSpec>(serde_json::json!({
            "id": "fixture/hook",
            "path": "/fixture/hook",
            "methods": ["POST"],
            "auth": "hmac",
            "response": { "mode": "launch" },
            "authority": { "capabilities": ["ryeos.execute.tool.echo"], "scopes": [] },
        }));
        assert!(parsed.is_err());
    }
}
