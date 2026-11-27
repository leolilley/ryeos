//! A call carrying the minted grant of a live browser session is a principal.
//! A caller with no verified key and no such grant is the public, whatever
//! scopes or identity string it presents.

use ryeos_api::test_support as test_state;
use ryeos_app::handler_context::{HandlerContext, MintedSessionGrant};
use ryeos_app::operator_authority::disclosure_class_for;
use ryeos_node_runtime::disclosure::DisclosureClass;

#[test]
fn a_live_session_grant_is_a_principal_and_nothing_else_unverified_is() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let caps = vec!["ryeos.execute.service.ui/seat/open".to_owned()];
    let session = HandlerContext::new("session:abc".to_owned(), vec![], false)
        .with_minted_session_grant(Some(MintedSessionGrant {
            session_id: "abc".to_owned(),
            granted_caps: caps.clone(),
        }));
    assert_eq!(
        disclosure_class_for(&state, &session),
        DisclosureClass::Principal
    );
    // Scopes and a "session:" string alone are not a grant.
    let lookalike = HandlerContext::new("session:abc".to_owned(), caps, false);
    assert_eq!(
        disclosure_class_for(&state, &lookalike),
        DisclosureClass::Public
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
