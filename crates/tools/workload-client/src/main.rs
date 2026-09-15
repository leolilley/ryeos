//! Restricted workload-local RyeOS client.
//!
//! This artifact is staged as `ryeos` only inside an admitted worker
//! realization. It deliberately has no ordinary CLI dispatcher, app-root
//! discovery, daemon HTTP/UDS fallback, signing, auth, remote, installation,
//! lifecycle, or project-publication code.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use ryeos_runtime::workload_client::{
    WORKLOAD_CLIENT_ENDPOINT_ENV, WORKLOAD_CLIENT_PROTOCOL, WorkloadClientExecuteRequest,
    WorkloadClientOperation, WorkloadClientOutcome, WorkloadClientRequestFrame,
    WorkloadClientResponseFrame,
};

// Exact release realization publication extracts this private ELF section and
// compares it byte-for-byte with the retained build testimony. This keeps
// build provenance out of the operational CLI surface: the admitted binary
// still accepts only `ryeos execute`, while an ordinary development build is
// mechanically distinguishable from a publishable exact build.
const BUILD_TESTIMONY_LEN: usize =
    include_bytes!(concat!(env!("OUT_DIR"), "/ryeos-workload-client-build")).len();

#[repr(C, align(1))]
struct EmbeddedBuildTestimony([u8; BUILD_TESTIMONY_LEN]);

#[used]
#[unsafe(link_section = ".ryeos_workload_client_build")]
static EMBEDDED_BUILD_TESTIMONY: EmbeddedBuildTestimony = EmbeddedBuildTestimony(*include_bytes!(
    concat!(env!("OUT_DIR"), "/ryeos-workload-client-build")
));

fn main() {
    // Keep the exact-build section reachable as data as well as marking it
    // `used`, so a release link cannot garbage-collect the private testimony
    // that the realization packager must extract and compare.
    std::hint::black_box(&EMBEDDED_BUILD_TESTIMONY);
    if let Err(error) = run() {
        eprintln!("ryeos workload client: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    lillux::disable_process_core_dumps().map_err(anyhow::Error::msg)?;
    let invocation = parse_invocation(std::env::args_os().skip(1))?;
    let endpoint = std::env::var_os(WORKLOAD_CLIENT_ENDPOINT_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("workload-client endpoint is not admitted"))?;
    let mut stream = lillux::LocalDuplexStream::connect_isolated_runtime_broker(&endpoint)?;
    let random = lillux::crypto::generate_random_bytes::<32>();
    let frame = WorkloadClientRequestFrame {
        protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
        request_id: format!("wc-{}", lillux::sha256_hex(&random)),
        operation: WorkloadClientOperation::Execute(invocation),
    };
    frame.validate()?;
    ryeos_runtime::workload_client::write_frame(&mut stream, &frame)
        .context("execution contact may have occurred; do not rerun as a new CLI invocation")?;
    let response: WorkloadClientResponseFrame =
        ryeos_runtime::workload_client::read_frame(&mut stream)
            .context("execution outcome is unknown; do not rerun as a new CLI invocation")?;
    response
        .validate()
        .context("execution outcome is unknown: invalid response; do not rerun")?;
    if response.request_id != frame.request_id {
        bail!(
            "workload-client response request id does not match; execution outcome is unknown, do not rerun"
        );
    }
    let succeeded = response.outcome.succeeded();
    match response.outcome {
        WorkloadClientOutcome::Dispatched { response } => {
            println!("{}", serde_json::to_string(&response.result)?);
            if !succeeded {
                bail!("child execution did not succeed");
            }
            Ok(())
        }
        WorkloadClientOutcome::Failed {
            code,
            message,
            retryable,
        } => {
            if matches!(
                code.as_str(),
                ryeos_runtime::callback::RUNTIME_ACTION_OUTCOME_UNKNOWN_CODE
                    | ryeos_runtime::callback::RUNTIME_ACTION_RESULT_UNAVAILABLE_CODE
            ) {
                bail!(
                    "{code}: {message}; do not rerun automatically: a new CLI invocation creates a new operation"
                );
            }
            bail!("{code}: {message} (retryable={retryable})");
        }
    }
}

fn parse_invocation(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<WorkloadClientExecuteRequest> {
    let mut args = args.into_iter();
    let command = utf8(args.next(), "command")?;
    if command != "execute" {
        bail!("restricted workload client supports only `ryeos execute`");
    }
    let item_ref = utf8(args.next(), "item ref")?;
    let mut params = serde_json::Value::Object(Default::default());
    let mut params_set = false;
    let mut ref_bindings = BTreeMap::new();
    let mut method = None;
    let mut method_args = None;
    while let Some(argument) = args.next() {
        let argument = utf8(Some(argument), "argument")?;
        match argument.as_str() {
            "--params" => {
                if params_set {
                    bail!("execution params were supplied more than once");
                }
                let raw = utf8(args.next(), "--params value")?;
                params = serde_json::from_str(&raw).context("decode --params JSON")?;
                params_set = true;
            }
            "--method" => {
                if method.is_some() {
                    bail!("--method was supplied more than once");
                }
                method = Some(utf8(args.next(), "--method value")?);
            }
            "--method-args" => {
                if method_args.is_some() {
                    bail!("--method-args was supplied more than once");
                }
                let raw = utf8(args.next(), "--method-args value")?;
                method_args =
                    Some(serde_json::from_str(&raw).context("decode --method-args JSON")?);
            }
            "--ref" => {
                let binding = utf8(args.next(), "--ref value")?;
                let (name, item_ref) = binding
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("--ref must be NAME=ITEM_REF"))?;
                if ref_bindings
                    .insert(name.to_owned(), item_ref.to_owned())
                    .is_some()
                {
                    bail!("duplicate workload-client ref binding `{name}`");
                }
            }
            _ if argument.starts_with('-') => {
                bail!("unsupported restricted workload-client option `{argument}`")
            }
            _ if !params_set => {
                params = serde_json::from_str(&argument).context("decode execution params JSON")?;
                params_set = true;
            }
            _ => bail!("unexpected restricted workload-client argument `{argument}`"),
        }
    }
    let call = (method.is_some() || method_args.is_some()).then_some(
        ryeos_runtime::callback::MethodCall {
            method,
            args: method_args,
        },
    );
    let request = WorkloadClientExecuteRequest {
        item_ref,
        ref_bindings,
        params,
        call,
    };
    let frame = WorkloadClientRequestFrame {
        protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
        request_id: "validation".to_owned(),
        operation: WorkloadClientOperation::Execute(request.clone()),
    };
    frame.validate()?;
    Ok(request)
}

fn utf8(value: Option<std::ffi::OsString>, label: &str) -> Result<String> {
    value
        .ok_or_else(|| anyhow::anyhow!("missing {label}"))?
        .into_string()
        .map_err(|_| anyhow::anyhow!("{label} is not UTF-8"))
}
