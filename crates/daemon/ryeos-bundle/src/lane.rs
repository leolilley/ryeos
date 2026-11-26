//! What a bundle in the publication lane may carry.
//!
//! A bundle registered with a lane marker arrived through a publication pull:
//! its publisher signed the bytes and the node pinned that key, but nothing
//! built, qualified or attested it. The check compares three facts and names
//! no kind:
//!
//! - what each kind says about itself: its signed schema's `activation`
//!   (`invoked` items act only when a caller invokes them under that caller's
//!   own grants; `node_loaded` items are loaded by the node into itself);
//! - what the bundle says it carries: its signed manifest (`uses_kinds`,
//!   `requires_kinds`, and the facets that carry node-level authority);
//! - what this node admits: [`LaneGrants`], built from the node's signed
//!   `publication_lane` policy for this bundle.
//!
//! The path rules run on a publication's file list before any blob is
//! fetched; the whole check runs on the staged tree at admission. A byte-level
//! guard refuses any file that begins like an executable image.
//!
//! Some things are never admitted, whatever the policy says, because the lane
//! cannot ship a program or reach node-owned state: `provides_kinds`,
//! `operator_vault`, `state_home`, `route_capabilities`, `isolation_backends`,
//! `external_providers`, `external_lifecycle_adapters`, executors and any
//! native payload.

use std::fs;
use std::path::Path;

use ryeos_engine::kind_registry::Activation;

use crate::manifest::BundleManifest;

/// The closing sentence of every refusal that no policy entry can lift.
const NOT_LIFTABLE: &str = "No policy entry lifts this.";
/// The closing sentence of every refusal a policy entry can lift.
const LIFTABLE: &str =
    "Only this node's signed publication_lane policy naming this bundle could admit it.";

/// The bundle structure that is neither an item nor a kind directory: the
/// signed manifest and its source, the content-addressed payload, the one ref
/// that names the executor manifest, and the publisher's trust file.
const CONTROL_PATHS: &[&str] = &[
    ".ai/manifest.yaml",
    ".ai/manifest.source.yaml",
    ".ai/objects",
    ".ai/refs/bundles/manifest",
    "PUBLISHER_TRUST.toml",
];

/// Native executable directories: never admitted.
const NATIVE_EXECUTABLE_DIRS: &[&str] = &["bin", ".ai/bin"];

/// The signed ref that names the bundle's executor manifest.
const EXECUTOR_MANIFEST_REF: &str = ".ai/refs/bundles/manifest";

/// Leading bytes of an executable image: ELF, the four Mach-O thin magics
/// in both byte orders, and the fat/universal magic. A PE image is
/// recognised separately.
const EXECUTABLE_MAGICS: &[&[u8]] = &[
    b"\x7fELF",
    &[0xfe, 0xed, 0xfa, 0xce],
    &[0xfe, 0xed, 0xfa, 0xcf],
    &[0xce, 0xfa, 0xed, 0xfe],
    &[0xcf, 0xfa, 0xed, 0xfe],
    &[0xca, 0xfe, 0xba, 0xbe],
];

/// How much of a file is read to recognise an executable image.
const MAGIC_PROBE_BYTES: usize = 4096;

/// A family of runtime authority, or another manifest declaration, that a
/// policy entry can lift for a named bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Facet {
    BundleEvents,
    RuntimeVault,
    ItemAuthoring,
    ProjectSnapshots,
    OwnsNamespaces,
}

impl Facet {
    /// The name a policy entry uses.
    pub fn name(self) -> &'static str {
        match self {
            Self::BundleEvents => "bundle_events",
            Self::RuntimeVault => "runtime_vault",
            Self::ItemAuthoring => "item_authoring",
            Self::ProjectSnapshots => "project_snapshots",
            Self::OwnsNamespaces => "owns_namespaces",
        }
    }

    /// The facet a policy entry names, if it names a liftable one.
    pub fn parse(name: &str) -> Option<Self> {
        [
            Self::BundleEvents,
            Self::RuntimeVault,
            Self::ItemAuthoring,
            Self::ProjectSnapshots,
            Self::OwnsNamespaces,
        ]
        .into_iter()
        .find(|facet| facet.name() == name)
    }
}

/// Manifest declarations no policy can lift. This is the floor: a policy
/// listing one of these as liftable is refused, so a policy mistake cannot
/// lower it.
pub const NEVER_LIFTABLE: &[&str] = &[
    "provides_kinds",
    "operator_vault",
    "state_home",
    "route_capabilities",
    "isolation_backends",
    "external_providers",
    "external_lifecycle_adapters",
    "executors",
];

/// What this node admits for one lane bundle, taken from its signed policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneGrants {
    /// Whether items of `invoked` kinds are admitted.
    pub invoked: bool,
    /// The exact `node_loaded` kinds this bundle may carry.
    pub kinds: Vec<String>,
    /// The manifest facets this bundle may declare.
    pub facets: Vec<Facet>,
}

impl LaneGrants {
    /// The default for a bundle no policy entry names: invoked items only.
    pub fn invoked_only() -> Self {
        Self {
            invoked: true,
            kinds: Vec::new(),
            facets: Vec::new(),
        }
    }
}

/// Finds the kind that owns an `.ai` subdirectory, with its activation.
pub type KindLookup<'a> = &'a dyn Fn(&str) -> Option<(String, Activation)>;

/// Everything the check compares.
pub struct LaneContext<'a> {
    pub kind_in_directory: KindLookup<'a>,
    pub manifest: &'a BundleManifest,
    pub grants: &'a LaneGrants,
}

/// Why a lane bundle was refused. The message starts with the stable code.
#[derive(Debug, thiserror::Error)]
pub enum LaneRefusal {
    #[error(
        "bundle_lane_kinds: the bundle manifest provides kinds {kinds:?}; a publication-lane bundle may not provide kinds. {}",
        NOT_LIFTABLE
    )]
    ProvidesKinds { kinds: Vec<String> },
    #[error(
        "publication_lane_never_admitted: {what}; a publication-lane bundle cannot carry it. {}",
        NOT_LIFTABLE
    )]
    NeverAdmitted { what: String },
    #[error(
        "publication_lane_facet_not_admitted: the bundle declares {facet}; this node does not admit it for this bundle. {}",
        LIFTABLE
    )]
    FacetNotAdmitted { facet: &'static str },
    #[error(
        "publication_lane_kind_refused: {path} is an item of kind `{kind}`; {reason}. {}",
        LIFTABLE
    )]
    KindRefused {
        kind: String,
        path: String,
        reason: String,
    },
    #[error(
        "publication_lane_kind_undeclared: {path} is an item of kind `{kind}`, which the bundle manifest does not declare in uses_kinds or requires_kinds"
    )]
    KindUndeclared { kind: String, path: String },
    #[error(
        "publication_lane_stray_file: {path} is outside the item directories and is not bundle control data; a publication-lane bundle may carry nothing else. {}",
        NOT_LIFTABLE
    )]
    StrayFile { path: String },
    #[error(
        "publication_lane_executable_image: {path} begins with the bytes of an executable image; a publication-lane bundle may not carry one. {}",
        NOT_LIFTABLE
    )]
    ExecutableImage { path: String },
    #[error(
        "bundle_lane_unreadable: the bundle tree at {path} could not be inspected ({detail}); a publication-lane bundle is refused when it cannot be checked. {}",
        NOT_LIFTABLE
    )]
    Unreadable { path: String, detail: String },
}

impl LaneRefusal {
    /// The stable code the message starts with.
    pub fn code(&self) -> &'static str {
        match self {
            Self::ProvidesKinds { .. } => "bundle_lane_kinds",
            Self::NeverAdmitted { .. } => "publication_lane_never_admitted",
            Self::FacetNotAdmitted { .. } => "publication_lane_facet_not_admitted",
            Self::KindRefused { .. } => "publication_lane_kind_refused",
            Self::KindUndeclared { .. } => "publication_lane_kind_undeclared",
            Self::StrayFile { .. } => "publication_lane_stray_file",
            Self::ExecutableImage { .. } => "publication_lane_executable_image",
            Self::Unreadable { .. } => "bundle_lane_unreadable",
        }
    }
}

/// Refuse the manifest's own declarations: provided kinds, anything the lane
/// can never carry, and any facet the grants do not name.
pub fn refuse_lane_manifest(
    manifest: &BundleManifest,
    grants: &LaneGrants,
) -> Result<(), LaneRefusal> {
    if !manifest.provides_kinds.is_empty() {
        return Err(LaneRefusal::ProvidesKinds {
            kinds: manifest.provides_kinds.clone(),
        });
    }
    let authority = &manifest.runtime_authority;
    let never = [
        (authority.operator_vault, "operator_vault"),
        (authority.state_home.is_some(), "state_home"),
        (
            !authority.route_capabilities.is_empty(),
            "route_capabilities",
        ),
        (
            !manifest.isolation_backends.is_empty(),
            "isolation_backends",
        ),
        (
            !manifest.external_providers.is_empty(),
            "external_providers",
        ),
        (
            !manifest.external_lifecycle_adapters.is_empty(),
            "external_lifecycle_adapters",
        ),
    ];
    if let Some((_, name)) = never.iter().find(|(declared, _)| *declared) {
        return Err(LaneRefusal::NeverAdmitted {
            what: format!("the bundle manifest declares {name}"),
        });
    }
    let facets = [
        (!authority.bundle_events.is_empty(), Facet::BundleEvents),
        (!authority.runtime_vault.is_empty(), Facet::RuntimeVault),
        (!authority.item_authoring.is_empty(), Facet::ItemAuthoring),
        (
            !authority.project_snapshots.is_empty(),
            Facet::ProjectSnapshots,
        ),
        (!manifest.owns_namespaces.is_empty(), Facet::OwnsNamespaces),
    ];
    for (declared, facet) in facets {
        if declared && !grants.facets.contains(&facet) {
            return Err(LaneRefusal::FacetNotAdmitted {
                facet: facet.name(),
            });
        }
    }
    Ok(())
}

fn under(path: &str, dir: &str) -> bool {
    path == dir
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Check an installed laned bundle again, as the node does every time it
/// loads its registrations: its manifest as parsed from the installed tree,
/// its whole tree, and what `grants` admit.
pub fn audit_installed_lane_bundle(
    root: &Path,
    name: &str,
    kinds: &ryeos_engine::kind_registry::KindRegistry,
    grants: &LaneGrants,
) -> Result<(), LaneRefusal> {
    let manifest =
        crate::manifest::parse_manifest(root, name).map_err(|error| LaneRefusal::Unreadable {
            path: root.display().to_string(),
            detail: format!("{error:#}"),
        })?;
    let lookup = |directory: &str| {
        kinds
            .kind_in_directory(directory)
            .map(|(kind, activation)| (kind.to_string(), activation))
    };
    refuse_lane_tree(
        root,
        &LaneContext {
            kind_in_directory: &lookup,
            manifest: &manifest,
            grants,
        },
    )
}

/// Refuse a file list that has an item directory the manifest does not
/// declare. The publisher's freeze runs this, so a bundle that no lane would
/// accept is caught where it is published. A directory no known kind owns is
/// not this check's concern.
pub fn refuse_undeclared_kinds<'a>(
    paths: impl IntoIterator<Item = &'a str>,
    kind_in_directory: KindLookup<'_>,
    manifest: &BundleManifest,
) -> Result<(), LaneRefusal> {
    for path in paths {
        if CONTROL_PATHS.iter().any(|control| under(path, control)) {
            continue;
        }
        let Some(directory) = path
            .strip_prefix(".ai/")
            .and_then(|rest| rest.split('/').next())
            .filter(|directory| !directory.is_empty())
        else {
            continue;
        };
        let Some((kind, _)) = kind_in_directory(directory) else {
            continue;
        };
        let declared = manifest
            .uses_kinds
            .iter()
            .chain(manifest.requires_kinds.iter())
            .chain(manifest.provides_kinds.iter())
            .any(|declared| *declared == kind);
        if !declared {
            return Err(LaneRefusal::KindUndeclared {
                kind,
                path: path.to_string(),
            });
        }
    }
    Ok(())
}

/// One bundle-relative `/`-separated path against the lane rules, in order:
/// native executable directories (never), bundle control data, then the item
/// directories through the kind lookup.
pub fn refuse_lane_path(path: &str, ctx: &LaneContext<'_>) -> Result<(), LaneRefusal> {
    if NATIVE_EXECUTABLE_DIRS.iter().any(|dir| under(path, dir)) {
        return Err(LaneRefusal::NeverAdmitted {
            what: format!("{path} is in a native executable directory"),
        });
    }
    if CONTROL_PATHS.iter().any(|control| under(path, control)) {
        return Ok(());
    }
    let Some(directory) = path
        .strip_prefix(".ai/")
        .and_then(|rest| rest.split('/').next())
        .filter(|directory| !directory.is_empty())
    else {
        return Err(LaneRefusal::StrayFile {
            path: path.to_string(),
        });
    };
    let Some((kind, activation)) = (ctx.kind_in_directory)(directory) else {
        return Err(LaneRefusal::KindRefused {
            kind: directory.to_string(),
            path: path.to_string(),
            reason: "no kind this node knows owns that directory".to_string(),
        });
    };
    let declared = ctx
        .manifest
        .uses_kinds
        .iter()
        .chain(ctx.manifest.requires_kinds.iter())
        .chain(ctx.manifest.provides_kinds.iter())
        .any(|declared| *declared == kind);
    if !declared {
        return Err(LaneRefusal::KindUndeclared {
            kind,
            path: path.to_string(),
        });
    }
    let admitted = match activation {
        Activation::Invoked => ctx.grants.invoked,
        Activation::NodeLoaded => ctx.grants.kinds.contains(&kind),
    };
    if admitted {
        return Ok(());
    }
    let reason = match activation {
        Activation::Invoked => "this node admits no invoked items from the lane".to_string(),
        Activation::NodeLoaded => {
            "items of this kind are loaded by the node into itself".to_string()
        }
    };
    Err(LaneRefusal::KindRefused {
        kind,
        path: path.to_string(),
        reason,
    })
}

/// Refuse a publication's file list before any blob is fetched.
pub fn refuse_lane_paths<'a>(
    paths: impl IntoIterator<Item = &'a str>,
    ctx: &LaneContext<'_>,
) -> Result<(), LaneRefusal> {
    paths
        .into_iter()
        .try_for_each(|path| refuse_lane_path(path, ctx))
}

/// The whole check on a staged tree: the manifest, every path, the bytes of
/// every file, and the executor payload.
pub fn refuse_lane_tree(root: &Path, ctx: &LaneContext<'_>) -> Result<(), LaneRefusal> {
    refuse_lane_manifest(ctx.manifest, ctx.grants)?;
    let files = tree_files(root)?;
    for rel in &files {
        refuse_lane_path(rel, ctx)?;
        refuse_executable_image(root, rel)?;
    }
    refuse_executor_payload(root, &files)
}

/// The executor manifest is the only object a lane bundle may carry, and it
/// must declare no item source: executors arrive through the native lane. The
/// binary resolver loads executors and their payloads from `.ai/objects`
/// through the manifest ref, so any other object could be one.
fn refuse_executor_payload(root: &Path, files: &[String]) -> Result<(), LaneRefusal> {
    let objects: Vec<&String> = files
        .iter()
        .filter(|rel| under(rel, ".ai/objects"))
        .collect();
    let has_ref = files.iter().any(|rel| rel == EXECUTOR_MANIFEST_REF);
    if objects.is_empty() && !has_ref {
        return Ok(());
    }
    let stray = |path: &str| LaneRefusal::StrayFile {
        path: path.to_string(),
    };
    if !has_ref {
        return Err(stray(objects[0]));
    }
    let ref_path = root.join(EXECUTOR_MANIFEST_REF);
    let text = fs::read_to_string(&ref_path).map_err(|error| unreadable(&ref_path, &error))?;
    let hash = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .nth(1)
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| stray(EXECUTOR_MANIFEST_REF))?;
    let manifest_rel = format!(
        ".ai/objects/objects/{}/{}/{hash}.json",
        &hash[..2],
        &hash[2..4]
    );
    if !objects.iter().any(|rel| **rel == manifest_rel) {
        return Err(stray(EXECUTOR_MANIFEST_REF));
    }
    if let Some(extra) = objects.iter().find(|rel| ***rel != manifest_rel) {
        return Err(stray(extra));
    }
    let manifest_path = root.join(&manifest_rel);
    let bytes = fs::read(&manifest_path).map_err(|error| unreadable(&manifest_path, &error))?;
    let declared = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|value| value.get("item_source_hashes").cloned())
        .and_then(|sources| sources.as_object().map(|sources| sources.len()));
    match declared {
        Some(0) => Ok(()),
        Some(count) => Err(LaneRefusal::NeverAdmitted {
            what: format!("the executor manifest {manifest_rel} declares {count} item source(s)"),
        }),
        None => Err(LaneRefusal::NeverAdmitted {
            what: format!("the executor manifest {manifest_rel} could not be read as a manifest"),
        }),
    }
}

/// Every file below `root` as a bundle-relative `/`-separated path. A symlink
/// or a special file is refused: a lane bundle holds only the bytes it names.
fn tree_files(root: &Path) -> Result<Vec<String>, LaneRefusal> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(current) = pending.pop() {
        let entries = fs::read_dir(&current).map_err(|error| unreadable(&current, &error))?;
        for entry in entries {
            let entry = entry.map_err(|error| unreadable(&current, &error))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|error| unreadable(&path, &error))?;
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .display()
                .to_string()
                .replace('\\', "/");
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file() {
                files.push(relative);
            } else {
                return Err(LaneRefusal::StrayFile { path: relative });
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Refuse a file that begins like an ELF, Mach-O or PE image.
fn refuse_executable_image(root: &Path, relative: &str) -> Result<(), LaneRefusal> {
    use std::io::Read as _;
    let path = root.join(relative);
    let mut head = Vec::with_capacity(MAGIC_PROBE_BYTES);
    fs::File::open(&path)
        .and_then(|file| file.take(MAGIC_PROBE_BYTES as u64).read_to_end(&mut head))
        .map_err(|error| unreadable(&path, &error))?;
    let portable_executable = head.starts_with(b"MZ")
        && head
            .get(0x3c..0x40)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .map(u32::from_le_bytes)
            .and_then(|offset| usize::try_from(offset).ok())
            .is_some_and(|offset| head.get(offset..offset + 4) == Some(b"PE\0\0".as_slice()));
    if portable_executable
        || EXECUTABLE_MAGICS
            .iter()
            .any(|magic| head.starts_with(magic))
    {
        return Err(LaneRefusal::ExecutableImage {
            path: relative.to_string(),
        });
    }
    Ok(())
}

fn unreadable(path: &Path, error: &std::io::Error) -> LaneRefusal {
    LaneRefusal::Unreadable {
        path: path.display().to_string(),
        detail: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{BundleEventDecl, BundleEventOperation, RuntimeAuthorityDecls};

    fn manifest() -> BundleManifest {
        BundleManifest {
            name: "pulled".to_string(),
            version: "1.0.0".to_string(),
            description: String::new(),
            provides_kinds: Vec::new(),
            requires_kinds: Vec::new(),
            uses_kinds: ["tool", "directive", "graph", "knowledge", "service", "view"]
                .map(String::from)
                .to_vec(),
            runtime_authority: RuntimeAuthorityDecls::default(),
            smoke: Vec::new(),
            shadows: Vec::new(),
            owns_namespaces: Vec::new(),
            sync_include: Vec::new(),
            isolation_backends: Vec::new(),
            external_providers: Vec::new(),
            external_lifecycle_adapters: Vec::new(),
        }
    }

    /// A kind table the way a registry answers it: directory to kind and its
    /// activation. The check itself names none of these.
    fn lookup(directory: &str) -> Option<(String, Activation)> {
        let (kind, activation) = match directory {
            "tools" => ("tool", Activation::Invoked),
            "directives" => ("directive", Activation::Invoked),
            "graphs" => ("graph", Activation::Invoked),
            "knowledge" => ("knowledge", Activation::Invoked),
            "services" => ("service", Activation::NodeLoaded),
            "views" => ("view", Activation::NodeLoaded),
            _ => return None,
        };
        Some((kind.to_string(), activation))
    }

    fn check(
        root: &Path,
        manifest: &BundleManifest,
        grants: &LaneGrants,
    ) -> Result<(), LaneRefusal> {
        refuse_lane_tree(
            root,
            &LaneContext {
                kind_in_directory: &lookup,
                manifest,
                grants,
            },
        )
    }

    fn refusal(root: &Path) -> LaneRefusal {
        check(root, &manifest(), &LaneGrants::invoked_only()).unwrap_err()
    }

    fn write_bytes(root: &Path, rel: &str, bytes: &[u8]) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn write(root: &Path, rel: &str) {
        write_bytes(root, rel, b"x");
    }

    fn assert_code(error: &LaneRefusal, code: &str) {
        assert_eq!(error.code(), code);
        assert!(
            error.to_string().starts_with(&format!("{code}:")),
            "{error}"
        );
    }

    const MANIFEST_HASH: &str = "44a561f2da1b69e9223d9c1baeac7fb0e0567dad1bdeba1d9eb24f9f0768e174";

    fn write_executor_manifest(root: &Path, object: &str) {
        write_bytes(
            root,
            ".ai/refs/bundles/manifest",
            format!("# ryeos:signed:x\nryeos:bundle-executor-manifest\n{MANIFEST_HASH}\n")
                .as_bytes(),
        );
        write_bytes(
            root,
            &format!(".ai/objects/objects/44/a5/{MANIFEST_HASH}.json"),
            object.as_bytes(),
        );
    }

    #[test]
    fn a_bundle_of_only_invoked_items_and_control_data_passes() {
        let tmp = tempfile::tempdir().unwrap();
        for rel in [
            ".ai/tools/ryeos/pulled/tool.yaml",
            ".ai/tools/ryeos/pulled/src/helper.py",
            ".ai/directives/pulled/plan.md",
            ".ai/graphs/pulled/flow.yaml",
            ".ai/knowledge/pulled/note.md",
            ".ai/manifest.yaml",
            ".ai/manifest.source.yaml",
            "PUBLISHER_TRUST.toml",
        ] {
            write(tmp.path(), rel);
        }
        write_executor_manifest(
            tmp.path(),
            "{\"item_source_hashes\":{},\"kind\":\"source_manifest\"}",
        );
        check(tmp.path(), &manifest(), &LaneGrants::invoked_only()).unwrap();
    }

    #[test]
    fn a_node_loaded_kind_is_refused_unless_the_bundle_is_named_for_that_kind() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".ai/services/pulled/s.yaml");
        let error = refusal(tmp.path());
        assert_code(&error, "publication_lane_kind_refused");
        assert!(error.to_string().contains("`service`"), "{error}");
        assert!(error.to_string().contains(".ai/services/pulled/s.yaml"));

        let named = LaneGrants {
            invoked: true,
            kinds: vec!["service".to_string()],
            facets: Vec::new(),
        };
        check(tmp.path(), &manifest(), &named).unwrap();

        // Naming one kind does not admit another node-loaded kind.
        write(tmp.path(), ".ai/views/pulled/v.yaml");
        let error = check(tmp.path(), &manifest(), &named).unwrap_err();
        assert_code(&error, "publication_lane_kind_refused");
        assert!(error.to_string().contains("`view`"), "{error}");
    }

    #[test]
    fn invoked_items_are_refused_when_the_node_admits_none() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".ai/knowledge/pulled/note.md");
        let none = LaneGrants {
            invoked: false,
            kinds: Vec::new(),
            facets: Vec::new(),
        };
        let error = check(tmp.path(), &manifest(), &none).unwrap_err();
        assert_code(&error, "publication_lane_kind_refused");
    }

    #[test]
    fn a_directory_no_kind_owns_and_an_undeclared_kind_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".ai/whatever/x.yaml");
        let error = refusal(tmp.path());
        assert_code(&error, "publication_lane_kind_refused");
        assert!(error.to_string().contains("`whatever`"), "{error}");

        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".ai/knowledge/pulled/note.md");
        let mut undeclared = manifest();
        undeclared.uses_kinds = vec!["tool".to_string()];
        let error = check(tmp.path(), &undeclared, &LaneGrants::invoked_only()).unwrap_err();
        assert_code(&error, "publication_lane_kind_undeclared");
    }

    #[test]
    fn freeze_refuses_an_item_directory_the_manifest_does_not_declare() {
        let mut manifest = manifest();
        manifest.uses_kinds = vec!["knowledge".to_string()];
        refuse_undeclared_kinds(
            [
                ".ai/knowledge/a.md",
                ".ai/manifest.yaml",
                "README.md",
                ".ai/other/x",
            ],
            &lookup,
            &manifest,
        )
        .unwrap();
        let error = refuse_undeclared_kinds([".ai/tools/t.yaml"], &lookup, &manifest).unwrap_err();
        assert_code(&error, "publication_lane_kind_undeclared");
    }

    #[test]
    fn a_file_outside_the_item_directories_is_refused() {
        for rel in [
            "README.md",
            "scripts/install.sh",
            "other/.ai/knowledge/n.md",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            write(tmp.path(), rel);
            assert_code(&refusal(tmp.path()), "publication_lane_stray_file");
        }
    }

    #[test]
    fn other_refs_than_the_manifest_ref_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".ai/refs/bundles/pictures");
        assert_code(&refusal(tmp.path()), "publication_lane_kind_refused");
    }

    #[test]
    fn a_native_executable_directory_is_never_admitted() {
        for rel in ["bin/tool", ".ai/bin/tool"] {
            let tmp = tempfile::tempdir().unwrap();
            write(tmp.path(), rel);
            assert_code(&refusal(tmp.path()), "publication_lane_never_admitted");
        }
    }

    #[test]
    fn provided_kinds_are_refused_even_for_a_named_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let mut provides = manifest();
        provides.provides_kinds = vec!["widget".to_string()];
        let named = LaneGrants {
            invoked: true,
            kinds: vec!["widget".to_string()],
            facets: vec![Facet::BundleEvents, Facet::OwnsNamespaces],
        };
        let error = check(tmp.path(), &provides, &named).unwrap_err();
        assert_code(&error, "bundle_lane_kinds");
    }

    #[test]
    fn runtime_authority_facets_need_a_named_grant_and_three_are_never_admitted() {
        let tmp = tempfile::tempdir().unwrap();
        let mut events = manifest();
        events.runtime_authority.bundle_events = vec![BundleEventDecl {
            event_kind: "pulled.event".to_string(),
            operations: vec![BundleEventOperation::Append],
        }];
        let error = check(tmp.path(), &events, &LaneGrants::invoked_only()).unwrap_err();
        assert_code(&error, "publication_lane_facet_not_admitted");
        assert!(error.to_string().contains("bundle_events"), "{error}");
        let named = LaneGrants {
            invoked: true,
            kinds: Vec::new(),
            facets: vec![Facet::BundleEvents],
        };
        check(tmp.path(), &events, &named).unwrap();

        let mut namespaces = manifest();
        namespaces.owns_namespaces = vec!["pulled/".to_string()];
        let error = check(tmp.path(), &namespaces, &LaneGrants::invoked_only()).unwrap_err();
        assert_code(&error, "publication_lane_facet_not_admitted");

        let everything = LaneGrants {
            invoked: true,
            kinds: Vec::new(),
            facets: vec![
                Facet::BundleEvents,
                Facet::RuntimeVault,
                Facet::ItemAuthoring,
                Facet::ProjectSnapshots,
                Facet::OwnsNamespaces,
            ],
        };
        let mut vault = manifest();
        vault.runtime_authority.operator_vault = true;
        assert_code(
            &check(tmp.path(), &vault, &everything).unwrap_err(),
            "publication_lane_never_admitted",
        );
        let mut routes = manifest();
        routes.runtime_authority.route_capabilities = vec!["ryeos.use.x".to_string()];
        assert_code(
            &check(tmp.path(), &routes, &everything).unwrap_err(),
            "publication_lane_never_admitted",
        );
    }

    #[test]
    fn a_file_with_executable_magic_is_refused_wherever_it_sits() {
        let mut pe = vec![0_u8; 0x80];
        pe[..2].copy_from_slice(b"MZ");
        pe[0x3c..0x40].copy_from_slice(&0x40_u32.to_le_bytes());
        pe[0x40..0x44].copy_from_slice(b"PE\0\0");
        for (name, bytes) in [
            ("elf", b"\x7fELF\x02\x01".to_vec()),
            ("macho", vec![0xcf, 0xfa, 0xed, 0xfe, 0, 0]),
            ("fat", vec![0xca, 0xfe, 0xba, 0xbe, 0, 0]),
            ("pe", pe),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            write_bytes(
                tmp.path(),
                &format!(".ai/knowledge/pulled/{name}.md"),
                &bytes,
            );
            assert_code(&refusal(tmp.path()), "publication_lane_executable_image");
        }
        let tmp = tempfile::tempdir().unwrap();
        write_bytes(
            tmp.path(),
            ".ai/knowledge/pulled/mz.md",
            b"MZ is a prefix of prose",
        );
        check(tmp.path(), &manifest(), &LaneGrants::invoked_only()).unwrap();
    }

    #[test]
    fn the_only_object_is_an_empty_executor_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let object = "{\"item_source_hashes\":{\"bin:x\":\"aaaa\"},\"kind\":\"source_manifest\"}";
        write_executor_manifest(tmp.path(), object);
        assert_code(&refusal(tmp.path()), "publication_lane_never_admitted");

        let tmp = tempfile::tempdir().unwrap();
        write_executor_manifest(tmp.path(), object);
        write_bytes(
            tmp.path(),
            ".ai/objects/blobs/aa/aa/aaaa",
            b"#!/bin/sh\necho pwned\n",
        );
        assert_code(&refusal(tmp.path()), "publication_lane_stray_file");

        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".ai/objects/objects/44/a5/x.json");
        assert_code(&refusal(tmp.path()), "publication_lane_stray_file");

        let tmp = tempfile::tempdir().unwrap();
        write_executor_manifest(tmp.path(), "not json");
        assert_code(&refusal(tmp.path()), "publication_lane_never_admitted");
    }

    #[test]
    fn the_publication_file_list_is_refused_before_any_fetch() {
        let manifest = manifest();
        let grants = LaneGrants::invoked_only();
        let ctx = LaneContext {
            kind_in_directory: &lookup,
            manifest: &manifest,
            grants: &grants,
        };
        refuse_lane_paths([".ai/knowledge/a.md", ".ai/manifest.yaml"], &ctx).unwrap();
        for (path, code) in [
            ("bin/tool", "publication_lane_never_admitted"),
            (".ai/services/s.yaml", "publication_lane_kind_refused"),
            ("README.md", "publication_lane_stray_file"),
        ] {
            assert_eq!(
                refuse_lane_paths([".ai/knowledge/a.md", path], &ctx)
                    .unwrap_err()
                    .code(),
                code,
                "{path}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_and_a_fifo_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".ai/knowledge/a.md");
        std::os::unix::fs::symlink("/etc/passwd", tmp.path().join(".ai/knowledge/link.md"))
            .unwrap();
        assert_code(&refusal(tmp.path()), "publication_lane_stray_file");

        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".ai/knowledge/a.md");
        let fifo = tmp.path().join(".ai/knowledge/pipe.md");
        let status = std::process::Command::new("mkfifo").arg(&fifo).status();
        if matches!(status, Ok(status) if status.success()) {
            assert_code(&refusal(tmp.path()), "publication_lane_stray_file");
        }
    }

    #[test]
    fn an_unreadable_tree_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        assert_code(
            &refusal(&tmp.path().join("missing")),
            "bundle_lane_unreadable",
        );
    }

    #[test]
    fn facet_names_round_trip_and_only_the_liftable_ones_parse() {
        for facet in [
            Facet::BundleEvents,
            Facet::RuntimeVault,
            Facet::ItemAuthoring,
            Facet::ProjectSnapshots,
            Facet::OwnsNamespaces,
        ] {
            assert_eq!(Facet::parse(facet.name()), Some(facet));
        }
        for never in [
            "operator_vault",
            "state_home",
            "route_capabilities",
            "executors",
        ] {
            assert_eq!(Facet::parse(never), None, "{never}");
        }
    }
}
