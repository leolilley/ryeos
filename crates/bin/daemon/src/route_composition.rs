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
use ryeos_node_runtime::node_policy::sections::public_routes::PublicRoutesPolicy;

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

/// A public route the node did not serve, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedRoute {
    pub id: String,
    pub path: String,
    /// Stable code: `public_route_not_named`.
    pub reason: &'static str,
}

/// The table a node serves, and the public routes it left out.
pub struct ComposedRoutes {
    pub table: RouteTable,
    pub skipped: Vec<SkippedRoute>,
}

/// Compile the route table for a node configuration snapshot under the
/// node's `public_routes` policy.
pub fn compose_route_table(
    snapshot: &ryeos_app::node_config::NodeConfigSnapshot,
    public_routes: &PublicRoutesPolicy,
    ui: Arc<ryeos_ui::UiState>,
    kinds: &KindRegistry,
) -> Result<ComposedRoutes> {
    compose_route_table_from(&snapshot.routes, public_routes, ui, kinds)
}

/// Compile the route table for a set of route items, with the response
/// modes, auth types and UI extensions this daemon provides.
///
/// A route signed `public: true` is served only when `public_routes` names
/// its id; one it does not name is left out of the table, so its path is
/// answered as any unknown path is, and is returned in `skipped`. A public
/// route must be keyless and static; one that is not refuses the table.
pub fn compose_route_table_from(
    routes: &[RawRouteSpec],
    public_routes: &PublicRoutesPolicy,
    ui: Arc<ryeos_ui::UiState>,
    kinds: &KindRegistry,
) -> Result<ComposedRoutes> {
    let mut served: Vec<RawRouteSpec> = Vec::with_capacity(routes.len());
    let mut skipped = Vec::new();
    for raw in routes {
        if raw.public {
            if raw.auth != "none" || raw.response.mode != "static" {
                anyhow::bail!(
                    "route `{}` is signed public but is not a keyless static route (auth {}, mode {})",
                    raw.id,
                    raw.auth,
                    raw.response.mode
                );
            }
            if public_routes.limits_for(&raw.id).is_none() {
                skipped.push(SkippedRoute {
                    id: raw.id.clone(),
                    path: raw.path.clone(),
                    reason: PUBLIC_ROUTE_NOT_NAMED,
                });
                continue;
            }
        }
        served.push(raw.clone());
    }
    let table = build_table(&served, ui, kinds)?;
    Ok(ComposedRoutes { table, skipped })
}

/// Stable code for a public route the node's policy does not name.
pub const PUBLIC_ROUTE_NOT_NAMED: &str = "public_route_not_named";

fn build_table(
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
        let policy = PublicRoutesPolicy::as_shipped();
        let at_boot =
            compose_route_table_from(&routes, &policy, Arc::new(ryeos_ui::UiState::new()), &kinds)
                .expect("the shipped routes compose")
                .table;
        let prospective =
            compose_route_table_from(&routes, &policy, Arc::new(ryeos_ui::UiState::new()), &kinds)
                .expect("the shipped routes compose")
                .table;
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

    fn public_route(id: &str, path: &str) -> RawRouteSpec {
        let mut raw: RawRouteSpec = serde_yaml::from_str(&format!(
            "id: {id}\npath: {path}\nmethods: [GET]\nauth: none\nlimits:\n  body_bytes_max: 0\nresponse:\n  mode: static\n  status: 200\n  content_type: text/plain\n  body_b64: aGVsbG8=\npublic: true\n"
        ))
        .expect("a public route parses");
        raw.source_file = std::path::PathBuf::from(format!("/test/{id}.yaml"));
        raw
    }

    fn naming(ids: &[&str]) -> PublicRoutesPolicy {
        use ryeos_node_runtime::node_policy::sections::public_routes::PublicRouteLimits;
        let mut policy = PublicRoutesPolicy::as_shipped();
        for id in ids {
            policy.routes.insert(
                (*id).to_owned(),
                PublicRouteLimits {
                    daily_bytes: 1 << 20,
                    concurrent_max: 4,
                },
            );
        }
        policy
    }

    fn compose(routes: &[RawRouteSpec], policy: &PublicRoutesPolicy) -> ComposedRoutes {
        let kinds = ryeos_engine::test_support::load_live_kind_registry();
        compose_route_table_from(routes, policy, Arc::new(ryeos_ui::UiState::new()), &kinds)
            .expect("compose")
    }

    #[test]
    fn a_public_route_is_served_only_when_the_policy_names_it() {
        let mut routes = shipped_routes();
        let shipped = routes.len();
        routes.push(public_route("public/front", "/"));
        routes.push(public_route("public/assets/style", "/assets/style.css"));

        // Named: both served, nothing skipped, every other route still served.
        let named = compose(&routes, &naming(&["public/front", "public/assets/style"]));
        assert!(named.skipped.is_empty());
        assert_eq!(named.table.all.len(), shipped + 2);

        // Not named: both left out, reported, and the rest of the table compiles.
        let none = compose(&routes, &PublicRoutesPolicy::as_shipped());
        assert_eq!(none.table.all.len(), shipped);
        let mut skipped: Vec<&str> = none.skipped.iter().map(|s| s.id.as_str()).collect();
        skipped.sort();
        assert_eq!(skipped, ["public/assets/style", "public/front"]);
        assert!(
            none.skipped
                .iter()
                .all(|s| s.reason == PUBLIC_ROUTE_NOT_NAMED)
        );

        // Partly named: the named one served, the other skipped.
        let partly = compose(&routes, &naming(&["public/front"]));
        assert_eq!(partly.table.all.len(), shipped + 1);
        assert_eq!(partly.skipped.len(), 1);
        assert_eq!(partly.skipped[0].id, "public/assets/style");
        assert_eq!(partly.skipped[0].path, "/assets/style.css");
    }

    #[test]
    fn a_public_route_that_is_not_keyless_static_refuses_the_table() {
        let kinds = ryeos_engine::test_support::load_live_kind_registry();
        let mut keyed = public_route("public/front", "/");
        keyed.auth = "browser_session".into();
        let error = compose_route_table_from(
            &[keyed],
            &naming(&["public/front"]),
            Arc::new(ryeos_ui::UiState::new()),
            &kinds,
        )
        .err()
        .expect("a keyed public route is refused");
        assert!(error.to_string().contains("signed public"), "{error:#}");
    }

    /// Only the public-page bundle signs a route public. Every other shipped
    /// bundle's routes stay out of the node's `public_routes` limits.
    #[test]
    fn only_the_public_page_bundle_signs_a_route_public() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(read) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in read {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "yaml") {
                    out.push(path);
                }
            }
        }
        let bundles = ryeos_engine::test_support::workspace_root().join("bundles");
        let mut checked = 0usize;
        for bundle in std::fs::read_dir(&bundles).unwrap() {
            let bundle = bundle.unwrap().path();
            let name = bundle.file_name().unwrap().to_string_lossy().into_owned();
            let mut files = Vec::new();
            walk(&bundle.join(".ai/node/routes"), &mut files);
            for path in files {
                let raw: RawRouteSpec =
                    serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap())
                        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
                assert!(
                    !raw.public || name == "ryeos-public",
                    "{} is signed public, but only the ryeos-public bundle may sign a route public",
                    path.display()
                );
                checked += 1;
            }
        }
        assert!(checked > 0, "no shipped route was read");
    }
}
