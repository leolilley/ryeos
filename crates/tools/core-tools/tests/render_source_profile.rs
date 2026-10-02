//! Agreement of authored Render data with the publication declaration parser.
//! This exercises no signatures, node activation, native launch or provider.

use std::collections::BTreeSet;

#[test]
fn render_source_declaration_commits_exact_profile_and_bounded_capability_ceiling() {
    let source = include_str!("../../../../bundles/render-sandbox/.ai/manifest.source.yaml");
    let manifest: ryeos_bundle::manifest::BundleManifestSource =
        serde_yaml::from_str(source).unwrap();
    assert_eq!(manifest.name, "render-sandbox");
    assert_eq!(manifest.external_lifecycle_adapters.len(), 1);
    let declaration = &manifest.external_lifecycle_adapters[0];
    declaration.validate().unwrap();
    assert_eq!(declaration.id, "render-sandbox-early-access");
    assert_eq!(declaration.provider_spec.path, "specs/provider-spec.json");
    let profile = include_bytes!("../../../../bundles/render-sandbox/specs/provider-spec.json");
    assert_eq!(
        declaration.provider_spec.sha256,
        lillux::sha256_hex(profile)
    );
    let settings = include_bytes!(
        "../../../host-adapters/render-sandbox-adapter/fixtures/settings.schema.json"
    );
    assert_eq!(
        declaration.settings_schema_digest,
        lillux::sha256_hex(settings)
    );
    let capabilities = serde_json::to_value(&declaration.capabilities).unwrap();
    let capabilities: BTreeSet<&str> = capabilities
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert_eq!(
        capabilities,
        BTreeSet::from([
            "supervisor_activation",
            "authoritative_no_occurrence",
            "exact_terminal_observation",
        ])
    );
}
