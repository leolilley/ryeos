//! Authority limits for a bundle in the publication lane.
//!
//! A bundle registered with a lane marker arrived through a publication pull:
//! its publisher signed the bytes and the node pinned that key, but nothing
//! built, qualified or attested it. Such a bundle may carry items of exactly
//! four kinds (tool, directive, graph, knowledge) and nothing else: no other
//! item kind, no kinds of its own, no manifest-declared runtime authority, no
//! native executables, no node extensions, no file whose bytes are an
//! executable image and no file outside an item directory. The path rules run
//! on the publication's file list before any blob is fetched; the whole check
//! runs on the staged tree at admission and again whenever the node loads its
//! registrations, so a tree edited after install is refused too.
//!
//! Commands need no check here. The node-config loader already refuses a
//! command item from any bundle that the node's `command_registration` policy
//! does not name (`UnnamedBundleCommand`), and a laned bundle is never named.

use std::fs;
use std::path::Path;

use crate::manifest::BundleManifest;

/// Directories, relative to the bundle root, whose files extend the node
/// itself: handlers, parsers, runtimes and protocols. One entry per refused
/// directory, so ruling a new node-extension kind in is a one-line change.
const NODE_EXTENSION_DIRS: &[&str] = &[
    ".ai/handlers",
    ".ai/parsers",
    ".ai/runtimes",
    ".ai/protocols",
];

/// Directories, relative to the bundle root, that hold native executables.
const NATIVE_EXECUTABLE_DIRS: &[&str] = &["bin", ".ai/bin"];

/// The only item directories a lane bundle may carry, with the kind each one
/// holds. A tool's own source files live under the tools directory.
const ITEM_KIND_DIRS: &[(&str, &str)] = &[
    (".ai/tools", "tool"),
    (".ai/directives", "directive"),
    (".ai/graphs", "graph"),
    (".ai/knowledge", "knowledge"),
];

/// Directories that name a refused kind. A directory under `.ai` that is in
/// neither table is refused as a kind of its own name.
const REFUSED_KIND_DIRS: &[(&str, &str)] = &[
    (".ai/services", "service"),
    (".ai/config", "config"),
    (".ai/node", "node"),
    (".ai/routes", "route"),
    (".ai/ui", "ui"),
];

/// Bundle control data that is neither an item nor executable: the signed
/// manifest, the manifest's content-addressed payload and the one ref that
/// names it, and the publisher's trust file. Each is a path or a directory
/// prefix. Any other ref (a pictures ref, say) is refused as a stray file.
/// Only the binary resolver, the executor's manifest probe and the pictures
/// reader read `.ai/objects`; items resolve from their kind directories, so
/// a refused kind cannot be smuggled in by hash, and executable bytes in the
/// payload are caught by the magic check.
const CONTROL_PATHS: &[&str] = &[
    ".ai/manifest.yaml",
    ".ai/manifest.source.yaml",
    ".ai/objects",
    ".ai/refs/bundles/manifest",
    "PUBLISHER_TRUST.toml",
];

/// Leading bytes of an executable image: ELF, the four Mach-O thin magics
/// in both byte orders, and the fat/universal magic.
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

/// The closing sentence of every refusal.
const LANE_LIMIT_NOT_LIFTABLE: &str =
    "This limit cannot be lifted on this build; there is no flag or policy that lifts it yet.";

/// Why a publication-lane bundle was refused. The message starts with the
/// stable code.
#[derive(Debug, thiserror::Error)]
pub enum LaneRefusal {
    #[error(
        "bundle_lane_kinds: the bundle manifest provides kinds {kinds:?}; a publication-lane bundle may not provide kinds. {}",
        LANE_LIMIT_NOT_LIFTABLE
    )]
    Kinds { kinds: Vec<String> },
    #[error(
        "bundle_lane_runtime_authority: the bundle manifest declares runtime authority ({families}); a publication-lane bundle may not declare any. {}",
        LANE_LIMIT_NOT_LIFTABLE
    )]
    RuntimeAuthority { families: String },
    #[error(
        "bundle_lane_native_executable: the bundle ships a native executable directory at {path}; a publication-lane bundle may not ship executables. {}",
        LANE_LIMIT_NOT_LIFTABLE
    )]
    NativeExecutable { path: String },
    #[error(
        "bundle_lane_node_extension: the bundle ships a node extension ({found}); a publication-lane bundle may not extend the node. {}",
        LANE_LIMIT_NOT_LIFTABLE
    )]
    NodeExtension { found: String },
    #[error(
        "publication_lane_kind_refused: {path} is an item of kind `{kind}`; a publication-lane bundle may carry only tool, directive, graph and knowledge items. {}",
        LANE_LIMIT_NOT_LIFTABLE
    )]
    KindRefused { kind: String, path: String },
    #[error(
        "publication_lane_stray_file: {path} is outside the item directories and is not bundle control data; a publication-lane bundle may carry nothing else. {}",
        LANE_LIMIT_NOT_LIFTABLE
    )]
    StrayFile { path: String },
    #[error(
        "publication_lane_executable_image: {path} begins with the bytes of an executable image; a publication-lane bundle may not carry one. {}",
        LANE_LIMIT_NOT_LIFTABLE
    )]
    ExecutableImage { path: String },
    #[error(
        "bundle_lane_unreadable: the bundle tree at {path} could not be inspected ({detail}); a publication-lane bundle is refused when it cannot be checked. {}",
        LANE_LIMIT_NOT_LIFTABLE
    )]
    Unreadable { path: String, detail: String },
}

impl LaneRefusal {
    /// The stable code the message starts with.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Kinds { .. } => "bundle_lane_kinds",
            Self::RuntimeAuthority { .. } => "bundle_lane_runtime_authority",
            Self::NativeExecutable { .. } => "bundle_lane_native_executable",
            Self::NodeExtension { .. } => "bundle_lane_node_extension",
            Self::KindRefused { .. } => "publication_lane_kind_refused",
            Self::StrayFile { .. } => "publication_lane_stray_file",
            Self::ExecutableImage { .. } => "publication_lane_executable_image",
            Self::Unreadable { .. } => "bundle_lane_unreadable",
        }
    }
}

/// Refuse a publication-lane bundle that carries kinds, runtime authority,
/// native executables or node extensions.
pub fn refuse_lane_authority(
    bundle_root: &Path,
    manifest: &BundleManifest,
) -> Result<(), LaneRefusal> {
    if !manifest.provides_kinds.is_empty() {
        return Err(LaneRefusal::Kinds {
            kinds: manifest.provides_kinds.clone(),
        });
    }

    let families = runtime_authority_families(manifest);
    if !families.is_empty() {
        return Err(LaneRefusal::RuntimeAuthority {
            families: families.join(", "),
        });
    }

    let found = node_extension_manifest_fields(manifest);
    if !found.is_empty() {
        return Err(LaneRefusal::NodeExtension {
            found: found.join(", "),
        });
    }

    for rel in tree_files(bundle_root)? {
        refuse_lane_path(&rel)?;
        refuse_executable_image(bundle_root, &rel)?;
    }
    Ok(())
}

/// Check an installed laned bundle's whole tree and manifest again, as the
/// node does every time it loads its registrations. A bundle that fails is
/// not loaded; the refusal says why.
pub fn check_installed_lane_bundle(bundle_root: &Path, name: &str) -> Result<(), LaneRefusal> {
    let manifest = crate::manifest::parse_manifest(bundle_root, name).map_err(|error| {
        LaneRefusal::Unreadable {
            path: bundle_root.display().to_string(),
            detail: format!("{error:#}"),
        }
    })?;
    refuse_lane_authority(bundle_root, &manifest)
}

/// Refuse a publication's file list before any blob is fetched: the path
/// rules of the lane, applied to bundle-relative `/`-separated paths.
pub fn refuse_lane_paths<'a>(paths: impl IntoIterator<Item = &'a str>) -> Result<(), LaneRefusal> {
    paths.into_iter().try_for_each(refuse_lane_path)
}

fn under(path: &str, dir: &str) -> bool {
    path == dir
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The path rules, in order: native executable directories, node-extension
/// directories, then the allow-list of item directories and control data.
fn refuse_lane_path(path: &str) -> Result<(), LaneRefusal> {
    if let Some(dir) = NATIVE_EXECUTABLE_DIRS.iter().find(|dir| under(path, dir)) {
        return Err(LaneRefusal::NativeExecutable {
            path: (*dir).to_string(),
        });
    }
    if NODE_EXTENSION_DIRS.iter().any(|dir| under(path, dir)) {
        return Err(LaneRefusal::NodeExtension {
            found: format!("file {path}"),
        });
    }
    if ITEM_KIND_DIRS.iter().any(|(dir, _)| under(path, dir))
        || CONTROL_PATHS.iter().any(|control| under(path, control))
    {
        return Ok(());
    }
    if let Some(kind_dir) = path
        .strip_prefix(".ai/")
        .and_then(|rest| rest.split('/').next())
    {
        let dir = format!(".ai/{kind_dir}");
        let kind = REFUSED_KIND_DIRS
            .iter()
            .find(|(known, _)| *known == dir)
            .map_or(kind_dir, |(_, kind)| *kind);
        return Err(LaneRefusal::KindRefused {
            kind: kind.to_string(),
            path: path.to_string(),
        });
    }
    Err(LaneRefusal::StrayFile {
        path: path.to_string(),
    })
}

/// Every file below `root` as a bundle-relative `/`-separated path. A symlink
/// is refused: a lane bundle holds only the bytes it names.
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
            let relative = display_relative(root, &path).replace('\\', "/");
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

/// Names of the runtime-authority families the manifest declares.
fn runtime_authority_families(manifest: &BundleManifest) -> Vec<&'static str> {
    let authority = &manifest.runtime_authority;
    let mut families = Vec::new();
    if !authority.bundle_events.is_empty() {
        families.push("bundle_events");
    }
    if !authority.runtime_vault.is_empty() {
        families.push("runtime_vault");
    }
    if !authority.item_authoring.is_empty() {
        families.push("item_authoring");
    }
    if !authority.project_snapshots.is_empty() {
        families.push("project_snapshots");
    }
    if authority.state_home.is_some() {
        families.push("state_home");
    }
    if authority.operator_vault {
        families.push("operator_vault");
    }
    if !authority.route_capabilities.is_empty() {
        families.push("route_capabilities");
    }
    families
}

/// Manifest declarations that extend the node: isolation backends, external
/// providers and external lifecycle adapters.
fn node_extension_manifest_fields(manifest: &BundleManifest) -> Vec<String> {
    let mut found = Vec::new();
    if !manifest.isolation_backends.is_empty() {
        found.push("manifest isolation_backends".to_string());
    }
    if !manifest.external_providers.is_empty() {
        found.push("manifest external_providers".to_string());
    }
    if !manifest.external_lifecycle_adapters.is_empty() {
        found.push("manifest external_lifecycle_adapters".to_string());
    }
    found
}

fn display_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
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
            uses_kinds: Vec::new(),
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

    fn write(root: &Path, rel: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }

    fn refusal(root: &Path, manifest: &BundleManifest) -> LaneRefusal {
        refuse_lane_authority(root, manifest).unwrap_err()
    }

    fn assert_message(error: &LaneRefusal, code: &str) {
        assert_eq!(error.code(), code);
        let message = error.to_string();
        assert!(message.starts_with(&format!("{code}:")), "{message}");
        assert!(message.ends_with(LANE_LIMIT_NOT_LIFTABLE), "{message}");
    }

    fn write_bytes(root: &Path, rel: &str, bytes: &[u8]) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn a_bundle_of_only_the_four_kinds_and_control_data_passes() {
        let tmp = tempfile::tempdir().unwrap();
        for rel in [
            ".ai/tools/ryeos/pulled/tool.yaml",
            ".ai/tools/ryeos/pulled/src/helper.py",
            ".ai/directives/pulled/plan.md",
            ".ai/graphs/pulled/flow.yaml",
            ".ai/knowledge/pulled/note.md",
            ".ai/manifest.yaml",
            ".ai/manifest.source.yaml",
            ".ai/objects/objects/44/a5/blob.json",
            ".ai/refs/bundles/manifest",
            "PUBLISHER_TRUST.toml",
        ] {
            write(tmp.path(), rel);
        }
        for rel in [
            ".ai/directives/pulled/plan.md",
            ".ai/graphs/pulled/flow.yaml",
        ] {
            write(tmp.path(), rel);
        }
        fs::create_dir_all(tmp.path().join("bin")).unwrap();
        refuse_lane_authority(tmp.path(), &manifest()).unwrap();
    }

    #[test]
    fn every_other_kind_directory_is_refused_naming_kind_and_path() {
        for (rel, kind) in [
            (".ai/services/pulled/s.yaml", "service"),
            (".ai/config/pulled/c.yaml", "config"),
            (".ai/node/engine/kinds/k.yaml", "node"),
            (".ai/routes/pulled/r.yaml", "route"),
            (".ai/ui/pulled/v.yaml", "ui"),
            (".ai/schedules/pulled/s.yaml", "schedules"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            write(tmp.path(), rel);
            let error = refusal(tmp.path(), &manifest());
            assert_message(&error, "publication_lane_kind_refused");
            let message = error.to_string();
            assert!(message.contains(&format!("`{kind}`")), "{message}");
            assert!(message.contains(rel), "{message}");
        }
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
            assert_message(
                &refusal(tmp.path(), &manifest()),
                "publication_lane_stray_file",
            );
        }
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
            assert_message(
                &refusal(tmp.path(), &manifest()),
                "publication_lane_executable_image",
            );
        }
        let tmp = tempfile::tempdir().unwrap();
        write_bytes(
            tmp.path(),
            ".ai/knowledge/pulled/mz.md",
            b"MZ is a prefix of prose",
        );
        refuse_lane_authority(tmp.path(), &manifest()).unwrap();
    }

    #[test]
    fn the_publication_file_list_is_refused_before_any_fetch() {
        refuse_lane_paths([".ai/knowledge/a.md", ".ai/manifest.yaml"]).unwrap();
        for (path, code) in [
            ("bin/tool", "bundle_lane_native_executable"),
            (".ai/handlers/h.yaml", "bundle_lane_node_extension"),
            (".ai/services/s.yaml", "publication_lane_kind_refused"),
            ("README.md", "publication_lane_stray_file"),
        ] {
            assert_eq!(
                refuse_lane_paths([".ai/knowledge/a.md", path])
                    .unwrap_err()
                    .code(),
                code,
                "{path}"
            );
        }
    }

    #[test]
    fn a_pictures_ref_and_an_unknown_ai_directory_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".ai/refs/bundles/pictures");
        let error = refusal(tmp.path(), &manifest());
        assert_message(&error, "publication_lane_kind_refused");
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".ai/whatever/x.yaml");
        let error = refusal(tmp.path(), &manifest());
        assert_message(&error, "publication_lane_kind_refused");
        assert!(error.to_string().contains("`whatever`"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".ai/knowledge/a.md");
        let fifo = tmp.path().join(".ai/knowledge/pipe.md");
        let status = std::process::Command::new("mkfifo").arg(&fifo).status();
        if matches!(status, Ok(status) if status.success()) {
            assert_message(
                &refusal(tmp.path(), &manifest()),
                "publication_lane_stray_file",
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".ai/knowledge/a.md");
        std::os::unix::fs::symlink("/etc/passwd", tmp.path().join(".ai/knowledge/link.md"))
            .unwrap();
        assert_message(
            &refusal(tmp.path(), &manifest()),
            "publication_lane_stray_file",
        );
    }

    #[test]
    fn provided_kinds_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = manifest();
        manifest.provides_kinds = vec!["widget".to_string()];
        assert_message(&refusal(tmp.path(), &manifest), "bundle_lane_kinds");
    }

    #[test]
    fn runtime_authority_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = manifest();
        manifest.runtime_authority.bundle_events = vec![BundleEventDecl {
            event_kind: "pulled.event".to_string(),
            operations: vec![BundleEventOperation::Append],
        }];
        let error = refusal(tmp.path(), &manifest);
        assert_message(&error, "bundle_lane_runtime_authority");
        assert!(error.to_string().contains("bundle_events"));

        let mut second = manifest();
        second.runtime_authority.operator_vault = true;
        second.runtime_authority.route_capabilities = vec!["ryeos.use.x".to_string()];
        let error = refusal(tmp.path(), &second);
        assert_message(&error, "bundle_lane_runtime_authority");
        assert!(
            error
                .to_string()
                .contains("operator_vault, route_capabilities")
        );
    }

    #[test]
    fn native_executable_directories_are_refused() {
        for rel in ["bin/tool", ".ai/bin/tool"] {
            let tmp = tempfile::tempdir().unwrap();
            write(tmp.path(), rel);
            assert_message(
                &refusal(tmp.path(), &manifest()),
                "bundle_lane_native_executable",
            );
        }
    }

    #[test]
    fn node_extension_directories_are_refused() {
        for dir in NODE_EXTENSION_DIRS {
            let tmp = tempfile::tempdir().unwrap();
            write(tmp.path(), &format!("{dir}/nested/item.yaml"));
            let error = refusal(tmp.path(), &manifest());
            assert_message(&error, "bundle_lane_node_extension");
            assert!(error.to_string().contains(dir), "{error}");
        }
    }

    #[test]
    fn unreadable_tree_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        assert_message(
            &refusal(&tmp.path().join("missing"), &manifest()),
            "bundle_lane_unreadable",
        );
    }
}
