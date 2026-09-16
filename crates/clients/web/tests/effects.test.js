import assert from "node:assert/strict";
import test from "node:test";

import { failedResultFor, runEffect } from "../pkg/ryeos_effects.js";

const effect = {
  id: 9,
  kind: {
    type: "invoke_binding",
    request: {
      binding_digest: "sha256:test",
      coordinate: { kind: "affordance", view_ref: "view:test/x", affordance_id: "go" },
      payload: { kind: "selection", record: { id: "x" } },
    },
    request_bounds: { max_request_bytes: 4096, max_input_bytes: 128 },
  },
};

test("binding invocation preserves the closed request and success", async () => {
  const prior = globalThis.fetch;
  let posted;
  globalThis.fetch = async (_url, init) => {
    posted = JSON.parse(init.body);
    return new Response(JSON.stringify({ result: { changed: true } }), { status: 200 });
  };
  try {
    const result = await runEffect(effect);
    assert.deepEqual(posted, effect.kind.request);
    assert.equal(result.ok, true);
    assert.equal(result.kind, "binding_invoked");
  } finally {
    globalThis.fetch = prior;
  }
});

test("local bounds and received HTTP refusal are definite", async () => {
  const tooSmall = structuredClone(effect);
  tooSmall.kind.request_bounds.max_request_bytes = 1;
  const boundsError = await runEffect(tooSmall).catch((error) => error);
  assert.equal(failedResultFor(tooSmall, boundsError).error.outcome, "refused");

  const prior = globalThis.fetch;
  globalThis.fetch = async () => new Response("forbidden", { status: 403 });
  try {
    const httpError = await runEffect(effect).catch((error) => error);
    assert.equal(failedResultFor(effect, httpError).error.outcome, "refused");
  } finally {
    globalThis.fetch = prior;
  }
});

test("contact failure after invocation dispatch is outcome unknown", async () => {
  const prior = globalThis.fetch;
  globalThis.fetch = async () => {
    throw new TypeError("connection reset");
  };
  try {
    const error = await runEffect(effect).catch((cause) => cause);
    const result = failedResultFor(effect, error);
    assert.equal(result.error.outcome, "unknown");
    assert.equal(result.error.retryable, false);
  } finally {
    globalThis.fetch = prior;
  }
});

test("session replacement redeems the successor in the same tab", async () => {
  const prior = globalThis.location;
  let assigned;
  globalThis.location = {
    href: "https://node.example/ui",
    origin: "https://node.example",
    assign(url) {
      assigned = url;
    },
  };
  try {
    const replacement = {
      id: 10,
      kind: {
        type: "replace_session",
        session_id: "successor-session",
        launch_url: "/ui/launch/one-shot-token",
      },
    };
    const result = await runEffect(replacement);
    assert.equal(assigned, replacement.kind.launch_url);
    assert.deepEqual(result, {
      id: 10,
      ok: true,
      kind: "browser_only",
      data: null,
    });
  } finally {
    if (prior === undefined) delete globalThis.location;
    else globalThis.location = prior;
  }
});

test("session replacement refuses cross-origin and non-launch paths", async () => {
  const prior = globalThis.location;
  globalThis.location = {
    href: "https://node.example/ui",
    origin: "https://node.example",
    assign() { throw new Error("must not navigate"); },
  };
  try {
    for (const launch_url of ["https://evil.example/ui/launch/token", "/ui/not-launch/token"]) {
      const rejected = {
        id: 11,
        kind: { type: "replace_session", session_id: "successor", launch_url },
      };
      const error = await runEffect(rejected).catch((cause) => cause);
      const result = failedResultFor(rejected, error);
      assert.equal(result.ok, false);
    }
  } finally {
    if (prior === undefined) delete globalThis.location;
    else globalThis.location = prior;
  }
});

test("committed invocation with an unreadable response is outcome unknown", async () => {
  const prior = globalThis.fetch;
  globalThis.fetch = async () => new Response("not-json", { status: 200 });
  try {
    const error = await runEffect(effect).catch((cause) => cause);
    const result = failedResultFor(effect, error);
    assert.equal(result.ok, false);
    assert.equal(result.error.outcome, "unknown");
  } finally {
    globalThis.fetch = prior;
  }
});
