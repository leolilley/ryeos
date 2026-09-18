import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import test from "node:test";
import ts from "typescript";

async function loadTransport() {
  const sourceUrl = new URL("../browser/runtime/transport.ts", import.meta.url);
  const source = await readFile(sourceUrl, "utf8");
  const compiled = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
  }).outputText;
  return import(`data:text/javascript;base64,${Buffer.from(compiled).toString("base64")}`);
}

const eventBatch = (payload) => [{
  engine_seq: 0n,
  event_type: "seat.facet",
  payload,
}];

const digest = (encoded) => createHash("sha256").update(encoded).digest("hex");

test("seat digest vectors commit integers and f64 bits identically", async () => {
  const { encodeJsonBody, encodeSeatPayloadDigest } = await loadTransport();
  const vectors = [
    eventBatch({ key: "float", value: 0.000001 }),
    eventBatch({ key: "thresholds", value: [1e-7, 1.5, -0] }),
    eventBatch({ key: "u64", value: 18_446_744_073_709_551_615n }),
    eventBatch({
      key: "unicode",
      value: { "😀": "é", "𐀀": [null, true], "": { ["__proto__"]: "inert" } },
    }),
    eventBatch({ "é": "😀", a: "𐀀" }),
  ];
  const expected = [
    "84550fb1cf3501cfdde9221974751fe5085b5bc1aed9ce7329e88467aaebc950",
    "6d0e2ab6e500346d7a21fcc6baef8401bfa2b1ce0a13d8816bc53883709bdd3f",
    "b714cf712cbfc663f9eb93095596465167564a7d6bfa738c723376ff5609b209",
    "9f001b4f3bc1e8ccc924b29d7280435dbcefbc1777d5395a6ac080d4494d7835",
    "40442791210ae66e4e67d762a8102f444250935db90a1976d743cb9952eed572",
  ];

  assert.deepEqual(vectors.map((value) => digest(encodeSeatPayloadDigest(value))), expected);
  assert.match(encodeSeatPayloadDigest(vectors[0]), /d3eb0c6f7a0b5ed8d;/);
  assert.equal(encodeJsonBody(-0), "0", "wire-normalized -0 is integer zero");
  assert.equal(encodeSeatPayloadDigest(-0), encodeSeatPayloadDigest(0));
  assert.throws(() => encodeJsonBody("\ud800"), /unpaired high surrogate/);
  assert.throws(() => encodeJsonBody({ ["\udc00"]: true }), /unpaired low surrogate/);
});

test("lossless JSON decoding preserves integer tokens without changing strings or floats", async () => {
  const { decodeJsonBody } = await loadTransport();
  const decoded = decodeJsonBody(`{
    "u64":18446744073709551615,
    "i64":-9223372036854775808,
    "safe":7,
    "float":0.000001,
    "exponent":1e-7,
    "negative_zero":-0,
    "string":"18446744073709551615",
    "nested":[null,{"__proto__":1,"emoji":"😀"}]
  }`);

  assert.equal(decoded.u64, 18_446_744_073_709_551_615n);
  assert.equal(decoded.i64, -9_223_372_036_854_775_808n);
  assert.equal(decoded.safe, 7n);
  assert.equal(decoded.float, 0.000001);
  assert.equal(decoded.exponent, 1e-7);
  assert.ok(Object.is(decoded.negative_zero, -0));
  assert.equal(decoded.string, "18446744073709551615");
  assert.equal(decoded.nested[1].__proto__, 1n);
  assert.equal(Object.getPrototypeOf(decoded), null);
  assert.equal(Object.getPrototypeOf(decoded.nested[1]), null);

  assert.throws(() => decodeJsonBody('{"same":1,"same":2}'), /duplicate object key/);
  assert.throws(() => decodeJsonBody('{"broken":01}'), /trailing|expected/);
  assert.throws(() => decodeJsonBody("1e9999"), /finite f64/);
});

test("HTTP adapters decode daemon integers before they enter the shared model", async () => {
  const { getJson, postEncodedJson } = await loadTransport();
  const originalFetch = globalThis.fetch;
  const bodies = [
    '{"result":{"chain_seq":9007199254740993,"ratio":0.000001}}',
    '{"result":{"event_count":1,"last_engine_seq":18446744073709551615}}',
  ];
  globalThis.fetch = async () => new Response(bodies.shift(), {
    status: 200,
    headers: { "content-type": "application/json" },
  });
  try {
    const read = await getJson("/exact-read");
    assert.equal(read.result.chain_seq, 9_007_199_254_740_993n);
    assert.equal(read.result.ratio, 0.000001);
    const written = await postEncodedJson("/exact-write", "{}");
    assert.equal(written.result.event_count, 1n);
    assert.equal(written.result.last_engine_seq, 18_446_744_073_709_551_615n);
  } finally {
    globalThis.fetch = originalFetch;
  }
});
