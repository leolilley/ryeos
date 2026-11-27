use ryeos_api::test_support as test_state;

use std::sync::Arc;

use ryeos_api::handlers::external_content_activate::{Request, handle};
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::managed_external_content_operation::AcquisitionMode;
use ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass;

fn context(
    owner: String,
    class: AuthorizedKeyPrincipalClass,
    origin_site_id: Option<String>,
) -> HandlerContext {
    HandlerContext::new_with_authority(
        owner,
        vec!["ryeos.execute.service.external-content/activate".to_owned()],
        true,
        Some(class),
        origin_site_id,
    )
}

#[tokio::test]
async fn managed_activation_admits_an_origin_bound_remote_principal() {
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    let operator =
        ryeos_node_runtime::operator_identity::test_support::load_operator(&state.config.app_root)
            .unwrap()
            .principal_id();
    let error = handle(
        Request {
            activation_ref: "config:fixture/missing-activation".to_owned(),
            mode: AcquisitionMode::Offline,
            offline_archive_root: None,
        },
        context(
            operator,
            AuthorizedKeyPrincipalClass::RemotePrincipal,
            Some(format!("site:{}", "b".repeat(64))),
        ),
        Arc::new(state),
    )
    .await
    .expect_err("the empty test registry must reject the missing recipe");

    let detail = format!("{error:#}");
    assert!(
        !detail.contains("local_client configured operator")
            && !detail.contains("operator_owner_not_spoken_for")
            && !detail.contains("admitted operator required"),
        "managed activation must preserve admitted remote-principal authority: {detail}"
    );
}

#[tokio::test]
async fn managed_activation_refuses_a_stranger_remote_principal() {
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    let error = handle(
        Request {
            activation_ref: "config:fixture/missing-activation".to_owned(),
            mode: AcquisitionMode::Offline,
            offline_archive_root: None,
        },
        context(
            format!("fp:{}", "a".repeat(64)),
            AuthorizedKeyPrincipalClass::RemotePrincipal,
            Some(format!("site:{}", "b".repeat(64))),
        ),
        Arc::new(state),
    )
    .await
    .expect_err("a key that does not speak for the operator owner is not an operator");

    assert!(
        format!("{error:#}").contains("operator_owner_not_spoken_for"),
        "{error:#}"
    );
}

#[tokio::test]
async fn managed_activation_rejects_a_remote_node_principal() {
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    let error = handle(
        Request {
            activation_ref: "config:fixture/missing-activation".to_owned(),
            mode: AcquisitionMode::Offline,
            offline_archive_root: None,
        },
        context(
            format!("fp:{}", "a".repeat(64)),
            AuthorizedKeyPrincipalClass::RemoteNode,
            Some(format!("site:{}", "b".repeat(64))),
        ),
        Arc::new(state),
    )
    .await
    .expect_err("remote nodes are not activation operators");

    assert!(format!("{error:#}").contains("operator actions reject remote_node grants"));
}
