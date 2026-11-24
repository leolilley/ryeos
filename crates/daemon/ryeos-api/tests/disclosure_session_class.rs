//! A caller that holds a minted grant but no verified key (a browser session)
//! is a principal; a caller that holds no grant at all is the public.

use ryeos_api::test_support as test_state;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::operator_authority::disclosure_class_for;
use ryeos_node_runtime::disclosure::DisclosureClass;

#[test]
fn a_session_grant_is_a_principal_and_no_grant_is_the_public() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let session = HandlerContext::new(
        "session:abc".to_owned(),
        vec!["ryeos.execute.service.ui/seat/open".to_owned()],
        false,
    );
    assert_eq!(
        disclosure_class_for(&state, &session),
        DisclosureClass::Principal
    );
    let unauthenticated_route =
        HandlerContext::new("route:identity/public-key".to_owned(), vec![], false);
    assert_eq!(
        disclosure_class_for(&state, &unauthenticated_route),
        DisclosureClass::Public
    );
    assert_eq!(
        disclosure_class_for(&state, &HandlerContext::anonymous()),
        DisclosureClass::Public
    );
}
