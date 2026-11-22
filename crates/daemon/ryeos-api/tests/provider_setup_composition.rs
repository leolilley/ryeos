//! Native registry selection must match the selected provider service content.

#[test]
fn provider_setup_registry_matches_native_selection() {
    let registry = ryeos_api::registry::build_service_registry();
    let descriptor = ryeos_api::handlers::ALL
        .iter()
        .find(|descriptor| descriptor.endpoint == "model-providers.validate");
    assert_eq!(
        registry.has("model-providers.validate"),
        cfg!(feature = "model-provider-setup")
    );
    assert_eq!(descriptor.is_some(), cfg!(feature = "model-provider-setup"));
    if let Some(descriptor) = descriptor {
        assert_eq!(descriptor.service_ref, "service:model-providers/validate");
        assert_eq!(
            descriptor.required_caps,
            &["ryeos.execute.service.model-providers/validate"]
        );
    } else {
        assert!(registry.get("model-providers.validate").is_none());
    }
    // Shared operation inspection survives omission of provider setup.
    assert!(registry.has("node.status"));
}
