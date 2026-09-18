export class UnknownDeliveryError extends Error {
  readonly outcome = "unknown" as const;
}

export class HttpResponseError extends Error {
  constructor(readonly status: number, message: string) {
    super(message);
  }
}

/**
 * Encode the generated Rust/WASM value domain without losing u64 values.
 *
 * `serde-wasm-bindgen` deliberately exposes Rust u64 values as BigInt. Native
 * JSON.stringify rejects them, while converting through Number can silently
 * change an identity. This encoder emits BigInt as an exact JSON integer token
 * and rejects values outside the JSON data model before a request is sent.
 */
export function encodeJsonBody(value: unknown): string {
  return encodeJson(value, false);
}

/** Deterministic JSON key/string spelling; numeric protocol digests use the typed encoder below. */
export function encodeCanonicalJsonBody(value: unknown): string {
  return encodeJson(value, true);
}

/**
 * Canonical typed bytes for the durable seat payload digest.
 *
 * JSON text is not a cross-language canonical number representation: JS and
 * serde_json legitimately spell the same f64 differently around exponent
 * thresholds. This format instead distinguishes exact integers from f64s and
 * commits floats by their IEEE-754 bits. Length framing makes strings and
 * containers unambiguous, and the prefix keeps this protocol out of other hash
 * domains.
 */
export function encodeSeatPayloadDigest(value: unknown): string {
  const active = new Set<object>();

  function encode(current: unknown): string {
    if (current === null) return "n";
    switch (typeof current) {
      case "string": {
        assertValidUnicode(current);
        return `s${encodedByteLength(current)}:${current}`;
      }
      case "boolean": return current ? "t" : "f";
      case "bigint":
        assertJsonIntegerRange(current);
        return `i${current.toString(10)};`;
      case "number": {
        if (!Number.isFinite(current)) throw new TypeError("seat digest contains a non-finite number");
        // This mirrors the transmitted JSON: the body encoder writes -0 and
        // integral f64 values as integer tokens.
        if (Number.isInteger(current)) {
          if (!Number.isSafeInteger(current)) {
            throw new TypeError("seat digest contains an unsafe integer; use BigInt for exact identity");
          }
          return `i${Object.is(current, -0) ? "0" : String(current)};`;
        }
        return `d${float64Bits(current)};`;
      }
      case "object": break;
      default: throw new TypeError(`seat digest contains unsupported ${typeof current}`);
    }

    const object = current as object;
    if (active.has(object)) throw new TypeError("seat digest contains a cycle");
    active.add(object);
    try {
      if (Array.isArray(current)) {
        return `a${current.length}:{${current.map(encode).join("")}}`;
      }
      const prototype = Object.getPrototypeOf(current);
      if (prototype !== Object.prototype && prototype !== null) {
        throw new TypeError("seat digest contains a non-record object");
      }
      const record = current as Record<string, unknown>;
      const keys = Object.keys(record).sort(compareCanonicalKeys);
      return `o${keys.length}:{${keys.map((key) => `${encode(key)}${encode(record[key])}`).join("")}}`;
    } finally {
      active.delete(object);
    }
  }

  return `ryeos.seat.payload.v1|${encode(value)}`;
}

function encodeJson(value: unknown, sortKeys: boolean): string {
  const active = new Set<object>();

  function encode(current: unknown): string {
    if (current === null) return "null";
    switch (typeof current) {
      case "string": {
        assertValidUnicode(current);
        return sortKeys ? encodeCanonicalString(current) : JSON.stringify(current);
      }
      case "boolean": return current ? "true" : "false";
      case "number": {
        if (!Number.isFinite(current)) throw new TypeError("JSON body contains a non-finite number");
        if (Number.isInteger(current) && !Number.isSafeInteger(current)) {
          throw new TypeError("JSON body contains an unsafe integer; use BigInt for exact identity");
        }
        return Object.is(current, -0) ? "0" : String(current);
      }
      case "bigint":
        assertJsonIntegerRange(current);
        return current.toString(10);
      case "object": break;
      default: throw new TypeError(`JSON body contains unsupported ${typeof current}`);
    }

    const object = current as object;
    if (active.has(object)) throw new TypeError("JSON body contains a cycle");
    active.add(object);
    try {
      if (Array.isArray(current)) return `[${current.map(encode).join(",")}]`;
      const prototype = Object.getPrototypeOf(current);
      if (prototype !== Object.prototype && prototype !== null) {
        throw new TypeError("JSON body contains a non-record object");
      }
      const record = current as Record<string, unknown>;
      const keys = Object.keys(record);
      for (const key of keys) assertValidUnicode(key);
      if (sortKeys) keys.sort(compareCanonicalKeys);
      return `{${keys.map((key) => `${sortKeys ? encodeCanonicalString(key) : JSON.stringify(key)}:${encode(record[key])}`).join(",")}}`;
    } finally {
      active.delete(object);
    }
  }

  return encode(value === undefined ? {} : value);
}

/** Rust `String::cmp` orders valid Unicode scalar values by their UTF-8 bytes. */
function compareCanonicalKeys(left: string, right: string): number {
  assertValidUnicode(left);
  assertValidUnicode(right);
  const leftScalars = [...left];
  const rightScalars = [...right];
  const shared = Math.min(leftScalars.length, rightScalars.length);
  for (let index = 0; index < shared; index += 1) {
    const ordering = scalarValue(leftScalars[index]!) - scalarValue(rightScalars[index]!);
    if (ordering !== 0) return ordering;
  }
  return leftScalars.length - rightScalars.length;
}

/** Match `lillux::canonical_json`, including its lowercase non-ASCII escapes. */
function encodeCanonicalString(value: string): string {
  assertValidUnicode(value);
  let encoded = '"';
  for (const scalar of value) {
    const value = scalarValue(scalar);
    switch (value) {
      case 0x22: encoded += '\\"'; break;
      case 0x5c: encoded += '\\\\'; break;
      case 0x08: encoded += "\\b"; break;
      case 0x0c: encoded += "\\f"; break;
      case 0x0a: encoded += "\\n"; break;
      case 0x0d: encoded += "\\r"; break;
      case 0x09: encoded += "\\t"; break;
      default: {
        if (value < 0x20) {
          encoded += `\\u${hex4(value)}`;
        } else if (value <= 0x7f) {
          encoded += scalar;
        } else if (value <= 0xffff) {
          encoded += `\\u${hex4(value)}`;
        } else {
          const supplementary = value - 0x10000;
          encoded += `\\u${hex4(0xd800 + (supplementary >> 10))}`;
          encoded += `\\u${hex4(0xdc00 + (supplementary & 0x3ff))}`;
        }
      }
    }
  }
  return `${encoded}"`;
}

function assertValidUnicode(value: string): void {
  for (let index = 0; index < value.length; index += 1) {
    const unit = value.charCodeAt(index);
    if (unit >= 0xd800 && unit <= 0xdbff) {
      const trail = value.charCodeAt(index + 1);
      if (!(trail >= 0xdc00 && trail <= 0xdfff)) {
        throw new TypeError("JSON contains an unpaired high surrogate");
      }
      index += 1;
    } else if (unit >= 0xdc00 && unit <= 0xdfff) {
      throw new TypeError("JSON contains an unpaired low surrogate");
    }
  }
}

function scalarValue(scalar: string): number {
  return scalar.codePointAt(0) as number;
}

function hex4(value: number): string {
  return value.toString(16).padStart(4, "0");
}

const JSON_INTEGER_MIN = -(1n << 63n);
const JSON_INTEGER_MAX = (1n << 64n) - 1n;

function assertJsonIntegerRange(value: bigint): void {
  if (value < JSON_INTEGER_MIN || value > JSON_INTEGER_MAX) {
    throw new TypeError("JSON integer is outside the daemon i64/u64 value domain");
  }
}

function encodedByteLength(value: string): number {
  return new TextEncoder().encode(value).byteLength;
}

function float64Bits(value: number): string {
  const bytes = new Uint8Array(8);
  new DataView(bytes.buffer).setFloat64(0, value, false);
  return [...bytes].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

/**
 * Parse JSON without first coercing integer tokens through an IEEE-754 Number.
 * Integer tokens become BigInt, while strings, booleans, null and fractional /
 * exponent-form f64 tokens retain their ordinary JSON meaning.
 */
export function decodeJsonBody(encoded: string): unknown {
  let cursor = 0;
  let depth = 0;

  function fail(message: string): never {
    throw new SyntaxError(`invalid JSON at offset ${cursor}: ${message}`);
  }

  function whitespace(): void {
    while (cursor < encoded.length) {
      const unit = encoded.charCodeAt(cursor);
      if (unit !== 0x09 && unit !== 0x0a && unit !== 0x0d && unit !== 0x20) return;
      cursor += 1;
    }
  }

  function literal(token: string, value: unknown): unknown {
    if (!encoded.startsWith(token, cursor)) fail(`expected ${token}`);
    cursor += token.length;
    return value;
  }

  function string(): string {
    const start = cursor;
    cursor += 1;
    while (cursor < encoded.length) {
      const unit = encoded.charCodeAt(cursor);
      if (unit === 0x22) {
        cursor += 1;
        return JSON.parse(encoded.slice(start, cursor)) as string;
      }
      if (unit < 0x20) fail("unescaped control character in string");
      if (unit === 0x5c) {
        cursor += 1;
        const escape = encoded[cursor];
        if (escape === "u") {
          const scalar = encoded.slice(cursor + 1, cursor + 5);
          if (!/^[0-9a-fA-F]{4}$/.test(scalar)) fail("invalid Unicode escape");
          cursor += 5;
          continue;
        }
        if (!escape || !'"\\/bfnrt'.includes(escape)) fail("invalid string escape");
      }
      cursor += 1;
    }
    fail("unterminated string");
  }

  function number(): number | bigint {
    const tail = encoded.slice(cursor);
    const match = /^-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?/.exec(tail);
    if (!match) fail("invalid number");
    const token = match[0];
    cursor += token.length;
    if (!token.includes(".") && !/[eE]/.test(token)) {
      if (token === "-0") return -0;
      const value = BigInt(token);
      if (value < JSON_INTEGER_MIN || value > JSON_INTEGER_MAX) {
        fail("integer is outside the daemon i64/u64 value domain");
      }
      return value;
    }
    const value = Number(token);
    if (!Number.isFinite(value)) fail("number is outside the finite f64 domain");
    return value;
  }

  function array(): unknown[] {
    cursor += 1;
    depth += 1;
    if (depth > 128) fail("container nesting exceeds the daemon JSON limit");
    try {
      whitespace();
      if (encoded[cursor] === "]") {
        cursor += 1;
        return [];
      }
      const result: unknown[] = [];
      while (true) {
        result.push(value());
        whitespace();
        if (encoded[cursor] === "]") {
          cursor += 1;
          return result;
        }
        if (encoded[cursor] !== ",") fail("expected ',' or ']'");
        cursor += 1;
        whitespace();
      }
    } finally {
      depth -= 1;
    }
  }

  function object(): Record<string, unknown> {
    cursor += 1;
    depth += 1;
    if (depth > 128) fail("container nesting exceeds the daemon JSON limit");
    try {
      whitespace();
      if (encoded[cursor] === "}") {
        cursor += 1;
        return Object.create(null) as Record<string, unknown>;
      }
      const result: Record<string, unknown> = Object.create(null) as Record<string, unknown>;
      while (true) {
        if (encoded[cursor] !== '"') fail("expected object key");
        const key = string();
        if (Object.hasOwn(result, key)) fail(`duplicate object key ${JSON.stringify(key)}`);
        whitespace();
        if (encoded[cursor] !== ":") fail("expected ':'");
        cursor += 1;
        result[key] = value();
        whitespace();
        if (encoded[cursor] === "}") {
          cursor += 1;
          return result;
        }
        if (encoded[cursor] !== ",") fail("expected ',' or '}'");
        cursor += 1;
        whitespace();
      }
    } finally {
      depth -= 1;
    }
  }

  function value(): unknown {
    whitespace();
    const token = encoded[cursor];
    if (token === '"') return string();
    if (token === "{") return object();
    if (token === "[") return array();
    if (token === "t") return literal("true", true);
    if (token === "f") return literal("false", false);
    if (token === "n") return literal("null", null);
    if (token === "-" || (token !== undefined && token >= "0" && token <= "9")) return number();
    fail("expected a JSON value");
  }

  const result = value();
  whitespace();
  if (cursor !== encoded.length) fail("trailing input");
  return result;
}

export async function getJson(url: string, signal?: AbortSignal): Promise<unknown> {
  const response = await fetch(url, {
    credentials: "same-origin",
    ...(signal ? { signal } : {}),
  });
  if (!response.ok) throw new HttpResponseError(response.status, `${url}: ${response.status} ${await response.text()}`);
  return decodeJsonBody(await response.text());
}

export async function postJson(url: string, body: unknown, signal?: AbortSignal): Promise<unknown> {
  return postEncodedJson(url, encodeJsonBody(body), signal);
}

/** Send bytes that have already been validated and measured by the caller. */
export async function postEncodedJson(url: string, encodedBody: string, signal?: AbortSignal): Promise<unknown> {
  let response: Response;
  try {
    response = await fetch(url, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: encodedBody,
      credentials: "same-origin",
      ...(signal ? { signal } : {}),
    });
  } catch (error) {
    throw new UnknownDeliveryError(`${url}: delivery outcome is unknown: ${errorMessage(error)}`);
  }
  if (!response.ok) throw new HttpResponseError(response.status, `${url}: ${response.status} ${await response.text()}`);
  try {
    return decodeJsonBody(await response.text());
  } catch (error) {
    throw new UnknownDeliveryError(`${url}: response outcome is unknown: ${errorMessage(error)}`);
  }
}

export function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
