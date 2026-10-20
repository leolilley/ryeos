//! The external-candidate requirement documents the synthetic and real codex
//! fixtures declare. Kept apart from source authoring so a test that only
//! needs the requirement includes nothing else.

use std::collections::BTreeMap;

use ryeos_state::external_execution::admission::{
    ExternalCandidateExecutionRoute, ExternalCandidateProcFilesystem, ExternalCandidateRequirement,
    ExternalCandidateRuntimeRecipe,
};

pub fn real_codex_requirement() -> ExternalCandidateRequirement {
    let mut requirement = joined_external_candidate_requirement();
    requirement.runtime_recipe.executable_relative_path = "bin/codex".into();
    requirement.runtime_recipe.argv0 = "codex".into();
    requirement.runtime_recipe.arguments =
        vec!["exec-server".into(), "--listen".into(), "stdio".into()];
    requirement.runtime_recipe.environment.clear();
    requirement
}

pub fn joined_external_candidate_requirement() -> ExternalCandidateRequirement {
    ExternalCandidateRequirement {
        schema: 7,
        required_lifecycle_capabilities: Default::default(),
        protocol: ryeos_state::external_execution::admission::PROTOCOL.into(),
        connector_protocol: ryeos_state::external_execution::admission::CONNECTOR_PROTOCOL.into(),
        execution_route: ExternalCandidateExecutionRoute::ConnectorOnly,
        provider_declaration_id: "codex-hosted".into(),
        provider_configuration_destination: "environments.toml".into(),
        runtime_product_declaration_id: "auxiliary".into(),
        runtime_authority: ryeos_state::external_execution::admission::ExternalCandidateRuntimeAuthority::CapturedProduct,
        runtime_recipe: ExternalCandidateRuntimeRecipe {
            schema: 2,
            runtime_mount_destination: "/runtime".into(),
            executable_relative_path: "lib64/ld-linux-x86-64.so.2".into(),
            argv0: "ld-linux-x86-64.so.2".into(),
            arguments: vec![
                "--library-path".into(),
                "/runtime/usr/lib".into(),
                "/runtime/bin/candidate".into(),
            ],
            cwd: "/workspace".into(),
            environment: BTreeMap::from([(
                "RYEOS_SYNTHETIC_JOINED_PROVIDER_REQUIRED".into(),
                "1".into(),
            )]),
            max_stdout_bytes: 1024 * 1024,
            max_stderr_bytes: 1024 * 1024,
            proc_filesystem: ExternalCandidateProcFilesystem::PidNamespaceNested,
            contain_process_group: false,
            nested_sandbox: true,
        },
    }
}
