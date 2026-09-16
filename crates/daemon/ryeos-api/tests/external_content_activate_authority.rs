mod test_state;

use std::sync::Arc;

use ryeos_api::handlers::external_content_activate::{Request, handle};
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::identity::AuthorizedKeyPrincipalClass;
use ryeos_app::managed_external_content_operation::AcquisitionMode;

fn context(class: AuthorizedKeyPrincipalClass, origin_site_id: Option<String>) -> HandlerContext {
    HandlerContext::new_with_authority(
        format!("fp:{}", "a".repeat(64)),
        vec!["ryeos.execute.service.external-content/activate".to_owned()],
        true,
        Some(class),
        origin_site_id,
    )
}

#[tokio::test]
async fn managed_activation_admits_an_origin_bound_remote_operator() {
    let (_tmp, state) = test_state::build_test_state();
    let error = handle(
        Request {
            activation_ref: "config:fixture/missing-activation".to_owned(),
            mode: AcquisitionMode::Offline,
            offline_archive_root: None,
        },
        context(
            AuthorizedKeyPrincipalClass::RemoteOperator,
            Some(format!("site:{}", "b".repeat(64))),
        ),
        Arc::new(state),
    )
    .await
    .expect_err("the empty test registry must reject the missing recipe");

    let detail = format!("{error:#}");
    assert!(
        !detail.contains("local_client configured operator"),
        "managed activation must preserve admitted remote-operator authority: {detail}"
    );
}

#[tokio::test]
async fn managed_activation_rejects_a_remote_node_principal() {
    let (_tmp, state) = test_state::build_test_state();
    let error = handle(
        Request {
            activation_ref: "config:fixture/missing-activation".to_owned(),
            mode: AcquisitionMode::Offline,
            offline_archive_root: None,
        },
        context(
            AuthorizedKeyPrincipalClass::RemoteNode,
            Some(format!("site:{}", "b".repeat(64))),
        ),
        Arc::new(state),
    )
    .await
    .expect_err("remote nodes are not activation operators");

    assert!(format!("{error:#}").contains("operator actions reject remote_node grants"));
}
