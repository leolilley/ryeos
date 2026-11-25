//! The class that shapes a service's answer is the class the node admits the
//! key as: an admitted operator is the operator and every other key is not.

use ryeos_api::test_support as test_state;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::operator_authority::{disclosure_class_for, require_admitted_operator};
use ryeos_node_runtime::disclosure::DisclosureClass;
use ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass;

fn remote(class: AuthorizedKeyPrincipalClass, origin: Option<&str>) -> HandlerContext {
    HandlerContext::new_with_authority(
        format!("fp:{}", "a".repeat(64)),
        vec![],
        true,
        Some(class),
        origin.map(str::to_owned),
    )
}

#[test]
fn the_class_is_operator_exactly_when_the_node_admits_the_key_as_operator() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let local =
        ryeos_node_runtime::operator_identity::test_support::load_operator(&state.config.app_root)
            .unwrap();
    let configured = HandlerContext::new_with_authority(
        local.principal_id(),
        vec![],
        true,
        Some(AuthorizedKeyPrincipalClass::LocalClient),
        None,
    );
    let other_local = HandlerContext::new_with_authority(
        format!("fp:{}", "b".repeat(64)),
        vec![],
        true,
        Some(AuthorizedKeyPrincipalClass::LocalClient),
        None,
    );
    let cases: Vec<(&str, HandlerContext, DisclosureClass)> = vec![
        (
            "configured local operator",
            configured,
            DisclosureClass::Operator,
        ),
        (
            "forwarded operator with origin proof",
            remote(
                AuthorizedKeyPrincipalClass::RemotePrincipal,
                Some("site:source"),
            ),
            DisclosureClass::Operator,
        ),
        (
            "remote principal without origin proof",
            remote(AuthorizedKeyPrincipalClass::RemotePrincipal, None),
            DisclosureClass::Principal,
        ),
        (
            "remote node",
            remote(AuthorizedKeyPrincipalClass::RemoteNode, Some("site:source")),
            DisclosureClass::Principal,
        ),
        ("another local key", other_local, DisclosureClass::Principal),
        (
            "no verified key",
            HandlerContext::anonymous(),
            DisclosureClass::Public,
        ),
    ];
    for (label, context, expected) in cases {
        assert_eq!(disclosure_class_for(&state, &context), expected, "{label}");
        if expected == DisclosureClass::Operator {
            assert!(
                require_admitted_operator(&state, &context).is_ok(),
                "{label}"
            );
        } else if expected == DisclosureClass::Principal {
            assert!(
                require_admitted_operator(&state, &context).is_err(),
                "{label}"
            );
        }
    }
}
