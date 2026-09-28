//! Prebuilt native evaluator fixture inputs, not compiler or qualification
//! evidence. Public producer execution/capture/import/bind remain mandatory.
//! Host artifacts are explicitly supplied pinned files; guest execution uses
//! only the four captured members, never a discovered host interpreter/library.

use std::{ffi::OsStr, path::Path};

use anyhow::{Context as _, Result, ensure};
use lillux::{PinnedDirectory, PinnedRegularFile};
use ryeos_state::external_content::products::ProductCaptureEvidence;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::common::fast_fixture::{self, FastFixture};
use crate::retained_runtime_producer;

pub const TOOL_REF: &str = "tool:fixtures/native-evaluation/check";
pub const RUNTIME_REF: &str = "tool:fixtures/native-evaluation/runtime";
pub const RUNTIME_ROOT: &str = "/ryeos/realizations/native-evaluator";
const MAX_MEMBER: u64 = 8 * 1024 * 1024;
const MAX_TOTAL: u64 = 16 * 1024 * 1024;

/// Callers pin explicit build output and library files before this helper; it
/// neither searches PATH/ldconfig nor follows library symlinks on their behalf.
pub struct RuntimeInputs<'a> {
    pub evaluator: &'a PinnedRegularFile,
    pub loader: &'a PinnedRegularFile,
    pub libc: &'a PinnedRegularFile,
    pub libgcc: &'a PinnedRegularFile,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationRule {
    pub schema_version: u32,
    pub sample_path: String,
    pub expected_utf8: String,
    #[serde(default)]
    pub integration: Option<IntegrationRule>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationRule {
    pub path: String,
    pub required_utf8_marker: String,
}

fn validate_relative(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && path.len() <= 512
            && !path.contains('\\')
            && !path.chars().any(char::is_control)
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "rule path must be normalized and relative"
    );
    Ok(())
}

impl EvaluationRule {
    fn validate(&self) -> Result<()> {
        ensure!(self.schema_version == 1, "unknown rule version");
        validate_relative(&self.sample_path)?;
        ensure!(
            !self.expected_utf8.is_empty() && self.expected_utf8.len() <= 4096,
            "sample expectation exceeds bound"
        );
        if let Some(integration) = &self.integration {
            validate_relative(&integration.path)?;
            ensure!(
                !integration.required_utf8_marker.is_empty()
                    && integration.required_utf8_marker.len() <= 256,
                "integration marker exceeds bound"
            );
        }
        Ok(())
    }
}

fn recipe() -> Value {
    json!({
        "category":"fixtures", "version":"1.0.0",
        "description":"Retained native evaluator fixture; prebuilt infrastructure, not compiler production",
        "build_products":{"schema":"ryeos.build_products.v1","output_roots":[],
            "products":[{"name":retained_runtime_producer::PRODUCT_NAME,
                "source":{"kind":"retained_project"},"path":"products/external-runtime",
                "shape":"tree","storage":"content","required":true,
                "bounds":{"maximum_entries":8,"maximum_depth":3,
                    "maximum_file_bytes":MAX_MEMBER,"maximum_total_bytes":MAX_TOTAL}}]},
        "product_relationships":{"schema":"ryeos.product_relationships.v1","relationships":[]}
    })
}

fn read_elf(file: &PinnedRegularFile, maximum: u64) -> Result<Vec<u8>> {
    let observed = file.observation()?;
    let bytes = file.read_stable_bounded(&observed, maximum)?;
    // Finite Linux x86_64 fixture, not a dependency-closure or ABI qualifier.
    ensure!(
        bytes.len() >= 64
            && &bytes[..4] == b"\x7fELF"
            && bytes[4] == 2
            && bytes[5] == 1
            && bytes[18..20] == [62, 0],
        "fixture member must be a Linux x86_64 ELF input"
    );
    Ok(bytes)
}

/// Stage a fresh retained tree using no-follow, pinned directory creation.
/// Failures may leave disposable fixture inputs; no rollback/authority claim.
/// The caller next executes PRODUCER_REF and captures PRODUCT_NAME publicly.
pub fn write_producer(
    project: &Path,
    fixture: &FastFixture,
    inputs: RuntimeInputs<'_>,
) -> Result<()> {
    stage_runtime(project, inputs)?;
    retained_runtime_producer::write_retained_runtime_producer_sources(
        project,
        &fixture.publisher,
        &serde_yaml::to_string(&recipe())?,
    )
}

fn stage_runtime(project: &Path, inputs: RuntimeInputs<'_>) -> Result<()> {
    ensure!(
        std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64",
        "native evaluator fixture currently requires Linux x86_64"
    );
    let evaluator = read_elf(inputs.evaluator, MAX_MEMBER)?;
    let loader = read_elf(inputs.loader, 2 * 1024 * 1024)?;
    let libc = read_elf(inputs.libc, 4 * 1024 * 1024)?;
    let libgcc = read_elf(inputs.libgcc, 2 * 1024 * 1024)?;
    ensure!(
        [&evaluator, &loader, &libc, &libgcc]
            .iter()
            .map(|bytes| bytes.len() as u64)
            .sum::<u64>()
            <= MAX_TOTAL,
        "native runtime exceeds total bound"
    );
    let root = PinnedDirectory::open(project)?.context("fixture project absent")?;
    let products = root.open_or_create_child(OsStr::new("products"), 0o755)?;
    let runtime = products.create_child(OsStr::new("external-runtime"), 0o755)?;
    let bin = runtime.create_child(OsStr::new("bin"), 0o755)?;
    let lib64 = runtime.create_child(OsStr::new("lib64"), 0o755)?;
    let usr = runtime.create_child(OsStr::new("usr"), 0o755)?;
    let libraries = usr.create_child(OsStr::new("lib"), 0o755)?;
    for (directory, name, bytes) in [
        (&bin, "evaluator", &evaluator),
        (&lib64, "ld-linux-x86-64.so.2", &loader),
        (&libraries, "libc.so.6", &libc),
        (&libraries, "libgcc_s.so.1", &libgcc),
    ] {
        directory
            .atomic_create_pinned_regular(OsStr::new(name), bytes, 0o755)?
            .context("native runtime refuses member replacement")?;
    }
    Ok(())
}

fn runtime_definition() -> Value {
    json!({
        "category":"fixtures/native-evaluation","name":"runtime","version":"1.0.0",
        "description":"Captured native evaluator with explicit captured loader and library path",
        "executor_id":"@subprocess","execution_protocol":"protocol:ryeos/core/opaque",
        "effects":"live","filesystem_authority":"captured_execution","network_authority":"isolated",
        "source_scope":{"location":"item_namespace","load_roots":["item_directory"],"materialization":"read_only"},
        "env_config":{"interpreter":{"type":"realization_member","realization_id":"runtime",
            "relative_path":"lib64/ld-linux-x86-64.so.2"}},
        "config":{"command":"${interpreter}","args":["--inhibit-cache","--library-path",
            format!("{RUNTIME_ROOT}/usr/lib"),format!("{RUNTIME_ROOT}/bin/evaluator"),"${source.entry}"],
            "input_data":"${params_json}","timeout_secs":30}
    })
}

fn consumer_definition(manifest: &str, binding_id: &str, rule: &EvaluationRule) -> Value {
    json!({
        "category":"fixtures/native-evaluation","name":"check","version":"1.0.0",
        "description":"Evaluate frozen candidate data against B-owned admitted rules",
        "executor_id":RUNTIME_REF,"execution_protocol":"protocol:ryeos/core/opaque",
        "effects":"live","filesystem_authority":"captured_execution","network_authority":"isolated",
        "supported_target":{"os":"linux","arch":"x86_64","resources":[]},
        "execution_endpoint":{"kind":"external","binding_id":binding_id,"stdout_max_bytes":4096,"stderr_max_bytes":4096},
        "external_content":[{"id":"runtime","kind":"tree","mode":"pinned","digest":manifest,
            "mount_root":"execution_runtime","mount":"native-evaluator"}],
        "evaluation_rule":rule,
        "config_schema":{"type":"object","properties":{
            "base_snapshot_hash":{"type":"string","pattern":"^[a-f0-9]{64}$"},
            "candidate_snapshot_hash":{"type":"string","pattern":"^[a-f0-9]{64}$"},
            "expect_integration":{"type":"boolean"}},
            "required":["base_snapshot_hash","candidate_snapshot_hash"],"additionalProperties":false}
    })
}

fn write_json_source(
    project: &PinnedDirectory,
    components: &[&str],
    value: &Value,
    fixture: &FastFixture,
) -> Result<()> {
    let (name, parents) = components.split_last().context("empty source path")?;
    let mut directory = project.try_clone()?;
    for parent in parents {
        directory = directory.open_or_create_child(OsStr::new(parent), 0o755)?;
    }
    // JSON-as-YAML is intentional: the admitted SourceEntry bytes include Tool
    // metadata; evaluator consumes only the bounded nested evaluation_rule.
    let body = serde_json::to_string_pretty(value)?;
    let signed = lillux::signature::sign_content_at(
        &body,
        &fixture.publisher,
        "#",
        None,
        fast_fixture::FAST_FIXTURE_TIME,
    );
    ensure!(
        signed.len() <= 64 * 1024,
        "signed source exceeds evaluator bound"
    );
    directory
        .atomic_create_pinned_regular(OsStr::new(name), signed.as_bytes(), 0o644)?
        .context("native fixture refuses signed source replacement")?;
    Ok(())
}

/// Author before public snapshot/import/bind. Capture evidence is validated,
/// never manufactured or upgraded to a consumer admission witness here.
pub fn write_consumer(
    project: &Path,
    fixture: &FastFixture,
    captured: &ProductCaptureEvidence,
    binding_id: &str,
    rule: &EvaluationRule,
) -> Result<()> {
    rule.validate()?;
    captured.validate()?;
    ensure!(!binding_id.is_empty(), "external binding selector absent");
    ensure!(
        captured.producer.canonical_ref == retained_runtime_producer::PRODUCER_REF
            && captured.recipe_ref == retained_runtime_producer::RECIPE_REF
            && captured.recipe_binding == retained_runtime_producer::RECIPE_BINDING
            && captured.declaration.name == retained_runtime_producer::PRODUCT_NAME
            && captured.declaration.path == "products/external-runtime"
            && captured.manifest_kind == ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
        "native consumer requires exact public retained-runtime capture"
    );
    let project = PinnedDirectory::open(project)?.context("fixture project absent")?;
    write_json_source(
        &project,
        &[".ai", "config", "execution", "execution.yaml"],
        &json!({
            "category":"execution","version":"2.1.0","schema_version":"2.1.0",
            "items":{"tool":{"fixtures/native-evaluation/check":{"timeout":30}}}
        }),
        fixture,
    )?;
    write_json_source(
        &project,
        &[
            ".ai",
            "tools",
            "fixtures",
            "native-evaluation",
            "runtime.yaml",
        ],
        &runtime_definition(),
        fixture,
    )?;
    write_json_source(
        &project,
        &[
            ".ai",
            "tools",
            "fixtures",
            "native-evaluation",
            "check.yaml",
        ],
        &consumer_definition(&captured.manifest_hash, binding_id, rule),
        fixture,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> EvaluationRule {
        EvaluationRule {
            schema_version: 1,
            sample_path: "candidate-strategy.txt".into(),
            expected_utf8: "known-good\n".into(),
            integration: None,
        }
    }

    #[test]
    fn native_rule_and_runtime_are_finite_and_explicit() {
        rule().validate().unwrap();
        for path in ["/x", "a/../b", "./x", "a//b", "a\\b", ""] {
            let mut invalid = rule();
            invalid.sample_path = path.into();
            assert!(invalid.validate().is_err(), "{path}");
        }
        let runtime = runtime_definition();
        assert_eq!(runtime["config"]["input_data"], "${params_json}");
        assert_eq!(
            runtime["config"]["args"],
            json!([
                "--inhibit-cache",
                "--library-path",
                "/ryeos/realizations/native-evaluator/usr/lib",
                "/ryeos/realizations/native-evaluator/bin/evaluator",
                "${source.entry}"
            ])
        );
        assert_eq!(
            runtime["env_config"]["interpreter"]["relative_path"],
            "lib64/ld-linux-x86-64.so.2"
        );
        let source = consumer_definition(&"a".repeat(64), "fixture", &rule());
        let body = serde_json::to_string_pretty(&source).unwrap();
        let yaml: Value = serde_yaml::from_str(&body).unwrap();
        assert_eq!(yaml, source);
        let parsed: EvaluationRule =
            serde_json::from_value(yaml["evaluation_rule"].clone()).unwrap();
        assert_eq!(parsed.expected_utf8, "known-good\n");
        assert_eq!(
            source["external_content"][0]["mount_root"],
            "execution_runtime"
        );
    }

    #[test]
    fn native_recipe_bounds_fit_exact_four_member_tree() {
        use ryeos_state::external_content::products::ProductDeclarations;
        let declarations =
            ProductDeclarations::from_value(recipe()["build_products"].clone()).unwrap();
        let selected = declarations
            .select(retained_runtime_producer::PRODUCT_NAME)
            .unwrap();
        assert_eq!(selected.path, "products/external-runtime");
        assert_eq!(
            recipe()["build_products"]["products"][0]["bounds"]["maximum_entries"],
            8
        );
    }

    #[test]
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn native_staging_is_exact_nonreplacing_and_refuses_symlink_parent() {
        let source = tempfile::tempdir().unwrap();
        let source_root = PinnedDirectory::open(source.path()).unwrap().unwrap();
        // Header-only bytes test staging mechanics, NOT executable validity.
        let mut bytes = vec![0u8; 64];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[18] = 62;
        let file = source_root
            .atomic_create_pinned_regular(OsStr::new("input"), &bytes, 0o755)
            .unwrap()
            .unwrap();
        let inputs = || RuntimeInputs {
            evaluator: &file,
            loader: &file,
            libc: &file,
            libgcc: &file,
        };
        assert!(read_elf(&file, 63).is_err());
        let project = tempfile::tempdir().unwrap();
        stage_runtime(project.path(), inputs()).unwrap();
        let root = PinnedDirectory::open(project.path()).unwrap().unwrap();
        for member in [
            "bin/evaluator",
            "lib64/ld-linux-x86-64.so.2",
            "usr/lib/libc.so.6",
            "usr/lib/libgcc_s.so.1",
        ] {
            let path = format!("products/external-runtime/{member}");
            let retained = root
                .open_pinned_regular_descendant(Path::new(&path), false)
                .unwrap()
                .unwrap();
            assert_eq!(retained.read_bounded(64).unwrap(), bytes);
            assert_eq!(retained.permission_mode().unwrap(), 0o755);
        }
        assert!(stage_runtime(project.path(), inputs()).is_err());
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), project.path().join("products")).unwrap();
        assert!(stage_runtime(project.path(), inputs()).is_err());
        assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
    }
}
