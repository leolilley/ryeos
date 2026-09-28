//! Direct exec-server fixture input: no app-server, scripted model or Worker.
//! Exact-child exit is not namespace settlement. Only routed-guest's checked
//! native result may supply that observation; diagnostic failure never does.

use super::*;
use lillux::subordinate_process::{
    SubordinateDiagnosticDrain, SubordinateProcessExit, SubordinateProcessInput,
    SubordinateProcessOutput,
};

pub(super) struct DirectGuest {
    pub root: PathBuf,
    pub request_hash: String,
    pub child: SubordinateProcess,
    pub input: Option<SubordinateProcessInput>,
    pub output: SubordinateProcessOutput,
    diagnostics: SubordinateDiagnosticDrain,
    deadline: MonotonicDeadline,
    settled: bool,
}

impl DirectGuest {
    pub fn start(capture_limit: u64) -> Result<Self> {
        Self::start_with_candidate(capture_limit, None)
    }

    pub fn start_with_candidate(capture_limit: u64, seed: Option<(&str, &[u8])>) -> Result<Self> {
        Self::start_with_recipe(capture_limit, seed, real_codex_requirement().runtime_recipe)
    }

    fn start_with_recipe(
        capture_limit: u64,
        seed: Option<(&str, &[u8])>,
        recipe: ryeos_state::external_execution::admission::ExternalCandidateRuntimeRecipe,
    ) -> Result<Self> {
        ensure!(
            (1..=1024 * 1024).contains(&capture_limit),
            "capture limit invalid"
        );
        let (codex, codex_hash) = pinned_codex_artifact();
        let tools = stage_pinned_codex_command_tools(&codex, true);
        let root = tempfile::tempdir()?.keep();
        eprintln!(
            "direct routed guest retained occurrence: {}",
            root.display()
        );
        std::fs::create_dir(root.join("runtime"))?;
        std::fs::create_dir(root.join("runtime/bin"))?;
        std::fs::copy(&codex, root.join("runtime/bin/codex"))?;
        std::fs::set_permissions(
            root.join("runtime/bin/codex"),
            std::fs::Permissions::from_mode(0o755),
        )?;
        std::fs::create_dir(root.join("candidate"))?;
        if let Some((name, bytes)) = seed {
            ensure!(
                !name.is_empty()
                    && name.len() <= 128
                    && name != "."
                    && name != ".."
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                    && bytes.len() <= 512 * 1024,
                "candidate seed must be one bounded fixture leaf"
            );
            // Before spawn/native preparation: no reliance on propagation of
            // later controller writes into the guest's private mount topology.
            std::fs::write(root.join("candidate").join(name), bytes)?;
        }
        let tools = copy_tree(tools.path(), &root.join("tools"))?;
        let request = serde_json::to_vec(&json!({
            "schema":"test.routed_guest.v1",
            "recipe":recipe,
            "effective_environment":fixture_guest_environment()?,
            "runtime":{"bin/codex":{"sha256":codex_hash,"mode":0o755}},
            "tools":tools,"capture_limit":capture_limit,
        }))?;
        std::fs::write(root.join("guest-request.json"), &request)?;
        let program = Path::new(env!("CARGO_BIN_EXE_ryeos-synthetic-routed-guest"));
        ensure!(
            program.is_absolute() && std::fs::symlink_metadata(program)?.is_file(),
            "relay executable absent"
        );
        let deadline = MonotonicDeadline::after(lillux::time::Duration::from_secs(115));
        let mut child = SubordinateProcess::spawn(SubordinateProcessRequest {
            cmd: program.to_string_lossy().into_owned(),
            argv0: None,
            args: vec![],
            cwd: root.to_string_lossy().into_owned(),
            envs: vec![("LANG".into(), "C".into()), ("LC_ALL".into(), "C".into())],
            limits: None,
            inherited_fds: vec![],
        })
        .map_err(anyhow::Error::msg)?;
        let input = child.take_input().map_err(anyhow::Error::msg)?;
        let output = child.take_output().map_err(anyhow::Error::msg)?;
        let diagnostics = child
            .take_error()
            .map_err(anyhow::Error::msg)?
            .start_discarding()?;
        Ok(Self {
            root,
            request_hash: lillux::sha256_hex(&request),
            child,
            input: Some(input),
            output,
            diagnostics,
            deadline,
            settled: false,
        })
    }

    pub fn send(&mut self, message: Value) -> Result<()> {
        let mut frame = serde_json::to_vec(&message)?;
        frame.push(b'\n');
        ensure!(frame.len() <= MAX_EVENT_BYTES, "direct request bound");
        self.input
            .as_mut()
            .context("direct input already closed")?
            .write_all_until(&frame, self.deadline)?;
        Ok(())
    }

    pub fn request(&mut self, id: u64, method: &str, params: Value) -> Result<Value> {
        self.send(json!({"id":id,"method":method,"params":params}))?;
        // These fixture requests should complete within ten seconds, while
        // native cleanup retains its separate original lifetime deadline.
        let deadline = MonotonicDeadline::after(lillux::time::Duration::from_secs(10));
        for _ in 0..16 {
            let frame = self
                .output
                .read_frame_until(b'\n', MAX_EVENT_BYTES, deadline)?;
            ensure!(
                frame.last() == Some(&b'\n'),
                "direct guest ended before response"
            );
            let response: Value = serde_json::from_slice(&frame)?;
            if response.get("id").is_some() {
                ensure!(
                    response["id"] == id && response.get("error").is_none(),
                    "direct guest refused or mismatched response"
                );
                return response
                    .get("result")
                    .cloned()
                    .context("direct result absent");
            }
            ensure!(
                response["method"].is_string() && response["params"].is_object(),
                "malformed direct notification"
            );
        }
        anyhow::bail!("direct response notification limit")
    }

    pub fn initialize(&mut self) -> Result<()> {
        let initialized = self.request(
            1,
            "initialize",
            json!({"clientName":"ryeos-routed-negative-fixture"}),
        )?;
        ensure!(
            initialized["sessionId"]
                .as_str()
                .is_some_and(|id| !id.is_empty()),
            "native exec-server identity absent"
        );
        self.send(json!({"method":"initialized","params":{}}))?;
        let info = self.request(2, "environment/info", json!({}))?;
        ensure!(
            info["cwd"] == "file:///workspace",
            "native guest cwd changed"
        );
        Ok(())
    }

    pub fn read_result(&self, name: &str) -> Result<Value> {
        ensure!(
            matches!(name, "guest-observation.json" | "guest-failure.json"),
            "unknown direct evidence member"
        );
        let directory =
            lillux::PinnedDirectory::open(&self.root)?.context("direct occurrence absent")?;
        let file = directory
            .open_pinned_regular(std::ffi::OsStr::new(name), false)?
            .context("direct evidence absent")?;
        let bytes = file.read_stable_bounded(&file.observation()?, 3 * MAX_TOTAL_BYTES as u64)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn settle(&mut self) -> Result<SubordinateProcessExit> {
        self.settle_until(self.deadline)
    }

    pub fn settle_until(&mut self, deadline: MonotonicDeadline) -> Result<SubordinateProcessExit> {
        // Callers may shorten the original bound, never renew this occurrence.
        let deadline = if deadline.remaining() < self.deadline.remaining() {
            deadline
        } else {
            self.deadline
        };
        let exit = self
            .child
            .wait_exact_child_until(deadline)
            .map_err(anyhow::Error::msg);
        // Forceful direct-child cleanup is failure, not a substitute for the
        // native namespace settlement and capture path we intended to test.
        let timed_out = !matches!(exit, Ok(Some(_)));
        let killed = if timed_out {
            self.child
                .kill_exact_child_until(MonotonicDeadline::after(
                    lillux::time::Duration::from_secs(5),
                ))
                .map_err(anyhow::Error::msg)
        } else {
            Ok(None)
        };
        self.input = None;
        let drained = self
            .diagnostics
            .cancel_until(MonotonicDeadline::after(lillux::time::Duration::from_secs(
                5,
            )))
            .is_some();
        self.settled = drained && (matches!(exit, Ok(Some(_))) || matches!(killed, Ok(Some(_))));
        ensure!(
            !timed_out,
            "direct fixture exceeded native lifetime; exact-child cleanup is not namespace evidence"
        );
        ensure!(drained, "direct diagnostic reader join uncertain");
        exit?.context("native guest exit absent")
    }
}

#[test]
#[ignore = "exact pinned Codex/tools; changed signed recipe must refuse before native contact"]
fn changed_runtime_recipe_refuses_before_guest_launch() -> Result<()> {
    let mut recipe = real_codex_requirement().runtime_recipe;
    recipe.arguments.push("--unexpected".into());
    let mut guest = DirectGuest::start_with_recipe(4096, None, recipe)?;
    let exit = guest.settle()?;
    ensure!(
        !exit.success && exit.code == Some(1),
        "changed recipe did not refuse"
    );
    let failure = guest.read_result("guest-failure.json")?;
    ensure!(
        failure["settlement"] == "not_attested"
            && failure["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("selected signed Codex recipe"))
            && !guest.root.join("guest-started").exists()
            && !guest.root.join("guest-observation.json").exists(),
        "changed recipe reached guest contact or claimed settlement"
    );
    Ok(())
}

impl Drop for DirectGuest {
    fn drop(&mut self) {
        if !self.settled {
            // An assertion/early return still closes transport and requests
            // bounded cleanup. Discarded errors confer no successful evidence.
            self.input = None;
            let _ = self.settle();
        }
    }
}

#[test]
#[ignore = "exact pinned Codex/tools and native namespace support; no model or app-server"]
fn native_capture_exact_limit_succeeds_and_one_byte_less_refuses() -> Result<()> {
    const BYTES: &[u8] = b"bounded-candidate";
    for limit in [BYTES.len() as u64, BYTES.len() as u64 - 1] {
        let mut guest = DirectGuest::start(limit)?;
        guest.initialize()?;
        guest.request(3,"fs/writeFile",json!({"path":"file:///workspace/budget.txt","dataBase64":STANDARD.encode(BYTES),"sandbox":null}))?;
        std::fs::write(guest.root.join("finish"), b"capture")?;
        let exit = guest.settle()?;
        ensure!(
            std::fs::read(guest.root.join("candidate/budget.txt"))? == BYTES,
            "guest candidate changed"
        );
        if limit == BYTES.len() as u64 {
            ensure!(exit.success, "exact-limit capture failed");
            let observation = guest.read_result("guest-observation.json")?;
            let file = ryeos_state::objects::ProjectFile {
                blob_hash: lillux::sha256_hex(BYTES),
                size: BYTES.len() as u64,
                normalized_mode: 0o644,
            };
            let file_hash = ryeos_state::objects::canonical_value_digest(&file.to_value())?;
            ensure!(
                observation["request_sha256"] == guest.request_hash
                    && observation["namespace_exit"] == "Signal(9)"
                    && observation["captured_files"] == json!({"budget.txt":file_hash}),
                "exact-limit capture observation differs"
            );
            ensure!(
                !guest.root.join("guest-failure.json").exists(),
                "exact-limit also produced refusal"
            );
        } else {
            ensure!(
                !exit.success && exit.code == Some(1),
                "over-budget capture did not refuse"
            );
            let failure = guest.read_result("guest-failure.json")?;
            ensure!(
                failure["schema"] == "test.routed_guest_failure.v1"
                    && failure["settlement"] == "not_attested",
                "refusal promoted settlement authority"
            );
            let reason = failure["reason"]
                .as_str()
                .context("capture refusal reason absent")?;
            ensure!(
                reason.contains(&format!("streaming CAS source exceeds {limit} bytes:")),
                "unexpected capture refusal boundary: {reason}"
            );
            ensure!(
                !guest.root.join("guest-observation.json").exists(),
                "over-budget capture returned a successful tree"
            );
            // Capture may have staged CAS blobs before refusing; no rollback
            // claim follows from the absence of a returned successful tree.
        }
        eprintln!(
            "direct capture evidence: {}",
            json!({"scope":"native_capture_mechanism_only","request_sha256":guest.request_hash,"capture_limit":limit,"candidate_bytes":BYTES.len(),"success":exit.success,"model_contact":false})
        );
    }
    Ok(())
}
