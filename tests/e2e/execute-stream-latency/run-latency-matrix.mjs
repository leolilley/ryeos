#!/usr/bin/env node

import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const REPO_ROOT = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../..",
);
const APP_ROOT =
  process.env.RYEOS_APP_ROOT ?? "/home/leo/.local/share/ryeos";
const PROJECT_PATH =
  process.env.RYEOS_PROJECT_PATH ??
  path.join(REPO_ROOT, "tests/e2e/execute-stream-latency");
const RYEOSD_URL = process.env.RYEOSD_URL ?? "http://127.0.0.1:7400";
const KEY_PATH =
  process.env.RYEOS_CLI_KEY_PATH ??
  path.join(APP_ROOT, ".ai/config/keys/signing/private_key.pem");
const DAEMON_LOG = path.join(
  APP_ROOT,
  ".ai/state/ryeosd-start.stderr.log",
);
const DEFAULT_OUTPUT_DIR = path.resolve(
  path.join(
    REPO_ROOT,
    "tests/e2e/execute-stream-latency/outputs/full-matrix",
  ),
);
const REQUEST_TIMEOUT_MS = 180_000;
const CORPUS_ID = "execute-stream-trivial-v1";
const EXPECTED_OUTPUT = "OK";
const CORPUS_PARAMETERS = Object.freeze({
  message: "Reply with exactly OK.",
  history: "[]",
  db_context: "",
  workspace_state: "",
});
const PROJECT_EXECUTION_POLICY = Object.freeze({
  schema_version: 2,
  ownership: "daemon_owned",
  recovery: "none",
  response: "wait",
  target: { kind: "here" },
  environment: {
    kind: "project_overlay",
    include_operator_vault: true,
    name_policy: { kind: "declared_required" },
  },
  project: {
    kind: "live_direct",
    access: "read_write",
    child_policy: { kind: "inherit" },
  },
});
const MODELS = Object.freeze({
  zai_flash: {
    id: "glm-4.7-flash",
    directive_ref: "directive:test/latency/zai_flash",
    fixture_path: ".ai/directives/test/latency/zai_flash.md",
  },
});
const CLASSES = Object.freeze([
  "post-install-cold-observed",
  "warm",
  "restart-cold",
]);

function parseArgs(argv) {
  const args = {
    samples: 50,
    models: Object.keys(MODELS),
    classes: ["warm", "restart-cold"],
    outputDir: DEFAULT_OUTPUT_DIR,
    probeOnly: false,
    attachExisting: false,
    resume: false,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const value = argv[index];
    if (value === "--samples") {
      args.samples = Number(argv[++index]);
    } else if (value === "--models") {
      args.models = argv[++index].split(",").filter(Boolean);
    } else if (value === "--classes") {
      args.classes = argv[++index].split(",").filter(Boolean);
    } else if (value === "--output") {
      args.outputDir = path.resolve(argv[++index]);
    } else if (value === "--probe-only") {
      args.probeOnly = true;
      args.samples = 1;
      args.classes = ["warm"];
    } else if (value === "--attach-existing") {
      args.attachExisting = true;
    } else if (value === "--resume") {
      args.resume = true;
    } else {
      throw new Error(`unknown argument: ${value}`);
    }
  }
  if (!Number.isInteger(args.samples) || args.samples < 1) {
    throw new Error("--samples must be a positive integer");
  }
  for (const model of args.models) {
    if (!MODELS[model]) throw new Error(`unknown model key: ${model}`);
  }
  for (const className of args.classes) {
    if (!CLASSES.includes(className)) {
      throw new Error(`unknown class: ${className}`);
    }
  }
  if (
    args.classes.includes("post-install-cold-observed") &&
    args.samples !== 1
  ) {
    throw new Error(
      "post-install-cold-observed requires --samples 1; the harness does not reinstall between samples",
    );
  }
  return args;
}

const args = parseArgs(process.argv.slice(2));
const outputDir = args.outputDir;
const samplesPath = path.join(outputDir, "samples.jsonl");
const warmupsPath = path.join(outputDir, "warmups.jsonl");
const metadataPath = path.join(outputDir, "metadata.json");
const summaryPath = path.join(outputDir, "summary.json");
fs.mkdirSync(outputDir, { recursive: true });

const privateKey = crypto.createPrivateKey(fs.readFileSync(KEY_PATH, "utf8"));
const publicDer = crypto
  .createPublicKey(privateKey)
  .export({ type: "spki", format: "der" });
const clientFingerprint = crypto
  .createHash("sha256")
  .update(publicDer.subarray(-32))
  .digest("hex");
let audience = null;
let stopping = false;

function monotonicMs(started) {
  return Number(process.hrtime.bigint() - started) / 1_000_000;
}

function errorDetail(caught) {
  const cause =
    caught && typeof caught === "object" && caught.cause
      ? caught.cause
      : null;
  return {
    message: String(caught).slice(0, 1_000),
    name:
      caught && typeof caught === "object" && typeof caught.name === "string"
        ? caught.name
        : null,
    cause_code:
      cause && typeof cause === "object" && typeof cause.code === "string"
        ? cause.code
        : null,
    cause_message:
      cause && typeof cause === "object" && cause.message
        ? String(cause.message).slice(0, 1_000)
        : null,
  };
}

function sha256Hex(value) {
  return crypto.createHash("sha256").update(value).digest("hex");
}

function command(program, commandArgs, options = {}) {
  const result = spawnSync(program, commandArgs, {
    encoding: "utf8",
    timeout: options.timeoutMs ?? 120_000,
    env: process.env,
  });
  if (result.error) throw result.error;
  if (result.status !== 0 && !options.allowFailure) {
    throw new Error(
      `${program} ${commandArgs.join(" ")} failed (${result.status}): ` +
        `${result.stderr || result.stdout}`.trim().slice(0, 2_000),
    );
  }
  return result;
}

function startNode() {
  const result = command("ryeos", ["start"], { timeoutMs: 60_000 });
  const combined = `${result.stdout}\n${result.stderr}`;
  if (!combined.includes("running")) {
    throw new Error(`ryeos start did not report running: ${combined.trim()}`);
  }
  audience = null;
}

function stopNode({ allowFailure = false } = {}) {
  const result = command("ryeos", ["stop"], {
    timeoutMs: 60_000,
    allowFailure,
  });
  audience = null;
  return result;
}

async function discoverAudience() {
  if (audience) return audience;
  const response = await fetch(`${RYEOSD_URL}/public-key`, {
    headers: {
      accept: "application/json",
      connection: "close",
    },
  });
  if (!response.ok) {
    throw new Error(
      `/public-key returned ${response.status}: ${(await response.text()).slice(0, 300)}`,
    );
  }
  const payload = await response.json();
  if (typeof payload.principal_id !== "string" || !payload.principal_id) {
    throw new Error("/public-key response omitted principal_id");
  }
  audience = payload.principal_id;
  return audience;
}

async function signedHeaders(method, routePath, body) {
  const requestAudience = await discoverAudience();
  const timestamp = Math.floor(Date.now() / 1_000).toString();
  const nonce = crypto.randomBytes(16).toString("hex");
  const canonical = [
    "ryeos-request-v1",
    method.toUpperCase(),
    routePath,
    sha256Hex(body),
    timestamp,
    nonce,
    requestAudience,
  ].join("\n");
  const contentHash = sha256Hex(Buffer.from(canonical, "utf8"));
  const signature = crypto.sign(
    null,
    Buffer.from(contentHash, "utf8"),
    privateKey,
  );
  return {
    "x-ryeos-key-id": `fp:${clientFingerprint}`,
    "x-ryeos-timestamp": timestamp,
    "x-ryeos-nonce": nonce,
    "x-ryeos-signature": signature.toString("base64"),
  };
}

function unwrapPayload(value) {
  if (!value || typeof value !== "object") return value;
  if (value.payload && typeof value.payload === "object") return value.payload;
  return value;
}

function stringField(value, ...keys) {
  const payload = unwrapPayload(value);
  if (!payload || typeof payload !== "object") return null;
  for (const key of keys) {
    if (typeof payload[key] === "string") return payload[key];
  }
  return null;
}

function parseSseBlock(block) {
  let event = null;
  const data = [];
  for (const line of block.split(/\r?\n/)) {
    if (line.startsWith("event:")) event = line.slice(6).trim();
    if (line.startsWith("data:")) data.push(line.slice(5).trimStart());
  }
  const rawData = data.join("\n");
  let parsed = null;
  if (rawData) {
    try {
      parsed = JSON.parse(rawData);
    } catch {
      parsed = null;
    }
  }
  return { event, rawData, parsed };
}

function logOffset() {
  try {
    return fs.statSync(DAEMON_LOG).size;
  } catch {
    return 0;
  }
}

function parseTrailingJson(line, marker) {
  const index = line.indexOf(marker);
  if (index === -1) return null;
  try {
    return JSON.parse(line.slice(index + marker.length).trim());
  } catch {
    return null;
  }
}

function parseQuotedJson(line, marker) {
  const index = line.indexOf(marker);
  if (index === -1) return null;
  const quoted = line.slice(index + marker.length).trim();
  try {
    return JSON.parse(JSON.parse(quoted));
  } catch {
    return null;
  }
}

function readTimingWindow(offset, threadId) {
  if (!threadId) return { snapshots: {}, child_stage: null, provider_calls: [] };
  let raw = "";
  try {
    const descriptor = fs.openSync(DAEMON_LOG, "r");
    const end = fs.fstatSync(descriptor).size;
    const length = Math.max(0, end - offset);
    const buffer = Buffer.alloc(length);
    fs.readSync(descriptor, buffer, 0, length, offset);
    fs.closeSync(descriptor);
    raw = buffer.toString("utf8");
  } catch {
    return { snapshots: {}, child_stage: null, provider_calls: [] };
  }
  const snapshots = {};
  let childStage = null;
  const providerCalls = [];
  for (const line of raw.split("\n")) {
    if (!line.includes(threadId)) continue;
    if (line.includes('event="launch_stage_timings"')) {
      const observation =
        line.match(/ observation="([^"]+)"/)?.[1] ??
        line.match(/ observation=([^ ]+)/)?.[1];
      const snapshot = parseTrailingJson(line, "timings_json=");
      if (observation && snapshot) snapshots[observation] = snapshot;
    }
    if (
      line.includes('child_event="directive_runtime_stage_timing"') &&
      line.includes("child_timing_json=")
    ) {
      childStage = parseQuotedJson(line, "child_timing_json=");
    }
    if (
      line.includes('child_event="directive_provider_call_timing"') &&
      line.includes("child_timing_json=")
    ) {
      const provider = parseQuotedJson(line, "child_timing_json=");
      if (provider) providerCalls.push(provider);
    }
  }
  return {
    snapshots,
    child_stage: childStage,
    provider_calls: providerCalls,
  };
}

async function cancelOutstanding({ launchId, threadId }) {
  const routePath = threadId
    ? `/threads/${encodeURIComponent(threadId)}/cancel`
    : launchId
      ? `/launches/${encodeURIComponent(launchId)}/cancel`
      : null;
  if (!routePath) return null;
  const body = Buffer.alloc(0);
  try {
    const response = await fetch(`${RYEOSD_URL}${routePath}`, {
      method: "POST",
      headers: {
        ...(await signedHeaders("POST", routePath, body)),
        connection: "close",
      },
      body,
    });
    return {
      route: routePath,
      status: response.status,
      ok: response.ok,
      response: (await response.text()).slice(0, 500),
    };
  } catch (error) {
    return { route: routePath, ok: false, error: String(error).slice(0, 500) };
  }
}

async function runProbe({
  sampleId,
  className,
  modelKey,
  ordinal,
  counted,
  attempt,
}) {
  const model = MODELS[modelKey];
  const body = Buffer.from(
    JSON.stringify({
      item_ref: model.directive_ref,
      ref_bindings: { model: model.directive_ref },
      project_path: PROJECT_PATH,
      parameters: CORPUS_PARAMETERS,
      execution_policy: PROJECT_EXECUTION_POLICY,
    }),
    "utf8",
  );
  const routePath = "/execute/stream";
  const logStart = logOffset();
  const started = process.hrtime.bigint();
  const startedAt = new Date().toISOString();
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);
  const client = {
    http_status: null,
    headers_ms: null,
    first_sse_byte_ms: null,
    execution_planning_ms: null,
    stream_started_ms: null,
    first_text_ms: null,
    terminal_ms: null,
  };
  const eventCounts = {};
  let launchId = null;
  let threadId = null;
  let terminalEvent = null;
  let terminalDetail = null;
  let outcome = "error";
  let error = null;
  let error_detail = null;
  let cancellation = null;
  let parseFailures = 0;
  let cognitionText = "";
  try {
    const headers = {
      "content-type": "application/json",
      accept: "text/event-stream",
      connection: "close",
      ...(await signedHeaders("POST", routePath, body)),
    };
    const response = await fetch(`${RYEOSD_URL}${routePath}`, {
      method: "POST",
      headers,
      body,
      signal: controller.signal,
    });
    client.http_status = response.status;
    client.headers_ms = monotonicMs(started);
    if (!response.ok) {
      throw new Error(
        `/execute/stream returned ${response.status}: ${(await response.text()).slice(0, 500)}`,
      );
    }
    if (!response.body) throw new Error("/execute/stream response has no body");
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      if (client.first_sse_byte_ms === null) {
        client.first_sse_byte_ms = monotonicMs(started);
      }
      buffer += decoder.decode(value, { stream: true });
      while (true) {
        const match = buffer.match(/\r?\n\r?\n/);
        if (!match || match.index === undefined) break;
        const block = buffer.slice(0, match.index);
        buffer = buffer.slice(match.index + match[0].length);
        const event = parseSseBlock(block);
        if (!event.event) continue;
        eventCounts[event.event] = (eventCounts[event.event] ?? 0) + 1;
        if (event.rawData && event.parsed === null) parseFailures += 1;
        if (event.event === "execution_planning") {
          client.execution_planning_ms ??= monotonicMs(started);
          launchId ??= stringField(event.parsed, "launch_id");
        } else if (event.event === "stream_started") {
          client.stream_started_ms ??= monotonicMs(started);
          threadId ??= stringField(event.parsed, "thread_id");
        } else if (event.event === "cognition_out") {
          const delta = stringField(event.parsed, "delta", "content", "text");
          if (typeof delta === "string") cognitionText += delta;
          if (
            client.first_text_ms === null &&
            typeof delta === "string" &&
            /\S/.test(delta)
          ) {
            client.first_text_ms = monotonicMs(started);
          }
        }
        if (
          [
            "thread_completed",
            "thread_failed",
            "thread_cancelled",
            "stream_error",
          ].includes(event.event)
        ) {
          terminalEvent = event.event;
          terminalDetail = event.parsed ?? event.rawData;
          client.terminal_ms ??= monotonicMs(started);
        }
      }
    }
    const normalizedOutput = cognitionText.trim();
    outcome =
      terminalEvent === "thread_completed" &&
      normalizedOutput === EXPECTED_OUTPUT
        ? "success"
        : "failure";
    if (
      terminalEvent === "thread_completed" &&
      normalizedOutput !== EXPECTED_OUTPUT
    ) {
      error =
        `unexpected normalized output: expected ${JSON.stringify(EXPECTED_OUTPUT)}, ` +
        `got ${JSON.stringify(normalizedOutput.slice(0, 200))}`;
    }
    if (!terminalEvent) {
      error = "stream ended without a terminal event";
      outcome = "failure";
    } else if (terminalEvent !== "thread_completed" && error === null) {
      error =
        stringField(terminalDetail, "error", "message", "detail") ??
        `terminal event: ${terminalEvent}`;
    }
  } catch (caught) {
    error_detail = errorDetail(caught);
    error = error_detail.message;
    outcome = controller.signal.aborted ? "timeout" : "error";
    cancellation = await cancelOutstanding({ launchId, threadId });
  } finally {
    clearTimeout(timeout);
  }
  await new Promise((resolve) => setTimeout(resolve, 200));
  const daemon = readTimingWindow(logStart, threadId);
  const normalizedOutput = cognitionText.trim();
  return {
    schema_version: 1,
    harness_status: "directional_unversioned",
    sample_id: sampleId,
    counted,
    class: className,
    model_key: modelKey,
    model_id: model.id,
    directive_ref: model.directive_ref,
    ordinal,
    attempt,
    corpus_id: CORPUS_ID,
    started_at: startedAt,
    outcome,
    terminal_event: terminalEvent,
    terminal_detail: terminalDetail,
    error,
    error_detail,
    cancellation,
    launch_id: launchId,
    thread_id: threadId,
    client,
    event_counts: eventCounts,
    sse_parse_failures: parseFailures,
    output: {
      expected: EXPECTED_OUTPUT,
      normalized_text: normalizedOutput.slice(0, 200),
      normalized_bytes: Buffer.byteLength(normalizedOutput, "utf8"),
      normalized_sha256: sha256Hex(Buffer.from(normalizedOutput, "utf8")),
      matches_expected: normalizedOutput === EXPECTED_OUTPUT,
    },
    daemon,
  };
}

function appendJsonl(filePath, value) {
  fs.appendFileSync(filePath, `${JSON.stringify(value)}\n`, "utf8");
}

function readRecords(filePath) {
  if (!fs.existsSync(filePath)) return [];
  return fs
    .readFileSync(filePath, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line));
}

function percentile(values, quantile) {
  const sorted = values
    .filter((value) => Number.isFinite(value))
    .sort((left, right) => left - right);
  if (sorted.length === 0) return null;
  const index = Math.min(
    sorted.length - 1,
    Math.max(0, Math.ceil(quantile * sorted.length) - 1),
  );
  return Number(sorted[index].toFixed(3));
}

function metricSummary(records, accessor) {
  const values = records.map(accessor).filter((value) => Number.isFinite(value));
  return {
    n: values.length,
    p50: percentile(values, 0.5),
    p90: percentile(values, 0.9),
    p95: percentile(values, 0.95),
  };
}

function stageElapsed(record, collection, stage) {
  const snapshot = record.daemon?.snapshots?.gateway_stream_started;
  const entries = snapshot?.[collection];
  if (!Array.isArray(entries)) return null;
  return entries.find((entry) => entry.stage === stage)?.elapsed_us / 1_000;
}

function writeSummary() {
  const records = readRecords(samplesPath);
  const groups = {};
  for (const className of args.classes) {
    for (const modelKey of args.models) {
      const group = records.filter(
        (record) =>
          record.class === className && record.model_key === modelKey,
      );
      const successes = group.filter((record) => record.outcome === "success");
      groups[`${className}/${modelKey}`] = {
        attempts: group.length,
        successes: successes.length,
        failures: group.length - successes.length,
        success_rate:
          group.length === 0
            ? null
            : Number((successes.length / group.length).toFixed(6)),
        client_headers_ms: metricSummary(successes, (record) => record.client.headers_ms),
        execution_planning_ms: metricSummary(
          successes,
          (record) => record.client.execution_planning_ms,
        ),
        stream_started_ms: metricSummary(
          successes,
          (record) => record.client.stream_started_ms,
        ),
        first_text_ms: metricSummary(
          successes,
          (record) => record.client.first_text_ms,
        ),
        completion_ms: metricSummary(
          successes,
          (record) => record.client.terminal_ms,
        ),
        preflight_admission_ms: metricSummary(
          successes,
          (record) =>
            stageElapsed(record, "top_level", "preflight_admission"),
        ),
        background_dispatch_ms: metricSummary(
          successes,
          (record) =>
            stageElapsed(record, "top_level", "background_dispatch"),
        ),
        augmentation_ms: metricSummary(
          successes,
          (record) => stageElapsed(record, "nested", "launch_augmentation"),
        ),
        runtime_preparation_ms: metricSummary(
          successes,
          (record) => stageElapsed(record, "nested", "runtime_preparation"),
        ),
        daemon_unattributed_ms: metricSummary(successes, (record) => {
          const value =
            record.daemon?.snapshots?.gateway_stream_started?.unattributed_us;
          return Number.isFinite(value) ? value / 1_000 : null;
        }),
        child_provider_headers_ms: metricSummary(successes, (record) => {
          const timing = record.daemon?.child_stage?.timing;
          if (
            !Number.isFinite(timing?.provider_request_submitted_us) ||
            !Number.isFinite(timing?.provider_response_headers_us)
          ) {
            return null;
          }
          return (
            timing.provider_response_headers_us -
            timing.provider_request_submitted_us
          ) / 1_000;
        }),
      };
    }
  }
  const invariants = {
    accounted_union_exceeds_total: records
      .filter((record) => {
        const snapshot =
          record.daemon?.snapshots?.gateway_stream_started;
        return (
          snapshot &&
          snapshot.accounted_union_us > snapshot.total_us
        );
      })
      .map((record) => record.sample_id),
    missing_gateway_stream_started_snapshot: records
      .filter(
        (record) =>
          record.outcome === "success" &&
          !record.daemon?.snapshots?.gateway_stream_started,
      )
      .map((record) => record.sample_id),
    sse_parse_failures: records
      .filter((record) => record.sse_parse_failures > 0)
      .map((record) => ({
        sample_id: record.sample_id,
        count: record.sse_parse_failures,
      })),
  };
  fs.writeFileSync(
    summaryPath,
    `${JSON.stringify(
      {
        schema_version: 1,
        generated_at: new Date().toISOString(),
        samples_path: samplesPath,
        groups,
        invariants,
      },
      null,
      2,
    )}\n`,
    "utf8",
  );
}

function alternatingModelOrder(ordinal) {
  if (args.models.length < 2 || ordinal % 2 === 1) return [...args.models];
  return [...args.models].reverse();
}

function prepareClass(className) {
  if (className !== "restart-cold") return;
  stopNode();
  startNode();
}

function daemonIdentity() {
  try {
    return JSON.parse(
      fs.readFileSync(path.join(APP_ROOT, "daemon.json"), "utf8"),
    );
  } catch {
    return null;
  }
}

function projectFixtureManifest() {
  return args.models.map((key) => {
    const model = MODELS[key];
    const fixturePath = path.join(PROJECT_PATH, model.fixture_path);
    return {
      key,
      directive_ref: model.directive_ref,
      path: model.fixture_path,
      sha256: sha256Hex(fs.readFileSync(fixturePath)),
    };
  });
}

function runMetadata(scriptHash, createdAt) {
  return {
    schema_version: 1,
    harness_status: "directional_unversioned",
    created_at: createdAt,
    harness_sha256: scriptHash,
    app_root: APP_ROOT,
    project_path: PROJECT_PATH,
    ryeosd_url: RYEOSD_URL,
    client_fingerprint: clientFingerprint,
    corpus: {
      id: CORPUS_ID,
      parameters: CORPUS_PARAMETERS,
      expected_normalized_output: EXPECTED_OUTPUT,
    },
    requested_samples_per_model_class: args.samples,
    models: args.models.map((key) => ({ key, ...MODELS[key] })),
    project_fixture_manifest: projectFixtureManifest(),
    classes: args.classes,
    class_semantics: {
      warm: "independent root launch on a warm daemon",
      "restart-cold":
        "independent root launch after daemon restart; persistent storage and host page cache remain warm",
      "post-install-cold-observed":
        "single observation immediately after operator-completed installation",
    },
    note:
      "The downstream repository has no checked-in versioned latency harness; this raw run is directional evidence and must not be called a release gate. This runner never installs or replaces bundles.",
  };
}

function resumeIdentity(metadata) {
  const {
    created_at: _createdAt,
    node_identity: _nodeIdentity,
    audience: _audience,
    last_resumed_at: _lastResumedAt,
    ...identity
  } = metadata;
  return identity;
}

async function main() {
  const scriptHash = sha256Hex(fs.readFileSync(new URL(import.meta.url)));
  const existing = readRecords(samplesPath);
  const priorMetadata = fs.existsSync(metadataPath)
    ? JSON.parse(fs.readFileSync(metadataPath, "utf8"))
    : null;
  if ((existing.length > 0 || priorMetadata) && !args.resume) {
    throw new Error(
      `output already contains a run: ${outputDir}; choose a new --output or pass --resume`,
    );
  }
  if (args.resume) {
    if (!priorMetadata) {
      throw new Error(`cannot resume without metadata: ${metadataPath}`);
    }
    const expected = runMetadata(scriptHash, priorMetadata.created_at);
    if (
      JSON.stringify(resumeIdentity(priorMetadata)) !==
      JSON.stringify(resumeIdentity(expected))
    ) {
      throw new Error(
        "resume refused because the harness, corpus, model, class, or environment identity changed",
      );
    }
  }
  const completed = new Set(existing.map((record) => record.sample_id));
  const metadata = runMetadata(
    scriptHash,
    priorMetadata?.created_at ?? new Date().toISOString(),
  );
  fs.writeFileSync(
    metadataPath,
    `${JSON.stringify(metadata, null, 2)}\n`,
    "utf8",
  );

  if (!args.attachExisting) startNode();
  await discoverAudience();
  const currentNodeIdentity = daemonIdentity();
  if (
    args.resume &&
    priorMetadata?.node_identity &&
    (priorMetadata.node_identity.version !== currentNodeIdentity?.version ||
      priorMetadata.node_identity.revision !== currentNodeIdentity?.revision)
  ) {
    throw new Error(
      "resume refused because the daemon version or revision changed",
    );
  }
  fs.writeFileSync(
    metadataPath,
    `${JSON.stringify(
      {
        ...JSON.parse(fs.readFileSync(metadataPath, "utf8")),
        node_identity: currentNodeIdentity,
        audience,
        ...(args.resume ? { last_resumed_at: new Date().toISOString() } : {}),
      },
      null,
      2,
    )}\n`,
    "utf8",
  );

  for (const className of args.classes) {
    const pending = [];
    for (let ordinal = 1; ordinal <= args.samples; ordinal += 1) {
      for (const modelKey of alternatingModelOrder(ordinal)) {
        const sampleId = `${className}/${modelKey}/${String(ordinal).padStart(3, "0")}`;
        if (!completed.has(sampleId)) pending.push({ sampleId, ordinal, modelKey });
      }
    }
    if (pending.length === 0) {
      console.log(`[matrix] ${className}: already complete`);
      continue;
    }

    if (className === "warm" && !args.probeOnly) {
      for (const modelKey of args.models) {
        const sampleId = `warmup/${modelKey}/${Date.now()}`;
        console.log(`[matrix] ${sampleId}`);
        const warmup = await runProbe({
          sampleId,
          className: "warmup",
          modelKey,
          ordinal: 0,
          counted: false,
          attempt: 1,
        });
        appendJsonl(warmupsPath, warmup);
        if (warmup.outcome !== "success") {
          throw new Error(`warmup failed: ${JSON.stringify(warmup).slice(0, 2_000)}`);
        }
      }
    }

    for (const item of pending) {
      if (className !== "warm") prepareClass(className);
      console.log(
        `[matrix] ${item.sampleId} (${readRecords(samplesPath).length + 1}/` +
          `${args.samples * args.models.length * args.classes.length})`,
      );
      const record = await runProbe({
        ...item,
        className,
        counted: true,
        attempt: 1,
      });
      appendJsonl(samplesPath, record);
      completed.add(item.sampleId);
      writeSummary();
      console.log(
        `[matrix] ${item.sampleId} outcome=${record.outcome} ` +
          `headers=${record.client.headers_ms?.toFixed(1) ?? "n/a"}ms ` +
          `stream_started=${record.client.stream_started_ms?.toFixed(1) ?? "n/a"}ms ` +
          `first_text=${record.client.first_text_ms?.toFixed(1) ?? "n/a"}ms`,
      );
      if (args.probeOnly && record.outcome !== "success") {
        throw new Error(
          `probe failed: ${JSON.stringify(record).slice(0, 2_000)}`,
        );
      }
      await new Promise((resolve) => setTimeout(resolve, 250));
    }
  }
  writeSummary();
}

function stopForSignal(signal) {
  if (stopping) return;
  stopping = true;
  if (args.attachExisting) {
    console.error(`[matrix] received ${signal}; leaving attached daemon running`);
  } else {
    console.error(`[matrix] received ${signal}; stopping local daemon`);
    stopNode({ allowFailure: true });
  }
  process.exit(signal === "SIGINT" ? 130 : 143);
}

process.on("SIGINT", () => stopForSignal("SIGINT"));
process.on("SIGTERM", () => stopForSignal("SIGTERM"));

try {
  await main();
} finally {
  if (!stopping && !args.attachExisting) {
    stopping = true;
    stopNode({ allowFailure: true });
  }
}
