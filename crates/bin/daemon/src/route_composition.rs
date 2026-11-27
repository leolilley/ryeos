//! The one composition of the node's route table.
//!
//! A running node's routes never change: the table is compiled once at boot
//! from the node's configuration and never replaced. The same composition
//! compiles a candidate configuration for prospective admission (a bundle
//! install or replacement on a stopped node), so a configuration admitted
//! there compiles to the table the node boots with.

use std::sync::Arc;

use anyhow::Result;
use ryeos_api::routes::RouteTable;
use ryeos_app::route_raw::RawRouteSpec;
use ryeos_app::service_registry::ServiceDescriptor;
use ryeos_engine::kind_registry::KindRegistry;

/// Every service compiled into this daemon: the API's and the UI's.
pub fn service_descriptors() -> &'static [ServiceDescriptor] {
    static DESCRIPTORS: std::sync::OnceLock<Vec<ServiceDescriptor>> = std::sync::OnceLock::new();
    DESCRIPTORS
        .get_or_init(|| {
            ryeos_api::handlers::ALL
                .iter()
                .chain(ryeos_ui::handlers::ALL.iter())
                .copied()
                .collect()
        })
        .as_slice()
}

/// Compile the route table for a node configuration snapshot.
pub fn compose_route_table(
    snapshot: &ryeos_app::node_config::NodeConfigSnapshot,
    ui: Arc<ryeos_ui::UiState>,
    kinds: &KindRegistry,
) -> Result<RouteTable> {
    compose_route_table_from(&snapshot.routes, ui, kinds)
}

/// Compile the route table for a set of route items, with the response
/// modes, auth types and UI extensions this daemon provides.
pub fn compose_route_table_from(
    routes: &[RawRouteSpec],
    ui: Arc<ryeos_ui::UiState>,
    kinds: &KindRegistry,
) -> Result<RouteTable> {
    let mut mode_registry =
        ryeos_api::routes::response_modes::ResponseModeRegistry::with_api_builtins_from(
            service_descriptors(),
            Arc::new(kinds.clone()),
        );
    let mut extensions = ryeos_api::routes::RouteExtensionRegistry {
        auth: ryeos_api::routes::invokers::AuthInvokerRegistry::with_api_builtins(),
    };
    extensions.auth.register(
        "external_occurrence",
        Arc::new(
            ryeos_api::routes::invokers::external_occurrence_invocation::ExternalOccurrenceAuthFactory,
        ),
    );
    extensions.auth.register(
        "external_channel",
        Arc::new(
            ryeos_api::routes::invokers::external_occurrence_invocation::ExternalChannelAuthFactory,
        ),
    );

    ryeos_ui::register_extensions(&mut extensions, &mut mode_registry, ui);

    ryeos_api::routes::build_route_table_with_extensions(routes, &mode_registry, &extensions)
        .map_err(|errors| {
            let msgs: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
            anyhow::anyhow!(
                "route table build failed ({} error(s)): {}",
                errors.len(),
                msgs.join("; ")
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every route the core and UI bundles ship, read as a node reads them.
    fn shipped_routes() -> Vec<RawRouteSpec> {
        let root = ryeos_engine::test_support::workspace_root();
        let mut routes = Vec::new();
        for bundle in ["core", "ryeos-ui"] {
            let dir = root.join("bundles").join(bundle).join(".ai/node/routes");
            let mut files: Vec<_> = std::fs::read_dir(&dir)
                .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "yaml"))
                .collect();
            files.sort();
            for path in files {
                let text = std::fs::read_to_string(&path).unwrap();
                let mut raw: RawRouteSpec = serde_yaml::from_str(&text)
                    .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
                raw.source_file = path;
                routes.push(raw);
            }
        }
        assert!(!routes.is_empty(), "no shipped routes found");
        routes
    }

    /// Boot and prospective admission each compose with their own UI state;
    /// the same route items must compile to the same table either way.
    #[test]
    fn the_same_routes_compose_to_the_same_table_fingerprint() {
        let kinds = ryeos_engine::test_support::load_live_kind_registry();
        let routes = shipped_routes();
        let at_boot = compose_route_table_from(&routes, Arc::new(ryeos_ui::UiState::new()), &kinds)
            .expect("the shipped routes compose");
        let prospective =
            compose_route_table_from(&routes, Arc::new(ryeos_ui::UiState::new()), &kinds)
                .expect("the shipped routes compose");
        assert!(!at_boot.fingerprint().is_empty());
        assert_eq!(at_boot.fingerprint(), prospective.fingerprint());
        assert_eq!(at_boot.all.len(), routes.len());
    }

    /// The table is stored once, at boot: `ApiState` holds it as a plain
    /// `Arc`, so nothing can replace it in a running node, and no swap exists.
    #[test]
    fn the_route_table_is_never_replaced_in_a_running_node() {
        let root = ryeos_engine::test_support::workspace_root();
        let api_state =
            std::fs::read_to_string(root.join("crates/daemon/ryeos-api/src/api_state.rs")).unwrap();
        assert!(
            api_state.contains("pub route_table: Arc<RouteTable>,"),
            "ApiState must hold the route table as an immutable Arc"
        );
        let routes =
            std::fs::read_to_string(root.join("crates/daemon/ryeos-api/src/routes/mod.rs"))
                .unwrap();
        assert!(
            !routes.contains("pub fn swap("),
            "a route-table swap was reintroduced; a running node's routes never change"
        );
    }
}
