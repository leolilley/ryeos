//#region browser/runtime/commit.ts
/**
* Serialize all Rust mutations while allowing visual generations to coalesce.
*
* Effect registration is deliberately synchronous with envelope acceptance.
* Svelte flush timing therefore cannot drop, merge, repeat or delay discovery
* of an effect emitted by an intermediate Rust generation.
*/
function createCommitRuntime(options) {
	const mutationQueue = [];
	const startedEffects = /* @__PURE__ */ new Set();
	const scheduleRender = options.scheduleRender ?? ((render) => queueMicrotask(render));
	let draining = false;
	let closed = false;
	let renderScheduled = false;
	let renderRunning = false;
	let pendingEffects = 0;
	let latestRenderable = null;
	let acceptedSequence = 0;
	let settledSequence = 0;
	const idleWaiters = [];
	function enqueueMutation(mutation) {
		if (closed) return;
		mutationQueue.push(mutation);
		acceptedSequence += 1;
		drainMutations();
	}
	async function drainMutations() {
		if (draining) return;
		draining = true;
		try {
			while (mutationQueue.length > 0) {
				const mutation = mutationQueue.shift();
				if (!mutation) continue;
				acceptEnvelope(mutation());
				settledSequence += 1;
			}
		} finally {
			draining = false;
			resolveIdleWaiters();
			if (mutationQueue.length > 0) drainMutations();
		}
	}
	function acceptEnvelope(envelope) {
		for (const effect of envelope.effects) {
			if (startedEffects.has(effect.id)) continue;
			startedEffects.add(effect.id);
			pendingEffects += 1;
			let delivered = false;
			options.startEffect(effect).then((result) => deliverEffectResult(() => result), (error) => deliverEffectResult(() => options.failedEffectResult(effect, error)));
			function deliverEffectResult(result) {
				if (delivered) return;
				delivered = true;
				pendingEffects -= 1;
				if (closed) return;
				const resolved = result();
				enqueueMutation(() => options.applyEffectResult(resolved));
			}
		}
		latestRenderable = envelope;
		requestRender();
	}
	function requestRender() {
		if (renderScheduled || renderRunning) return;
		renderScheduled = true;
		scheduleRender(() => {
			flushRender();
		});
	}
	async function flushRender() {
		if (renderRunning) return;
		renderScheduled = false;
		if (closed) return;
		renderRunning = true;
		try {
			const envelope = latestRenderable;
			latestRenderable = null;
			if (envelope) await options.render(envelope);
		} finally {
			renderRunning = false;
			if (latestRenderable) requestRender();
			resolveIdleWaiters();
		}
	}
	function resolveIdleWaiters() {
		if (draining || mutationQueue.length > 0 || pendingEffects > 0 || renderScheduled || renderRunning || latestRenderable) return;
		for (let index = idleWaiters.length - 1; index >= 0; index -= 1) {
			const waiter = idleWaiters[index];
			if (waiter && settledSequence >= waiter.sequence) {
				idleWaiters.splice(index, 1);
				waiter.resolve();
			}
		}
	}
	return {
		enqueueEvent(event) {
			enqueueMutation(() => options.reduce(event));
		},
		enqueueEnvelope(envelope) {
			enqueueMutation(() => envelope);
		},
		commitMutation(mutation) {
			enqueueMutation(mutation);
		},
		close() {
			if (closed) return;
			closed = true;
			mutationQueue.length = 0;
			latestRenderable = null;
			for (const waiter of idleWaiters.splice(0)) waiter.resolve();
		},
		idle() {
			if (closed) return Promise.resolve();
			const sequence = acceptedSequence;
			if (!draining && mutationQueue.length === 0 && pendingEffects === 0 && !renderScheduled && !renderRunning && !latestRenderable && settledSequence >= sequence) return Promise.resolve();
			return new Promise((resolve) => idleWaiters.push({
				sequence,
				resolve
			}));
		}
	};
}
//#endregion
//#region browser/runtime/transport.ts
var UnknownDeliveryError = class extends Error {
	outcome = "unknown";
};
var HttpResponseError = class extends Error {
	status;
	constructor(status, message) {
		super(message);
		this.status = status;
	}
};
/**
* Encode the generated Rust/WASM value domain without losing u64 values.
*
* `serde-wasm-bindgen` deliberately exposes Rust u64 values as BigInt. Native
* JSON.stringify rejects them, while converting through Number can silently
* change an identity. This encoder emits BigInt as an exact JSON integer token
* and rejects values outside the JSON data model before a request is sent.
*/
function encodeJsonBody(value) {
	return encodeJson(value, false);
}
/** Deterministic JSON key/string spelling; numeric protocol digests use the typed encoder below. */
function encodeCanonicalJsonBody(value) {
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
function encodeSeatPayloadDigest(value) {
	const active = /* @__PURE__ */ new Set();
	function encode(current) {
		if (current === null) return "n";
		switch (typeof current) {
			case "string":
				assertValidUnicode(current);
				return `s${encodedByteLength$1(current)}:${current}`;
			case "boolean": return current ? "t" : "f";
			case "bigint":
				assertJsonIntegerRange(current);
				return `i${current.toString(10)};`;
			case "number":
				if (!Number.isFinite(current)) throw new TypeError("seat digest contains a non-finite number");
				if (Number.isInteger(current)) {
					if (!Number.isSafeInteger(current)) throw new TypeError("seat digest contains an unsafe integer; use BigInt for exact identity");
					return `i${Object.is(current, -0) ? "0" : String(current)};`;
				}
				return `d${float64Bits(current)};`;
			case "object": break;
			default: throw new TypeError(`seat digest contains unsupported ${typeof current}`);
		}
		const object = current;
		if (active.has(object)) throw new TypeError("seat digest contains a cycle");
		active.add(object);
		try {
			if (Array.isArray(current)) return `a${current.length}:{${current.map(encode).join("")}}`;
			const prototype = Object.getPrototypeOf(current);
			if (prototype !== Object.prototype && prototype !== null) throw new TypeError("seat digest contains a non-record object");
			const record = current;
			const keys = Object.keys(record).sort(compareCanonicalKeys);
			return `o${keys.length}:{${keys.map((key) => `${encode(key)}${encode(record[key])}`).join("")}}`;
		} finally {
			active.delete(object);
		}
	}
	return `ryeos.seat.payload.v1|${encode(value)}`;
}
function encodeJson(value, sortKeys) {
	const active = /* @__PURE__ */ new Set();
	function encode(current) {
		if (current === null) return "null";
		switch (typeof current) {
			case "string":
				assertValidUnicode(current);
				return sortKeys ? encodeCanonicalString(current) : JSON.stringify(current);
			case "boolean": return current ? "true" : "false";
			case "number":
				if (!Number.isFinite(current)) throw new TypeError("JSON body contains a non-finite number");
				if (Number.isInteger(current) && !Number.isSafeInteger(current)) throw new TypeError("JSON body contains an unsafe integer; use BigInt for exact identity");
				return Object.is(current, -0) ? "0" : String(current);
			case "bigint":
				assertJsonIntegerRange(current);
				return current.toString(10);
			case "object": break;
			default: throw new TypeError(`JSON body contains unsupported ${typeof current}`);
		}
		const object = current;
		if (active.has(object)) throw new TypeError("JSON body contains a cycle");
		active.add(object);
		try {
			if (Array.isArray(current)) return `[${current.map(encode).join(",")}]`;
			const prototype = Object.getPrototypeOf(current);
			if (prototype !== Object.prototype && prototype !== null) throw new TypeError("JSON body contains a non-record object");
			const record = current;
			const keys = Object.keys(record);
			for (const key of keys) assertValidUnicode(key);
			if (sortKeys) keys.sort(compareCanonicalKeys);
			return `{${keys.map((key) => `${sortKeys ? encodeCanonicalString(key) : JSON.stringify(key)}:${encode(record[key])}`).join(",")}}`;
		} finally {
			active.delete(object);
		}
	}
	return encode(value === void 0 ? {} : value);
}
/** Rust `String::cmp` orders valid Unicode scalar values by their UTF-8 bytes. */
function compareCanonicalKeys(left, right) {
	assertValidUnicode(left);
	assertValidUnicode(right);
	const leftScalars = [...left];
	const rightScalars = [...right];
	const shared = Math.min(leftScalars.length, rightScalars.length);
	for (let index = 0; index < shared; index += 1) {
		const ordering = scalarValue(leftScalars[index]) - scalarValue(rightScalars[index]);
		if (ordering !== 0) return ordering;
	}
	return leftScalars.length - rightScalars.length;
}
/** Match `lillux::canonical_json`, including its lowercase non-ASCII escapes. */
function encodeCanonicalString(value) {
	assertValidUnicode(value);
	let encoded = "\"";
	for (const scalar of value) {
		const value = scalarValue(scalar);
		switch (value) {
			case 34:
				encoded += "\\\"";
				break;
			case 92:
				encoded += "\\\\";
				break;
			case 8:
				encoded += "\\b";
				break;
			case 12:
				encoded += "\\f";
				break;
			case 10:
				encoded += "\\n";
				break;
			case 13:
				encoded += "\\r";
				break;
			case 9:
				encoded += "\\t";
				break;
			default: if (value < 32) encoded += `\\u${hex4(value)}`;
			else if (value <= 127) encoded += scalar;
			else if (value <= 65535) encoded += `\\u${hex4(value)}`;
			else {
				const supplementary = value - 65536;
				encoded += `\\u${hex4(55296 + (supplementary >> 10))}`;
				encoded += `\\u${hex4(56320 + (supplementary & 1023))}`;
			}
		}
	}
	return `${encoded}"`;
}
function assertValidUnicode(value) {
	for (let index = 0; index < value.length; index += 1) {
		const unit = value.charCodeAt(index);
		if (unit >= 55296 && unit <= 56319) {
			const trail = value.charCodeAt(index + 1);
			if (!(trail >= 56320 && trail <= 57343)) throw new TypeError("JSON contains an unpaired high surrogate");
			index += 1;
		} else if (unit >= 56320 && unit <= 57343) throw new TypeError("JSON contains an unpaired low surrogate");
	}
}
function scalarValue(scalar) {
	return scalar.codePointAt(0);
}
function hex4(value) {
	return value.toString(16).padStart(4, "0");
}
var JSON_INTEGER_MIN = -(1n << 63n);
var JSON_INTEGER_MAX = (1n << 64n) - 1n;
function assertJsonIntegerRange(value) {
	if (value < JSON_INTEGER_MIN || value > JSON_INTEGER_MAX) throw new TypeError("JSON integer is outside the daemon i64/u64 value domain");
}
function encodedByteLength$1(value) {
	return new TextEncoder().encode(value).byteLength;
}
function float64Bits(value) {
	const bytes = /* @__PURE__ */ new Uint8Array(8);
	new DataView(bytes.buffer).setFloat64(0, value, false);
	return [...bytes].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}
/**
* Parse JSON without first coercing integer tokens through an IEEE-754 Number.
* Integer tokens become BigInt, while strings, booleans, null and fractional /
* exponent-form f64 tokens retain their ordinary JSON meaning.
*/
function decodeJsonBody(encoded) {
	let cursor = 0;
	let depth = 0;
	function fail(message) {
		throw new SyntaxError(`invalid JSON at offset ${cursor}: ${message}`);
	}
	function whitespace() {
		while (cursor < encoded.length) {
			const unit = encoded.charCodeAt(cursor);
			if (unit !== 9 && unit !== 10 && unit !== 13 && unit !== 32) return;
			cursor += 1;
		}
	}
	function literal(token, value) {
		if (!encoded.startsWith(token, cursor)) fail(`expected ${token}`);
		cursor += token.length;
		return value;
	}
	function string() {
		const start = cursor;
		cursor += 1;
		while (cursor < encoded.length) {
			const unit = encoded.charCodeAt(cursor);
			if (unit === 34) {
				cursor += 1;
				return JSON.parse(encoded.slice(start, cursor));
			}
			if (unit < 32) fail("unescaped control character in string");
			if (unit === 92) {
				cursor += 1;
				const escape = encoded[cursor];
				if (escape === "u") {
					const scalar = encoded.slice(cursor + 1, cursor + 5);
					if (!/^[0-9a-fA-F]{4}$/.test(scalar)) fail("invalid Unicode escape");
					cursor += 5;
					continue;
				}
				if (!escape || !"\"\\/bfnrt".includes(escape)) fail("invalid string escape");
			}
			cursor += 1;
		}
		fail("unterminated string");
	}
	function number() {
		const tail = encoded.slice(cursor);
		const match = /^-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?/.exec(tail);
		if (!match) fail("invalid number");
		const token = match[0];
		cursor += token.length;
		if (!token.includes(".") && !/[eE]/.test(token)) {
			if (token === "-0") return -0;
			const value = BigInt(token);
			if (value < JSON_INTEGER_MIN || value > JSON_INTEGER_MAX) fail("integer is outside the daemon i64/u64 value domain");
			return value;
		}
		const value = Number(token);
		if (!Number.isFinite(value)) fail("number is outside the finite f64 domain");
		return value;
	}
	function array() {
		cursor += 1;
		depth += 1;
		if (depth > 128) fail("container nesting exceeds the daemon JSON limit");
		try {
			whitespace();
			if (encoded[cursor] === "]") {
				cursor += 1;
				return [];
			}
			const result = [];
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
	function object() {
		cursor += 1;
		depth += 1;
		if (depth > 128) fail("container nesting exceeds the daemon JSON limit");
		try {
			whitespace();
			if (encoded[cursor] === "}") {
				cursor += 1;
				return Object.create(null);
			}
			const result = Object.create(null);
			while (true) {
				if (encoded[cursor] !== "\"") fail("expected object key");
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
	function value() {
		whitespace();
		const token = encoded[cursor];
		if (token === "\"") return string();
		if (token === "{") return object();
		if (token === "[") return array();
		if (token === "t") return literal("true", true);
		if (token === "f") return literal("false", false);
		if (token === "n") return literal("null", null);
		if (token === "-" || token !== void 0 && token >= "0" && token <= "9") return number();
		fail("expected a JSON value");
	}
	const result = value();
	whitespace();
	if (cursor !== encoded.length) fail("trailing input");
	return result;
}
async function getJson(url, signal) {
	const response = await fetch(url, {
		credentials: "same-origin",
		...signal ? { signal } : {}
	});
	if (!response.ok) throw new HttpResponseError(response.status, `${url}: ${response.status} ${await response.text()}`);
	return decodeJsonBody(await response.text());
}
async function postJson(url, body, signal) {
	return postEncodedJson(url, encodeJsonBody(body), signal);
}
/** Send bytes that have already been validated and measured by the caller. */
async function postEncodedJson(url, encodedBody, signal) {
	let response;
	try {
		response = await fetch(url, {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: encodedBody,
			credentials: "same-origin",
			...signal ? { signal } : {}
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
function errorMessage(error) {
	return error instanceof Error ? error.message : String(error);
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/shared/utils.js
var is_array = Array.isArray;
var index_of = Array.prototype.indexOf;
var includes = Array.prototype.includes;
var array_from = Array.from;
var define_property = Object.defineProperty;
var get_descriptor = Object.getOwnPropertyDescriptor;
var get_descriptors = Object.getOwnPropertyDescriptors;
var object_prototype = Object.prototype;
var array_prototype = Array.prototype;
var get_prototype_of = Object.getPrototypeOf;
var is_extensible = Object.isExtensible;
var noop = () => {};
/** @param {Array<() => void>} arr */
function run_all(arr) {
	for (var i = 0; i < arr.length; i++) arr[i]();
}
/**
* TODO replace with Promise.withResolvers once supported widely enough
* @template [T=void]
*/
function deferred() {
	/** @type {(value: T) => void} */
	var resolve;
	/** @type {(reason: any) => void} */
	var reject;
	return {
		promise: new Promise((res, rej) => {
			resolve = res;
			reject = rej;
		}),
		resolve,
		reject
	};
}
var CLEAN = 1024;
var DIRTY = 2048;
var MAYBE_DIRTY = 4096;
var INERT = 8192;
var DESTROYED = 16384;
/** Set once a reaction has run for the first time */
var REACTION_RAN = 32768;
/** Effect is in the process of getting destroyed. Can be observed in child teardown functions */
var DESTROYING = 1 << 25;
/**
* 'Transparent' effects do not create a transition boundary.
* This is on a block effect 99% of the time but may also be on a branch effect if its parent block effect was pruned
*/
var EFFECT_TRANSPARENT = 65536;
var EFFECT_PRESERVED = 1 << 19;
var USER_EFFECT = 1 << 20;
var EFFECT_OFFSCREEN = 1 << 25;
/**
* Tells that we marked this derived and its reactions as visited during the "mark as (maybe) dirty"-phase.
* Will be lifted during execution of the derived and during checking its dirty state (both are necessary
* because a derived might be checked but not executed). This is a pure performance optimization flag and
* should not be used for any other purpose!
*/
var WAS_MARKED = 65536;
var REACTION_IS_UPDATING = 1 << 21;
var ASYNC = 1 << 22;
var ERROR_VALUE = 1 << 23;
var STATE_SYMBOL = Symbol("$state");
/** Marks component export objects, so that `proxy(...)` leaves them untouched */
var COMPONENT_SYMBOL = Symbol("component");
var LEGACY_PROPS = Symbol("legacy props");
var LOADING_ATTR_SYMBOL = Symbol("");
var ATTRIBUTES_CACHE = Symbol("attributes");
var CLASS_CACHE = Symbol("class");
var STYLE_CACHE = Symbol("style");
var TEXT_CACHE = Symbol("text");
var FORM_RESET_HANDLER = Symbol("form reset");
/** allow users to ignore aborted signal errors if `reason.name === 'StaleReactionError` */
var STALE_REACTION = new class StaleReactionError extends Error {
	name = "StaleReactionError";
	message = "The reaction that called `getAbortSignal()` was re-run or destroyed";
}();
var IS_XHTML = !!globalThis.document?.contentType && /* @__PURE__ */ globalThis.document.contentType.includes("xml");
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/constants.js
var HYDRATION_ERROR = {};
var UNINITIALIZED = Symbol("uninitialized");
var NAMESPACE_HTML = "http://www.w3.org/1999/xhtml";
/**
* Reading a derived belonging to a now-destroyed effect may result in stale values
*/
function derived_inert() {
	console.warn(`https://svelte.dev/e/derived_inert`);
}
/**
* Hydration failed because the initial UI does not match what was rendered on the server. The error occurred near %location%
* @param {string | undefined | null} [location]
*/
function hydration_mismatch(location) {
	console.warn(`https://svelte.dev/e/hydration_mismatch`);
}
/**
* A `<svelte:boundary>` `reset` function only resets the boundary the first time it is called
*/
function svelte_boundary_reset_noop() {
	console.warn(`https://svelte.dev/e/svelte_boundary_reset_noop`);
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/dom/hydration.js
/** @import { TemplateNode } from '#client' */
/**
* Use this variable to guard everything related to hydration code so it can be treeshaken out
* if the user doesn't use the `hydrate` method and these code paths are therefore not needed.
*/
var hydrating = false;
/** @param {boolean} value */
function set_hydrating(value) {
	hydrating = value;
}
/**
* The node that is currently being hydrated. This starts out as the first node inside the opening
* <!--[--> comment, and updates each time a component calls `$.child(...)` or `$.sibling(...)`.
* When entering a block (e.g. `{#if ...}`), `hydrate_node` is the block opening comment; by the
* time we leave the block it is the closing comment, which serves as the block's anchor.
* @type {TemplateNode}
*/
var hydrate_node;
/** @param {TemplateNode | null} node */
function set_hydrate_node(node) {
	if (node === null) {
		hydration_mismatch();
		throw HYDRATION_ERROR;
	}
	return hydrate_node = node;
}
function hydrate_next() {
	return set_hydrate_node(/* @__PURE__ */ get_next_sibling(hydrate_node));
}
/** @param {TemplateNode} node */
function reset(node) {
	if (!hydrating) return;
	if (/* @__PURE__ */ get_next_sibling(hydrate_node) !== null) {
		hydration_mismatch();
		throw HYDRATION_ERROR;
	}
	hydrate_node = node;
}
function next(count = 1) {
	if (hydrating) {
		var i = count;
		var node = hydrate_node;
		while (i--) node = /* @__PURE__ */ get_next_sibling(node);
		hydrate_node = node;
	}
}
/**
* Skips or removes (depending on {@link remove}) all nodes starting at `hydrate_node` up until the next hydration end comment
* @param {boolean} remove
*/
function skip_nodes(remove = true) {
	var depth = 0;
	var node = hydrate_node;
	while (true) {
		if (node.nodeType === 8) {
			var data = node.data;
			if (data === "]") {
				if (depth === 0) return node;
				depth -= 1;
			} else if (data === "[" || data === "[!" || data[0] === "[" && !isNaN(Number(data.slice(1)))) depth += 1;
		}
		var next = /* @__PURE__ */ get_next_sibling(node);
		if (remove) node.remove();
		node = next;
	}
}
/**
*
* @param {TemplateNode} node
*/
function read_hydration_instruction(node) {
	if (!node || node.nodeType !== 8) {
		hydration_mismatch();
		throw HYDRATION_ERROR;
	}
	return node.data;
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/reactivity/equality.js
/** @import { Equals } from '#client' */
/** @type {Equals} */
function equals(value) {
	return value === this.v;
}
/**
* @param {unknown} a
* @param {unknown} b
* @returns {boolean}
*/
function safe_not_equal(a, b) {
	return a != a ? b == b : a !== b || a !== null && typeof a === "object" || typeof a === "function";
}
/** @type {Equals} */
function safe_equals(value) {
	return !safe_not_equal(value, this.v);
}
/**
* `%name%(...)` can only be used during component initialisation
* @param {string} name
* @returns {never}
*/
function lifecycle_outside_component(name) {
	throw new Error(`https://svelte.dev/e/lifecycle_outside_component`);
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/errors.js
/**
* Cannot create a `$derived(...)` with an `await` expression outside of an effect tree
* @returns {never}
*/
function async_derived_orphan() {
	throw new Error(`https://svelte.dev/e/async_derived_orphan`);
}
/**
* Keyed each block has duplicate key `%value%` at indexes %a% and %b%
* @param {string} a
* @param {string} b
* @param {string | undefined | null} [value]
* @returns {never}
*/
function each_key_duplicate(a, b, value) {
	throw new Error(`https://svelte.dev/e/each_key_duplicate`);
}
/**
* `%rune%` cannot be used inside an effect cleanup function
* @param {string} rune
* @returns {never}
*/
function effect_in_teardown(rune) {
	throw new Error(`https://svelte.dev/e/effect_in_teardown`);
}
/**
* Effect cannot be created inside a `$derived` value that was not itself created inside an effect
* @returns {never}
*/
function effect_in_unowned_derived() {
	throw new Error(`https://svelte.dev/e/effect_in_unowned_derived`);
}
/**
* `%rune%` can only be used inside an effect (e.g. during component initialisation)
* @param {string} rune
* @returns {never}
*/
function effect_orphan(rune) {
	throw new Error(`https://svelte.dev/e/effect_orphan`);
}
/**
* Maximum update depth exceeded. This typically indicates that an effect reads and writes the same piece of state
* @returns {never}
*/
function effect_update_depth_exceeded() {
	throw new Error(`https://svelte.dev/e/effect_update_depth_exceeded`);
}
/**
* Cannot do `bind:%key%={undefined}` when `%key%` has a fallback value
* @param {string} key
* @returns {never}
*/
function props_invalid_value(key) {
	throw new Error(`https://svelte.dev/e/props_invalid_value`);
}
/**
* `setContext` must be called when a component first initializes, not in a subsequent effect or after an `await` expression
* @returns {never}
*/
function set_context_after_init() {
	throw new Error(`https://svelte.dev/e/set_context_after_init`);
}
/**
* Property descriptors defined on `$state` objects must contain `value` and always be `enumerable`, `configurable` and `writable`.
* @returns {never}
*/
function state_descriptors_fixed() {
	throw new Error(`https://svelte.dev/e/state_descriptors_fixed`);
}
/**
* Cannot set prototype of `$state` object
* @returns {never}
*/
function state_prototype_fixed() {
	throw new Error(`https://svelte.dev/e/state_prototype_fixed`);
}
/**
* Updating state inside `$derived(...)`, `$inspect(...)` or a template expression is forbidden. If the value should not be reactive, declare it without `$state`
* @returns {never}
*/
function state_unsafe_mutation() {
	throw new Error(`https://svelte.dev/e/state_unsafe_mutation`);
}
/**
* A `<svelte:boundary>` `reset` function cannot be called while an error is still being handled
* @returns {never}
*/
function svelte_boundary_reset_onerror() {
	throw new Error(`https://svelte.dev/e/svelte_boundary_reset_onerror`);
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/flags/index.js
/** True if experimental.async=true */
var async_mode_flag = false;
/** True if we're not certain that we only have Svelte 5 code in the compilation */
var legacy_mode_flag = false;
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/shared/context.js
/**
* @typedef {{ p: Context | null, c: Map<unknown, unknown> | null }} Context
*/
/**
* @param {Context} context
* @returns {Map<unknown, unknown> | null}
*/
function get_parent_context(context) {
	let parent = context.p;
	while (parent !== null && parent.c === null) parent = parent.p;
	return parent?.c ?? null;
}
/**
* @param {Context | null} context
* @param {string} name
* @returns {Map<unknown, unknown>}
*/
function get_or_init_context_map(context, name) {
	if (context === null) lifecycle_outside_component(name);
	return context.c ??= new Map(get_parent_context(context) || void 0);
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/context.js
/** @import { ComponentContext, DevStackEntry, Effect } from '#client' */
/** @type {ComponentContext | null} */
var component_context = null;
/** @param {ComponentContext | null} context */
function set_component_context(context) {
	component_context = context;
}
/**
* Retrieves the context set with the specified `key` in the current component or any of its
* ancestors. If multiple components set the same key, the value from the closest one is returned.
* A `setContext` call in the current component is only visible to `getContext` calls that run after it.
* Must be called during component initialisation.
*
* [`createContext`](https://svelte.dev/docs/svelte/svelte#createContext) is a type-safe alternative.
*
* @template T
* @param {any} key
* @returns {T}
*/
function getContext(key) {
	return get_or_init_context_map(component_context, "getContext").get(key);
}
/**
* Associates an arbitrary `context` object with the current component and the specified `key`
* and returns that object. The context is then available to the component itself and all of its
* descendants (including slotted content) with `getContext`.
*
* Like lifecycle functions, this must be called during component initialisation.
*
* [`createContext`](https://svelte.dev/docs/svelte/svelte#createContext) is a type-safe alternative.
*
* @template T
* @param {any} key
* @param {T} context
* @returns {T}
*/
function setContext(key, context) {
	const context_map = get_or_init_context_map(component_context, "setContext");
	if (async_mode_flag) {
		var flags = active_effect.f;
		if (!(!active_reaction && (flags & 32) !== 0 && !component_context.i)) set_context_after_init();
	}
	context_map.set(key, context);
	return context;
}
/**
* @param {Record<string, unknown>} props
* @param {any} runes
* @param {Function} [fn]
* @returns {void}
*/
function push(props, runes = false, fn) {
	component_context = {
		p: component_context,
		i: false,
		c: null,
		e: null,
		s: props,
		x: null,
		r: active_effect,
		l: legacy_mode_flag && !runes ? {
			s: null,
			u: null,
			$: []
		} : null
	};
}
/**
* @template {Record<string, any>} T
* @param {T} [component]
* @returns {T}
*/
function pop(component) {
	var context = component_context;
	var effects = context.e;
	if (effects !== null) {
		context.e = null;
		for (var fn of effects) create_user_effect(fn);
	}
	if (component !== void 0) context.x = component;
	context.i = true;
	component_context = context.p;
	return mark_as_component(component);
}
/**
* Add a symbol to the object (or create one if undefined) to mark it as a component so it isn't proxified.
* @param {any} component
*/
function mark_as_component(component = {}) {
	define_property(component, COMPONENT_SYMBOL, { value: true });
	return component;
}
/** @returns {boolean} */
function is_runes() {
	return !legacy_mode_flag || component_context !== null && component_context.l === null;
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/dom/task.js
/** @type {Array<() => void>} */
var micro_tasks = [];
function run_micro_tasks() {
	var tasks = micro_tasks;
	micro_tasks = [];
	run_all(tasks);
}
/**
* @param {() => void} fn
*/
function queue_micro_task(fn) {
	if (micro_tasks.length === 0 && !is_flushing_sync) {
		var tasks = micro_tasks;
		queueMicrotask(() => {
			if (tasks === micro_tasks) run_micro_tasks();
		});
	}
	micro_tasks.push(fn);
}
/**
* Synchronously run any queued tasks.
*/
function flush_tasks() {
	while (micro_tasks.length > 0) run_micro_tasks();
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/reactivity/status.js
/** @import { Derived, Signal } from '#client' */
var STATUS_MASK = ~(DIRTY | MAYBE_DIRTY | CLEAN);
/**
* @param {Signal} signal
* @param {number} status
*/
function set_signal_status(signal, status) {
	signal.f = signal.f & STATUS_MASK | status;
}
/**
* Set a derived's status to CLEAN or MAYBE_DIRTY based on its connection state.
* @param {Derived} derived
*/
function update_derived_status(derived) {
	if ((derived.f & 512) !== 0 || derived.deps === null) set_signal_status(derived, CLEAN);
	else set_signal_status(derived, MAYBE_DIRTY);
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/reactivity/utils.js
/** @import { Derived, Effect, Value } from '#client' */
/**
* @param {Value[] | null} deps
*/
function clear_marked(deps) {
	if (deps === null) return;
	for (const dep of deps) {
		if ((dep.f & 2) === 0 || (dep.f & 65536) === 0) continue;
		dep.f ^= WAS_MARKED;
		clear_marked(
			/** @type {Derived} */
			dep.deps
		);
	}
}
/**
* @param {Effect} effect
* @param {Set<Effect>} dirty_effects
* @param {Set<Effect>} maybe_dirty_effects
*/
function defer_effect(effect, dirty_effects, maybe_dirty_effects) {
	if ((effect.f & 2048) !== 0) dirty_effects.add(effect);
	else if ((effect.f & 4096) !== 0) maybe_dirty_effects.add(effect);
	clear_marked(effect.deps);
	set_signal_status(effect, CLEAN);
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/reactivity/store.js
/**
* We set this to `true` when updating a store so that we correctly
* schedule effects if the update takes place inside a `$:` effect
*/
var legacy_is_updating_store = false;
/**
* Whether or not the prop currently being read is a store binding, as in
* `<Child bind:x={$y} />`. If it is, we treat the prop as mutable even in
* runes mode, and skip `binding_property_non_reactive` validation
*/
var is_store_binding = false;
/**
* Returns a tuple that indicates whether `fn()` reads a prop that is a store binding.
* Used to prevent `binding_property_non_reactive` validation false positives and
* ensure that these props are treated as mutable even in runes mode
* @template T
* @param {() => T} fn
* @returns {[T, boolean]}
*/
function capture_store_binding(fn) {
	var previous_is_store_binding = is_store_binding;
	try {
		is_store_binding = false;
		return [fn(), is_store_binding];
	} finally {
		is_store_binding = previous_is_store_binding;
	}
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/dom/elements/misc.js
/**
* The child of a textarea actually corresponds to the defaultValue property, so we need
* to remove it upon hydration to avoid a bug when someone resets the form value.
* @param {HTMLTextAreaElement} dom
* @returns {void}
*/
function remove_textarea_child(dom) {
	if (hydrating && /* @__PURE__ */ get_first_child(dom) !== null) clear_text_content(dom);
}
var listening_to_form_reset = false;
function add_form_reset_listener() {
	if (!listening_to_form_reset) {
		listening_to_form_reset = true;
		document.addEventListener("reset", (evt) => {
			Promise.resolve().then(() => {
				if (!evt.defaultPrevented) for (const e of evt.target.elements)
 /** @type {any} */ e[FORM_RESET_HANDLER]?.();
			});
		}, { capture: true });
	}
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/dom/elements/bindings/shared.js
/**
* @template T
* @param {() => T} fn
*/
function without_reactive_context(fn) {
	var previous_reaction = active_reaction;
	var previous_effect = active_effect;
	set_active_reaction(null);
	set_active_effect(null);
	try {
		return fn();
	} finally {
		set_active_reaction(previous_reaction);
		set_active_effect(previous_effect);
	}
}
/**
* Listen to the given event, and then instantiate a global form reset listener if not already done,
* to notify all bindings when the form is reset
* @param {HTMLElement} element
* @param {string} event
* @param {(is_reset?: true) => void} handler
* @param {(is_reset?: true) => void} [on_reset]
*/
function listen_to_event_and_reset_event(element, event, handler, on_reset = handler) {
	element.addEventListener(event, () => without_reactive_context(handler));
	const prev = element[FORM_RESET_HANDLER];
	if (prev)
 /** @type {any} */ element[FORM_RESET_HANDLER] = () => {
		prev();
		on_reset(true);
	};
	else
 /** @type {any} */ element[FORM_RESET_HANDLER] = () => on_reset(true);
	add_form_reset_listener();
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/reactivity/async.js
/** @import { Blocker, Effect, Source, Value } from '#client' */
/**
* @param {Blocker[]} blockers
* @param {Array<() => any>} sync
* @param {Array<() => Promise<any>>} async
* @param {(values: Value[]) => any} fn
*/
function flatten(blockers, sync, async, fn) {
	const d = is_runes() ? derived : derived_safe_equal;
	var pending = blockers.filter((b) => !b.settled);
	var deriveds = sync.map(d);
	if (async.length === 0 && pending.length === 0) {
		fn(deriveds);
		return;
	}
	var parent = active_effect;
	var restore = capture();
	var blocker_promise = pending.length === 1 ? pending[0].promise : pending.length > 1 ? Promise.all(pending.map((b) => b.promise)) : null;
	/**
	* @param {Source[]} async
	*/
	function finish(async) {
		if ((parent.f & 16384) !== 0) return;
		restore();
		try {
			fn([...deriveds, ...async]);
		} catch (error) {
			invoke_error_boundary(error, parent);
		}
		unset_context();
	}
	var decrement_pending = increment_pending();
	if (async.length === 0) {
		/** @type {Promise<any>} */ blocker_promise.then(() => finish([])).finally(decrement_pending);
		return;
	}
	function run() {
		Promise.all(async.map((expression) => /* @__PURE__ */ async_derived(expression))).then(finish).catch((error) => invoke_error_boundary(error, parent)).finally(decrement_pending);
	}
	if (blocker_promise) blocker_promise.then(() => {
		restore();
		run();
		unset_context();
	});
	else run();
}
/**
* Captures the current effect context so that we can restore it after
* some asynchronous work has happened (so that e.g. `await a + b`
* causes `b` to be registered as a dependency).
*/
function capture() {
	var previous_effect = active_effect;
	var previous_reaction = active_reaction;
	var previous_component_context = component_context;
	var previous_batch = current_batch;
	return function restore(activate_batch = true) {
		set_active_effect(previous_effect);
		set_active_reaction(previous_reaction);
		set_component_context(previous_component_context);
		if (activate_batch && (previous_effect.f & 16384) === 0) {
			previous_batch?.activate();
			previous_batch?.apply();
		}
	};
}
function unset_context(deactivate_batch = true) {
	set_active_effect(null);
	set_active_reaction(null);
	set_component_context(null);
	if (deactivate_batch) current_batch?.deactivate();
}
/**
* @returns {(skip?: boolean) => void}
*/
function increment_pending() {
	var effect = active_effect;
	var boundary = effect.b;
	var batch = current_batch;
	var blocking = !!boundary?.is_rendered();
	boundary?.update_pending_count(1, batch);
	batch.increment(blocking, effect);
	return () => {
		boundary?.update_pending_count(-1, batch);
		batch.decrement(blocking, effect);
	};
}
/**
* @template V
* @param {() => V} fn
* @returns {Derived<V>}
*/
/*#__NO_SIDE_EFFECTS__*/
function derived(fn) {
	var flags = 2 | DIRTY;
	if (active_effect !== null) active_effect.f |= EFFECT_PRESERVED;
	return {
		ctx: component_context,
		deps: null,
		effects: null,
		equals,
		f: flags,
		fn,
		reactions: null,
		rv: 0,
		v: UNINITIALIZED,
		wv: 0,
		parent: active_effect,
		ac: null
	};
}
var OBSOLETE = Symbol("obsolete");
/**
* @template V
* @param {() => V | Promise<V>} fn
* @param {string} [label]
* @param {string} [location] If provided, print a warning if the value is not read immediately after update
* @returns {Promise<Source<V>>}
*/
/*#__NO_SIDE_EFFECTS__*/
function async_derived(fn, label, location) {
	let parent = active_effect;
	if (parent === null) async_derived_orphan();
	var promise = void 0;
	var signal = source(UNINITIALIZED);
	var should_suspend = !active_reaction;
	/** @type {Set<ReturnType<typeof deferred<V>>>} */
	var deferreds = /* @__PURE__ */ new Set();
	async_effect(() => {
		var effect = active_effect;
		/** @type {ReturnType<typeof deferred<V>>} */
		var d = deferred();
		promise = d.promise;
		try {
			Promise.resolve(fn()).then(d.resolve, (e) => {
				if (e !== STALE_REACTION) d.reject(e);
			}).finally(unset_context);
		} catch (error) {
			d.reject(error);
			unset_context();
		}
		var batch = current_batch;
		if (should_suspend) {
			if ((effect.f & 32768) !== 0) var decrement_pending = increment_pending();
			if (parent.b?.is_rendered()) batch.async_deriveds.get(effect)?.reject(OBSOLETE);
			else for (const d of deferreds.values()) d.reject(OBSOLETE);
			deferreds.add(d);
			batch.async_deriveds.set(effect, d);
		}
		/**
		* @param {any} value
		* @param {unknown} error
		*/
		const handler = (value, error = void 0) => {
			decrement_pending?.();
			deferreds.delete(d);
			if (error === OBSOLETE) return;
			batch.activate();
			if (error) {
				signal.f |= ERROR_VALUE;
				internal_set(signal, error);
			} else {
				if ((signal.f & 8388608) !== 0) signal.f ^= ERROR_VALUE;
				internal_set(signal, value);
			}
			batch.deactivate();
		};
		d.promise.then(handler, (e) => handler(null, e || "unknown"));
	});
	teardown(() => {
		for (const d of deferreds) d.reject(OBSOLETE);
	});
	return new Promise((fulfil) => {
		/** @param {Promise<V>} p */
		function next(p) {
			function go() {
				if (p === promise) fulfil(signal);
				else next(promise);
			}
			p.then(go, go);
		}
		next(promise);
	});
}
/**
* @template V
* @param {() => V} fn
* @returns {Derived<V>}
*/
/*#__NO_SIDE_EFFECTS__*/
function user_derived(fn) {
	const d = /* @__PURE__ */ derived(fn);
	if (!async_mode_flag) push_reaction_value(d);
	return d;
}
/**
* @template V
* @param {() => V} fn
* @returns {Derived<V>}
*/
/*#__NO_SIDE_EFFECTS__*/
function derived_safe_equal(fn) {
	const signal = /* @__PURE__ */ derived(fn);
	signal.equals = safe_equals;
	return signal;
}
/**
* @param {Derived} derived
* @returns {void}
*/
function destroy_derived_effects(derived) {
	var effects = derived.effects;
	if (effects !== null) {
		derived.effects = null;
		for (var i = 0; i < effects.length; i += 1) destroy_effect(effects[i]);
	}
}
/**
* @template T
* @param {Derived} derived
* @returns {T}
*/
function execute_derived(derived) {
	var value;
	var prev_active_effect = active_effect;
	var parent = derived.parent;
	if (!is_destroying_effect && parent !== null && derived.v !== UNINITIALIZED && (parent.f & 24576) !== 0) {
		derived_inert();
		return derived.v;
	}
	set_active_effect(parent);
	try {
		derived.f &= ~WAS_MARKED;
		destroy_derived_effects(derived);
		value = update_reaction(derived);
	} finally {
		set_active_effect(prev_active_effect);
	}
	return value;
}
/**
* @param {Derived} derived
* @returns {void}
*/
function update_derived(derived) {
	var value = execute_derived(derived);
	if (!derived.equals(value)) {
		derived.wv = increment_write_version();
		if (!current_batch?.is_fork || derived.deps === null) {
			if (current_batch !== null) {
				current_batch.capture(derived, value, true);
				previous_batch?.capture(derived, value, true);
			} else derived.v = value;
			if (derived.deps === null) {
				set_signal_status(derived, CLEAN);
				return;
			}
		}
	}
	if (is_destroying_effect) return;
	if (batch_values !== null) {
		if (effect_tracking() || current_batch?.is_fork) batch_values.set(derived, value);
	} else update_derived_status(derived);
}
/**
* @param {Derived} derived
*/
function freeze_derived_effects(derived) {
	if (derived.effects === null) return;
	for (const e of derived.effects) if (e.teardown || e.ac) {
		e.teardown?.();
		if (e.ac !== null) without_reactive_context(() => {
			/** @type {AbortController} */ e.ac.abort(STALE_REACTION);
			e.ac = null;
		});
		if (e.fn !== null) e.teardown = noop;
		remove_reactions(e, 0);
		destroy_effect_children(e);
	}
}
/**
* @param {Derived} derived
*/
function unfreeze_derived_effects(derived) {
	if (derived.effects === null) return;
	for (const e of derived.effects) if (e.teardown && e.fn !== null) update_effect(e);
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/reactivity/batch.js
/** @import { Fork } from 'svelte' */
/** @import { Derived, Effect, Reaction, Source, Value } from '#client' */
/** @type {Batch | null} */
var first_batch = null;
/** @type {Batch | null} */
var last_batch = null;
/** @type {Batch | null} */
var current_batch = null;
/**
* This is needed to avoid overwriting inputs
* @type {Batch | null}
*/
var previous_batch = null;
/**
* When time travelling (i.e. working in one batch, while other batches
* still have ongoing work), we ignore the real values of affected
* signals in favour of their values within the batch
* @type {Map<Value, any> | null}
*/
var batch_values = null;
/** @type {Effect | null} */
var last_scheduled_effect = null;
var is_flushing_sync = false;
var is_processing = false;
/**
* During traversal, this is an array. Newly created effects are (if not immediately
* executed) pushed to this array, rather than going through the scheduling
* rigamarole that would cause another turn of the flush loop.
* @type {Effect[] | null}
*/
var collected_effects = null;
/**
* An array of effects that are marked during traversal as a result of a `set`
* (not `internal_set`) call. These will be added to the next batch and
* trigger another `batch.process()`
* @type {Effect[] | null}
* @deprecated when we get rid of legacy mode and stores, we can get rid of this
*/
var legacy_updates = null;
var flush_count = 0;
var uid = 1;
var Batch = class Batch {
	id = uid++;
	/** True as soon as `#process` was called */
	#started = false;
	linked = true;
	/** @type {Batch | null} */
	#prev = null;
	/** @type {Batch | null} */
	#next = null;
	/** @type {Map<Effect, ReturnType<typeof deferred<any>>>} */
	async_deriveds = /* @__PURE__ */ new Map();
	/**
	* The current values of any signals that are updated in this batch.
	* Tuple format: [value, is_derived] (note: is_derived is false for deriveds, too, if they were overridden via assignment)
	* They keys of this map are identical to `this.#previous`
	* @type {Map<Value, [any, boolean]>}
	*/
	current = /* @__PURE__ */ new Map();
	/**
	* The values of any signals (sources and deriveds) that are updated in this batch _before_ those updates took place.
	* They keys of this map are identical to `this.#current`
	* @type {Map<Value, any>}
	*/
	previous = /* @__PURE__ */ new Map();
	/**
	* When the batch is committed (and the DOM is updated), we need to remove old branches
	* and append new ones by calling the functions added inside (if/each/key/etc) blocks
	* @type {Set<(batch: Batch) => void>}
	*/
	#commit_callbacks = /* @__PURE__ */ new Set();
	/**
	* If a fork is discarded, we need to destroy any effects that are no longer needed
	* @type {Set<(batch: Batch) => void>}
	*/
	#discard_callbacks = /* @__PURE__ */ new Set();
	/**
	* The number of async effects that are currently in flight
	*/
	#pending = 0;
	/**
	* Async effects that are currently in flight, _not_ inside a pending boundary
	* @type {Map<Effect, number>}
	*/
	#blocking_pending = /* @__PURE__ */ new Map();
	/**
	* A deferred that resolves when the batch is committed, used with `settled()`
	* TODO replace with Promise.withResolvers once supported widely enough
	* @type {{ promise: Promise<void>, resolve: (value?: any) => void, reject: (reason: unknown) => void } | null}
	*/
	#deferred = null;
	/**
	* The root effects that need to be flushed
	* @type {Effect[]}
	*/
	#roots = [];
	/**
	* Effects created while this batch was active.
	* @type {Effect[]}
	*/
	#new_effects = [];
	/**
	* Deferred effects (which run after async work has completed) that are DIRTY
	* @type {Set<Effect>}
	*/
	#dirty_effects = /* @__PURE__ */ new Set();
	/**
	* Deferred effects that are MAYBE_DIRTY
	* @type {Set<Effect>}
	*/
	#maybe_dirty_effects = /* @__PURE__ */ new Set();
	/**
	* A map of branches that still exist, but will be destroyed when this batch
	* is committed — we skip over these during `process`.
	* The value contains child effects that were dirty/maybe_dirty before being reset,
	* so they can be rescheduled if the branch survives.
	* @type {Map<Effect, { d: Effect[], m: Effect[] }>}
	*/
	#skipped_branches = /* @__PURE__ */ new Map();
	/**
	* Inverse of #skipped_branches which we need to tell prior batches to unskip them when committing
	* @type {Set<Effect>}
	*/
	#unskipped_branches = /* @__PURE__ */ new Set();
	is_fork = false;
	#decrement_queued = false;
	constructor() {
		if (last_batch === null) first_batch = last_batch = this;
		else {
			last_batch.#next = this;
			this.#prev = last_batch;
		}
		last_batch = this;
	}
	#is_deferred() {
		if (this.is_fork) return true;
		for (const effect of this.#blocking_pending.keys()) {
			var e = effect;
			var skipped = false;
			while (e.parent !== null) {
				if (this.#skipped_branches.has(e)) {
					skipped = true;
					break;
				}
				e = e.parent;
			}
			if (!skipped) return true;
		}
		return false;
	}
	/**
	* Add an effect to the #skipped_branches map and reset its children
	* @param {Effect} effect
	*/
	skip_effect(effect) {
		if (!this.#skipped_branches.has(effect)) this.#skipped_branches.set(effect, {
			d: [],
			m: []
		});
		this.#unskipped_branches.delete(effect);
	}
	/**
	* Remove an effect from the #skipped_branches map and reschedule
	* any tracked dirty/maybe_dirty child effects
	* @param {Effect} effect
	* @param {(e: Effect) => void} callback
	*/
	unskip_effect(effect, callback = (e) => this.schedule(e)) {
		var tracked = this.#skipped_branches.get(effect);
		if (tracked) {
			this.#skipped_branches.delete(effect);
			for (var e of tracked.d) {
				set_signal_status(e, DIRTY);
				callback(e);
			}
			for (e of tracked.m) {
				set_signal_status(e, MAYBE_DIRTY);
				callback(e);
			}
		}
		this.#unskipped_branches.add(effect);
	}
	#process() {
		this.#started = true;
		if (flush_count++ > 1e3) {
			this.#unlink();
			infinite_loop_guard();
		}
		for (const e of this.#dirty_effects) {
			this.#maybe_dirty_effects.delete(e);
			set_signal_status(e, DIRTY);
			this.schedule(e);
		}
		for (const e of this.#maybe_dirty_effects) {
			set_signal_status(e, MAYBE_DIRTY);
			this.schedule(e);
		}
		const roots = this.#roots;
		this.#roots = [];
		this.apply();
		/** @type {Effect[]} */
		var effects = collected_effects = [];
		/** @type {Effect[]} */
		var render_effects = [];
		/**
		* @type {Effect[]}
		* @deprecated when we get rid of legacy mode and stores, we can get rid of this
		*/
		var updates = legacy_updates = [];
		for (const root of roots) try {
			this.#traverse(root, effects, render_effects);
		} catch (e) {
			reset_all(root);
			if (!this.#is_deferred()) this.discard();
			throw e;
		}
		current_batch = null;
		if (updates.length > 0) {
			var batch = Batch.ensure();
			for (const e of updates) batch.schedule(e);
		}
		collected_effects = null;
		legacy_updates = null;
		if (this.#is_deferred()) {
			this.#defer_effects(render_effects);
			this.#defer_effects(effects);
			for (const [e, t] of this.#skipped_branches) reset_branch(e, t);
			if (updates.length > 0)
 /** @type {Batch} */ current_batch.#process();
			return;
		}
		const earlier_batch = this.#find_earlier_batch();
		if (earlier_batch) {
			this.#defer_effects(render_effects);
			this.#defer_effects(effects);
			earlier_batch.#merge(this);
			return;
		}
		this.#dirty_effects.clear();
		this.#maybe_dirty_effects.clear();
		for (const fn of this.#commit_callbacks) fn(this);
		this.#commit_callbacks.clear();
		previous_batch = this;
		flush_queued_effects(render_effects);
		flush_queued_effects(effects);
		previous_batch = null;
		this.#deferred?.resolve();
		var next_batch = current_batch;
		if (this.#pending === 0 && (this.#roots.length === 0 || next_batch !== null)) {
			this.#unlink();
			if (async_mode_flag) {
				this.#commit();
				current_batch = next_batch;
			}
		}
		if (this.#roots.length > 0) {
			if (next_batch !== null) {
				const batch = next_batch;
				batch.#roots.push(...this.#roots.filter((r) => !batch.#roots.includes(r)));
			} else next_batch = this;
		}
		if (next_batch !== null) {
			old_values.clear();
			next_batch.#process();
		}
	}
	/**
	* Traverse the effect tree, executing effects or stashing
	* them for later execution as appropriate
	* @param {Effect} root
	* @param {Effect[]} effects
	* @param {Effect[]} render_effects
	*/
	#traverse(root, effects, render_effects) {
		root.f ^= CLEAN;
		var effect = root.first;
		while (effect !== null) {
			var flags = effect.f;
			var is_branch = (flags & 96) !== 0;
			if (!(is_branch && (flags & 1024) !== 0 || (flags & 8192) !== 0 || this.#skipped_branches.has(effect)) && effect.fn !== null) {
				if (is_branch) effect.f ^= CLEAN;
				else if ((flags & 4) !== 0) effects.push(effect);
				else if (async_mode_flag && (flags & 16777224) !== 0) render_effects.push(effect);
				else if (is_dirty(effect)) {
					if ((flags & 16) !== 0) this.#maybe_dirty_effects.add(effect);
					update_effect(effect);
				}
				var child = effect.first;
				if (child !== null) {
					effect = child;
					continue;
				}
			}
			while (effect !== null) {
				var next = effect.next;
				if (next !== null) {
					effect = next;
					break;
				}
				effect = effect.parent;
			}
		}
	}
	#find_earlier_batch() {
		var batch = this.#prev;
		while (batch !== null) {
			if (!batch.is_fork) {
				for (const [value, [, is_derived]] of this.current) if (batch.current.has(value) && !is_derived) return batch;
			}
			batch = batch.#prev;
		}
		return null;
	}
	/**
	* @param {Batch} batch
	*/
	#merge(batch) {
		for (const [source, value] of batch.current) {
			if (!this.previous.has(source) && batch.previous.has(source)) this.previous.set(source, batch.previous.get(source));
			this.current.set(source, value);
		}
		for (const [effect, deferred] of batch.async_deriveds) {
			const d = this.async_deriveds.get(effect);
			if (d) deferred.promise.then(d.resolve).catch(d.reject);
		}
		batch.async_deriveds.clear();
		this.transfer_effects(batch.#dirty_effects, batch.#maybe_dirty_effects);
		/**
		* mark all effects that depend on `batch.current`, except the
		* async effects that we just resolved (TODO unless they depend
		* on values in this batch that are NOT in the later batch?).
		* Through this we also will populate the correct #skipped_branches,
		* oncommit callbacks etc, so we don't need to merge them separately.
		* @param {Value} value
		*/
		const mark = (value) => {
			var reactions = value.reactions;
			if (reactions === null) return;
			if ((value.f & 2) !== 0 && (value.f & 6144) === 0) return;
			for (const reaction of reactions) {
				var flags = reaction.f;
				if ((flags & 2) !== 0) mark(reaction);
				else {
					var effect = reaction;
					if (flags & 4194320 && !this.async_deriveds.has(effect)) {
						this.#maybe_dirty_effects.delete(effect);
						set_signal_status(effect, DIRTY);
						this.schedule(effect);
					}
				}
			}
		};
		for (const source of this.current.keys()) mark(source);
		this.oncommit(() => batch.discard());
		batch.#unlink();
		current_batch = this;
		this.#process();
	}
	/**
	* @param {Effect[]} effects
	*/
	#defer_effects(effects) {
		for (var i = 0; i < effects.length; i += 1) defer_effect(effects[i], this.#dirty_effects, this.#maybe_dirty_effects);
	}
	/**
	* Associate a change to a given source with the current
	* batch, noting its previous and current values
	* @param {Value} source
	* @param {any} value
	* @param {boolean} [is_derived]
	*/
	capture(source, value, is_derived = false) {
		if (source.v !== UNINITIALIZED && !this.previous.has(source)) this.previous.set(source, source.v);
		if ((source.f & 8388608) === 0) {
			this.current.set(source, [value, is_derived]);
			batch_values?.set(source, value);
		}
		if (!this.is_fork) source.v = value;
	}
	activate() {
		current_batch = this;
	}
	deactivate() {
		current_batch = null;
		batch_values = null;
	}
	flush() {
		try {
			is_processing = true;
			current_batch = this;
			this.#process();
		} finally {
			flush_count = 0;
			last_scheduled_effect = null;
			collected_effects = null;
			legacy_updates = null;
			is_processing = false;
			current_batch = null;
			batch_values = null;
			old_values.clear();
		}
	}
	discard() {
		for (const fn of this.#discard_callbacks) fn(this);
		this.#discard_callbacks.clear();
		for (const deferred of this.async_deriveds.values()) deferred.reject(OBSOLETE);
		this.#unlink();
		this.#deferred?.resolve();
	}
	/**
	* @param {Effect} effect
	*/
	register_created_effect(effect) {
		this.#new_effects.push(effect);
	}
	#commit() {
		for (let batch = first_batch; batch !== null; batch = batch.#next) {
			var is_earlier = batch.id < this.id;
			/** @type {Source[]} */
			var sources = [];
			for (const [source, [value, is_derived]] of this.current) {
				if (batch.current.has(source)) {
					var batch_value = batch.current.get(source)[0];
					if (is_earlier && value !== batch_value) batch.current.set(source, [value, is_derived]);
					else continue;
				}
				sources.push(source);
			}
			if (is_earlier) for (const [effect, deferred] of this.async_deriveds) {
				const d = batch.async_deriveds.get(effect);
				if (d) deferred.promise.then(d.resolve).catch(d.reject);
			}
			var current = [...batch.current.keys()].filter((source) => !batch.current.get(source)[1]);
			if (!batch.#started || current.length === 0) continue;
			var others = current.filter((source) => !this.current.has(source));
			if (others.length === 0) {
				if (is_earlier) batch.discard();
			} else if (sources.length > 0) {
				if (is_earlier) for (const unskipped of this.#unskipped_branches) batch.unskip_effect(unskipped, (e) => {
					if ((e.f & 4194320) !== 0) batch.schedule(e);
					else batch.#defer_effects([e]);
				});
				batch.activate();
				/** @type {Set<Value>} */
				var marked = /* @__PURE__ */ new Set();
				/** @type {Map<Reaction, boolean>} */
				var checked = /* @__PURE__ */ new Map();
				for (var source of sources) mark_effects(source, others, marked, checked);
				checked = /* @__PURE__ */ new Map();
				var current_unequal = [...batch.current].filter(([c, v1]) => {
					const v2 = this.current.get(c);
					if (!v2) return true;
					return v2[0] !== v1[0] || v2[1] !== v1[1];
				}).map(([c]) => c);
				if (current_unequal.length > 0) {
					for (const effect of this.#new_effects) if ((effect.f & 155648) === 0 && depends_on(effect, current_unequal, checked)) {
						if ((effect.f & 4194320) !== 0) {
							set_signal_status(effect, DIRTY);
							batch.schedule(effect);
						} else batch.#dirty_effects.add(effect);
					}
				}
				if (batch.#roots.length > 0 && !batch.#decrement_queued) {
					batch.apply();
					for (var root of batch.#roots) batch.#traverse(root, [], []);
					batch.#roots = [];
				}
				batch.deactivate();
			}
		}
	}
	/**
	* @param {boolean} blocking
	* @param {Effect} effect
	*/
	increment(blocking, effect) {
		this.#pending += 1;
		if (blocking) {
			let blocking_pending_count = this.#blocking_pending.get(effect) ?? 0;
			this.#blocking_pending.set(effect, blocking_pending_count + 1);
		}
	}
	/**
	* @param {boolean} blocking
	* @param {Effect} effect
	*/
	decrement(blocking, effect) {
		this.#pending -= 1;
		if (blocking) {
			let blocking_pending_count = this.#blocking_pending.get(effect) ?? 0;
			if (blocking_pending_count === 1) this.#blocking_pending.delete(effect);
			else this.#blocking_pending.set(effect, blocking_pending_count - 1);
		}
		if (this.#decrement_queued) return;
		this.#decrement_queued = true;
		queue_micro_task(() => {
			this.#decrement_queued = false;
			if (this.linked) this.flush();
		});
	}
	/**
	* @param {Set<Effect>} dirty_effects
	* @param {Set<Effect>} maybe_dirty_effects
	*/
	transfer_effects(dirty_effects, maybe_dirty_effects) {
		for (const e of dirty_effects) this.#dirty_effects.add(e);
		for (const e of maybe_dirty_effects) this.#maybe_dirty_effects.add(e);
		dirty_effects.clear();
		maybe_dirty_effects.clear();
	}
	/** @param {(batch: Batch) => void} fn */
	oncommit(fn) {
		this.#commit_callbacks.add(fn);
	}
	/** @param {(batch: Batch) => void} fn */
	ondiscard(fn) {
		this.#discard_callbacks.add(fn);
	}
	settled() {
		return (this.#deferred ??= deferred()).promise;
	}
	static ensure() {
		if (current_batch === null) {
			const batch = current_batch = new Batch();
			if (!is_processing && !is_flushing_sync) queue_micro_task(() => {
				if (!batch.#started) batch.flush();
			});
		}
		return current_batch;
	}
	apply() {
		if (!async_mode_flag || !this.is_fork && this.#prev === null && this.#next === null) {
			batch_values = null;
			return;
		}
		batch_values = /* @__PURE__ */ new Map();
		for (const [source, [value]] of this.current) batch_values.set(source, value);
		for (let batch = first_batch; batch !== null; batch = batch.#next) {
			if (batch === this || batch.is_fork) continue;
			var intersects = false;
			if (batch.id < this.id) for (const [source, [, is_derived]] of batch.current) {
				if (is_derived) continue;
				if (this.current.has(source)) {
					intersects = true;
					break;
				}
			}
			if (!intersects) {
				for (const [source, previous] of batch.previous) if (!batch_values.has(source)) batch_values.set(source, previous);
			}
		}
	}
	/**
	*
	* @param {Effect} effect
	*/
	schedule(effect) {
		last_scheduled_effect = effect;
		if (effect.b?.is_pending && (effect.f & 16777228) !== 0 && (effect.f & 32768) === 0) {
			effect.b.defer_effect(effect);
			return;
		}
		var e = effect;
		while (e.parent !== null) {
			e = e.parent;
			var flags = e.f;
			if (collected_effects !== null && e === active_effect) {
				if (async_mode_flag) return;
				if ((active_reaction === null || (active_reaction.f & 2) === 0) && !legacy_is_updating_store) return;
			}
			if ((flags & 96) !== 0) {
				if ((flags & 1024) === 0) return;
				e.f ^= CLEAN;
			}
		}
		this.#roots.push(e);
	}
	#unlink() {
		if (!this.linked) return;
		var prev = this.#prev;
		var next = this.#next;
		if (prev === null) first_batch = next;
		else prev.#next = next;
		if (next === null) last_batch = prev;
		else next.#prev = prev;
		this.linked = false;
	}
};
/**
* Synchronously flush any pending updates.
* Returns void if no callback is provided, otherwise returns the result of calling the callback.
* @template [T=void]
* @param {(() => T) | undefined} [fn]
* @returns {T}
*/
function flushSync(fn) {
	var was_flushing_sync = is_flushing_sync;
	is_flushing_sync = true;
	try {
		var result;
		if (fn) {
			if (current_batch !== null && !current_batch.is_fork) current_batch.flush();
			result = fn();
		}
		while (true) {
			flush_tasks();
			if (current_batch === null) return result;
			current_batch.flush();
		}
	} finally {
		is_flushing_sync = was_flushing_sync;
	}
}
function infinite_loop_guard() {
	try {
		effect_update_depth_exceeded();
	} catch (error) {
		invoke_error_boundary(error, last_scheduled_effect);
	}
}
/** @type {Set<Effect> | null} */
var eager_block_effects = null;
/**
* @param {Array<Effect>} effects
* @returns {void}
*/
function flush_queued_effects(effects) {
	var length = effects.length;
	if (length === 0) return;
	var i = 0;
	while (i < length) {
		var effect = effects[i++];
		if ((effect.f & 24576) === 0 && is_dirty(effect)) {
			eager_block_effects = /* @__PURE__ */ new Set();
			update_effect(effect);
			if (effect.deps === null && effect.first === null && effect.nodes === null && effect.teardown === null && effect.ac === null) unlink_effect(effect);
			if (eager_block_effects?.size > 0) {
				old_values.clear();
				for (const e of eager_block_effects) {
					if ((e.f & 24576) !== 0) continue;
					/** @type {Effect[]} */
					const ordered_effects = [e];
					let ancestor = e.parent;
					while (ancestor !== null) {
						if (eager_block_effects.has(ancestor)) {
							eager_block_effects.delete(ancestor);
							ordered_effects.push(ancestor);
						}
						ancestor = ancestor.parent;
					}
					for (let j = ordered_effects.length - 1; j >= 0; j--) {
						const e = ordered_effects[j];
						if ((e.f & 24576) !== 0) continue;
						update_effect(e);
					}
				}
				eager_block_effects.clear();
			}
		}
	}
	eager_block_effects = null;
}
/**
* This is similar to `mark_reactions`, but it only marks async/block effects
* depending on `value` and at least one of the other `sources`, so that
* these effects can re-run after another batch has been committed
* @param {Value} value
* @param {Source[]} sources
* @param {Set<Value>} marked
* @param {Map<Reaction, boolean>} checked
*/
function mark_effects(value, sources, marked, checked) {
	if (marked.has(value)) return;
	marked.add(value);
	if (value.reactions !== null) for (const reaction of value.reactions) {
		const flags = reaction.f;
		if ((flags & 2) !== 0) mark_effects(reaction, sources, marked, checked);
		else if ((flags & 4194320) !== 0 && (flags & 2048) === 0 && depends_on(reaction, sources, checked)) {
			set_signal_status(reaction, DIRTY);
			schedule_effect(reaction);
		}
	}
}
/**
* @param {Reaction} reaction
* @param {Source[]} sources
* @param {Map<Reaction, boolean>} checked
*/
function depends_on(reaction, sources, checked) {
	const depends = checked.get(reaction);
	if (depends !== void 0) return depends;
	if (reaction.deps !== null) for (const dep of reaction.deps) {
		if (includes.call(sources, dep)) return true;
		if ((dep.f & 2) !== 0 && depends_on(dep, sources, checked)) {
			checked.set(dep, true);
			return true;
		}
	}
	checked.set(reaction, false);
	return false;
}
/**
* @param {Effect} effect
* @returns {void}
*/
function schedule_effect(effect) {
	/** @type {Batch} */ current_batch.schedule(effect);
}
/**
* Mark all the effects inside a skipped branch CLEAN, so that
* they can be correctly rescheduled later. Tracks dirty and maybe_dirty
* effects so they can be rescheduled if the branch survives.
* @param {Effect} effect
* @param {{ d: Effect[], m: Effect[] }} tracked
*/
function reset_branch(effect, tracked) {
	if ((effect.f & 32) !== 0 && (effect.f & 1024) !== 0) return;
	if ((effect.f & 2048) !== 0) tracked.d.push(effect);
	else if ((effect.f & 4096) !== 0) tracked.m.push(effect);
	set_signal_status(effect, CLEAN);
	var e = effect.first;
	while (e !== null) {
		reset_branch(e, tracked);
		e = e.next;
	}
}
/**
* Mark an entire effect tree clean following an error
* @param {Effect} effect
*/
function reset_all(effect) {
	set_signal_status(effect, CLEAN);
	var e = effect.first;
	while (e !== null) {
		reset_all(e);
		e = e.next;
	}
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/reactivity/sources.js
/** @import { Derived, Effect, Source, Value } from '#client' */
/** @type {Set<Effect>} */
var eager_effects = /* @__PURE__ */ new Set();
/** @type {Map<Source, any>} */
var old_values = /* @__PURE__ */ new Map();
var eager_effects_deferred = false;
/**
* @template V
* @param {V} v
* @param {Error | null} [stack]
* @returns {Source<V>}
*/
function source(v, stack) {
	return {
		f: 0,
		v,
		reactions: null,
		equals,
		rv: 0,
		wv: 0
	};
}
/**
* @template V
* @param {V} v
* @param {Error | null} [stack]
*/
/*#__NO_SIDE_EFFECTS__*/
function state(v, stack) {
	const s = source(v, stack);
	push_reaction_value(s);
	return s;
}
/**
* @template V
* @param {V} initial_value
* @param {boolean} [immutable]
* @returns {Source<V>}
*/
/*#__NO_SIDE_EFFECTS__*/
function mutable_source(initial_value, immutable = false, trackable = true) {
	const s = source(initial_value);
	if (!immutable) s.equals = safe_equals;
	if (legacy_mode_flag && trackable && component_context !== null && component_context.l !== null) (component_context.l.s ??= []).push(s);
	return s;
}
/**
* @template V
* @param {Source<V>} source
* @param {V} value
* @param {boolean} [should_proxy]
* @returns {V}
*/
function set(source, value, should_proxy = false) {
	if (active_reaction !== null && (!untracking || (active_reaction.f & 131072) !== 0) && is_runes() && (active_reaction.f & 4325394) !== 0 && (current_sources === null || !current_sources.has(source))) state_unsafe_mutation();
	return internal_set(source, should_proxy ? proxy(value) : value, legacy_updates);
}
/**
* @template V
* @param {Source<V>} source
* @param {V} value
* @param {Effect[] | null} [updated_during_traversal]
* @returns {V}
*/
function internal_set(source, value, updated_during_traversal = null) {
	if (!source.equals(value)) {
		if (is_destroying_effect) old_values.set(source, value);
		else if (!old_values.has(source)) old_values.set(source, source.v);
		var batch = Batch.ensure();
		batch.capture(source, value);
		if ((source.f & 2) !== 0) {
			const derived = source;
			if ((source.f & 2048) !== 0) execute_derived(derived);
			if (batch_values === null) update_derived_status(derived);
		}
		source.wv = increment_write_version();
		mark_reactions(source, DIRTY, updated_during_traversal);
		if (is_runes() && active_effect !== null && (active_effect.f & 1024) !== 0 && (active_effect.f & 96) === 0) {
			if (untracked_writes === null) set_untracked_writes([source]);
			else untracked_writes.push(source);
		}
		if (!batch.is_fork && eager_effects.size > 0 && !eager_effects_deferred) flush_eager_effects();
	}
	return value;
}
function flush_eager_effects() {
	eager_effects_deferred = false;
	for (const effect of eager_effects) {
		if ((effect.f & 1024) !== 0) set_signal_status(effect, MAYBE_DIRTY);
		let dirty;
		try {
			dirty = is_dirty(effect);
		} catch {
			dirty = true;
		}
		if (dirty) update_effect(effect);
	}
	eager_effects.clear();
}
/**
* Silently (without using `get`) increment a source
* @param {Source<number>} source
*/
function increment(source) {
	set(source, source.v + 1);
}
/**
* @param {Value} signal
* @param {number} status should be DIRTY or MAYBE_DIRTY
* @param {Effect[] | null} updated_during_traversal
* @returns {void}
*/
function mark_reactions(signal, status, updated_during_traversal) {
	var reactions = signal.reactions;
	if (reactions === null) return;
	var runes = is_runes();
	var length = reactions.length;
	for (var i = 0; i < length; i++) {
		var reaction = reactions[i];
		var flags = reaction.f;
		if (!runes && reaction === active_effect) continue;
		var not_dirty = (flags & DIRTY) === 0;
		if (not_dirty) set_signal_status(reaction, status);
		if ((flags & 131072) !== 0) eager_effects.add(reaction);
		else if ((flags & 2) !== 0) {
			var derived = reaction;
			batch_values?.delete(derived);
			if ((flags & 65536) === 0) {
				if (flags & 512 && (active_effect === null || (active_effect.f & 2097152) === 0)) reaction.f |= WAS_MARKED;
				mark_reactions(derived, MAYBE_DIRTY, updated_during_traversal);
			}
		} else if (not_dirty) {
			var effect = reaction;
			if ((flags & 16) !== 0 && eager_block_effects !== null) eager_block_effects.add(effect);
			if (updated_during_traversal !== null) updated_during_traversal.push(effect);
			else schedule_effect(effect);
		}
	}
}
/**
* @template T
* @param {T} value
* @returns {T}
*/
function proxy(value) {
	if (typeof value !== "object" || value === null || STATE_SYMBOL in value || COMPONENT_SYMBOL in value) return value;
	const prototype = get_prototype_of(value);
	if (prototype !== object_prototype && prototype !== array_prototype) return value;
	/** @type {Map<any, Source<any>>} */
	var sources = /* @__PURE__ */ new Map();
	var is_proxied_array = is_array(value);
	var version = /* @__PURE__ */ state(0);
	var stack = null;
	var parent_version = update_version;
	/**
	* Executes the proxy in the context of the reaction it was originally created in, if any
	* @template T
	* @param {() => T} fn
	*/
	var with_parent = (fn) => {
		if (update_version === parent_version) return fn();
		var reaction = active_reaction;
		var version = update_version;
		set_active_reaction(null);
		set_update_version(parent_version);
		var result = fn();
		set_active_reaction(reaction);
		set_update_version(version);
		return result;
	};
	if (is_proxied_array) sources.set("length", /* @__PURE__ */ state(
		/** @type {any[]} */
		value.length,
		stack
	));
	return new Proxy(value, {
		defineProperty(_, prop, descriptor) {
			if (!("value" in descriptor) || descriptor.configurable === false || descriptor.enumerable === false || descriptor.writable === false) state_descriptors_fixed();
			var s = sources.get(prop);
			if (s === void 0) with_parent(() => {
				var s = /* @__PURE__ */ state(descriptor.value, stack);
				sources.set(prop, s);
				return s;
			});
			else set(s, descriptor.value, true);
			return true;
		},
		deleteProperty(target, prop) {
			var s = sources.get(prop);
			if (s === void 0) {
				if (prop in target) {
					const s = with_parent(() => /* @__PURE__ */ state(UNINITIALIZED, stack));
					sources.set(prop, s);
					increment(version);
				}
			} else {
				set(s, UNINITIALIZED);
				increment(version);
			}
			return true;
		},
		get(target, prop, receiver) {
			if (prop === STATE_SYMBOL) return value;
			var s = sources.get(prop);
			var exists = prop in target;
			if (s === void 0 && (!exists || get_descriptor(target, prop)?.writable)) {
				s = with_parent(() => {
					return /* @__PURE__ */ state(proxy(exists ? target[prop] : UNINITIALIZED), stack);
				});
				sources.set(prop, s);
			}
			if (s !== void 0) {
				var v = get(s);
				return v === UNINITIALIZED ? void 0 : v;
			}
			return Reflect.get(target, prop, receiver);
		},
		getOwnPropertyDescriptor(target, prop) {
			var descriptor = Reflect.getOwnPropertyDescriptor(target, prop);
			if (descriptor && "value" in descriptor) {
				var s = sources.get(prop);
				if (s) descriptor.value = get(s);
			} else if (descriptor === void 0) {
				var source = sources.get(prop);
				var value = source?.v;
				if (source !== void 0 && value !== UNINITIALIZED) return {
					enumerable: true,
					configurable: true,
					value,
					writable: true
				};
			}
			return descriptor;
		},
		has(target, prop) {
			if (prop === STATE_SYMBOL) return true;
			var s = sources.get(prop);
			var has = s !== void 0 && s.v !== UNINITIALIZED || Reflect.has(target, prop);
			if (s !== void 0 || active_effect !== null && (!has || get_descriptor(target, prop)?.writable)) {
				if (s === void 0) {
					s = with_parent(() => {
						return /* @__PURE__ */ state(has ? proxy(target[prop]) : UNINITIALIZED, stack);
					});
					sources.set(prop, s);
				}
				if (get(s) === UNINITIALIZED) return false;
			}
			return has;
		},
		set(target, prop, value, receiver) {
			var s = sources.get(prop);
			var has = prop in target;
			if (is_proxied_array && prop === "length") for (var i = value; i < s.v; i += 1) {
				var other_s = sources.get(i + "");
				if (other_s !== void 0) set(other_s, UNINITIALIZED);
				else if (i in target) {
					other_s = with_parent(() => /* @__PURE__ */ state(UNINITIALIZED, stack));
					sources.set(i + "", other_s);
				}
			}
			if (s === void 0) {
				if (!has || get_descriptor(target, prop)?.writable) {
					s = with_parent(() => /* @__PURE__ */ state(void 0, stack));
					set(s, proxy(value));
					sources.set(prop, s);
				}
			} else {
				has = s.v !== UNINITIALIZED;
				var p = with_parent(() => proxy(value));
				set(s, p);
			}
			var descriptor = Reflect.getOwnPropertyDescriptor(target, prop);
			if (descriptor?.set) descriptor.set.call(receiver, value);
			if (!has) {
				if (is_proxied_array && typeof prop === "string") {
					var ls = sources.get("length");
					var n = Number(prop);
					if (Number.isInteger(n) && n >= ls.v) set(ls, n + 1);
				}
				increment(version);
			}
			return true;
		},
		ownKeys(target) {
			get(version);
			var own_keys = Reflect.ownKeys(target).filter((key) => {
				var source = sources.get(key);
				return source === void 0 || source.v !== UNINITIALIZED;
			});
			for (var [key, source] of sources) if (source.v !== UNINITIALIZED && !(key in target)) own_keys.push(key);
			return own_keys;
		},
		setPrototypeOf() {
			state_prototype_fixed();
		}
	});
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/dom/operations.js
/** @import { Effect, TemplateNode } from '#client' */
/** @type {Window} */
var $window;
/** @type {boolean} */
var is_firefox;
/** @type {() => Node | null} */
var first_child_getter;
/** @type {() => Node | null} */
var next_sibling_getter;
/**
* Initialize these lazily to avoid issues when using the runtime in a server context
* where these globals are not available while avoiding a separate server entry point
*/
function init_operations() {
	if ($window !== void 0) return;
	$window = window;
	is_firefox = /Firefox/.test(navigator.userAgent);
	var element_prototype = Element.prototype;
	var node_prototype = Node.prototype;
	var text_prototype = Text.prototype;
	first_child_getter = get_descriptor(node_prototype, "firstChild").get;
	next_sibling_getter = get_descriptor(node_prototype, "nextSibling").get;
	if (is_extensible(element_prototype)) {
		/** @type {any} */ element_prototype[CLASS_CACHE] = void 0;
		/** @type {any} */ element_prototype[ATTRIBUTES_CACHE] = null;
		/** @type {any} */ element_prototype[STYLE_CACHE] = void 0;
		element_prototype.__e = void 0;
	}
	if (is_extensible(text_prototype))
 /** @type {any} */ text_prototype[TEXT_CACHE] = void 0;
}
/**
* @param {string} value
* @returns {Text}
*/
function create_text(value = "") {
	return document.createTextNode(value);
}
/**
* @template {Node} N
* @param {N} node
*/
/*@__NO_SIDE_EFFECTS__*/
function get_first_child(node) {
	return first_child_getter.call(node);
}
/**
* @template {Node} N
* @param {N} node
*/
/*@__NO_SIDE_EFFECTS__*/
function get_next_sibling(node) {
	return next_sibling_getter.call(node);
}
/**
* Don't mark this as side-effect-free, hydration needs to walk all nodes
* @template {Node} N
* @param {N} node
* @param {boolean} is_text
* @returns {TemplateNode | null}
*/
function child(node, is_text) {
	if (!hydrating) return /* @__PURE__ */ get_first_child(node);
	var child = /* @__PURE__ */ get_first_child(hydrate_node);
	if (child === null) child = hydrate_node.appendChild(create_text());
	else if (is_text && child.nodeType !== 3) {
		var text = create_text();
		child?.before(text);
		set_hydrate_node(text);
		return text;
	}
	if (is_text) merge_text_nodes(child);
	set_hydrate_node(child);
	return child;
}
/**
* Don't mark this as side-effect-free, hydration needs to walk all nodes
* @param {TemplateNode} node
* @param {boolean} [is_text]
* @returns {TemplateNode | null}
*/
function first_child(node, is_text = false) {
	if (!hydrating) {
		var first = /* @__PURE__ */ get_first_child(node);
		if (first instanceof Comment && first.data === "") return /* @__PURE__ */ get_next_sibling(first);
		return first;
	}
	if (is_text) {
		if (hydrate_node?.nodeType !== 3) {
			var text = create_text();
			hydrate_node?.before(text);
			set_hydrate_node(text);
			return text;
		}
		merge_text_nodes(hydrate_node);
	}
	return hydrate_node;
}
/**
* `child`, for the very common case of an element with exactly one child. Resetting the
* hydration cursor is part of the same step, so the compiler doesn't have to emit a
* separate `reset` call for every `<p>{text}</p>` in an app.
* Don't mark this as side-effect-free, hydration needs to walk all nodes
* @param {TemplateNode} node
* @param {boolean} [is_text]
* @returns {TemplateNode | null}
*/
function only_child(node, is_text = false) {
	if (!hydrating) return /* @__PURE__ */ get_first_child(node);
	var first = child(node, is_text);
	reset(node);
	return first;
}
/**
* Don't mark this as side-effect-free, hydration needs to walk all nodes
* @param {TemplateNode} node
* @param {number} count
* @param {boolean} is_text
* @returns {TemplateNode | null}
*/
function sibling(node, count = 1, is_text = false) {
	let next_sibling = hydrating ? hydrate_node : node;
	var last_sibling;
	while (count--) {
		last_sibling = next_sibling;
		next_sibling = /* @__PURE__ */ get_next_sibling(next_sibling);
	}
	if (!hydrating) return next_sibling;
	if (is_text) {
		if (next_sibling?.nodeType !== 3) {
			var text = create_text();
			if (next_sibling === null) last_sibling?.after(text);
			else next_sibling.before(text);
			set_hydrate_node(text);
			return text;
		}
		merge_text_nodes(next_sibling);
	}
	set_hydrate_node(next_sibling);
	return next_sibling;
}
/**
* @template {Node} N
* @param {N} node
* @returns {void}
*/
function clear_text_content(node) {
	node.textContent = "";
}
/**
* Returns `true` if we're updating the current block, for example `condition` in
* an `{#if condition}` block just changed. In this case, the branch should be
* appended (or removed) at the same time as other updates within the
* current `<svelte:boundary>`
*/
function should_defer_append() {
	if (!async_mode_flag) return false;
	if (eager_block_effects !== null) return false;
	return (active_effect.f & REACTION_RAN) !== 0;
}
/**
* Branching here is intentional and load-bearing for perf. `createElement(tag)`
* hits a fast path in Blink that `createElementNS(NAMESPACE_HTML, tag)` doesn't,
* and passing an explicit `undefined` as the trailing options arg measurably
* slows both APIs. Funnelling every case through a single `createElementNS(ns,
* tag, options)` call would be smaller but slower on the HTML path.
*
* @template {keyof HTMLElementTagNameMap | string} T
* @param {T} tag
* @param {string} [namespace]
* @param {string} [is]
* @returns {T extends keyof HTMLElementTagNameMap ? HTMLElementTagNameMap[T] : Element}
*/
function create_element(tag, namespace, is) {
	if (namespace == null || namespace === "http://www.w3.org/1999/xhtml") return is ? document.createElement(tag, { is }) : document.createElement(tag);
	return is ? document.createElementNS(namespace, tag, { is }) : document.createElementNS(namespace, tag);
}
/**
* Browsers split text nodes larger than 65536 bytes when parsing.
* For hydration to succeed, we need to stitch them back together
* @param {Text} text
*/
function merge_text_nodes(text) {
	if (text.nodeValue.length < 65536) return;
	let next = text.nextSibling;
	while (next !== null && next.nodeType === 3) {
		next.remove();
		/** @type {string} */ text.nodeValue += next.nodeValue;
		next = text.nextSibling;
	}
}
/**
* @param {unknown} error
*/
function handle_error(error) {
	var effect = active_effect;
	if (effect === null) {
		/** @type {Derived} */ active_reaction.f |= ERROR_VALUE;
		return error;
	}
	if ((effect.f & 32768) === 0 && (effect.f & 4) === 0) throw error;
	invoke_error_boundary(error, effect);
}
/**
* @param {unknown} error
* @param {Effect | null} effect
*/
function invoke_error_boundary(error, effect) {
	if (effect !== null && (effect.f & 16384) !== 0) return;
	while (effect !== null) {
		if ((effect.f & 128) !== 0 && (effect.f & 33570816) === 0) {
			if ((effect.f & 32768) === 0) throw error;
			try {
				/** @type {Boundary} */ effect.b.error(error);
				return;
			} catch (e) {
				error = e;
			}
		}
		effect = effect.parent;
	}
	throw error;
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/reactivity/effects.js
/** @import { Blocker, ComponentContext, ComponentContextLegacy, Derived, Effect, TemplateNode, TransitionManager } from '#client' */
/**
* @param {'$effect' | '$effect.pre' | '$inspect'} rune
*/
function validate_effect(rune) {
	if (active_effect === null) {
		if (active_reaction === null) effect_orphan(rune);
		effect_in_unowned_derived();
	}
	if (is_destroying_effect) effect_in_teardown(rune);
}
/**
* @param {Effect} effect
* @param {Effect} parent_effect
*/
function push_effect(effect, parent_effect) {
	var parent_last = parent_effect.last;
	if (parent_last === null) parent_effect.last = parent_effect.first = effect;
	else {
		parent_last.next = effect;
		effect.prev = parent_last;
		parent_effect.last = effect;
	}
}
/**
* @param {number} type
* @param {null | (() => void | (() => void))} fn
* @returns {Effect}
*/
function create_effect(type, fn) {
	var parent = active_effect;
	if (parent !== null && (parent.f & 8192) !== 0) type |= INERT;
	/** @type {Effect} */
	var effect = {
		ctx: component_context,
		deps: null,
		nodes: null,
		f: type | DIRTY | 512,
		first: null,
		fn,
		last: null,
		next: null,
		parent,
		b: parent && parent.b,
		prev: null,
		teardown: null,
		wv: 0,
		ac: null
	};
	current_batch?.register_created_effect(effect);
	/** @type {Effect | null} */
	var e = effect;
	if ((type & 4) !== 0) {
		if (collected_effects !== null) collected_effects.push(effect);
		else Batch.ensure().schedule(effect);
	} else if (fn !== null) {
		try {
			update_effect(effect);
		} catch (e) {
			destroy_effect(effect);
			throw e;
		}
		if (e.deps === null && e.teardown === null && e.nodes === null && e.first === e.last && (e.f & 524288) === 0) {
			e = e.first;
			if ((type & 16) !== 0 && (type & 65536) !== 0 && e !== null) e.f |= EFFECT_TRANSPARENT;
		}
	}
	if (e !== null) {
		e.parent = parent;
		if (parent !== null) push_effect(e, parent);
		if (active_reaction !== null && (active_reaction.f & 2) !== 0 && (type & 64) === 0) {
			var derived = active_reaction;
			(derived.effects ??= []).push(e);
		}
	}
	return effect;
}
/**
* Internal representation of `$effect.tracking()`
* @returns {boolean}
*/
function effect_tracking() {
	return active_reaction !== null && !untracking;
}
/**
* @param {() => void} fn
*/
function teardown(fn) {
	const effect = create_effect(8, null);
	set_signal_status(effect, CLEAN);
	effect.teardown = fn;
	return effect;
}
/**
* Internal representation of `$effect(...)`
* @param {() => void | (() => void)} fn
*/
function user_effect(fn) {
	validate_effect("$effect");
	var flags = active_effect.f;
	if (!active_reaction && (flags & 32) !== 0 && component_context !== null && !component_context.i) {
		var context = component_context;
		(context.e ??= []).push(fn);
	} else return create_user_effect(fn);
}
/**
* @param {() => void | (() => void)} fn
*/
function create_user_effect(fn) {
	return create_effect(4 | USER_EFFECT, fn);
}
/**
* An effect root whose children can transition out
* @param {() => void} fn
* @returns {(options?: { outro?: boolean }) => Promise<void>}
*/
function component_root(fn) {
	Batch.ensure();
	const effect = create_effect(64 | EFFECT_PRESERVED, fn);
	return (options = {}) => {
		return new Promise((fulfil) => {
			if (options.outro) pause_effect(effect, () => {
				destroy_effect(effect);
				fulfil(void 0);
			});
			else {
				destroy_effect(effect);
				fulfil(void 0);
			}
		});
	};
}
/**
* @param {() => void | (() => void)} fn
* @returns {Effect}
*/
function effect(fn) {
	return create_effect(4, fn);
}
/**
* @param {() => void | (() => void)} fn
* @returns {Effect}
*/
function async_effect(fn) {
	return create_effect(ASYNC | EFFECT_PRESERVED, fn);
}
/**
* @param {() => void | (() => void)} fn
* @returns {Effect}
*/
function render_effect(fn, flags = 0) {
	return create_effect(8 | flags, fn);
}
/**
* @param {(...expressions: any) => void | (() => void)} fn
* @param {Array<() => any>} sync
* @param {Array<() => Promise<any>>} async
* @param {Blocker[]} blockers
*/
function template_effect(fn, sync = [], async = [], blockers = []) {
	flatten(blockers, sync, async, (values) => {
		create_effect(8, () => {
			fn(...values.map(get));
		});
	});
}
/**
* @param {(() => void)} fn
* @param {number} flags
*/
function block(fn, flags = 0) {
	return create_effect(16 | flags, fn);
}
/**
* @param {(() => void)} fn
*/
function branch(fn) {
	return create_effect(32 | EFFECT_PRESERVED, fn);
}
/**
* @param {Effect} effect
*/
function execute_effect_teardown(effect) {
	var teardown = effect.teardown;
	if (teardown !== null) {
		const previously_destroying_effect = is_destroying_effect;
		const previous_reaction = active_reaction;
		set_is_destroying_effect(true);
		set_active_reaction(null);
		try {
			teardown.call(null);
		} catch (error) {
			invoke_error_boundary(error, effect.parent);
		} finally {
			set_is_destroying_effect(previously_destroying_effect);
			set_active_reaction(previous_reaction);
		}
	}
}
/**
* @param {Effect} signal
* @param {boolean} remove_dom
* @returns {void}
*/
function destroy_effect_children(signal, remove_dom = false) {
	var effect = signal.first;
	signal.first = signal.last = null;
	while (effect !== null) {
		const controller = effect.ac;
		if (controller !== null) without_reactive_context(() => {
			controller.abort(STALE_REACTION);
		});
		var next = effect.next;
		if ((effect.f & 64) !== 0) effect.parent = null;
		else destroy_effect(effect, remove_dom);
		effect = next;
	}
}
/**
* @param {Effect} signal
* @returns {void}
*/
function destroy_block_effect_children(signal) {
	var effect = signal.first;
	while (effect !== null) {
		var next = effect.next;
		if ((effect.f & 32) === 0) destroy_effect(effect);
		effect = next;
	}
}
/**
* @param {Effect} effect
* @param {boolean} [remove_dom]
* @returns {void}
*/
function destroy_effect(effect, remove_dom = true) {
	var removed = false;
	if ((remove_dom || (effect.f & 262144) !== 0) && effect.nodes !== null && effect.nodes.end !== null) {
		remove_effect_dom(effect.nodes.start, effect.nodes.end);
		removed = true;
	}
	effect.f |= DESTROYING;
	destroy_effect_children(effect, remove_dom && !removed);
	remove_reactions(effect, 0);
	var transitions = effect.nodes && effect.nodes.t;
	if (transitions !== null) for (const transition of transitions) transition.stop();
	execute_effect_teardown(effect);
	effect.f ^= DESTROYING;
	effect.f |= DESTROYED;
	var parent = effect.parent;
	if (parent !== null && parent.first !== null) unlink_effect(effect);
	effect.next = effect.prev = effect.teardown = effect.ctx = effect.deps = effect.fn = effect.nodes = effect.ac = effect.b = null;
}
/**
*
* @param {TemplateNode | null} node
* @param {TemplateNode} end
*/
function remove_effect_dom(node, end) {
	while (node !== null) {
		/** @type {TemplateNode | null} */
		var next = node === end ? null : /* @__PURE__ */ get_next_sibling(node);
		node.remove();
		node = next;
	}
}
/**
* Detach an effect from the effect tree, freeing up memory and
* reducing the amount of work that happens on subsequent traversals
* @param {Effect} effect
*/
function unlink_effect(effect) {
	var parent = effect.parent;
	var prev = effect.prev;
	var next = effect.next;
	if (prev !== null) prev.next = next;
	if (next !== null) next.prev = prev;
	if (parent !== null) {
		if (parent.first === effect) parent.first = next;
		if (parent.last === effect) parent.last = prev;
	}
}
/**
* When a block effect is removed, we don't immediately destroy it or yank it
* out of the DOM, because it might have transitions. Instead, we 'pause' it.
* It stays around (in memory, and in the DOM) until outro transitions have
* completed, and if the state change is reversed then we _resume_ it.
* A paused effect does not update, and the DOM subtree becomes inert.
* @param {Effect} effect
* @param {() => void} [callback]
* @param {boolean} [destroy]
*/
function pause_effect(effect, callback, destroy = true) {
	/** @type {TransitionManager[]} */
	var transitions = [];
	effect.f |= 256;
	pause_children(effect, transitions, true);
	var fn = () => {
		if (destroy) destroy_effect(effect);
		if (callback) callback();
	};
	var remaining = transitions.length;
	if (remaining > 0) {
		var check = () => --remaining || fn();
		for (var transition of transitions) transition.out(check);
	} else fn();
}
/**
* @param {Effect} effect
* @param {TransitionManager[]} transitions
* @param {boolean} local
*/
function pause_children(effect, transitions, local) {
	if ((effect.f & 8192) !== 0) return;
	effect.f ^= INERT;
	var t = effect.nodes && effect.nodes.t;
	if (t !== null) {
		for (const transition of t) if (transition.is_global || local) transitions.push(transition);
	}
	var child = effect.first;
	while (child !== null) {
		var sibling = child.next;
		if ((child.f & 64) === 0) {
			var transparent = (child.f & 65536) !== 0 || (child.f & 32) !== 0 && (effect.f & 16) !== 0;
			pause_children(child, transitions, transparent ? local : false);
		}
		child = sibling;
	}
}
/**
* The opposite of `pause_effect`. We call this if (for example)
* `x` becomes falsy then truthy: `{#if x}...{/if}`
* @param {Effect} effect
*/
function resume_effect(effect) {
	effect.f &= -257;
	resume_children(effect, true);
}
/**
* @param {Effect} effect
* @param {boolean} local
*/
function resume_children(effect, local) {
	if ((effect.f & 256) !== 0) return;
	if ((effect.f & 8192) === 0) return;
	effect.f ^= INERT;
	if ((effect.f & 1024) === 0) {
		set_signal_status(effect, DIRTY);
		Batch.ensure().schedule(effect);
	}
	var child = effect.first;
	while (child !== null) {
		var sibling = child.next;
		var transparent = (child.f & 65536) !== 0 || (child.f & 32) !== 0;
		resume_children(child, transparent ? local : false);
		child = sibling;
	}
	var t = effect.nodes && effect.nodes.t;
	if (t !== null) {
		for (const transition of t) if (transition.is_global || local) transition.in();
	}
}
/**
* @param {Effect} effect
* @param {DocumentFragment} fragment
*/
function move_effect(effect, fragment) {
	if (!effect.nodes) return;
	/** @type {TemplateNode | null} */
	var node = effect.nodes.start;
	var end = effect.nodes.end;
	while (node !== null) {
		/** @type {TemplateNode | null} */
		var next = node === end ? null : /* @__PURE__ */ get_next_sibling(node);
		fragment.append(node);
		node = next;
	}
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/legacy.js
/**
* @type {Set<Value> | null}
* @deprecated
*/
var captured_signals = null;
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/runtime.js
/** @import { Derived, Effect, Reaction, Source, Value } from '#client' */
/**
* True if updating in an effect context that is reactive (i.e. not branch/root effects)
*/
var is_updating_effect = false;
var is_destroying_effect = false;
/** @param {boolean} value */
function set_is_destroying_effect(value) {
	is_destroying_effect = value;
}
/** @type {null | Reaction} */
var active_reaction = null;
var untracking = false;
/** @param {null | Reaction} reaction */
function set_active_reaction(reaction) {
	active_reaction = reaction;
}
/** @type {null | Effect} */
var active_effect = null;
/** @param {null | Effect} effect */
function set_active_effect(effect) {
	active_effect = effect;
}
/**
* When sources are created within a reaction, reading and writing
* them within that reaction should not cause a re-run
* @type {null | Set<Source>}
*/
var current_sources = null;
/** @param {Value} value */
function push_reaction_value(value) {
	if (active_reaction !== null && (!async_mode_flag || (active_reaction.f & 2) !== 0)) (current_sources ??= /* @__PURE__ */ new Set()).add(value);
}
/**
* The dependencies of the reaction that is currently being executed. In many cases,
* the dependencies are unchanged between runs, and so this will be `null` unless
* and until a new dependency is accessed — we track this via `skipped_deps`
* @type {null | Value[]}
*/
var new_deps = null;
var skipped_deps = 0;
/**
* Tracks writes that the effect it's executed in doesn't listen to yet,
* so that the dependency can be added to the effect later on if it then reads it
* @type {null | Source[]}
*/
var untracked_writes = null;
/** @param {null | Source[]} value */
function set_untracked_writes(value) {
	untracked_writes = value;
}
/**
* @type {number} Used by sources and deriveds for handling updates.
* Version starts from 1 so that unowned deriveds differentiate between a created effect and a run one for tracing
**/
var write_version = 1;
/** @type {number} Used to version each read of a source of derived to avoid duplicating dependencies inside a reaction */
var read_version = 0;
var update_version = read_version;
/** @param {number} value */
function set_update_version(value) {
	update_version = value;
}
function increment_write_version() {
	return ++write_version;
}
/**
* Determines whether a derived or effect is dirty.
* If it is MAYBE_DIRTY, will set the status to CLEAN
* @param {Reaction} reaction
* @returns {boolean}
*/
function is_dirty(reaction) {
	var flags = reaction.f;
	if ((flags & 2048) !== 0) return true;
	if (flags & 2) reaction.f &= ~WAS_MARKED;
	if ((flags & 4096) !== 0) {
		var dependencies = reaction.deps;
		var length = dependencies.length;
		for (var i = 0; i < length; i++) {
			var dependency = dependencies[i];
			if (is_dirty(dependency)) update_derived(dependency);
			if (dependency.wv > reaction.wv) return true;
		}
		if ((flags & 512) !== 0 && batch_values === null) set_signal_status(reaction, CLEAN);
	}
	return false;
}
/**
* @param {Value} signal
* @param {Effect} effect
* @param {boolean} [root]
*/
function schedule_possible_effect_self_invalidation(signal, effect, root = true) {
	var reactions = signal.reactions;
	if (reactions === null) return;
	if (!async_mode_flag && current_sources !== null && current_sources.has(signal)) return;
	for (var i = 0; i < reactions.length; i++) {
		var reaction = reactions[i];
		if ((reaction.f & 2) !== 0) schedule_possible_effect_self_invalidation(reaction, effect, false);
		else if (effect === reaction) {
			if (root) set_signal_status(reaction, DIRTY);
			else if ((reaction.f & 1024) !== 0) set_signal_status(reaction, MAYBE_DIRTY);
			schedule_effect(reaction);
		}
	}
}
/** @param {Reaction} reaction */
function update_reaction(reaction) {
	var previous_deps = new_deps;
	var previous_skipped_deps = skipped_deps;
	var previous_untracked_writes = untracked_writes;
	var previous_reaction = active_reaction;
	var previous_sources = current_sources;
	var previous_component_context = component_context;
	var previous_untracking = untracking;
	var previous_update_version = update_version;
	var flags = reaction.f;
	new_deps = null;
	skipped_deps = 0;
	untracked_writes = null;
	active_reaction = (flags & 96) === 0 ? reaction : null;
	current_sources = null;
	set_component_context(reaction.ctx);
	untracking = false;
	update_version = ++read_version;
	if (reaction.ac !== null) {
		without_reactive_context(() => {
			/** @type {AbortController} */ reaction.ac.abort(STALE_REACTION);
		});
		reaction.ac = null;
	}
	try {
		reaction.f |= REACTION_IS_UPDATING;
		var fn = reaction.fn;
		var result = fn();
		reaction.f |= REACTION_RAN;
		var deps = update_dependencies(reaction);
		if (is_runes() && untracked_writes !== null && !untracking && deps !== null && (reaction.f & 6146) === 0) for (var i = 0; i < untracked_writes.length; i++) schedule_possible_effect_self_invalidation(untracked_writes[i], reaction);
		if (previous_reaction !== null && previous_reaction !== reaction) {
			read_version++;
			if (previous_reaction.deps !== null) for (let i = 0; i < previous_skipped_deps; i += 1) previous_reaction.deps[i].rv = read_version;
			if (previous_deps !== null) for (const dep of previous_deps) dep.rv = read_version;
			if (untracked_writes !== null) {
				if (previous_untracked_writes === null) previous_untracked_writes = untracked_writes;
				else previous_untracked_writes.push(...untracked_writes);
			}
		}
		if ((reaction.f & 8388608) !== 0) reaction.f ^= ERROR_VALUE;
		return result;
	} catch (error) {
		update_dependencies(reaction);
		return handle_error(error);
	} finally {
		reaction.f ^= REACTION_IS_UPDATING;
		new_deps = previous_deps;
		skipped_deps = previous_skipped_deps;
		untracked_writes = previous_untracked_writes;
		active_reaction = previous_reaction;
		current_sources = previous_sources;
		set_component_context(previous_component_context);
		untracking = previous_untracking;
		update_version = previous_update_version;
	}
}
/**
* @param {Reaction} reaction
*/
function update_dependencies(reaction) {
	var deps = reaction.deps;
	var is_fork = current_batch?.is_fork;
	if (new_deps !== null) {
		var i;
		if (!is_fork) remove_reactions(reaction, skipped_deps);
		if (deps !== null && skipped_deps > 0) {
			deps.length = skipped_deps + new_deps.length;
			for (i = 0; i < new_deps.length; i++) deps[skipped_deps + i] = new_deps[i];
		} else reaction.deps = deps = new_deps;
		if (effect_tracking() && (reaction.f & 512) !== 0) for (i = skipped_deps; i < deps.length; i++) (deps[i].reactions ??= []).push(reaction);
	} else if (!is_fork && deps !== null && skipped_deps < deps.length) {
		remove_reactions(reaction, skipped_deps);
		deps.length = skipped_deps;
	}
	return deps;
}
/**
* @template V
* @param {Reaction} signal
* @param {Value<V>} dependency
* @returns {void}
*/
function remove_reaction(signal, dependency) {
	let reactions = dependency.reactions;
	if (reactions !== null) {
		var index = index_of.call(reactions, signal);
		if (index !== -1) {
			var new_length = reactions.length - 1;
			if (new_length === 0) reactions = dependency.reactions = null;
			else {
				reactions[index] = reactions[new_length];
				reactions.pop();
			}
		}
	}
	if (reactions === null && (dependency.f & 2) !== 0 && (new_deps === null || !includes.call(new_deps, dependency))) {
		var derived = dependency;
		if ((derived.f & 512) !== 0) {
			derived.f ^= 512;
			derived.f &= ~WAS_MARKED;
		}
		if (derived.v !== UNINITIALIZED) update_derived_status(derived);
		if (derived.ac !== null) without_reactive_context(() => {
			/** @type {AbortController} */ derived.ac.abort(STALE_REACTION);
			derived.ac = null;
			set_signal_status(derived, DIRTY);
		});
		freeze_derived_effects(derived);
		remove_reactions(derived, 0);
	}
}
/**
* @param {Reaction} signal
* @param {number} start_index
* @returns {void}
*/
function remove_reactions(signal, start_index) {
	var dependencies = signal.deps;
	if (dependencies === null) return;
	for (var i = start_index; i < dependencies.length; i++) remove_reaction(signal, dependencies[i]);
}
/**
* @param {Effect} effect
* @returns {void}
*/
function update_effect(effect) {
	var flags = effect.f;
	if ((flags & 16384) !== 0) return;
	set_signal_status(effect, CLEAN);
	var previous_effect = active_effect;
	var was_updating_effect = is_updating_effect;
	active_effect = effect;
	is_updating_effect = (flags & 96) === 0;
	try {
		if ((flags & 16777232) !== 0) destroy_block_effect_children(effect);
		else destroy_effect_children(effect);
		execute_effect_teardown(effect);
		var teardown = update_reaction(effect);
		effect.teardown = typeof teardown === "function" ? teardown : null;
		effect.wv = write_version;
	} finally {
		is_updating_effect = was_updating_effect;
		active_effect = previous_effect;
	}
}
/**
* Returns a promise that resolves once any pending state changes have been applied.
* @returns {Promise<void>}
*/
async function tick() {
	if (async_mode_flag) return new Promise((f) => {
		requestAnimationFrame(() => f());
		setTimeout(() => f());
	});
	await Promise.resolve();
	flushSync();
}
/**
* @template V
* @param {Value<V>} signal
* @returns {V}
*/
function get(signal) {
	var is_derived = (signal.f & 2) !== 0;
	captured_signals?.add(signal);
	if (active_reaction !== null && !untracking) {
		if (!(active_effect !== null && (active_effect.f & 16384) !== 0) && (current_sources === null || !current_sources.has(signal))) {
			var deps = active_reaction.deps;
			if ((active_reaction.f & 2097152) !== 0) {
				if (signal.rv < read_version) {
					signal.rv = read_version;
					if (new_deps === null && deps !== null && deps[skipped_deps] === signal) skipped_deps++;
					else if (new_deps === null) new_deps = [signal];
					else new_deps.push(signal);
				}
			} else {
				active_reaction.deps ??= [];
				if (!includes.call(active_reaction.deps, signal)) active_reaction.deps.push(signal);
				var reactions = signal.reactions;
				if (reactions === null) signal.reactions = [active_reaction];
				else if (!includes.call(reactions, active_reaction)) reactions.push(active_reaction);
			}
		}
	}
	if (is_destroying_effect && old_values.has(signal)) return old_values.get(signal);
	if (is_derived) {
		var derived = signal;
		if (is_destroying_effect) {
			var value = derived.v;
			if ((derived.f & 1024) === 0 && derived.reactions !== null || depends_on_old_values(derived)) value = execute_derived(derived);
			old_values.set(derived, value);
			return value;
		}
		var should_connect = (derived.f & 512) === 0 && !untracking && active_reaction !== null && (is_updating_effect || (active_reaction.f & 512) !== 0);
		var is_new = (derived.f & REACTION_RAN) === 0;
		if (is_dirty(derived)) {
			if (should_connect) derived.f |= 512;
			update_derived(derived);
		}
		if (should_connect && !is_new) {
			unfreeze_derived_effects(derived);
			reconnect(derived);
		}
	}
	if (batch_values?.has(signal)) return batch_values.get(signal);
	if ((signal.f & 8388608) !== 0) throw signal.v;
	return signal.v;
}
/**
* (Re)connect a disconnected derived, so that it is notified
* of changes in `mark_reactions`
* @param {Derived} derived
*/
function reconnect(derived) {
	derived.f |= 512;
	if (derived.deps === null) return;
	for (const dep of derived.deps) {
		(dep.reactions ??= []).push(derived);
		if ((dep.f & 2) !== 0 && (dep.f & 512) === 0) {
			unfreeze_derived_effects(dep);
			reconnect(dep);
		}
	}
}
/** @param {Derived} derived */
function depends_on_old_values(derived) {
	if (derived.v === UNINITIALIZED) return true;
	if (derived.deps === null) return false;
	for (const dep of derived.deps) {
		if (old_values.has(dep)) return true;
		if ((dep.f & 2) !== 0 && depends_on_old_values(dep)) return true;
	}
	return false;
}
/**
* When used inside a [`$derived`](https://svelte.dev/docs/svelte/$derived) or [`$effect`](https://svelte.dev/docs/svelte/$effect),
* any state read inside `fn` will not be treated as a dependency.
*
* ```ts
* $effect(() => {
*   // this will run when `data` changes, but not when `time` changes
*   save(data, {
*     timestamp: untrack(() => time)
*   });
* });
* ```
* @template T
* @param {() => T} fn
* @returns {T}
*/
function untrack(fn) {
	var previous_untracking = untracking;
	try {
		untracking = true;
		return fn();
	} finally {
		untracking = previous_untracking;
	}
}
/**
* Subset of delegated events which should be passive by default.
* These two are already passive via browser defaults on window, document and body.
* But since
* - we're delegating them
* - they happen often
* - they apply to mobile which is generally less performant
* we're marking them as passive by default for other elements, too.
*/
var PASSIVE_EVENTS = ["touchstart", "touchmove"];
/**
* Returns `true` if `name` is a passive event
* @param {string} name
*/
function is_passive_event(name) {
	return PASSIVE_EVENTS.includes(name);
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/dom/elements/events.js
/**
* Used on elements, as a map of event type -> event handler,
* and on events themselves to track which element handled an event
*/
var event_symbol = Symbol("events");
/** @type {Set<string>} */
var all_registered_events = /* @__PURE__ */ new Set();
/** @type {Set<(events: Array<string>) => void>} */
var root_event_handles = /* @__PURE__ */ new Set();
/**
* @param {string} event_name
* @param {EventTarget} dom
* @param {EventListener} [handler]
* @param {AddEventListenerOptions} [options]
*/
function create_event(event_name, dom, handler, options = {}) {
	/**
	* @this {EventTarget}
	*/
	function target_handler(event) {
		if (!options.capture) handle_event_propagation.call(dom, event);
		if (!event.cancelBubble) return without_reactive_context(() => {
			return handler?.call(this, event);
		});
	}
	if (event_name.startsWith("pointer") || event_name.startsWith("touch") || event_name === "wheel") queue_micro_task(() => {
		dom.addEventListener(event_name, target_handler, options);
	});
	else dom.addEventListener(event_name, target_handler, options);
	return target_handler;
}
/**
* @param {string} event_name
* @param {Element} dom
* @param {EventListener} [handler]
* @param {boolean} [capture]
* @param {boolean} [passive]
* @returns {void}
*/
function event(event_name, dom, handler, capture, passive) {
	var options = {
		capture,
		passive
	};
	var target_handler = create_event(event_name, dom, handler, options);
	if (dom === document.body || dom === window || dom === document || dom instanceof HTMLMediaElement) teardown(() => {
		dom.removeEventListener(event_name, target_handler, options);
	});
}
/**
* @param {string} event_name
* @param {Element} element
* @param {EventListener} [handler]
* @returns {void}
*/
function delegated(event_name, element, handler) {
	(element[event_symbol] ??= {})[event_name] = handler;
}
/**
* @param {Array<string>} events
* @returns {void}
*/
function delegate(events) {
	for (var i = 0; i < events.length; i++) all_registered_events.add(events[i]);
	for (var fn of root_event_handles) fn(events);
}
var last_propagated_event = null;
var last_propagated_event_clear_scheduled = false;
/**
* @this {EventTarget}
* @param {Event} event
* @returns {void}
*/
function handle_event_propagation(event) {
	var handler_element = this;
	var owner_document = handler_element.ownerDocument;
	var event_name = event.type;
	var path = event.composedPath?.() || [];
	var current_target = path[0] || event.target;
	last_propagated_event = event;
	if (!last_propagated_event_clear_scheduled) {
		last_propagated_event_clear_scheduled = true;
		setTimeout(() => {
			last_propagated_event_clear_scheduled = false;
			last_propagated_event = null;
		});
	}
	var path_idx = 0;
	var handled_at = last_propagated_event === event && event[event_symbol];
	if (handled_at) {
		var at_idx = path.indexOf(handled_at);
		if (at_idx !== -1 && (handler_element === document || handler_element === window)) {
			event[event_symbol] = handler_element;
			return;
		}
		var handler_idx = path.indexOf(handler_element);
		if (handler_idx === -1) return;
		if (at_idx <= handler_idx) path_idx = at_idx;
	}
	current_target = path[path_idx] || event.target;
	if (current_target === handler_element) return;
	define_property(event, "currentTarget", {
		configurable: true,
		get() {
			return current_target || owner_document;
		}
	});
	var previous_reaction = active_reaction;
	var previous_effect = active_effect;
	set_active_reaction(null);
	set_active_effect(null);
	try {
		/**
		* @type {unknown}
		*/
		var throw_error;
		/**
		* @type {unknown[]}
		*/
		var other_errors = [];
		while (current_target !== null) {
			if (current_target === handler_element) break;
			try {
				var delegated = current_target[event_symbol]?.[event_name];
				if (delegated != null && (!current_target.disabled || event.target === current_target)) delegated.call(current_target, event);
			} catch (error) {
				if (throw_error) other_errors.push(error);
				else throw_error = error;
			}
			if (event.cancelBubble) break;
			path_idx++;
			current_target = path_idx < path.length ? path[path_idx] : null;
		}
		if (throw_error) {
			for (let error of other_errors) queueMicrotask(() => {
				throw error;
			});
			throw throw_error;
		}
	} finally {
		event[event_symbol] = handler_element;
		delete event.currentTarget;
		set_active_reaction(previous_reaction);
		set_active_effect(previous_effect);
	}
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/dom/reconciler.js
var policy = globalThis?.window?.trustedTypes && /* @__PURE__ */ globalThis.window.trustedTypes.createPolicy("svelte-trusted-html", {
/** @param {string} html */
createHTML: (html) => {
	return html;
} });
/** @param {string} html */
function create_trusted_html(html) {
	return policy?.createHTML(html) ?? html;
}
/**
* @param {string} html
*/
function create_fragment_from_html(html) {
	var elem = create_element("template");
	elem.innerHTML = create_trusted_html(html.replaceAll("<!>", "<!---->"));
	return elem.content;
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/dom/template.js
/** @import { Effect, EffectNodes, TemplateNode } from '#client' */
/** @import { TemplateStructure } from './types' */
/**
* @param {TemplateNode} start
* @param {TemplateNode | null} end
*/
function assign_nodes(start, end) {
	var effect = active_effect;
	if (effect.nodes === null) effect.nodes = {
		start,
		end,
		a: null,
		t: null
	};
}
/**
* @param {string} content
* @param {number} flags
* @returns {() => Node | Node[]}
*/
/*#__NO_SIDE_EFFECTS__*/
function from_html(content, flags) {
	var is_fragment = (flags & 1) !== 0;
	var use_import_node = (flags & 2) !== 0;
	/** @type {Node} */
	var node;
	/**
	* Whether or not the first item is a text/element node. If not, we need to
	* create an additional comment node to act as `effect.nodes.start`
	*/
	var has_start = !content.startsWith("<!>");
	return () => {
		if (hydrating) {
			assign_nodes(hydrate_node, null);
			return hydrate_node;
		}
		if (node === void 0) {
			node = create_fragment_from_html(has_start ? content : "<!>" + content);
			if (!is_fragment) node = /* @__PURE__ */ get_first_child(node);
		}
		var clone = use_import_node || is_firefox ? document.importNode(node, true) : node.cloneNode(true);
		if (is_fragment) {
			var start = /* @__PURE__ */ get_first_child(clone);
			var end = clone.lastChild;
			assign_nodes(start, end);
		} else assign_nodes(clone, clone);
		return clone;
	};
}
/**
* @param {string} content
* @param {number} flags
* @param {'svg' | 'math'} ns
* @returns {() => Node | Node[]}
*/
/*#__NO_SIDE_EFFECTS__*/
function from_namespace(content, flags, ns = "svg") {
	/**
	* Whether or not the first item is a text/element node. If not, we need to
	* create an additional comment node to act as `effect.nodes.start`
	*/
	var has_start = !content.startsWith("<!>");
	var is_fragment = (flags & 1) !== 0;
	var wrapped = `<${ns}>${has_start ? content : "<!>" + content}</${ns}>`;
	/** @type {Element | DocumentFragment} */
	var node;
	return () => {
		if (hydrating) {
			assign_nodes(hydrate_node, null);
			return hydrate_node;
		}
		if (!node) {
			var root = /* @__PURE__ */ get_first_child(create_fragment_from_html(wrapped));
			if (is_fragment) {
				node = document.createDocumentFragment();
				while (/* @__PURE__ */ get_first_child(root)) node.appendChild(/* @__PURE__ */ get_first_child(root));
			} else node = /* @__PURE__ */ get_first_child(root);
		}
		var clone = node.cloneNode(true);
		if (is_fragment) {
			var start = /* @__PURE__ */ get_first_child(clone);
			var end = clone.lastChild;
			assign_nodes(start, end);
		} else assign_nodes(clone, clone);
		return clone;
	};
}
/**
* @param {string} content
* @param {number} flags
*/
/*#__NO_SIDE_EFFECTS__*/
function from_svg(content, flags) {
	return /* @__PURE__ */ from_namespace(content, flags, "svg");
}
/**
* Don't mark this as side-effect-free, hydration needs to walk all nodes
* @param {any} value
*/
function text(value = "") {
	if (!hydrating) {
		var t = create_text(value + "");
		assign_nodes(t, t);
		return t;
	}
	var node = hydrate_node;
	if (node.nodeType !== 3) {
		node.before(node = create_text());
		set_hydrate_node(node);
	} else merge_text_nodes(node);
	assign_nodes(node, node);
	return node;
}
/**
* @returns {TemplateNode | DocumentFragment}
*/
function comment() {
	if (hydrating) {
		assign_nodes(hydrate_node, null);
		return hydrate_node;
	}
	var frag = document.createDocumentFragment();
	var start = document.createComment("");
	var anchor = create_text();
	frag.append(start, anchor);
	assign_nodes(start, anchor);
	return frag;
}
/**
* Assign the created (or in hydration mode, traversed) dom elements to the current block
* and insert the elements into the dom (in client mode).
* @param {Text | Comment | Element} anchor
* @param {DocumentFragment | Element} dom
*/
function append(anchor, dom) {
	if (hydrating) {
		var effect = active_effect;
		if ((effect.f & 32768) === 0 || effect.nodes.end === null) effect.nodes.end = hydrate_node;
		hydrate_next();
		return;
	}
	if (anchor === null) return;
	anchor.before(dom);
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/reactivity/create-subscriber.js
/**
* Returns a `subscribe` function that integrates external event-based systems with Svelte's reactivity.
* It's particularly useful for integrating with web APIs like `MediaQuery`, `IntersectionObserver`, or `WebSocket`.
*
* If `subscribe` is called inside an effect (including indirectly, for example inside a getter),
* the `start` callback will be called with an `update` function. Whenever `update` is called, the effect re-runs.
*
* If `start` returns a cleanup function, it will be called when the effect is destroyed.
*
* If `subscribe` is called in multiple effects, `start` will only be called once as long as the effects
* are active, and the returned teardown function will only be called when all effects are destroyed.
*
* It's best understood with an example. Here's an implementation of [`MediaQuery`](https://svelte.dev/docs/svelte/svelte-reactivity#MediaQuery):
*
* ```js
* import { createSubscriber } from 'svelte/reactivity';
* import { on } from 'svelte/events';
*
* export class MediaQuery {
* 	#query;
* 	#subscribe;
*
* 	constructor(query) {
* 		this.#query = window.matchMedia(`(${query})`);
*
* 		this.#subscribe = createSubscriber((update) => {
* 			// when the `change` event occurs, re-run any effects that read `this.current`
* 			const off = on(this.#query, 'change', update);
*
* 			// stop listening when all the effects are destroyed
* 			return () => off();
* 		});
* 	}
*
* 	get current() {
* 		// This makes the getter reactive, if read in an effect
* 		this.#subscribe();
*
* 		// Return the current state of the query, whether or not we're in an effect
* 		return this.#query.matches;
* 	}
* }
* ```
* @param {(update: () => void) => (() => void) | void} start
* @since 5.7.0
*/
function createSubscriber(start) {
	let subscribers = 0;
	let version = source(0);
	/** @type {(() => void) | void} */
	let stop;
	return () => {
		if (effect_tracking()) {
			get(version);
			render_effect(() => {
				if (subscribers === 0) stop = untrack(() => start(() => increment(version)));
				subscribers += 1;
				return () => {
					queue_micro_task(() => {
						subscribers -= 1;
						if (subscribers === 0) {
							stop?.();
							stop = void 0;
							increment(version);
						}
					});
				};
			});
		}
	};
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/dom/blocks/boundary.js
/** @import { Effect, Source, TemplateNode, } from '#client' */
/**
* @typedef {{
* 	 onerror?: ((error: unknown, reset: () => void) => void) | null;
*   failed?: ((anchor: Node, error: () => unknown, reset: () => () => void) => void) | null;
*   pending?: ((anchor: Node) => void) | null;
* }} BoundaryProps
*/
var flags = EFFECT_TRANSPARENT | EFFECT_PRESERVED;
/**
* @param {TemplateNode} node
* @param {BoundaryProps} props
* @param {((anchor: Node) => void)} children
* @param {((error: unknown) => unknown) | undefined} [transform_error]
* @returns {void}
*/
function boundary(node, props, children, transform_error) {
	new Boundary(node, props, children, transform_error);
}
var Boundary = class {
	/** @type {Boundary | null} */
	parent;
	is_pending = false;
	/**
	* API-level transformError transform function. Transforms errors before they reach the `failed` snippet.
	* Inherited from parent boundary, or defaults to identity.
	* @type {(error: unknown) => unknown}
	*/
	transform_error;
	/** @type {TemplateNode} */
	#anchor;
	/** @type {TemplateNode | null} */
	#hydrate_open = hydrating ? hydrate_node : null;
	/** @type {BoundaryProps} */
	#props;
	/** @type {((anchor: Node) => void)} */
	#children;
	/** @type {Effect} */
	#effect;
	/** @type {Effect | null} */
	#main_effect = null;
	/** @type {Effect | null} */
	#pending_effect = null;
	/** @type {Effect | null} */
	#failed_effect = null;
	/** @type {DocumentFragment | null} */
	#offscreen_fragment = null;
	#local_pending_count = 0;
	#pending_count = 0;
	#pending_count_update_queued = false;
	/** @type {Set<Effect>} */
	#dirty_effects = /* @__PURE__ */ new Set();
	/** @type {Set<Effect>} */
	#maybe_dirty_effects = /* @__PURE__ */ new Set();
	/**
	* A source containing the number of pending async deriveds/expressions.
	* Only created if `$effect.pending()` is used inside the boundary,
	* otherwise updating the source results in needless `Batch.ensure()`
	* calls followed by no-op flushes
	* @type {Source<number> | null}
	*/
	#effect_pending = null;
	#effect_pending_subscriber = createSubscriber(() => {
		this.#effect_pending = source(this.#local_pending_count);
		return () => {
			this.#effect_pending = null;
		};
	});
	/**
	* @param {TemplateNode} node
	* @param {BoundaryProps} props
	* @param {((anchor: Node) => void)} children
	* @param {((error: unknown) => unknown) | undefined} [transform_error]
	*/
	constructor(node, props, children, transform_error) {
		this.#anchor = node;
		this.#props = props;
		this.#children = (anchor) => {
			var effect = active_effect;
			effect.b = this;
			effect.f |= 128;
			children(anchor);
		};
		this.parent = active_effect.b;
		this.transform_error = transform_error ?? this.parent?.transform_error ?? ((e) => e);
		this.#effect = block(() => {
			if (hydrating) {
				const comment = this.#hydrate_open;
				hydrate_next();
				const server_rendered_pending = comment.data === "[!";
				if (comment.data.startsWith("[?")) {
					const serialized_error = JSON.parse(comment.data.slice(2));
					this.#hydrate_failed_content(serialized_error);
				} else if (server_rendered_pending) this.#hydrate_pending_content();
				else this.#hydrate_resolved_content();
			} else this.#render();
		}, flags);
		if (hydrating) this.#anchor = hydrate_node;
	}
	#hydrate_resolved_content() {
		try {
			this.#main_effect = branch(() => this.#children(this.#anchor));
		} catch (error) {
			this.error(error);
		}
	}
	/**
	* @param {unknown} error The deserialized error from the server's hydration comment
	*/
	#hydrate_failed_content(error) {
		const failed = this.#props.failed;
		const { reset, invoke_onerror } = this.#create_reset(error);
		queue_micro_task(invoke_onerror);
		if (!failed) return;
		this.#failed_effect = branch(() => {
			failed(this.#anchor, () => error, () => reset);
		});
	}
	/**
	* Creates the `reset` function for a failed boundary, along with a function
	* that invokes `onerror` with it (if provided)
	* @param {unknown} error
	* @returns {{ reset: () => void, invoke_onerror: () => void }}
	*/
	#create_reset(error) {
		var did_reset = false;
		var calling_on_error = false;
		const reset = () => {
			if (did_reset) {
				svelte_boundary_reset_noop();
				return;
			}
			did_reset = true;
			if (calling_on_error) svelte_boundary_reset_onerror();
			if (this.#failed_effect !== null) pause_effect(this.#failed_effect, () => {
				this.#failed_effect = null;
			});
			this.#run(() => {
				this.#render();
			});
		};
		const invoke_onerror = () => {
			try {
				calling_on_error = true;
				this.#props.onerror?.(error, reset);
				calling_on_error = false;
			} catch (err) {
				invoke_error_boundary(err, this.#effect && this.#effect.parent);
			}
		};
		return {
			reset,
			invoke_onerror
		};
	}
	#hydrate_pending_content() {
		const pending = this.#props.pending;
		if (!pending) return;
		this.is_pending = true;
		this.#pending_effect = branch(() => pending(this.#anchor));
		queue_micro_task(() => {
			var fragment = this.#offscreen_fragment = document.createDocumentFragment();
			var anchor = create_text();
			var handled = false;
			fragment.append(anchor);
			this.#main_effect = this.#run(() => {
				try {
					return branch(() => this.#children(anchor));
				} catch (error) {
					try {
						this.error(error);
						handled = true;
					} catch (error) {
						invoke_error_boundary(error, this.#effect.parent);
					}
					return null;
				}
			});
			if (this.#main_effect === null) {
				this.#offscreen_fragment = null;
				if (handled) this.#resolve(current_batch);
				return;
			}
			if (this.#pending_count === 0) {
				this.#anchor.before(fragment);
				this.#offscreen_fragment = null;
				pause_effect(this.#pending_effect, () => {
					this.#pending_effect = null;
				});
				this.#resolve(current_batch);
			}
		});
	}
	#render() {
		try {
			this.is_pending = this.has_pending_snippet();
			this.#pending_count = 0;
			this.#local_pending_count = 0;
			this.#main_effect = branch(() => {
				this.#children(this.#anchor);
			});
			if (this.#pending_count > 0) {
				var fragment = this.#offscreen_fragment = document.createDocumentFragment();
				move_effect(this.#main_effect, fragment);
				const pending = this.#props.pending;
				this.#pending_effect = branch(() => pending(this.#anchor));
			} else this.#resolve(current_batch);
		} catch (error) {
			this.error(error);
		}
	}
	/**
	* @param {Batch} batch
	*/
	#resolve(batch) {
		this.is_pending = false;
		batch.transfer_effects(this.#dirty_effects, this.#maybe_dirty_effects);
	}
	/**
	* Defer an effect inside a pending boundary until the boundary resolves
	* @param {Effect} effect
	*/
	defer_effect(effect) {
		defer_effect(effect, this.#dirty_effects, this.#maybe_dirty_effects);
	}
	/**
	* Returns `false` if the effect exists inside a boundary whose pending snippet is shown
	* @returns {boolean}
	*/
	is_rendered() {
		return !this.is_pending && (!this.parent || this.parent.is_rendered());
	}
	has_pending_snippet() {
		return !!this.#props.pending;
	}
	/**
	* @template T
	* @param {() => T} fn
	*/
	#run(fn) {
		var previous_effect = active_effect;
		var previous_reaction = active_reaction;
		var previous_ctx = component_context;
		set_active_effect(this.#effect);
		set_active_reaction(this.#effect);
		set_component_context(this.#effect.ctx);
		try {
			Batch.ensure();
			return fn();
		} finally {
			set_active_effect(previous_effect);
			set_active_reaction(previous_reaction);
			set_component_context(previous_ctx);
		}
	}
	/**
	* Updates the pending count associated with the currently visible pending snippet,
	* if any, such that we can replace the snippet with content once work is done
	* @param {1 | -1} d
	* @param {Batch} batch
	*/
	#update_pending_count(d, batch) {
		if (!this.has_pending_snippet()) {
			if (this.parent) this.parent.#update_pending_count(d, batch);
			return;
		}
		this.#pending_count += d;
		if (this.#pending_count === 0) {
			this.#resolve(batch);
			if (this.#pending_effect) pause_effect(this.#pending_effect, () => {
				this.#pending_effect = null;
			});
			if (this.#offscreen_fragment) {
				this.#anchor.before(this.#offscreen_fragment);
				this.#offscreen_fragment = null;
			}
		}
	}
	/**
	* Update the source that powers `$effect.pending()` inside this boundary,
	* and controls when the current `pending` snippet (if any) is removed.
	* Do not call from inside the class
	* @param {1 | -1} d
	* @param {Batch} batch
	*/
	update_pending_count(d, batch) {
		this.#update_pending_count(d, batch);
		this.#local_pending_count += d;
		if (!this.#effect_pending || this.#pending_count_update_queued) return;
		this.#pending_count_update_queued = true;
		queue_micro_task(() => {
			this.#pending_count_update_queued = false;
			if (this.#effect_pending) internal_set(this.#effect_pending, this.#local_pending_count);
		});
	}
	get_effect_pending() {
		this.#effect_pending_subscriber();
		return get(this.#effect_pending);
	}
	/** @param {unknown} error */
	error(error) {
		if (!this.#props.onerror && !this.#props.failed) throw error;
		if (current_batch?.is_fork) {
			if (this.#main_effect) current_batch.skip_effect(this.#main_effect);
			if (this.#pending_effect) current_batch.skip_effect(this.#pending_effect);
			if (this.#failed_effect) current_batch.skip_effect(this.#failed_effect);
			current_batch.oncommit(() => {
				this.#handle_error(error);
			});
		} else this.#handle_error(error);
	}
	/**
	* @param {unknown} error
	*/
	#handle_error(error) {
		if (this.#main_effect) {
			destroy_effect(this.#main_effect);
			this.#main_effect = null;
		}
		if (this.#pending_effect) {
			destroy_effect(this.#pending_effect);
			this.#pending_effect = null;
		}
		if (this.#failed_effect) {
			destroy_effect(this.#failed_effect);
			this.#failed_effect = null;
		}
		if (hydrating) {
			set_hydrate_node(this.#hydrate_open);
			next();
			set_hydrate_node(skip_nodes());
		}
		let failed = this.#props.failed;
		/** @param {unknown} transformed_error */
		const handle_error_result = (transformed_error) => {
			const { reset, invoke_onerror } = this.#create_reset(transformed_error);
			invoke_onerror();
			if (failed) this.#failed_effect = this.#run(() => {
				try {
					return branch(() => {
						var effect = active_effect;
						effect.b = this;
						effect.f |= 128;
						failed(this.#anchor, () => transformed_error, () => reset);
					});
				} catch (error) {
					invoke_error_boundary(error, this.#effect.parent);
					return null;
				}
			});
		};
		queue_micro_task(() => {
			/** @type {unknown} */
			var result;
			try {
				result = this.transform_error(error);
			} catch (e) {
				invoke_error_boundary(e, this.#effect && this.#effect.parent);
				return;
			}
			if (result !== null && typeof result === "object" && typeof result.then === "function")
 /** @type {any} */ result.then(
				handle_error_result,
				/** @param {unknown} e */
				(e) => invoke_error_boundary(e, this.#effect && this.#effect.parent)
			);
			else handle_error_result(result);
		});
	}
};
/**
* @param {Element} text
* @param {string} value
* @returns {void}
*/
function set_text(text, value) {
	var str = value == null ? "" : typeof value === "object" ? `${value}` : value;
	if (str !== (text[TEXT_CACHE] ??= text.nodeValue)) {
		/** @type {any} */ text[TEXT_CACHE] = str;
		text.nodeValue = `${str}`;
	}
}
/**
* Mounts a component to the given target and returns the exports and potentially the props (if compiled with `accessors: true`) of the component.
* Transitions will play during the initial render unless the `intro` option is set to `false`.
*
* @template {Record<string, any>} Props
* @template {Record<string, any>} Exports
* @param {ComponentType<SvelteComponent<Props>> | Component<Props, Exports, any>} component
* @param {MountOptions<Props>} options
* @returns {Exports}
*/
function mount(component, options) {
	return _mount(component, options);
}
/** @type {Map<EventTarget, Map<string, number>>} */
var listeners = /* @__PURE__ */ new Map();
/**
* @template {Record<string, any>} Exports
* @param {ComponentType<SvelteComponent<any>> | Component<any>} Component
* @param {MountOptions} options
* @returns {Exports}
*/
function _mount(Component, { target, anchor, props = {}, events, context, intro = true, transformError }) {
	init_operations();
	/** @type {Exports} */
	var component = void 0;
	var unmount = component_root(() => {
		var anchor_node = anchor ?? target.appendChild(create_text());
		boundary(anchor_node, { pending: () => {} }, (anchor_node) => {
			push({});
			var ctx = component_context;
			if (context) ctx.c = context;
			if (events)
 /** @type {any} */ props.$$events = events;
			if (hydrating) assign_nodes(anchor_node, null);
			component = Component(anchor_node, props) || mark_as_component();
			if (hydrating) {
				/** @type {Effect & { nodes: EffectNodes }} */ active_effect.nodes.end = hydrate_node;
				if (hydrate_node === null || hydrate_node.nodeType !== 8 || hydrate_node.data !== "]") {
					hydration_mismatch();
					throw HYDRATION_ERROR;
				}
			}
			pop();
		}, transformError);
		/** @type {Set<string>} */
		var registered_events = /* @__PURE__ */ new Set();
		/** @param {Array<string>} events */
		var event_handle = (events) => {
			for (var i = 0; i < events.length; i++) {
				var event_name = events[i];
				if (registered_events.has(event_name)) continue;
				registered_events.add(event_name);
				var passive = is_passive_event(event_name);
				for (const node of [target, document]) {
					var counts = listeners.get(node);
					if (counts === void 0) {
						counts = /* @__PURE__ */ new Map();
						listeners.set(node, counts);
					}
					var count = counts.get(event_name);
					if (count === void 0) {
						node.addEventListener(event_name, handle_event_propagation, { passive });
						counts.set(event_name, 1);
					} else counts.set(event_name, count + 1);
				}
			}
		};
		event_handle(array_from(all_registered_events));
		root_event_handles.add(event_handle);
		return () => {
			for (var event_name of registered_events) for (const node of [target, document]) {
				var counts = listeners.get(node);
				var count = counts.get(event_name);
				if (--count == 0) {
					node.removeEventListener(event_name, handle_event_propagation);
					counts.delete(event_name);
					if (counts.size === 0) listeners.delete(node);
				} else counts.set(event_name, count);
			}
			root_event_handles.delete(event_handle);
			if (anchor_node !== anchor) anchor_node.parentNode?.removeChild(anchor_node);
		};
	});
	mounted_components.set(component, unmount);
	return component;
}
/**
* References of the components that were mounted or hydrated.
* Uses a `WeakMap` to avoid memory leaks.
*/
var mounted_components = /* @__PURE__ */ new WeakMap();
/**
* Unmounts a component that was previously mounted using `mount` or `hydrate`.
*
* Since 5.13.0, if `options.outro` is `true`, [transitions](https://svelte.dev/docs/svelte/transition) will play before the component is removed from the DOM.
*
* Returns a `Promise` that resolves after transitions have completed if `options.outro` is true, or immediately otherwise (prior to 5.13.0, returns `void`).
*
* ```js
* import { mount, unmount } from 'svelte';
* import App from './App.svelte';
*
* const app = mount(App, { target: document.body });
*
* // later...
* unmount(app, { outro: true });
* ```
* @param {Record<string, any>} component
* @param {{ outro?: boolean }} [options]
* @returns {Promise<void>}
*/
function unmount(component, options) {
	const fn = mounted_components.get(component);
	if (fn) {
		mounted_components.delete(component);
		return fn(options);
	}
	return Promise.resolve();
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/dom/blocks/branches.js
/** @import { Effect, TemplateNode } from '#client' */
/**
* @typedef {{ effect: Effect, fragment: DocumentFragment }} Branch
*/
/**
* @template Key
*/
var BranchManager = class {
	/** @type {TemplateNode} */
	anchor;
	/** @type {Map<Batch, Key>} */
	#batches = /* @__PURE__ */ new Map();
	/**
	* Map of keys to effects that are currently rendered in the DOM.
	* These effects are visible and actively part of the document tree.
	* Example:
	* ```
	* {#if condition}
	* 	foo
	* {:else}
	* 	bar
	* {/if}
	* ```
	* Can result in the entries `true->Effect` and `false->Effect`
	* @type {Map<Key, Effect>}
	*/
	#onscreen = /* @__PURE__ */ new Map();
	/**
	* Similar to #onscreen with respect to the keys, but contains branches that are not yet
	* in the DOM, because their insertion is deferred.
	* @type {Map<Key, Branch>}
	*/
	#offscreen = /* @__PURE__ */ new Map();
	/**
	* Keys of effects that are currently outroing
	* @type {Set<Key>}
	*/
	#outroing = /* @__PURE__ */ new Set();
	/**
	* Whether to pause (i.e. outro) on change, or destroy immediately.
	* This is necessary for `<svelte:element>`
	*/
	#transition = true;
	/**
	* @param {TemplateNode} anchor
	* @param {boolean} transition
	*/
	constructor(anchor, transition = true) {
		this.anchor = anchor;
		this.#transition = transition;
	}
	/**
	* @param {Batch} batch
	*/
	#commit = (batch) => {
		if (!this.#batches.has(batch)) return;
		var key = this.#batches.get(batch);
		var onscreen = this.#onscreen.get(key);
		if (onscreen) {
			resume_effect(onscreen);
			this.#outroing.delete(key);
		} else {
			var offscreen = this.#offscreen.get(key);
			if (offscreen) {
				resume_effect(offscreen.effect);
				this.#onscreen.set(key, offscreen.effect);
				this.#offscreen.delete(key);
				/** @type {TemplateNode} */ offscreen.fragment.lastChild.remove();
				this.anchor.before(offscreen.fragment);
				onscreen = offscreen.effect;
			}
		}
		for (const [b, k] of this.#batches) {
			this.#batches.delete(b);
			if (b === batch) break;
			const offscreen = this.#offscreen.get(k);
			if (offscreen) {
				destroy_effect(offscreen.effect);
				this.#offscreen.delete(k);
			}
		}
		for (const [k, effect] of this.#onscreen) {
			if (k === key || this.#outroing.has(k)) continue;
			const on_destroy = () => {
				if (Array.from(this.#batches.values()).includes(k)) {
					var fragment = document.createDocumentFragment();
					move_effect(effect, fragment);
					fragment.append(create_text());
					this.#offscreen.set(k, {
						effect,
						fragment
					});
				} else destroy_effect(effect);
				this.#outroing.delete(k);
				this.#onscreen.delete(k);
			};
			if (this.#transition || !onscreen) {
				this.#outroing.add(k);
				pause_effect(effect, on_destroy, false);
			} else on_destroy();
		}
	};
	/**
	* @param {Batch} batch
	*/
	#discard = (batch) => {
		this.#batches.delete(batch);
		const keys = Array.from(this.#batches.values());
		for (const [k, branch] of this.#offscreen) if (!keys.includes(k)) {
			destroy_effect(branch.effect);
			this.#offscreen.delete(k);
		}
	};
	/**
	*
	* @param {any} key
	* @param {null | ((target: TemplateNode) => void)} fn
	*/
	ensure(key, fn) {
		var batch = current_batch;
		var defer = should_defer_append();
		if (fn && !this.#onscreen.has(key) && !this.#offscreen.has(key)) {
			if (defer) {
				var fragment = document.createDocumentFragment();
				var target = create_text();
				fragment.append(target);
				this.#offscreen.set(key, {
					effect: branch(() => fn(target)),
					fragment
				});
			} else this.#onscreen.set(key, branch(() => fn(this.anchor)));
		}
		this.#batches.set(batch, key);
		if (defer) {
			for (const [k, effect] of this.#onscreen) if (k === key) batch.unskip_effect(effect);
			else batch.skip_effect(effect);
			for (const [k, branch] of this.#offscreen) if (k === key) batch.unskip_effect(branch.effect);
			else batch.skip_effect(branch.effect);
			batch.oncommit(this.#commit);
			batch.ondiscard(this.#discard);
		} else {
			if (hydrating) this.anchor = hydrate_node;
			this.#commit(batch);
		}
	}
};
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/dom/blocks/if.js
/** @import { TemplateNode } from '#client' */
/**
* @param {TemplateNode} node
* @param {(branch: (fn: (anchor: Node) => void, key?: number | false) => void) => void} fn
* @param {boolean} [elseif] True if this is an `{:else if ...}` block rather than an `{#if ...}`, as that affects which transitions are considered 'local'
* @returns {void}
*/
function if_block(node, fn, elseif = false) {
	/** @type {TemplateNode | undefined} */
	var marker;
	if (hydrating) {
		marker = hydrate_node;
		hydrate_next();
	}
	var branches = new BranchManager(node);
	var flags = elseif ? EFFECT_TRANSPARENT : 0;
	/**
	* @param {number | false} key
	* @param {null | ((anchor: Node) => void)} fn
	*/
	function update_branch(key, fn) {
		if (hydrating) {
			var data = read_hydration_instruction(marker);
			if (key !== parseInt(data.substring(1))) {
				var anchor = skip_nodes();
				set_hydrate_node(anchor);
				branches.anchor = anchor;
				set_hydrating(false);
				branches.ensure(key, fn);
				set_hydrating(true);
				return;
			}
		}
		branches.ensure(key, fn);
	}
	block(() => {
		var has_branch = false;
		fn((fn, key = 0) => {
			has_branch = true;
			update_branch(key, fn);
		});
		if (!has_branch) update_branch(-1, null);
	}, flags);
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/dom/blocks/key.js
/** @import { TemplateNode } from '#client' */
var NAN = Symbol("NaN");
/**
* @template V
* @param {TemplateNode} node
* @param {() => V} get_key
* @param {(anchor: Node) => TemplateNode | void} render_fn
* @returns {void}
*/
function key(node, get_key, render_fn) {
	if (hydrating) hydrate_next();
	var branches = new BranchManager(node);
	var legacy = !is_runes();
	block(() => {
		var key = get_key();
		if (key !== key) key = NAN;
		if (legacy && key !== null && typeof key === "object") key = {};
		branches.ensure(key, render_fn);
	});
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/dom/blocks/each.js
/** @import { EachItem, EachOutroGroup, EachState, Effect, EffectNodes, MaybeSource, Source, TemplateNode, TransitionManager, Value } from '#client' */
/** @import { Batch } from '../../reactivity/batch.js'; */
/**
* @param {any} _
* @param {number} i
*/
function index(_, i) {
	return i;
}
/**
* Pause multiple effects simultaneously, and coordinate their
* subsequent destruction. Used in each blocks
* @param {EachState} state
* @param {Effect[]} to_destroy
* @param {null | Node} controlled_anchor
*/
function pause_effects(state, to_destroy, controlled_anchor) {
	/** @type {TransitionManager[]} */
	var transitions = [];
	var length = to_destroy.length;
	/** @type {EachOutroGroup} */
	var group;
	var remaining = to_destroy.length;
	for (var i = 0; i < length; i++) {
		let effect = to_destroy[i];
		pause_effect(effect, () => {
			if (group) {
				group.pending.delete(effect);
				group.done.add(effect);
				if (group.pending.size === 0) {
					var groups = state.outrogroups;
					destroy_effects(state, array_from(group.done));
					groups.delete(group);
					if (groups.size === 0) state.outrogroups = null;
				}
			} else remaining -= 1;
		}, false);
	}
	if (remaining === 0) {
		var fast_path = transitions.length === 0 && controlled_anchor !== null && state.pending.size === 0;
		if (fast_path) {
			var anchor = controlled_anchor;
			var parent_node = anchor.parentNode;
			clear_text_content(parent_node);
			parent_node.append(anchor);
			state.items.clear();
		}
		destroy_effects(state, to_destroy, !fast_path);
	} else {
		group = {
			pending: new Set(to_destroy),
			done: /* @__PURE__ */ new Set()
		};
		(state.outrogroups ??= /* @__PURE__ */ new Set()).add(group);
	}
}
/**
* @param {EachState} state
* @param {Effect[]} to_destroy
* @param {boolean} remove_dom
*/
function destroy_effects(state, to_destroy, remove_dom = true) {
	/** @type {Set<Effect> | undefined} */
	var preserved_effects;
	if (state.pending.size > 0) {
		preserved_effects = /* @__PURE__ */ new Set();
		for (const keys of state.pending.values()) for (const key of keys) preserved_effects.add(
			/** @type {EachItem} */
			state.items.get(key).e
		);
	}
	for (var i = 0; i < to_destroy.length; i++) {
		var e = to_destroy[i];
		if (preserved_effects?.has(e)) {
			e.f |= EFFECT_OFFSCREEN;
			move_effect(e, document.createDocumentFragment());
		} else destroy_effect(to_destroy[i], remove_dom);
	}
}
/** @type {TemplateNode} */
var offscreen_anchor;
/**
* @template V
* @param {Element | Comment} node The next sibling node, or the parent node if this is a 'controlled' block
* @param {number} flags
* @param {() => V[]} get_collection
* @param {(value: V, index: number) => any} get_key
* @param {(anchor: Node, item: MaybeSource<V>, index: MaybeSource<number>) => void} render_fn
* @param {null | ((anchor: Node) => void)} fallback_fn
* @returns {void}
*/
function each(node, flags, get_collection, get_key, render_fn, fallback_fn = null) {
	var anchor = node;
	/** @type {Map<any, EachItem>} */
	var items = /* @__PURE__ */ new Map();
	if ((flags & 4) !== 0) {
		var parent_node = node;
		anchor = hydrating ? set_hydrate_node(/* @__PURE__ */ get_first_child(parent_node)) : parent_node.appendChild(create_text());
	}
	if (hydrating) hydrate_next();
	/** @type {Effect | null} */
	var fallback = null;
	var each_array = /* @__PURE__ */ derived_safe_equal(() => {
		var collection = get_collection();
		return is_array(collection) ? collection : collection == null ? [] : array_from(collection);
	});
	/** @type {V[]} */
	var array;
	/** @type {Map<Batch, Set<any>>} */
	var pending = /* @__PURE__ */ new Map();
	var first_run = true;
	/**
	* @param {Batch} batch
	*/
	function commit(batch) {
		if ((state.effect.f & 16384) !== 0) return;
		state.pending.delete(batch);
		state.fallback = fallback;
		reconcile(state, array, anchor, flags, get_key);
		if (fallback !== null) {
			if (array.length === 0) {
				if ((fallback.f & 33554432) === 0) resume_effect(fallback);
				else {
					fallback.f ^= EFFECT_OFFSCREEN;
					move(fallback, null, anchor);
				}
			} else pause_effect(fallback, () => {
				fallback = null;
			});
		}
	}
	/**
	* @param {Batch} batch
	*/
	function discard(batch) {
		state.pending.delete(batch);
	}
	/** @type {EachState} */
	var state = {
		effect: block(() => {
			array = get(each_array);
			var length = array.length;
			/** `true` if there was a hydration mismatch. Needs to be a `let` or else it isn't treeshaken out */
			let mismatch = false;
			if (hydrating) {
				if (read_hydration_instruction(anchor) === "[!" !== (length === 0)) {
					anchor = skip_nodes();
					set_hydrate_node(anchor);
					set_hydrating(false);
					mismatch = true;
				}
			}
			var keys = /* @__PURE__ */ new Set();
			var batch = current_batch;
			var defer = should_defer_append();
			for (var index = 0; index < length; index += 1) {
				if (hydrating && hydrate_node.nodeType === 8 && hydrate_node.data === "]") {
					anchor = hydrate_node;
					mismatch = true;
					set_hydrating(false);
				}
				var value = array[index];
				var key = get_key(value, index);
				var item = first_run ? null : items.get(key);
				if (item) {
					if (item.v) internal_set(item.v, value);
					if (item.i) internal_set(item.i, index);
					if (defer) batch.unskip_effect(item.e);
				} else {
					item = create_item(items, first_run ? anchor : offscreen_anchor ??= create_text(), value, key, index, render_fn, flags, get_collection);
					if (!first_run) item.e.f |= EFFECT_OFFSCREEN;
					items.set(key, item);
				}
				keys.add(key);
			}
			if (length === 0 && fallback_fn && !fallback) {
				if (first_run) fallback = branch(() => fallback_fn(anchor));
				else {
					fallback = branch(() => fallback_fn(offscreen_anchor ??= create_text()));
					fallback.f |= EFFECT_OFFSCREEN;
				}
			}
			if (length > keys.size) each_key_duplicate("", "", "");
			if (hydrating && length > 0) set_hydrate_node(skip_nodes());
			if (!first_run) {
				pending.set(batch, keys);
				if (defer) {
					for (const [key, item] of items) if (!keys.has(key)) batch.skip_effect(item.e);
					batch.oncommit(commit);
					batch.ondiscard(discard);
				} else commit(batch);
			}
			if (mismatch) set_hydrating(true);
			get(each_array);
		}),
		flags,
		items,
		pending,
		outrogroups: null,
		fallback
	};
	first_run = false;
	if (hydrating) anchor = hydrate_node;
}
/**
* Skip past any non-branch effects (which could be created with `createSubscriber`, for example) to find the next branch effect
* @param {Effect | null} effect
* @returns {Effect | null}
*/
function skip_to_branch(effect) {
	while (effect !== null && (effect.f & 32) === 0) effect = effect.next;
	return effect;
}
/**
* Add, remove, or reorder items output by an each block as its input changes
* @template V
* @param {EachState} state
* @param {Array<V>} array
* @param {Element | Comment | Text} anchor
* @param {number} flags
* @param {(value: V, index: number) => any} get_key
* @returns {void}
*/
function reconcile(state, array, anchor, flags, get_key) {
	var is_animated = (flags & 8) !== 0;
	var length = array.length;
	var items = state.items;
	var current = skip_to_branch(state.effect.first);
	/** @type {undefined | Set<Effect>} */
	var seen;
	/** @type {Effect | null} */
	var prev = null;
	/** @type {undefined | Set<Effect>} */
	var to_animate;
	/** @type {Effect[]} */
	var matched = [];
	/** @type {Effect[]} */
	var stashed = [];
	/** @type {V} */
	var value;
	/** @type {any} */
	var key;
	/** @type {Effect | undefined} */
	var effect;
	/** @type {number} */
	var i;
	if (is_animated) for (i = 0; i < length; i += 1) {
		value = array[i];
		key = get_key(value, i);
		effect = items.get(key).e;
		if ((effect.f & 33554432) === 0) {
			effect.nodes?.a?.measure();
			(to_animate ??= /* @__PURE__ */ new Set()).add(effect);
		}
	}
	for (i = 0; i < length; i += 1) {
		value = array[i];
		key = get_key(value, i);
		effect = items.get(key).e;
		if (state.outrogroups !== null) for (const group of state.outrogroups) {
			group.pending.delete(effect);
			group.done.delete(effect);
		}
		if ((effect.f & 8192) !== 0) {
			resume_effect(effect);
			if (is_animated) {
				effect.nodes?.a?.unfix();
				(to_animate ??= /* @__PURE__ */ new Set()).delete(effect);
			}
		}
		if ((effect.f & 33554432) !== 0) {
			effect.f ^= EFFECT_OFFSCREEN;
			if (effect === current) move(effect, null, anchor);
			else {
				var next = prev ? prev.next : current;
				if (effect === state.effect.last) state.effect.last = effect.prev;
				if (effect.prev) effect.prev.next = effect.next;
				if (effect.next) effect.next.prev = effect.prev;
				link(state, prev, effect);
				link(state, effect, next);
				move(effect, next, anchor);
				prev = effect;
				matched = [];
				stashed = [];
				current = skip_to_branch(prev.next);
				continue;
			}
		}
		if (effect !== current) {
			if (seen !== void 0 && seen.has(effect)) {
				if (matched.length < stashed.length) {
					var start = stashed[0];
					var j;
					prev = start.prev;
					var a = matched[0];
					var b = matched[matched.length - 1];
					for (j = 0; j < matched.length; j += 1) move(matched[j], start, anchor);
					for (j = 0; j < stashed.length; j += 1) seen.delete(stashed[j]);
					link(state, a.prev, b.next);
					link(state, prev, a);
					link(state, b, start);
					current = start;
					prev = b;
					i -= 1;
					matched = [];
					stashed = [];
				} else {
					seen.delete(effect);
					move(effect, current, anchor);
					link(state, effect.prev, effect.next);
					link(state, effect, prev === null ? state.effect.first : prev.next);
					link(state, prev, effect);
					prev = effect;
				}
				continue;
			}
			matched = [];
			stashed = [];
			while (current !== null && current !== effect) {
				(seen ??= /* @__PURE__ */ new Set()).add(current);
				stashed.push(current);
				current = skip_to_branch(current.next);
			}
			if (current === null) continue;
		}
		if ((effect.f & 33554432) === 0) matched.push(effect);
		prev = effect;
		current = skip_to_branch(effect.next);
	}
	if (state.outrogroups !== null) {
		for (const group of state.outrogroups) if (group.pending.size === 0) {
			destroy_effects(state, array_from(group.done));
			state.outrogroups?.delete(group);
		}
		if (state.outrogroups.size === 0) state.outrogroups = null;
	}
	if (current !== null || seen !== void 0) {
		/** @type {Effect[]} */
		var to_destroy = [];
		if (seen !== void 0) {
			for (effect of seen) if ((effect.f & 8192) === 0) to_destroy.push(effect);
		}
		while (current !== null) {
			if ((current.f & 8192) === 0 && current !== state.fallback) to_destroy.push(current);
			current = skip_to_branch(current.next);
		}
		var destroy_length = to_destroy.length;
		if (destroy_length > 0) {
			var controlled_anchor = (flags & 4) !== 0 && length === 0 ? anchor : null;
			if (is_animated) {
				for (i = 0; i < destroy_length; i += 1) to_destroy[i].nodes?.a?.measure();
				for (i = 0; i < destroy_length; i += 1) to_destroy[i].nodes?.a?.fix();
			}
			pause_effects(state, to_destroy, controlled_anchor);
		}
	}
	if (is_animated) queue_micro_task(() => {
		if (to_animate === void 0) return;
		for (effect of to_animate) effect.nodes?.a?.apply();
	});
}
/**
* @template V
* @param {Map<any, EachItem>} items
* @param {Node} anchor
* @param {V} value
* @param {unknown} key
* @param {number} index
* @param {(anchor: Node, item: V | Source<V>, index: number | Value<number>, collection: () => V[]) => void} render_fn
* @param {number} flags
* @param {() => V[]} get_collection
* @returns {EachItem}
*/
function create_item(items, anchor, value, key, index, render_fn, flags, get_collection) {
	var v = (flags & 1) !== 0 ? (flags & 16) === 0 ? /* @__PURE__ */ mutable_source(value, false, false) : source(value) : null;
	var i = (flags & 2) !== 0 ? source(index) : null;
	return {
		v,
		i,
		e: branch(() => {
			render_fn(anchor, v ?? value, i ?? index, get_collection);
			return () => {
				items.delete(key);
			};
		})
	};
}
/**
* @param {Effect} effect
* @param {Effect | null} next
* @param {Text | Element | Comment} anchor
*/
function move(effect, next, anchor) {
	if (!effect.nodes) return;
	var node = effect.nodes.start;
	var end = effect.nodes.end;
	var dest = next && (next.f & 33554432) === 0 ? next.nodes.start : anchor;
	while (node !== null) {
		var next_node = /* @__PURE__ */ get_next_sibling(node);
		dest.before(node);
		if (node === end) return;
		node = next_node;
	}
}
/**
* @param {EachState} state
* @param {Effect | null} prev
* @param {Effect | null} next
*/
function link(state, prev, next) {
	if (prev === null) state.effect.first = next;
	else prev.next = next;
	if (next === null) state.effect.last = prev;
	else next.prev = prev;
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/shared/attributes.js
var whitespace = [..." 	\n\r\f\xA0\v﻿"];
/**
* @param {any} value
* @param {string | null} [hash]
* @param {Record<string, boolean>} [directives]
* @returns {string | null}
*/
function to_class(value, hash, directives) {
	var classname = value == null ? "" : "" + value;
	if (hash) classname = classname ? classname + " " + hash : hash;
	if (directives) {
		for (var key of Object.keys(directives)) if (directives[key]) classname = classname ? classname + " " + key : key;
		else if (classname.length) {
			var len = key.length;
			var a = 0;
			while ((a = classname.indexOf(key, a)) >= 0) {
				var b = a + len;
				if ((a === 0 || whitespace.includes(classname[a - 1])) && (b === classname.length || whitespace.includes(classname[b]))) classname = (a === 0 ? "" : classname.substring(0, a)) + classname.substring(b + 1);
				else a = b;
			}
		}
	}
	return classname === "" ? null : classname;
}
/**
*
* @param {Record<string,any>} styles
* @param {boolean} important
*/
function append_styles(styles, important = false) {
	var separator = important ? " !important;" : ";";
	var css = "";
	for (var key of Object.keys(styles)) {
		var value = styles[key];
		if (value != null && value !== "") css += " " + key + ": " + value + separator;
	}
	return css;
}
/**
* @param {string} name
* @returns {string}
*/
function to_css_name(name) {
	if (name[0] !== "-" || name[1] !== "-") return name.toLowerCase();
	return name;
}
/**
* @param {any} value
* @param {Record<string, any> | [Record<string, any>, Record<string, any>]} [styles]
* @returns {string | null}
*/
function to_style(value, styles) {
	if (styles) {
		var new_style = "";
		/** @type {Record<string,any> | undefined} */
		var normal_styles;
		/** @type {Record<string,any> | undefined} */
		var important_styles;
		if (Array.isArray(styles)) {
			normal_styles = styles[0];
			important_styles = styles[1];
		} else normal_styles = styles;
		if (value) {
			value = String(value).replaceAll(/\/\*.*?\*\//g, "").trim();
			/** @type {boolean | '"' | "'"} */
			var in_str = false;
			var in_apo = 0;
			var in_comment = false;
			var reserved_names = [];
			if (normal_styles) reserved_names.push(...Object.keys(normal_styles).map(to_css_name));
			if (important_styles) reserved_names.push(...Object.keys(important_styles).map(to_css_name));
			var start_index = 0;
			var name_index = -1;
			const len = value.length;
			for (var i = 0; i < len; i++) {
				var c = value[i];
				if (in_comment) {
					if (c === "/" && value[i - 1] === "*") in_comment = false;
				} else if (in_str) {
					if (in_str === c) in_str = false;
				} else if (c === "/" && value[i + 1] === "*") in_comment = true;
				else if (c === "\"" || c === "'") in_str = c;
				else if (c === "(") in_apo++;
				else if (c === ")") in_apo--;
				if (!in_comment && in_str === false && in_apo === 0) {
					if (c === ":" && name_index === -1) name_index = i;
					else if (c === ";" || i === len - 1) {
						if (name_index !== -1) {
							var name = to_css_name(value.substring(start_index, name_index).trim());
							if (!reserved_names.includes(name)) {
								if (c !== ";") i++;
								var property = value.substring(start_index, i).trim();
								new_style += " " + property + ";";
							}
						}
						start_index = i + 1;
						name_index = -1;
					}
				}
			}
		}
		if (normal_styles) new_style += append_styles(normal_styles);
		if (important_styles) new_style += append_styles(important_styles, true);
		new_style = new_style.trim();
		return new_style === "" ? null : new_style;
	}
	return value == null ? null : String(value);
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/dom/elements/class.js
/**
* @param {Element} dom
* @param {boolean | number} is_html
* @param {string | null} value
* @param {string} [hash]
* @param {Record<string, any>} [prev_classes]
* @param {Record<string, any>} [next_classes]
* @returns {Record<string, boolean> | undefined}
*/
function set_class(dom, is_html, value, hash, prev_classes, next_classes) {
	var prev = dom[CLASS_CACHE];
	if (hydrating || prev !== value || prev === void 0) {
		var next_class_name = to_class(value, hash, next_classes);
		if (!hydrating || next_class_name !== dom.getAttribute("class")) {
			if (next_class_name == null) dom.removeAttribute("class");
			else if (is_html) dom.className = next_class_name;
			else dom.setAttribute("class", next_class_name);
		}
		/** @type {any} */ dom[CLASS_CACHE] = value;
	} else if (next_classes && prev_classes !== next_classes) for (var key in next_classes) {
		var is_present = !!next_classes[key];
		if (prev_classes == null || is_present !== !!prev_classes[key]) dom.classList.toggle(key, is_present);
	}
	return next_classes;
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/dom/elements/style.js
/**
* @param {Element & ElementCSSInlineStyle} dom
* @param {Record<string, any>} prev
* @param {Record<string, any>} next
* @param {string} [priority]
*/
function update_styles(dom, prev = {}, next, priority) {
	for (var key in next) {
		var value = next[key];
		if (prev[key] !== value) {
			if (next[key] == null) dom.style.removeProperty(key);
			else dom.style.setProperty(key, value, priority);
		}
	}
}
/**
* @param {Element & ElementCSSInlineStyle} dom
* @param {string | null} value
* @param {Record<string, any> | [Record<string, any>, Record<string, any>]} [prev_styles]
* @param {Record<string, any> | [Record<string, any>, Record<string, any>]} [next_styles]
*/
function set_style(dom, value, prev_styles, next_styles) {
	var prev = dom[STYLE_CACHE];
	if (hydrating || prev !== value) {
		var next_style_attr = to_style(value, next_styles);
		if (!hydrating || next_style_attr !== dom.getAttribute("style")) {
			if (next_style_attr == null) dom.removeAttribute("style");
			else dom.style.cssText = next_style_attr;
		}
		/** @type {any} */ dom[STYLE_CACHE] = value;
	} else if (next_styles) {
		if (Array.isArray(next_styles)) {
			update_styles(dom, prev_styles?.[0], next_styles[0]);
			update_styles(dom, prev_styles?.[1], next_styles[1], "important");
		} else update_styles(dom, prev_styles, next_styles);
	}
	return next_styles;
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/dom/elements/attributes.js
/** @import { Blocker, Effect } from '#client' */
var IS_CUSTOM_ELEMENT = Symbol("is custom element");
var IS_HTML = Symbol("is html");
var LINK_TAG = IS_XHTML ? "link" : "LINK";
var PROGRESS_TAG = IS_XHTML ? "progress" : "PROGRESS";
/**
* The value/checked attribute in the template actually corresponds to the defaultValue property, so we need
* to remove it upon hydration to avoid a bug when someone resets the form value.
* @param {HTMLInputElement} input
* @returns {void}
*/
function remove_input_defaults(input) {
	if (!hydrating) return;
	var already_removed = false;
	var remove_defaults = () => {
		if (already_removed) return;
		already_removed = true;
		if (input.hasAttribute("value")) {
			var value = input.value;
			set_attribute(input, "value", null);
			input.value = value;
		}
		if (input.hasAttribute("checked")) {
			var checked = input.checked;
			set_attribute(input, "checked", null);
			input.checked = checked;
		}
	};
	/** @type {any} */ input[FORM_RESET_HANDLER] = remove_defaults;
	queue_micro_task(remove_defaults);
	add_form_reset_listener();
}
/**
* @param {Element} element
* @param {any} value
*/
function set_value(element, value) {
	var attributes = get_attributes(element);
	if (attributes.value === (attributes.value = value ?? void 0) || element.value === value && (value !== 0 || element.nodeName !== PROGRESS_TAG)) return;
	element.value = value ?? "";
}
/**
* @param {Element} element
* @param {string} attribute
* @param {string | null} value
* @param {boolean} [skip_warning]
*/
function set_attribute(element, attribute, value, skip_warning) {
	var attributes = get_attributes(element);
	if (hydrating) {
		attributes[attribute] = element.getAttribute(attribute);
		if (attribute === "src" || attribute === "srcset" || attribute === "href" && element.nodeName === LINK_TAG) {
			if (!skip_warning);
			return;
		}
	}
	if (attributes[attribute] === (attributes[attribute] = value)) return;
	if (attribute === "loading") element[LOADING_ATTR_SYMBOL] = value;
	if (value == null) element.removeAttribute(attribute);
	else if (typeof value !== "string" && get_setters(element).has(attribute)) element[attribute] = value;
	else element.setAttribute(attribute, value);
}
/**
*
* @param {Element} element
*/
function get_attributes(element) {
	return element[ATTRIBUTES_CACHE] ??= {
		[IS_CUSTOM_ELEMENT]: element.nodeName.includes("-"),
		[IS_HTML]: element.namespaceURI === NAMESPACE_HTML
	};
}
/** @type {Map<string, Set<string>>} */
var setters_cache = /* @__PURE__ */ new Map();
/** @param {Element} element */
function get_setters(element) {
	var cache_key = element.getAttribute("is") || element.nodeName;
	var setters = setters_cache.get(cache_key);
	if (setters) return setters;
	setters_cache.set(cache_key, setters = /* @__PURE__ */ new Set());
	var descriptors;
	var proto = element;
	var element_proto = Element.prototype;
	while (element_proto !== proto) {
		descriptors = get_descriptors(proto);
		for (var key in descriptors) if (descriptors[key].set && key !== "innerHTML" && key !== "textContent" && key !== "innerText") setters.add(key);
		proto = get_prototype_of(proto);
	}
	return setters;
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/dom/elements/bindings/input.js
/** @import { Batch } from '../../../reactivity/batch.js' */
/**
* @param {HTMLInputElement} input
* @param {() => unknown} get
* @param {(value: unknown) => void} set
* @returns {void}
*/
function bind_value(input, get, set = get) {
	var batches = /* @__PURE__ */ new WeakSet();
	listen_to_event_and_reset_event(input, "input", async (is_reset) => {
		/** @type {any} */
		var value = is_reset ? input.defaultValue : input.value;
		value = is_numberlike_input(input) ? to_number(value) : value;
		set(value);
		if (current_batch !== null) batches.add(current_batch);
		await tick();
		if (value !== (value = get())) {
			var start = input.selectionStart;
			var end = input.selectionEnd;
			var length = input.value.length;
			input.value = value ?? "";
			if (end !== null) {
				var new_length = input.value.length;
				if (start === end && end === length && new_length > length) {
					input.selectionStart = new_length;
					input.selectionEnd = new_length;
				} else {
					input.selectionStart = start;
					input.selectionEnd = Math.min(end, new_length);
				}
			}
		}
	});
	if (hydrating && input.defaultValue !== input.value || untrack(get) == null && input.value) {
		set(is_numberlike_input(input) ? to_number(input.value) : input.value);
		if (current_batch !== null) batches.add(current_batch);
	}
	render_effect(() => {
		var value = get();
		if (input === document.activeElement) {
			var batch = async_mode_flag ? previous_batch : current_batch;
			if (batches.has(batch)) return;
		}
		if (is_numberlike_input(input) && value === to_number(input.value)) return;
		if (input.type === "date" && !value && !input.value) return;
		if (value !== input.value) input.value = value ?? "";
	});
}
/**
* @param {HTMLInputElement} input
*/
function is_numberlike_input(input) {
	var type = input.type;
	return type === "number" || type === "range";
}
/**
* @param {string} value
*/
function to_number(value) {
	return value === "" ? null : +value;
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/dom/elements/bindings/this.js
/** @import { ComponentContext, Effect } from '#client' */
/**
* @param {any} bound_value
* @param {Element} element_or_component
* @returns {boolean}
*/
function is_bound_this(bound_value, element_or_component) {
	return bound_value === element_or_component || bound_value?.[STATE_SYMBOL] === element_or_component;
}
/**
* @param {any} element_or_component
* @param {(value: unknown, ...parts: unknown[]) => void} update
* @param {(...parts: unknown[]) => unknown} get_value
* @param {() => unknown[]} [get_parts] Set if the this binding is used inside an each block,
* 										returns all the parts of the each block context that are used in the expression
* @returns {void}
*/
function bind_this(element_or_component = mark_as_component(), update, get_value, get_parts) {
	var component_effect = component_context.r;
	var parent = active_effect;
	effect(() => {
		/** @type {unknown[]} */
		var old_parts;
		/** @type {unknown[]} */
		var parts;
		render_effect(() => {
			old_parts = parts;
			parts = get_parts?.() || [];
			untrack(() => {
				if (!is_bound_this(get_value(...parts), element_or_component)) {
					update(element_or_component, ...parts);
					if (old_parts && is_bound_this(get_value(...old_parts), element_or_component)) update(null, ...old_parts);
				}
			});
		});
		return () => {
			let p = parent;
			while (p !== component_effect && p.parent !== null && p.parent.f & 33554432) p = p.parent;
			const teardown = () => {
				if (parts && is_bound_this(get_value(...parts), element_or_component)) update(null, ...parts);
			};
			const original_teardown = p.teardown;
			p.teardown = () => {
				teardown();
				original_teardown?.();
			};
		};
	});
	return element_or_component;
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/client/reactivity/props.js
/** @import { Derived, Effect, Source } from './types.js' */
/**
* This function is responsible for synchronizing a possibly bound prop with the inner component state.
* It is used whenever the compiler sees that the component writes to the prop, or when it has a default prop_value.
* @template V
* @param {Record<string, unknown>} props
* @param {string} key
* @param {number} flags
* @param {V | (() => V)} [fallback]
* @returns {(() => V | ((arg: V) => V) | ((arg: V, mutation: boolean) => V))}
*/
function prop(props, key, flags, fallback) {
	var runes = !legacy_mode_flag || (flags & 2) !== 0;
	var bindable = (flags & 8) !== 0;
	var lazy = (flags & 16) !== 0;
	var fallback_value = fallback;
	var fallback_dirty = true;
	var fallback_signal = void 0;
	var get_fallback = () => {
		if (lazy && runes) {
			fallback_signal ??= /* @__PURE__ */ derived(fallback);
			return get(fallback_signal);
		}
		if (fallback_dirty) {
			fallback_dirty = false;
			fallback_value = lazy ? untrack(fallback) : fallback;
		}
		return fallback_value;
	};
	/** @type {((v: V) => void) | undefined} */
	let setter;
	if (bindable) {
		var is_entry_props = STATE_SYMBOL in props || LEGACY_PROPS in props;
		setter = get_descriptor(props, key)?.set ?? (is_entry_props && key in props ? (v) => props[key] = v : void 0);
	}
	/** @type {V} */
	var initial_value;
	var is_store_sub = false;
	if (bindable) [initial_value, is_store_sub] = capture_store_binding(() => props[key]);
	else initial_value = props[key];
	if (initial_value === void 0 && fallback !== void 0) {
		initial_value = get_fallback();
		if (setter) {
			if (runes) props_invalid_value(key);
			setter(initial_value);
		}
	}
	/** @type {() => V} */
	var getter;
	if (runes) getter = () => {
		var value = props[key];
		if (value === void 0) return get_fallback();
		fallback_dirty = true;
		return value;
	};
	else getter = () => {
		var value = props[key];
		if (value !== void 0) fallback_value = void 0;
		return value === void 0 ? fallback_value : value;
	};
	if (runes && (flags & 4) === 0) return getter;
	if (setter) {
		var legacy_parent = props.$$legacy;
		return (function(value, mutation) {
			if (arguments.length > 0) {
				if (!runes || !mutation || legacy_parent || is_store_sub)
 /** @type {Function} */ setter(mutation ? getter() : value);
				return value;
			}
			return getter();
		});
	}
	var overridden = false;
	var d = ((flags & 1) !== 0 ? derived : derived_safe_equal)(() => {
		overridden = false;
		return getter();
	});
	if (bindable) get(d);
	var parent_effect = active_effect;
	return (function(value, mutation) {
		if (arguments.length > 0) {
			const new_value = mutation ? get(d) : runes && bindable ? proxy(value) : value;
			set(d, new_value);
			overridden = true;
			if (fallback_value !== void 0) fallback_value = new_value;
			return value;
		}
		if (is_destroying_effect && overridden || (parent_effect.f & 16384) !== 0) return d.v;
		return get(d);
	});
}
if (typeof HTMLElement === "function");
/**
* `onMount`, like [`$effect`](https://svelte.dev/docs/svelte/$effect), schedules a function to run as soon as the component has been mounted to the DOM.
* Unlike `$effect`, the provided function only runs once.
*
* It must be called during the component's initialisation (but doesn't need to live _inside_ the component;
* it can be called from an external module). If a function is returned _synchronously_ from `onMount`,
* it will be called when the component is unmounted.
*
* `onMount` functions do not run during [server-side rendering](https://svelte.dev/docs/svelte/svelte-server#render).
*
* @template T
* @param {() => NotFunction<T> | Promise<NotFunction<T>> | (() => any)} fn
* @returns {void}
*/
function onMount(fn) {
	if (component_context === null) lifecycle_outside_component("onMount");
	if (legacy_mode_flag && component_context.l !== null) init_update_callbacks(component_context).m.push(fn);
	else user_effect(() => {
		const cleanup = untrack(fn);
		if (typeof cleanup === "function") return cleanup;
	});
}
/**
* Schedules a callback to run immediately before the component is unmounted.
*
* Out of `onMount`, `beforeUpdate`, `afterUpdate` and `onDestroy`, this is the
* only one that runs inside a server-side component.
*
* @param {() => any} fn
* @returns {void}
*/
function onDestroy(fn) {
	if (component_context === null) lifecycle_outside_component("onDestroy");
	onMount(() => () => untrack(fn));
}
/**
* Legacy-mode: Init callbacks object for onMount/beforeUpdate/afterUpdate
* @param {ComponentContext} context
*/
function init_update_callbacks(context) {
	var l = context.l;
	return l.u ??= {
		a: [],
		b: [],
		m: []
	};
}
//#endregion
//#region ../../../../../crates/clients/web/node_modules/svelte/src/internal/disclose-version.js
if (typeof window !== "undefined") ((window.__svelte ??= {}).v ??= /* @__PURE__ */ new Set()).add("5");
//#endregion
//#region browser/runtime/context.ts
var DISPATCH_CONTEXT = Symbol("ryeos-ui-dispatch");
function provideDispatchUi(dispatch) {
	setContext(DISPATCH_CONTEXT, dispatch);
}
function dispatchUi() {
	return getContext(DISPATCH_CONTEXT);
}
//#endregion
//#region browser/app/Navigation.svelte
var root$20 = /* @__PURE__ */ from_html(`<button><span class="navigation-glyph" aria-hidden="true">◇</span> <span class="navigation-copy"><strong> </strong></span></button>`);
var root_1$15 = /* @__PURE__ */ from_html(`<button aria-label="Close view navigation"></button> <aside id="ryeos-navigation" aria-label="RyeOS view navigation"><div class="navigation-heading">Explorer <span> </span></div> <nav></nav></aside>`, 1);
function Navigation($$anchor, $$props) {
	push($$props, true);
	const dispatch = dispatchUi();
	let panel;
	user_effect(() => {
		if ($$props.open) tick().then(() => {
			panel?.querySelector("button:not([disabled])")?.focus();
		});
	});
	function handleKeys(event) {
		if (!$$props.open) return;
		if (event.key === "Escape") {
			event.preventDefault();
			$$props.onClose();
			return;
		}
		if (event.key !== "Tab") return;
		const controls = Array.from(panel.querySelectorAll("button:not([disabled]), [href], [tabindex]:not([tabindex='-1'])"));
		if (controls.length === 0) return;
		const first = controls.at(0);
		const last = controls.at(-1);
		if (!first || !last) return;
		if (event.shiftKey && document.activeElement === first) {
			event.preventDefault();
			last.focus();
		} else if (!event.shiftKey && document.activeElement === last) {
			event.preventDefault();
			first.focus();
		}
	}
	var fragment = root_1$15();
	event("keydown", $window, handleKeys);
	var button = first_child(fragment);
	let classes;
	var aside = sibling(button, 2);
	let classes_1;
	var div = child(aside);
	var text = only_child(sibling(child(div)), true);
	reset(div);
	var nav = sibling(div, 2);
	each(nav, 21, () => $$props.model.items, (item) => item.id, ($$anchor, item) => {
		var button_1 = root$20();
		let classes_2;
		var span_1 = sibling(child(button_1), 2);
		var text_1 = only_child(child(span_1), true);
		reset(span_1);
		reset(button_1);
		template_effect(() => {
			set_attribute(button_1, "aria-current", get(item).selected ? "page" : void 0);
			classes_2 = set_class(button_1, 1, "", null, classes_2, { active: get(item).selected });
			set_text(text_1, get(item).label);
		});
		delegated("click", button_1, () => {
			dispatch({
				type: "activate",
				intent: get(item).intent
			});
			$$props.onClose();
		});
		append($$anchor, button_1);
	});
	reset(nav);
	reset(aside);
	bind_this(aside, ($$value) => panel = $$value, () => panel);
	template_effect(($0) => {
		classes = set_class(button, 1, "navigation-scrim", null, classes, { open: $$props.open });
		set_attribute(button, "tabindex", $$props.open ? 0 : -1);
		classes_1 = set_class(aside, 1, "navigation", null, classes_1, { open: $$props.open });
		set_text(text, $0);
	}, [() => String($$props.model.items.length).padStart(2, "0")]);
	delegated("click", button, function(...$$args) {
		$$props.onClose?.apply(this, $$args);
	});
	append($$anchor, fragment);
	pop();
}
delegate(["click"]);
//#endregion
//#region browser/app/Notices.svelte
var root$19 = /* @__PURE__ */ from_html(`<div class="notice"><span> </span> <button aria-label="Dismiss notice">×</button></div>`);
var root_1$14 = /* @__PURE__ */ from_html(`<aside class="notice-stack" aria-label="RyeOS notices" aria-live="polite" aria-atomic="false"></aside>`);
function Notices($$anchor, $$props) {
	push($$props, true);
	const dispatch = dispatchUi();
	var fragment = comment();
	var node = first_child(fragment);
	var consequent = ($$anchor) => {
		var aside = root_1$14();
		each(aside, 21, () => $$props.notices, (notice) => notice.id, ($$anchor, notice) => {
			var div = root$19();
			var span = child(div);
			var text = only_child(span, true);
			var button = sibling(span, 2);
			reset(div);
			template_effect(() => {
				set_attribute(div, "data-tone", get(notice).tone);
				set_attribute(div, "role", get(notice).tone === "danger" ? "alert" : "status");
				set_text(text, get(notice).message);
				set_attribute(button, "data-focus-key", `notice:${get(notice).id}:dismiss`);
			});
			delegated("click", button, () => dispatch({
				type: "dismiss_notice",
				id: get(notice).id
			}));
			append($$anchor, div);
		});
		reset(aside);
		append($$anchor, aside);
	};
	if_block(node, ($$render) => {
		if ($$props.notices.length > 0) $$render(consequent);
	});
	append($$anchor, fragment);
	pop();
}
delegate(["click"]);
//#endregion
//#region browser/components/InputComposer.svelte
var root$18 = /* @__PURE__ */ from_html(`<div class="composer-route"><span class="route-state">●</span><span> </span><span class="draft-state">DRAFT</span></div>`);
var root_1$13 = /* @__PURE__ */ from_html(`<button>↑</button>`);
var root_2$10 = /* @__PURE__ */ from_html(`<section><!> <textarea></textarea> <div class="composer-actions"><span> </span><!></div></section>`);
function InputComposer($$anchor, $$props) {
	push($$props, true);
	const dispatch = dispatchUi();
	let composing = false;
	function cursorBytes(text, utf16Offset) {
		return BigInt(new TextEncoder().encode(text.slice(0, utf16Offset)).length);
	}
	function emitInput(target) {
		if (composing) return;
		dispatch({
			type: "input_at",
			address: $$props.model.address,
			action: {
				type: "set_text",
				text: target.value,
				cursor: cursorBytes(target.value, target.selectionStart ?? target.value.length)
			}
		});
	}
	var section = root_2$10();
	let classes;
	var node = child(section);
	var consequent = ($$anchor) => {
		var div = root$18();
		var text_1 = only_child(sibling(child(div)), true);
		next();
		reset(div);
		template_effect(() => set_text(text_1, $$props.model.route_label));
		append($$anchor, div);
	};
	if_block(node, ($$render) => {
		if (!$$props.model.live_filter) $$render(consequent);
	});
	var textarea = sibling(node, 2);
	remove_textarea_child(textarea);
	var div_1 = sibling(textarea, 2);
	var span_1 = child(div_1);
	var text_2 = only_child(span_1, true);
	var node_1 = sibling(span_1);
	var consequent_1 = ($$anchor) => {
		var button = root_1$13();
		template_effect(() => {
			set_attribute(button, "aria-label", `Send to ${$$props.model.route_label}`);
			button.disabled = !$$props.model.submit_enabled;
		});
		delegated("click", button, () => dispatch({
			type: "input_at",
			address: $$props.model.address,
			action: {
				type: "submit",
				interrupt: false
			}
		}));
		append($$anchor, button);
	};
	if_block(node_1, ($$render) => {
		if (!$$props.model.live_filter) $$render(consequent_1);
	});
	reset(div_1);
	reset(section);
	template_effect(() => {
		classes = set_class(section, 1, "composer", null, classes, { "live-filter": $$props.model.live_filter });
		set_attribute(section, "aria-label", $$props.model.route_label);
		set_attribute(textarea, "data-focus-key", `input:${$$props.model.address.buffer.view_instance_key}:${$$props.model.address.buffer.input_id}`);
		set_value(textarea, $$props.model.text);
		set_attribute(textarea, "placeholder", $$props.model.placeholder);
		set_attribute(textarea, "aria-label", $$props.model.route_label);
		set_text(text_2, $$props.model.hint);
	});
	event("focus", textarea, () => dispatch({
		type: "input_at",
		address: $$props.model.address,
		action: { type: "focus" }
	}));
	event("compositionstart", textarea, () => composing = true);
	event("compositionend", textarea, (event) => {
		composing = false;
		emitInput(event.currentTarget);
	});
	delegated("input", textarea, (event) => emitInput(event.currentTarget));
	delegated("keydown", textarea, (event) => {
		if (composing || event.isComposing) return;
		if ((event.key === "ArrowUp" || event.key === "ArrowDown") && $$props.model.live_filter && !event.ctrlKey && !event.altKey && !event.metaKey && !event.shiftKey) {
			event.preventDefault();
			event.stopPropagation();
			dispatch({
				type: "input_at",
				address: $$props.model.address,
				action: {
					type: "navigate_selection",
					delta: event.key === "ArrowDown" ? 1 : -1
				}
			});
		} else if (event.key === "Enter" && $$props.model.live_filter) {
			event.preventDefault();
			event.stopPropagation();
			dispatch({
				type: "input_at",
				address: $$props.model.address,
				action: { type: "activate_selection" }
			});
		} else if (event.key === "Enter" && !event.shiftKey && $$props.model.submit_enabled) {
			event.preventDefault();
			dispatch({
				type: "input_at",
				address: $$props.model.address,
				action: {
					type: "submit",
					interrupt: event.altKey
				}
			});
		}
	});
	append($$anchor, section);
	pop();
}
delegate([
	"input",
	"keydown",
	"click"
]);
//#endregion
//#region browser/components/EmptyState.svelte
var root$17 = /* @__PURE__ */ from_html(`<div class="empty-state"><span>◇</span><strong> </strong><p> </p></div>`);
function EmptyState($$anchor, $$props) {
	var div = root$17();
	var strong = sibling(child(div));
	var text = only_child(strong, true);
	var text_1 = only_child(sibling(strong), true);
	reset(div);
	template_effect(() => {
		set_text(text, $$props.title);
		set_text(text_1, $$props.message);
	});
	append($$anchor, div);
}
//#endregion
//#region browser/components/RowDetails.svelte
var root$16 = /* @__PURE__ */ from_html(`<div><dt> </dt> <dd> </dd></div>`);
var root_1$12 = /* @__PURE__ */ from_html(`<dl class="row-details"></dl>`);
function RowDetails($$anchor, $$props) {
	push($$props, true);
	let label = prop($$props, "label", 3, "Details");
	var fragment = comment();
	var node = first_child(fragment);
	var consequent = ($$anchor) => {
		var dl = root_1$12();
		each(dl, 23, () => $$props.details, (detail, index) => `${detail.field}:${index}`, ($$anchor, detail) => {
			var div = root$16();
			var dt = child(div);
			var text = only_child(dt, true);
			var text_1 = only_child(sibling(dt, 2), true);
			reset(div);
			template_effect(() => {
				set_attribute(div, "data-tone", get(detail).tone ?? void 0);
				set_text(text, get(detail).field || "detail");
				set_text(text_1, get(detail).value);
			});
			append($$anchor, div);
		});
		reset(dl);
		template_effect(() => set_attribute(dl, "aria-label", label()));
		append($$anchor, dl);
	};
	if_block(node, ($$render) => {
		if ($$props.details.length) $$render(consequent);
	});
	append($$anchor, fragment);
	pop();
}
//#endregion
//#region browser/visuals/ryeos_field_accessibility.js
function fieldAccessibilityModel(vm, instanceKey) {
	const byId = new Map((vm.entities || []).map((entity) => [entity.id, entity]));
	const groups = new Map((vm.groups || []).map((group) => [group.id, group]));
	const neighbors = new Map((vm.entities || []).map((entity) => [entity.id, []]));
	for (const relation of vm.relations || []) {
		if (neighbors.has(relation.source_id) && byId.has(relation.target_id)) neighbors.get(relation.source_id).push(`${relation.kind} to ${byId.get(relation.target_id).label}`);
		if (neighbors.has(relation.target_id) && byId.has(relation.source_id)) neighbors.get(relation.target_id).push(`${relation.kind} from ${byId.get(relation.source_id).label}`);
	}
	const ordered = (vm.traversal || []).map((id) => byId.get(id)).filter(Boolean);
	return ordered.map((entity, index) => {
		const group = entity.group_id ? groups.get(entity.group_id) : null;
		return {
			id: entity.id,
			domId: `ryeos-field-option-${safeId(instanceKey)}--${safeId(entity.id)}`,
			label: entity.accessibility_label || entity.label || entity.id,
			selected: vm.selected === entity.id,
			position: index + 1,
			size: ordered.length,
			level: entityLevel(entity, byId),
			groupId: group?.id || null,
			groupLabel: group?.label || null,
			expanded: group ? !group.collapsed : null,
			neighbors: (neighbors.get(entity.id) || []).sort().join("; "),
			selectIntent: entity.select_intent || null,
			activateIntent: entity.activate_intent || null,
			compare: entity.compare_available
		};
	});
}
function entityLevel(entity, byId) {
	let level = 1;
	let parentId = entity.parent_id;
	const visited = /* @__PURE__ */ new Set([entity.id]);
	while (parentId && byId.has(parentId) && !visited.has(parentId)) {
		visited.add(parentId);
		level += 1;
		parentId = byId.get(parentId).parent_id;
	}
	return level;
}
function safeId(value) {
	return [...String(value)].map((character) => character.codePointAt(0).toString(16)).join("-");
}
//#endregion
//#region browser/visuals/ryeos_field_layout.js
var GROUP_WIDTH = 520;
var RANK_SPACING = 210;
var GROUP_RIGHT_GUTTER = 300;
var GROUP_HEADER_HEIGHT = 26;
var LARGE_FIELD_ENTITY_COUNT = 240;
var LARGE_FIELD_RELATION_COUNT = 720;
var StaleFieldLayoutError = class extends Error {
	constructor() {
		super("field layout superseded");
		this.name = "StaleFieldLayoutError";
	}
};
function layoutField(vm, previous = /* @__PURE__ */ new Map()) {
	const prepared = prepareField(vm);
	return placeField(vm, prepared, stronglyConnectedComponents(prepared.ids, prepared.relations), previous);
}
async function layoutFieldChunked(vm, previous = /* @__PURE__ */ new Map(), { schedule = nextFrame, isStale = () => false } = {}) {
	const prepared = prepareField(vm);
	if (!isLarge(prepared)) return layoutField(vm, previous);
	await schedule();
	if (isStale()) throw new StaleFieldLayoutError();
	const components = stronglyConnectedComponents(prepared.ids, prepared.relations);
	await schedule();
	if (isStale()) throw new StaleFieldLayoutError();
	const layout = placeField(vm, prepared, components, previous);
	if (isStale()) throw new StaleFieldLayoutError();
	return layout;
}
function settleLayout(layout, amount = 1) {
	for (const node of layout.nodes.values()) {
		node.x += (node.targetX - node.x) * amount;
		node.y += (node.targetY - node.y) * amount;
	}
	for (const edge of layout.edges) edge.points = routeEdge(layout.nodes.get(edge.relation.source_id), layout.nodes.get(edge.relation.target_id));
	layout.groups = groupBounds(layout.groupDefinitions, layout.nodes, layout.groupOrigins);
	return layout;
}
function hitTest(layout, x, y) {
	return [...layout.nodes.values()].reverse().find((node) => Math.abs(x - node.x) <= node.width / 2 && Math.abs(y - node.y) <= node.height / 2) || null;
}
function hitTestGroup(layout, x, y) {
	return [...layout.groups].reverse().find((group) => x >= group.x && x <= group.x + group.width && y >= group.y && y <= group.y + GROUP_HEADER_HEIGHT) || null;
}
function fieldLayoutIsLarge(vm) {
	return (vm.entities || []).length >= LARGE_FIELD_ENTITY_COUNT || (vm.relations || []).length >= LARGE_FIELD_RELATION_COUNT;
}
function fieldLayoutMembershipKey(vm) {
	return JSON.stringify(visibleEntities(vm).map((entity) => entity.id));
}
function rebindFieldLayout(layout, vm) {
	const entities = new Map((vm?.entities || []).map((entity) => [entity.id, entity]));
	const relations = new Map((vm?.relations || []).map((relation) => [relation.id, relation]));
	for (const node of layout.nodes.values()) node.entity = entities.get(node.id) || node.entity;
	for (const edge of layout.edges) edge.relation = relations.get(edge.relation.id) || edge.relation;
	return layout;
}
function prepareField(vm) {
	const visible = visibleEntities(vm);
	const ids = visible.map((entity) => entity.id);
	const idSet = new Set(ids);
	return {
		visible,
		ids,
		relations: (vm.relations || []).filter((relation) => idSet.has(relation.source_id) && idSet.has(relation.target_id))
	};
}
function placeField(vm, prepared, components, previous) {
	const { visible, relations } = prepared;
	const componentById = /* @__PURE__ */ new Map();
	components.forEach((members, index) => {
		members.forEach((id) => componentById.set(id, index));
	});
	const componentRanks = rankComponents(components, componentById, relations, visible);
	const groupDefinitions = (vm.groups || []).map((group) => ({ ...group }));
	const groupOrder = new Map(groupDefinitions.map((group, index) => [group.id, index]));
	const laneNames = [...new Set(visible.map((entity) => entity.lane || "").filter(Boolean))].sort();
	const laneOrder = new Map(laneNames.map((lane, index) => [lane, index]));
	const buckets = /* @__PURE__ */ new Map();
	for (const entity of visible) {
		const key = `${groupOrder.get(entity.group_id) ?? groupOrder.size}\0${Number.isFinite(entity.rank) ? Number(entity.rank) : componentRanks.get(componentById.get(entity.id)) || 0}\0${laneOrder.get(entity.lane) ?? laneOrder.size}`;
		if (!buckets.has(key)) buckets.set(key, []);
		buckets.get(key).push(entity);
	}
	for (const entities of buckets.values()) entities.sort((left, right) => number(left.order) - number(right.order) || left.id.localeCompare(right.id));
	const maximumRankByGroup = /* @__PURE__ */ new Map();
	groupDefinitions.forEach((_, index) => maximumRankByGroup.set(index, 0));
	for (const key of buckets.keys()) {
		const [groupText, rankText] = key.split("\0");
		const group = Number(groupText);
		const rank = Number(rankText);
		maximumRankByGroup.set(group, Math.max(maximumRankByGroup.get(group) ?? 0, rank));
	}
	const groupOrigins = /* @__PURE__ */ new Map();
	let nextGroupOrigin = 0;
	for (const group of [...maximumRankByGroup.keys()].sort((left, right) => left - right)) {
		groupOrigins.set(group, nextGroupOrigin);
		nextGroupOrigin += Math.max(GROUP_WIDTH, (maximumRankByGroup.get(group) || 0) * RANK_SPACING + GROUP_RIGHT_GUTTER);
	}
	const nodes = /* @__PURE__ */ new Map();
	const incoming = incomingSources(relations);
	for (const [key, entities] of [...buckets.entries()].sort(([left], [right]) => left.localeCompare(right))) {
		const [groupText, rankText, laneText] = key.split("\0");
		const group = Number(groupText);
		const rank = Number(rankText);
		const lane = Number(laneText);
		entities.forEach((entity, index) => {
			const targetX = 120 + (groupOrigins.get(group) || 0) + rank * RANK_SPACING;
			const targetY = 90 + lane * 150 + index * 86;
			const seed = seedPosition(entity, previous, incoming, targetX, targetY);
			nodes.set(entity.id, {
				id: entity.id,
				entity,
				x: seed.x,
				y: seed.y,
				targetX,
				targetY,
				width: entity.traits?.shape === "aggregate" ? 156 : 136,
				height: entity.traits?.shape === "grid" ? 74 : 52,
				componentSize: components[componentById.get(entity.id)]?.length || 1
			});
		});
	}
	const edges = relations.map((relation) => ({
		relation,
		points: routeEdge(nodes.get(relation.source_id), nodes.get(relation.target_id))
	}));
	const groups = groupBounds(groupDefinitions, nodes, groupOrigins);
	return {
		nodes,
		edges,
		groups,
		groupDefinitions,
		groupOrigins,
		width: Math.max(640, ...[...nodes.values()].map((node) => node.targetX + node.width + 100), ...groups.map((group) => group.x + group.width + 40)),
		height: Math.max(360, ...[...nodes.values()].map((node) => node.targetY + node.height + 100), ...groups.map((group) => group.y + group.height + 40))
	};
}
function visibleEntities(vm) {
	const hidden = new Set((vm.layers || []).filter((layer) => !layer.visible).map((layer) => layer.id));
	const collapsed = new Set((vm.groups || []).filter((group) => group.collapsed).map((group) => group.id));
	return (vm.entities || []).filter((entity) => {
		if (entity.selected) return true;
		if (entity.group_id && collapsed.has(entity.group_id)) return false;
		if (!(entity.layer_ids || []).length) return true;
		return entity.layer_ids.some((layer) => !hidden.has(layer));
	});
}
function seedPosition(entity, previous, incoming, targetX, targetY) {
	const prior = previous.get(entity.id);
	if (prior) return {
		x: prior.x,
		y: prior.y
	};
	const parent = entity.parent_id && previous.get(entity.parent_id);
	if (parent) return {
		x: parent.x,
		y: parent.y
	};
	for (const sourceId of incoming.get(entity.id) || []) {
		const source = previous.get(sourceId);
		if (source) return {
			x: source.x,
			y: source.y
		};
	}
	return {
		x: targetX - 28,
		y: targetY
	};
}
function incomingSources(relations) {
	const incoming = /* @__PURE__ */ new Map();
	for (const relation of relations) {
		if (!incoming.has(relation.target_id)) incoming.set(relation.target_id, []);
		incoming.get(relation.target_id).push(relation.source_id);
	}
	for (const sources of incoming.values()) sources.sort();
	return incoming;
}
function stronglyConnectedComponents(ids, relations) {
	const graph = new Map(ids.map((id) => [id, []]));
	for (const relation of relations) graph.get(relation.source_id)?.push(relation.target_id);
	for (const targets of graph.values()) targets.sort();
	let index = 0;
	const stack = [];
	const onStack = /* @__PURE__ */ new Set();
	const indices = /* @__PURE__ */ new Map();
	const low = /* @__PURE__ */ new Map();
	const components = [];
	const visit = (id) => {
		indices.set(id, index);
		low.set(id, index);
		index += 1;
		stack.push(id);
		onStack.add(id);
		for (const next of graph.get(id) || []) if (!indices.has(next)) {
			visit(next);
			low.set(id, Math.min(low.get(id), low.get(next)));
		} else if (onStack.has(next)) low.set(id, Math.min(low.get(id), indices.get(next)));
		if (low.get(id) === indices.get(id)) {
			const component = [];
			let member;
			do {
				member = stack.pop();
				onStack.delete(member);
				component.push(member);
			} while (member !== id);
			component.sort();
			components.push(component);
		}
	};
	[...ids].sort().forEach((id) => {
		if (!indices.has(id)) visit(id);
	});
	return components;
}
function rankComponents(components, componentById, relations, entities) {
	const ranks = /* @__PURE__ */ new Map();
	const authored = new Map(entities.filter((entity) => Number.isFinite(entity.rank)).map((entity) => [componentById.get(entity.id), Number(entity.rank)]));
	const incoming = new Map(components.map((_, index) => [index, /* @__PURE__ */ new Set()]));
	const outgoing = new Map(components.map((_, index) => [index, /* @__PURE__ */ new Set()]));
	for (const relation of relations) {
		const source = componentById.get(relation.source_id);
		const target = componentById.get(relation.target_id);
		if (source === target) continue;
		outgoing.get(source).add(target);
		incoming.get(target).add(source);
	}
	const remainingIncoming = new Map([...incoming].map(([component, parents]) => [component, new Set(parents)]));
	const queue = components.map((_, index) => index).filter((index) => remainingIncoming.get(index).size === 0).sort((left, right) => left - right);
	while (queue.length) {
		const component = queue.shift();
		const rank = authored.get(component) ?? Math.max(0, ...[...incoming.get(component)].map((parent) => (ranks.get(parent) || 0) + 1));
		ranks.set(component, rank);
		for (const next of [...outgoing.get(component)].sort((left, right) => left - right)) {
			remainingIncoming.get(next).delete(component);
			if (remainingIncoming.get(next).size === 0) queue.push(next);
		}
		queue.sort((left, right) => left - right);
	}
	components.forEach((_, index) => {
		if (!ranks.has(index)) ranks.set(index, authored.get(index) || 0);
	});
	return ranks;
}
function routeEdge(source, target) {
	if (!source || !target) return [];
	const middle = (source.x + target.x) / 2;
	return [
		[source.x + source.width / 2, source.y],
		[middle, source.y],
		[middle, target.y],
		[target.x - target.width / 2, target.y]
	];
}
function groupBounds(groups, nodes, groupOrigins = /* @__PURE__ */ new Map()) {
	return groups.map((group, index) => {
		const members = [...nodes.values()].filter((node) => node.entity.group_id === group.id);
		if (!members.length) return {
			id: group.id,
			label: group.label,
			collapsed: !!group.collapsed,
			x: 20 + (groupOrigins.get(index) ?? index * GROUP_WIDTH),
			y: 20,
			width: 200,
			height: 38
		};
		const minX = Math.min(...members.map((node) => node.x - node.width / 2)) - 32;
		const maxX = Math.max(...members.map((node) => node.x + node.width / 2)) + 32;
		const minY = Math.min(...members.map((node) => node.y - node.height / 2)) - 42;
		const maxY = Math.max(...members.map((node) => node.y + node.height / 2)) + 28;
		return {
			id: group.id,
			label: group.label,
			collapsed: !!group.collapsed,
			x: minX,
			y: minY,
			width: maxX - minX,
			height: maxY - minY
		};
	});
}
function isLarge(prepared) {
	return prepared.visible.length >= LARGE_FIELD_ENTITY_COUNT || prepared.relations.length >= LARGE_FIELD_RELATION_COUNT;
}
function nextFrame() {
	return new Promise((resolve) => {
		if (globalThis.requestAnimationFrame) requestAnimationFrame(() => resolve());
		else setTimeout(resolve, 0);
	});
}
function number(value) {
	return Number.isFinite(value) ? Number(value) : 0;
}
//#endregion
//#region browser/visuals/ryeos_field_canvas.js
var TONES = {
	good: "#8ec07c",
	warn: "#fabd2f",
	danger: "#fb4934",
	accent: "#d65d0e",
	neutral: "#a89984"
};
var HIGH_CONTRAST_TONES = {
	good: "CanvasText",
	warn: "CanvasText",
	danger: "CanvasText",
	accent: "Highlight",
	neutral: "CanvasText"
};
var FieldCanvasController = class {
	constructor(canvas, interactions) {
		this.canvas = canvas;
		this.context = canvas.getContext?.("2d") || null;
		this.interactions = interactions;
		this.layout = emptyLayout();
		this.structuralRevision = null;
		this.viewport = {
			x: 20,
			y: 20,
			zoom: 1
		};
		this.frame = null;
		this.drag = null;
		this.layoutGeneration = 0;
		this.layoutCount = 0;
		this.tombstones = /* @__PURE__ */ new Map();
		this.unmounted = false;
		this.wireEvents();
	}
	update(vm) {
		this.vm = vm;
		this.captureExited(vm);
		const layoutRevision = `${vm.structural_revision || ""}\0${fieldLayoutMembershipKey(vm)}`;
		if (this.structuralRevision !== layoutRevision) {
			this.structuralRevision = layoutRevision;
			this.scheduleLayout(vm);
			return;
		}
		rebindFieldLayout(this.layout, vm);
		this.draw();
	}
	resize() {
		const rect = this.canvas.getBoundingClientRect?.() || {
			width: 640,
			height: 360
		};
		const ratio = globalThis.devicePixelRatio || 1;
		const width = Math.max(1, Math.floor(rect.width * ratio));
		const height = Math.max(1, Math.floor(rect.height * ratio));
		if (this.canvas.width !== width || this.canvas.height !== height) {
			this.canvas.width = width;
			this.canvas.height = height;
		}
		this.draw();
	}
	draw() {
		const context = this.context;
		if (!context) return;
		const ratio = globalThis.devicePixelRatio || 1;
		const highContrast = preference("(forced-colors: active)");
		context.setTransform(ratio * this.viewport.zoom, 0, 0, ratio * this.viewport.zoom, ratio * this.viewport.x, ratio * this.viewport.y);
		context.clearRect(-this.viewport.x / this.viewport.zoom, -this.viewport.y / this.viewport.zoom, this.canvas.width / ratio / this.viewport.zoom, this.canvas.height / ratio / this.viewport.zoom);
		drawGroups(context, this.layout.groups, highContrast);
		const changes = new Map((this.vm?.changes || []).map((change) => [change.id, change]));
		drawRelations(context, this.layout.edges, changes, this.viewport.zoom, highContrast);
		for (const node of this.layout.nodes.values()) drawEntity(context, node, changes.get(node.id), this.viewport.zoom, highContrast);
		drawTombstones(context, this.tombstones, highContrast);
	}
	scheduleLayout(vm) {
		const token = ++this.layoutGeneration;
		const previous = new Map(this.layout.nodes);
		const commit = (layout) => {
			if (this.unmounted || token !== this.layoutGeneration) return;
			rebindFieldLayout(layout, this.vm);
			this.layout = layout;
			this.layoutCount += 1;
			this.animate(token);
		};
		if (!fieldLayoutIsLarge(vm)) {
			commit(layoutField(vm, previous));
			return;
		}
		layoutFieldChunked(vm, previous, { isStale: () => this.unmounted || token !== this.layoutGeneration }).then(commit).catch((error) => {
			if (!(error instanceof StaleFieldLayoutError)) {
				this.structuralRevision = null;
				this.canvas.dataset.layoutError = String(error?.message || error);
			}
		});
	}
	animate(token) {
		this.cancelAnimation();
		if (preference("(prefers-reduced-motion: reduce)")) {
			settleLayout(this.layout, 1);
			this.draw();
			this.tombstones.clear();
			return;
		}
		let remaining = this.tombstones.size ? 22 : 12;
		const tick = () => {
			if (this.unmounted || token !== this.layoutGeneration) return;
			settleLayout(this.layout, remaining <= 1 ? 1 : .24);
			fadeTombstones(this.tombstones, remaining);
			this.draw();
			remaining -= 1;
			this.frame = remaining > 0 && globalThis.requestAnimationFrame ? requestAnimationFrame(tick) : null;
		};
		tick();
	}
	captureExited(vm) {
		for (const change of vm.changes || []) {
			if (change.kind !== "exited" || !change.tombstone) continue;
			const prior = this.layout.nodes.get(change.id);
			if (!prior) continue;
			this.tombstones.set(change.id, {
				id: change.id,
				label: change.tombstone.label || change.id,
				traits: change.tombstone.traits || {},
				x: prior.x,
				y: prior.y,
				width: prior.width,
				height: prior.height,
				alpha: .72
			});
		}
	}
	cancelAnimation() {
		if (this.frame != null && globalThis.cancelAnimationFrame) cancelAnimationFrame(this.frame);
		this.frame = null;
	}
	unmount() {
		this.unmounted = true;
		this.layoutGeneration += 1;
		this.cancelAnimation();
		this.canvas.onpointerdown = null;
		this.canvas.onpointermove = null;
		this.canvas.onpointerup = null;
		this.canvas.onpointercancel = null;
		this.canvas.onwheel = null;
		this.tombstones.clear();
	}
	wireEvents() {
		this.canvas.onpointerdown = (event) => {
			const point = this.fieldPoint(event);
			const hit = hitTest(this.layout, point.x, point.y);
			if (hit) {
				this.interactions.entity({
					entityId: hit.id,
					compare: event.shiftKey,
					activate: event.detail >= 2
				});
				return;
			}
			const group = hitTestGroup(this.layout, point.x, point.y);
			if (group) {
				this.interactions.group({
					groupId: group.id,
					collapsed: !group.collapsed
				});
				return;
			}
			this.drag = {
				pointerId: event.pointerId,
				clientX: event.clientX,
				clientY: event.clientY,
				viewportX: this.viewport.x,
				viewportY: this.viewport.y
			};
			this.canvas.setPointerCapture?.(event.pointerId);
		};
		this.canvas.onpointermove = (event) => {
			if (!this.drag || this.drag.pointerId !== event.pointerId) return;
			this.viewport.x = this.drag.viewportX + event.clientX - this.drag.clientX;
			this.viewport.y = this.drag.viewportY + event.clientY - this.drag.clientY;
			this.draw();
		};
		const end = (event) => {
			if (this.drag?.pointerId === event.pointerId) this.drag = null;
		};
		this.canvas.onpointerup = end;
		this.canvas.onpointercancel = end;
		this.canvas.onwheel = (event) => {
			event.preventDefault();
			const rect = this.canvas.getBoundingClientRect();
			const screenX = event.clientX - rect.left;
			const screenY = event.clientY - rect.top;
			const fieldX = (screenX - this.viewport.x) / this.viewport.zoom;
			const fieldY = (screenY - this.viewport.y) / this.viewport.zoom;
			const zoom = Math.max(.25, Math.min(3.5, this.viewport.zoom * Math.exp(-event.deltaY * .001)));
			this.viewport.zoom = zoom;
			this.viewport.x = screenX - fieldX * zoom;
			this.viewport.y = screenY - fieldY * zoom;
			this.draw();
		};
	}
	fieldPoint(event) {
		const rect = this.canvas.getBoundingClientRect();
		return {
			x: (event.clientX - rect.left - this.viewport.x) / this.viewport.zoom,
			y: (event.clientY - rect.top - this.viewport.y) / this.viewport.zoom
		};
	}
};
function drawGroups(context, groups, highContrast) {
	context.font = "12px ui-monospace, monospace";
	for (const group of groups) {
		context.strokeStyle = highContrast ? "CanvasText" : "rgba(168,153,132,.52)";
		context.setLineDash([6, 5]);
		context.strokeRect(group.x, group.y, group.width, group.height);
		context.setLineDash([]);
		context.fillStyle = highContrast ? "Canvas" : "rgba(29,32,33,.94)";
		context.fillRect(group.x, group.y, group.width, 26);
		context.fillStyle = highContrast ? "CanvasText" : "#a89984";
		context.textAlign = "left";
		context.textBaseline = "alphabetic";
		context.fillText(`${group.collapsed ? "▸" : "▾"} ${group.label || group.id}`, group.x + 8, group.y + 17);
	}
}
function drawRelations(context, edges, changes, zoom, highContrast) {
	for (const edge of edges) {
		if (edge.points.length < 2) continue;
		const relation = edge.relation;
		if (zoom < .4 && !relation.selected && relation.emphasis !== "strong" && relation.motion !== "flow") continue;
		const tones = highContrast ? HIGH_CONTRAST_TONES : TONES;
		context.strokeStyle = tones[relation.tone] || tones.neutral;
		context.lineWidth = changes.get(relation.id) ? 4 : relation.emphasis === "strong" ? 2.5 : relation.emphasis === "quiet" ? .7 : 1.3;
		context.setLineDash(relation.stroke === "dashed" ? [7, 5] : relation.stroke === "dotted" ? [2, 4] : []);
		context.beginPath();
		edge.points.forEach(([x, y], index) => index ? context.lineTo(x, y) : context.moveTo(x, y));
		context.stroke();
	}
	context.setLineDash([]);
}
function drawEntity(context, node, change, zoom, highContrast) {
	const entity = node.entity;
	const tones = highContrast ? HIGH_CONTRAST_TONES : TONES;
	const tone = tones[entity.tone] || tones.neutral;
	context.save();
	context.translate(node.x, node.y);
	context.fillStyle = entity.traits?.fill === "hollow" ? highContrast ? "Canvas" : "#1d2021" : tone;
	context.strokeStyle = entity.selected ? highContrast ? "Highlight" : "#ebdbb2" : tone;
	context.lineWidth = entity.selected ? 4 : 2;
	context.setLineDash(entity.traits?.stroke === "dashed" ? [7, 4] : entity.traits?.stroke === "dotted" ? [2, 3] : []);
	if (change) {
		context.save();
		context.globalAlpha = .32;
		context.strokeStyle = tone;
		context.lineWidth = 8;
		entityPath(context, entity.traits?.shape, node.width + 14, node.height + 14);
		context.stroke();
		context.restore();
	}
	const width = zoom < .4 ? Math.min(36, node.width) : node.width;
	const height = zoom < .4 ? Math.min(24, node.height) : node.height;
	entityPath(context, entity.traits?.shape, width, height);
	context.fill();
	context.stroke();
	context.setLineDash([]);
	if (zoom >= .55) {
		if (node.componentSize > 1) {
			context.fillStyle = highContrast ? "CanvasText" : "#fabd2f";
			context.fillText(`↻${node.componentSize}`, node.width / 2 - 24, -node.height / 2 + 13);
		}
		context.fillStyle = entity.traits?.fill === "hollow" ? highContrast ? "CanvasText" : "#ebdbb2" : highContrast ? "Canvas" : "#1d2021";
		context.textAlign = "center";
		context.textBaseline = "middle";
		context.font = "600 12px ui-monospace, monospace";
		context.fillText(clipLabel(entity.label || entity.id, zoom >= 1.25 ? 34 : 20), 0, -4);
		if (zoom >= .9) {
			context.font = "10px ui-monospace, monospace";
			context.fillText(entity.status || entity.kind || "", 0, 12);
		}
	}
	context.restore();
}
function drawTombstones(context, tombstones, highContrast) {
	for (const tombstone of tombstones.values()) {
		context.save();
		context.globalAlpha = tombstone.alpha;
		context.translate(tombstone.x, tombstone.y);
		context.strokeStyle = highContrast ? "CanvasText" : TONES.neutral;
		context.fillStyle = highContrast ? "Canvas" : "#1d2021";
		context.setLineDash([4, 5]);
		entityPath(context, tombstone.traits?.shape, tombstone.width, tombstone.height);
		context.fill();
		context.stroke();
		context.fillStyle = highContrast ? "CanvasText" : "#a89984";
		context.font = "11px ui-monospace, monospace";
		context.textAlign = "center";
		context.fillText(clipLabel(tombstone.label, 20), 0, 3);
		context.restore();
	}
}
function fadeTombstones(tombstones, remaining) {
	for (const [id, tombstone] of tombstones) {
		tombstone.alpha = Math.min(.72, remaining / 22);
		if (remaining <= 1) tombstones.delete(id);
	}
}
function entityPath(context, shape, width, height) {
	context.beginPath();
	if (shape === "disc" || shape === "ring" || shape === "dot") context.ellipse(0, 0, width / 2, height / 2, 0, 0, Math.PI * 2);
	else if (shape === "diamond") {
		context.moveTo(0, -height / 2);
		context.lineTo(width / 2, 0);
		context.lineTo(0, height / 2);
		context.lineTo(-width / 2, 0);
		context.closePath();
	} else if (shape === "hex") {
		for (let index = 0; index < 6; index += 1) {
			const angle = Math.PI / 3 * index;
			const x = Math.cos(angle) * width / 2;
			const y = Math.sin(angle) * height / 2;
			if (index) context.lineTo(x, y);
			else context.moveTo(x, y);
		}
		context.closePath();
	} else context.roundRect(-width / 2, -height / 2, width, height, shape === "capsule" ? height / 2 : 7);
}
function emptyLayout() {
	return {
		nodes: /* @__PURE__ */ new Map(),
		edges: [],
		groups: [],
		groupDefinitions: [],
		width: 640,
		height: 360
	};
}
function preference(query) {
	return !!globalThis.matchMedia?.(query)?.matches;
}
function clipLabel(value, length) {
	return value.length <= length ? value : `${value.slice(0, length - 1)}…`;
}
//#endregion
//#region browser/views/field_preview_groups.js
function fieldPreviewGroups(field) {
	const entities = new Map(field.entities.map((entity) => [entity.id, entity]));
	const previewsFor = (entity) => {
		const retainedIds = new Set(entity.preview_ids);
		return field.previews.filter((preview) => preview.entity_id === entity.id && retainedIds.has(preview.id));
	};
	const selected = field.selected ? entities.get(field.selected) : null;
	const compared = field.compare.flatMap((id) => {
		const entity = entities.get(id);
		return entity ? [{
			entity,
			previews: previewsFor(entity)
		}] : [];
	});
	return {
		selectedPreviews: selected ? previewsFor(selected) : [],
		compared
	};
}
//#endregion
//#region browser/views/field_empty_state.js
/** Describe only the Rust-projected source state of a zero-entity Field. */
function fieldEmptyState(field) {
	if (field.entities.length !== 0) return null;
	const sources = field.sources;
	if (sources.some((source) => source.phase === "error" || source.error)) return {
		tone: "error",
		title: "Source evidence failed",
		detail: "No entities are shown. Inspect Evidence for the source error."
	};
	if (sources.some((source) => source.phase === "unresolved")) return {
		tone: "pending",
		title: "Source unresolved",
		detail: "No entities are shown for the current subject selection."
	};
	if (sources.some((source) => source.phase === "loading" || source.phase === "refreshing")) return {
		tone: "pending",
		title: "Source evidence pending",
		detail: "No entities are shown while source evidence is loading or refreshing."
	};
	if (sources.some((source) => source.truncated)) return {
		tone: "bounded",
		title: "Bounded projection",
		detail: "No entities are shown in this bounded source response; retained evidence may extend beyond it."
	};
	if (sources.length > 0 && sources.every((source) => source.phase === "ready")) return {
		tone: "empty",
		title: "No entities in this projection",
		detail: "This does not establish that no retained run or result exists."
	};
	return {
		tone: "unknown",
		title: "No entities projected",
		detail: "Source availability is not established here. Inspect Evidence for retained context."
	};
}
//#endregion
//#region browser/visuals/ryeos_grid_canvas.js
function drawIndexedGrid(canvas, preview, options = {}) {
	const grid = preview?.grid;
	const context = canvas?.getContext?.("2d");
	if (!grid || !context || !grid.width || !grid.height) return false;
	const scale = Math.max(1, Number(options.scale || 10));
	canvas.width = grid.width * scale;
	canvas.height = grid.height * scale;
	const palette = new Map((grid.palette || []).map((entry) => [entry.index, entry]));
	const changed = new Set(grid.changed || []);
	for (let index = 0; index < grid.cells.length; index += 1) {
		const entry = palette.get(grid.cells[index]);
		if (!entry) continue;
		const x = index % grid.width * scale;
		const y = Math.floor(index / grid.width) * scale;
		context.fillStyle = entry.color || "#a89984";
		context.fillRect(x, y, scale, scale);
		if (entry.glyph && scale >= 7) {
			context.fillStyle = contrastingTextColor(entry.color);
			context.font = `${Math.max(6, Math.floor(scale * .72))}px monospace`;
			context.textAlign = "center";
			context.textBaseline = "middle";
			context.fillText(entry.glyph, x + scale / 2, y + scale / 2, scale);
		}
		if (changed.has(index)) {
			context.strokeStyle = options.changedColor || "#fb4934";
			context.lineWidth = Math.max(1, scale / 5);
			context.strokeRect(x + 1, y + 1, scale - 2, scale - 2);
		}
	}
	return true;
}
function indexedGridAccessibilityLabel(preview, label = preview?.label || preview?.id || "Grid") {
	const grid = preview?.grid;
	if (!grid) return label;
	const glyphs = new Map((grid.palette || []).map((entry) => [entry.index, entry.glyph]));
	const cells = (grid.cells || []).slice(0, 64).map((index) => glyphs.get(index) || String(index));
	const suffix = (grid.cells || []).length > cells.length ? "; remaining cells available in the grid transcript" : "";
	return `${label}; ${grid.width} by ${grid.height} indexed grid; cells ${cells.join(" ")}${suffix}`;
}
function indexedGridTranscriptPage(preview, requestedRow = 0, pageSize = 16) {
	const grid = preview?.grid;
	if (!grid || !Number.isInteger(grid.width) || !Number.isInteger(grid.height) || grid.width < 1 || grid.height < 1 || grid.width > 512 || grid.height > 512 || !Array.isArray(grid.cells) || grid.cells.length > 262144 || grid.cells.length !== grid.width * grid.height) return null;
	const size = Number.isInteger(pageSize) && pageSize > 0 ? Math.min(pageSize, 16) : 16;
	const start = Number.isInteger(requestedRow) ? Math.max(0, Math.min(requestedRow, grid.height - 1)) : 0;
	const end = Math.min(start + size, grid.height);
	const palette = new Map((grid.palette || []).map((entry) => [entry.index, entry]));
	const changed = new Set(grid.changed || []);
	const rows = [];
	for (let row = start; row < end; row += 1) {
		const cells = [];
		for (let column = 0; column < grid.width; column += 1) {
			const offset = row * grid.width + column;
			const index = grid.cells[offset];
			const entry = palette.get(index);
			const name = entry?.label || entry?.glyph || String(index);
			cells.push(`column ${column + 1}: ${name}${changed.has(offset) ? " (changed)" : ""}`);
		}
		rows.push({
			number: row + 1,
			cells
		});
	}
	return {
		rows,
		start,
		end,
		height: grid.height
	};
}
function contrastingTextColor(color) {
	const match = /^#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/i.exec(color || "");
	if (!match) return "CanvasText";
	return (Number.parseInt(match[1], 16) * 299 + Number.parseInt(match[2], 16) * 587 + Number.parseInt(match[3], 16) * 114) / 1e3 >= 140 ? "#111111" : "#ffffff";
}
//#endregion
//#region browser/views/GridPreview.svelte
var root$15 = /* @__PURE__ */ from_html(`<p><strong> </strong><span> </span></p>`);
var root_1$11 = /* @__PURE__ */ from_html(`<section class="field-grid-transcript"><div class="field-grid-transcript-nav"><button type="button">Previous rows</button> <span aria-live="polite"> </span> <button type="button">Next rows</button></div> <div class="field-grid-transcript-rows"></div></section>`);
var root_2$9 = /* @__PURE__ */ from_html(`<figure class="field-preview"><figcaption> </figcaption> <canvas></canvas> <button type="button" class="field-grid-transcript-toggle"> </button> <!></figure>`);
function GridPreview($$anchor, $$props) {
	push($$props, true);
	let canvas;
	let transcriptOpen = /* @__PURE__ */ state(false);
	let transcriptRow = /* @__PURE__ */ state(0);
	const label = /* @__PURE__ */ user_derived(() => indexedGridAccessibilityLabel($$props.preview, $$props.preview.label));
	const transcript = /* @__PURE__ */ user_derived(() => get(transcriptOpen) ? indexedGridTranscriptPage($$props.preview, get(transcriptRow)) : null);
	onMount(() => drawIndexedGrid(canvas, $$props.preview, { scale: 9 }));
	user_effect(() => {
		if (canvas) drawIndexedGrid(canvas, $$props.preview, { scale: 9 });
	});
	var figure = root_2$9();
	var figcaption = child(figure);
	var text = only_child(figcaption, true);
	var canvas_1 = sibling(figcaption, 2);
	bind_this(canvas_1, ($$value) => canvas = $$value, () => canvas);
	var button = sibling(canvas_1, 2);
	var text_1 = only_child(button, true);
	var node = sibling(button, 2);
	var consequent = ($$anchor) => {
		var section = root_1$11();
		var div = child(section);
		var button_1 = child(div);
		var span = sibling(button_1, 2);
		var text_2 = only_child(span);
		var button_2 = sibling(span, 2);
		reset(div);
		var div_1 = sibling(div, 2);
		each(div_1, 21, () => get(transcript).rows, (row) => row.number, ($$anchor, row) => {
			var p = root$15();
			var strong = child(p);
			var text_3 = only_child(strong);
			var text_4 = only_child(sibling(strong), true);
			reset(p);
			template_effect(($0) => {
				set_text(text_3, `Row ${get(row).number ?? ""}`);
				set_text(text_4, $0);
			}, [() => get(row).cells.join(" | ")]);
			append($$anchor, p);
		});
		reset(div_1);
		reset(section);
		template_effect(() => {
			set_attribute(section, "aria-label", `${$$props.preview.label} cells`);
			button_1.disabled = get(transcript).start === 0;
			set_text(text_2, `Rows ${get(transcript).start + 1}–${get(transcript).end ?? ""} of ${get(transcript).height ?? ""}`);
			button_2.disabled = get(transcript).end === get(transcript).height;
		});
		delegated("click", button_1, () => {
			set(transcriptRow, Math.max(0, get(transcript).start - 16), true);
		});
		delegated("click", button_2, () => {
			set(transcriptRow, get(transcript).end, true);
		});
		append($$anchor, section);
	};
	if_block(node, ($$render) => {
		if (get(transcriptOpen) && get(transcript)) $$render(consequent);
	});
	reset(figure);
	template_effect(() => {
		set_text(text, $$props.preview.label);
		set_attribute(canvas_1, "aria-label", get(label));
		set_attribute(canvas_1, "title", $$props.compareEnabled ? "Shift-click to compare" : void 0);
		set_attribute(button, "aria-expanded", get(transcriptOpen));
		set_text(text_1, get(transcriptOpen) ? "Hide grid cells" : "Read grid cells");
	});
	delegated("click", canvas_1, (event) => {
		if (event.shiftKey && $$props.compareEnabled) $$props.oncompare();
	});
	delegated("click", button, () => {
		set(transcriptOpen, !get(transcriptOpen));
		set(transcriptRow, 0);
	});
	append($$anchor, figure);
	pop();
}
delegate(["click"]);
//#endregion
//#region browser/views/FieldView.svelte
var root$14 = /* @__PURE__ */ from_html(`<button> </button>`);
var root_1$10 = /* @__PURE__ */ from_html(`<button>Activate</button>`);
var root_2$8 = /* @__PURE__ */ from_html(`<button>Clear</button>`);
var root_3$8 = /* @__PURE__ */ from_html(`<!> <button> </button> <button> </button> <!>`, 1);
var root_4$6 = /* @__PURE__ */ from_html(`<nav class="field-rail" aria-label="Durable execution events"></nav>`);
var root_5$5 = /* @__PURE__ */ from_html(`<div class="field-empty-state" role="status"><strong> </strong><p> </p></div>`);
var root_6$5 = /* @__PURE__ */ from_html(`<b>bounded</b>`);
var root_7$5 = /* @__PURE__ */ from_html(`<b>error</b>`);
var root_8$3 = /* @__PURE__ */ from_html(`<span> <small> </small><!><!></span>`);
var root_9$3 = /* @__PURE__ */ from_html(`<span><b>subjects</b></span>`);
var root_10$3 = /* @__PURE__ */ from_html(`<span><b>metrics</b></span>`);
var root_11$2 = /* @__PURE__ */ from_html(`<span><b>provenance</b></span>`);
var root_12$2 = /* @__PURE__ */ from_html(`<span><b>outside cut</b></span>`);
var root_13$2 = /* @__PURE__ */ from_html(`<span><b>bounded search</b></span>`);
var root_14$1 = /* @__PURE__ */ from_html(`<span><b>bounded expansion</b></span>`);
var root_15$1 = /* @__PURE__ */ from_html(`<p class="field-source-coordinate">Subject fingerprint <code> </code></p>`);
var root_16$1 = /* @__PURE__ */ from_html(`<p>Truncated response — this is not the complete retained field.</p>`);
var root_17$1 = /* @__PURE__ */ from_html(`<p class="field-source-error"> </p>`);
var root_18$1 = /* @__PURE__ */ from_html(`<small> </small>`);
var root_19$1 = /* @__PURE__ */ from_html(`<p> </p>`);
var root_20$1 = /* @__PURE__ */ from_html(`<section><strong> </strong> <p class="field-source-coordinate">Source <code> </code></p> <!> <!> <!> <!> <!> <!></section>`);
var root_21$1 = /* @__PURE__ */ from_html(`<br/>Definition <code> </code>`, 1);
var root_22$1 = /* @__PURE__ */ from_html(`<br/>Effective digest <code> </code>`, 1);
var root_23$1 = /* @__PURE__ */ from_html(`<li><code> </code> <code> </code> <!> <!></li>`);
var root_24$1 = /* @__PURE__ */ from_html(`<section aria-label="Projected source subjects"><strong>Projected source subjects</strong> <ul></ul></section>`);
var root_25$1 = /* @__PURE__ */ from_html(`<span> </span>`);
var root_26$1 = /* @__PURE__ */ from_html(`<li class="field-metric"><span> </span> <code> </code><!></li>`);
var root_27$1 = /* @__PURE__ */ from_html(`<section aria-label="Projected field metrics"><strong>Projected field metrics</strong> <ul></ul></section>`);
var root_28$1 = /* @__PURE__ */ from_html(`<li><code> </code></li>`);
var root_29$1 = /* @__PURE__ */ from_html(`<section aria-label="Projected entity and relation source references"><strong>Projected entity/relation source refs</strong> <ul></ul></section>`);
var root_30$1 = /* @__PURE__ */ from_html(`<p>Expansion is bounded — additional retained evidence may exist.</p>`);
var root_31$1 = /* @__PURE__ */ from_html(`<section aria-label="Selected entity expansion evidence"><strong> </strong> <p> </p> <!></section>`);
var root_32 = /* @__PURE__ */ from_html(`<li> </li>`);
var root_33 = /* @__PURE__ */ from_html(`<section aria-label="Concurrent events outside replay cut"><strong>Outside replay cut</strong> <p>Concurrent retained context is not included in the selected braid cut.</p> <ul></ul></section>`);
var root_34 = /* @__PURE__ */ from_html(`<section aria-label="Bounded field search"><strong>Bounded search results</strong> <p>The visible matches are not the complete matching field.</p></section>`);
var root_35 = /* @__PURE__ */ from_html(`<details><summary><span>Evidence</span> <!> <!> <!> <!> <!> <!> <!></summary> <div class="field-source-evidence"><!> <!> <!> <!> <!> <!> <!></div></details>`);
var root_36 = /* @__PURE__ */ from_html(`<div role="treeitem" tabindex="-1"> </div>`);
var root_37 = /* @__PURE__ */ from_html(`<dl class="row-details" aria-label="Retained artifact coordinate"><div><dt>Thread</dt><dd> </dd></div> <div><dt>Artifact</dt><dd> </dd></div> <div><dt>Content hash</dt><dd> </dd></div></dl>`);
var root_38 = /* @__PURE__ */ from_html(`<p class="field-preview-unavailable">This projected preview has no grid payload.</p>`);
var root_39 = /* @__PURE__ */ from_html(`<strong> </strong><small> </small> <!> <!> <!> <!>`, 1);
var root_40 = /* @__PURE__ */ from_html(`<p class="field-preview-unavailable">No grid preview is present in this projection.</p>`);
var root_41 = /* @__PURE__ */ from_html(`<article><h3> </h3><small><code> </code> </small> <!></article>`);
var root_42 = /* @__PURE__ */ from_html(`<section class="field-comparison" aria-label="Compared field entities"><strong>Compared entities</strong> <!></section>`);
var root_43 = /* @__PURE__ */ from_html(`<small class="field-warning"> </small>`);
var root_44 = /* @__PURE__ */ from_html(`<aside class="field-detail"><!> <!> <!></aside>`);
var root_45 = /* @__PURE__ */ from_html(`<section class="field-view"><header class="field-toolbar"><div class="field-identity"><strong> </strong><span> </span></div> <div class="field-controls"><button>◀</button> <button> </button> <button>▶</button> <button>Live</button> <!> <!> <!></div> <input type="search" placeholder="Search field" aria-label="Search field entities"/></header> <!> <div class="field-stage"><canvas aria-hidden="true"></canvas> <!></div> <!> <div class="field-accessibility" role="tree" tabindex="0"></div> <!></section>`);
function FieldView($$anchor, $$props) {
	push($$props, true);
	const dispatch = dispatchUi();
	let canvas;
	let controller = null;
	const selected = /* @__PURE__ */ user_derived(() => $$props.field.entities.find((entity) => entity.id === $$props.field.selected) ?? null);
	const selectedRelations = /* @__PURE__ */ user_derived(() => get(selected) ? $$props.field.relations.filter((relation) => relation.source_id === get(selected).id || relation.target_id === get(selected).id) : []);
	const previewGroups = /* @__PURE__ */ user_derived(() => fieldPreviewGroups($$props.field));
	const emptyState = /* @__PURE__ */ user_derived(() => fieldEmptyState($$props.field));
	const expansion = /* @__PURE__ */ user_derived(() => get(selected) ? $$props.field.expansions.find((item) => item.source === get(selected).source && item.root_id === get(selected).id) ?? null : null);
	const accessibilityItems = /* @__PURE__ */ user_derived(() => fieldAccessibilityModel($$props.field, $$props.instanceKey));
	const activeOptionId = /* @__PURE__ */ user_derived(() => get(accessibilityItems).find((item) => item.selected)?.domId);
	const selectEntity = (entityId) => dispatch({
		type: "set_field_selection",
		instance_key: $$props.instanceKey,
		entity_id: entityId
	});
	const activateEntity = (entityId) => {
		const entity = $$props.field.entities.find((candidate) => candidate.id === entityId);
		if (entity?.activate_intent) dispatch({
			type: "activate",
			intent: entity.activate_intent
		});
	};
	const compareEntity = (entityId) => {
		if ($$props.field.entities.find((candidate) => candidate.id === entityId)?.compare_available) dispatch({
			type: "toggle_field_compare",
			instance_key: $$props.instanceKey,
			entity_id: entityId
		});
	};
	onMount(() => {
		controller = new FieldCanvasController(canvas, {
			entity: ({ entityId, compare, activate }) => {
				selectEntity(entityId);
				if (compare) compareEntity(entityId);
				if (activate) activateEntity(entityId);
			},
			group: ({ groupId, collapsed }) => dispatch({
				type: "set_field_group_collapsed",
				instance_key: $$props.instanceKey,
				group_id: groupId,
				collapsed
			})
		});
		const observer = new ResizeObserver(() => controller?.resize());
		observer.observe(canvas);
		controller.update($$props.field);
		controller.resize();
		return () => {
			observer.disconnect();
			controller?.unmount();
			controller = null;
		};
	});
	user_effect(() => controller?.update($$props.field));
	var section = root_45();
	var header = child(section);
	var div = child(header);
	var strong = child(div);
	var text = only_child(strong, true);
	var text_1 = only_child(sibling(strong), true);
	reset(div);
	var div_1 = sibling(div, 2);
	var button = child(div_1);
	var button_1 = sibling(button, 2);
	var text_2 = only_child(button_1, true);
	var button_2 = sibling(button_1, 2);
	var button_3 = sibling(button_2, 2);
	var node = sibling(button_3, 2);
	each(node, 17, () => $$props.field.groups, (group) => group.id, ($$anchor, group) => {
		var button_4 = root$14();
		var text_3 = only_child(button_4);
		template_effect(() => set_text(text_3, `${get(group).collapsed ? "▸" : "▾"} ${get(group).label ?? ""}`));
		delegated("click", button_4, () => dispatch({
			type: "set_field_group_collapsed",
			instance_key: $$props.instanceKey,
			group_id: get(group).id,
			collapsed: !get(group).collapsed
		}));
		append($$anchor, button_4);
	});
	var node_1 = sibling(node, 2);
	each(node_1, 17, () => $$props.field.layers, (layer) => layer.id, ($$anchor, layer) => {
		var button_5 = root$14();
		var text_4 = only_child(button_5);
		template_effect(() => {
			set_attribute(button_5, "aria-pressed", get(layer).visible);
			set_text(text_4, `${get(layer).visible ? "●" : "○"} ${get(layer).label ?? ""}`);
		});
		delegated("click", button_5, () => dispatch({
			type: "set_field_layer_visible",
			instance_key: $$props.instanceKey,
			layer_id: get(layer).id,
			visible: !get(layer).visible
		}));
		append($$anchor, button_5);
	});
	var node_2 = sibling(node_1, 2);
	var consequent_2 = ($$anchor) => {
		var fragment = root_3$8();
		var node_3 = first_child(fragment);
		var consequent = ($$anchor) => {
			var button_6 = root_1$10();
			template_effect(() => set_attribute(button_6, "aria-label", `Activate ${get(selected).label}`));
			delegated("click", button_6, () => activateEntity(get(selected).id));
			append($$anchor, button_6);
		};
		if_block(node_3, ($$render) => {
			if (get(selected).activate_intent) $$render(consequent);
		});
		var button_7 = sibling(node_3, 2);
		var text_5 = only_child(button_7, true);
		var button_8 = sibling(button_7, 2);
		var text_6 = only_child(button_8, true);
		var node_4 = sibling(button_8, 2);
		var consequent_1 = ($$anchor) => {
			var button_9 = root_2$8();
			delegated("click", button_9, () => dispatch({
				type: "clear_field_expansion",
				instance_key: $$props.instanceKey,
				source: get(selected).source,
				root_id: get(selected).id
			}));
			append($$anchor, button_9);
		};
		if_block(node_4, ($$render) => {
			if (get(expansion)) $$render(consequent_1);
		});
		template_effect(($0) => {
			button_7.disabled = !get(selected).compare_available;
			set_text(text_5, $0);
			button_8.disabled = !!get(expansion) && !get(expansion).can_continue;
			set_text(text_6, get(expansion)?.can_continue ? "Continue" : get(expansion) ? "Expanded" : "Expand");
		}, [() => $$props.field.compare.includes(get(selected).id) ? "Uncompare" : "Compare"]);
		delegated("click", button_7, () => compareEntity(get(selected).id));
		delegated("click", button_8, () => dispatch({
			type: get(expansion)?.can_continue ? "continue_field_expansion" : "request_field_expansion",
			instance_key: $$props.instanceKey,
			source: get(selected).source,
			root_id: get(selected).id
		}));
		append($$anchor, fragment);
	};
	if_block(node_2, ($$render) => {
		if (get(selected)) $$render(consequent_2);
	});
	reset(div_1);
	var input = sibling(div_1, 2);
	remove_input_defaults(input);
	reset(header);
	var node_5 = sibling(header, 2);
	var consequent_3 = ($$anchor) => {
		var nav = root_4$6();
		each(nav, 21, () => $$props.field.replay.rail, index, ($$anchor, entry) => {
			var button_10 = root$14();
			let classes;
			var text_7 = only_child(button_10, true);
			template_effect(() => {
				set_attribute(button_10, "aria-pressed", get(entry).selected);
				classes = set_class(button_10, 1, "", null, classes, { selected: get(entry).selected });
				set_text(text_7, get(entry).label);
			});
			delegated("click", button_10, () => dispatch({
				type: "set_field_cursor",
				instance_key: $$props.instanceKey,
				cursor: {
					mode: "braid_cut",
					anchor: get(entry).event
				}
			}));
			append($$anchor, button_10);
		});
		reset(nav);
		append($$anchor, nav);
	};
	if_block(node_5, ($$render) => {
		if ($$props.field.replay.rail.length) $$render(consequent_3);
	});
	var div_2 = sibling(node_5, 2);
	var canvas_1 = child(div_2);
	bind_this(canvas_1, ($$value) => canvas = $$value, () => canvas);
	var node_6 = sibling(canvas_1, 2);
	var consequent_4 = ($$anchor) => {
		var div_3 = root_5$5();
		var strong_1 = child(div_3);
		var text_8 = only_child(strong_1, true);
		var text_9 = only_child(sibling(strong_1), true);
		reset(div_3);
		template_effect(() => {
			set_attribute(div_3, "data-tone", get(emptyState).tone);
			set_text(text_8, get(emptyState).title);
			set_text(text_9, get(emptyState).detail);
		});
		append($$anchor, div_3);
	};
	if_block(node_6, ($$render) => {
		if (get(emptyState)) $$render(consequent_4);
	});
	reset(div_2);
	var node_7 = sibling(div_2, 2);
	var consequent_28 = ($$anchor) => {
		var details = root_35();
		let classes_1;
		var summary = child(details);
		var node_8 = sibling(child(summary), 2);
		each(node_8, 17, () => $$props.field.sources, (source) => source.name, ($$anchor, source) => {
			var span_1 = root_8$3();
			var text_10 = child(span_1);
			var small = sibling(text_10);
			var text_11 = only_child(small, true);
			var node_9 = sibling(small);
			var consequent_5 = ($$anchor) => {
				append($$anchor, root_6$5());
			};
			if_block(node_9, ($$render) => {
				if (get(source).truncated) $$render(consequent_5);
			});
			var node_10 = sibling(node_9);
			var consequent_6 = ($$anchor) => {
				append($$anchor, root_7$5());
			};
			if_block(node_10, ($$render) => {
				if (get(source).error) $$render(consequent_6);
			});
			reset(span_1);
			template_effect(() => {
				set_attribute(span_1, "data-phase", get(source).phase);
				set_text(text_10, `${get(source).name ?? ""} `);
				set_text(text_11, get(source).phase);
			});
			append($$anchor, span_1);
		});
		var node_11 = sibling(node_8, 2);
		var consequent_7 = ($$anchor) => {
			append($$anchor, root_9$3());
		};
		if_block(node_11, ($$render) => {
			if ($$props.field.subjects.length) $$render(consequent_7);
		});
		var node_12 = sibling(node_11, 2);
		var consequent_8 = ($$anchor) => {
			append($$anchor, root_10$3());
		};
		if_block(node_12, ($$render) => {
			if ($$props.field.metrics.length) $$render(consequent_8);
		});
		var node_13 = sibling(node_12, 2);
		var consequent_9 = ($$anchor) => {
			append($$anchor, root_11$2());
		};
		if_block(node_13, ($$render) => {
			if ($$props.field.provenance.length) $$render(consequent_9);
		});
		var node_14 = sibling(node_13, 2);
		var consequent_10 = ($$anchor) => {
			append($$anchor, root_12$2());
		};
		if_block(node_14, ($$render) => {
			if ($$props.field.replay.outside_cut.length) $$render(consequent_10);
		});
		var node_15 = sibling(node_14, 2);
		var consequent_11 = ($$anchor) => {
			append($$anchor, root_13$2());
		};
		if_block(node_15, ($$render) => {
			if ($$props.field.search.truncated) $$render(consequent_11);
		});
		var node_16 = sibling(node_15, 2);
		var consequent_12 = ($$anchor) => {
			append($$anchor, root_14$1());
		};
		if_block(node_16, ($$render) => {
			if (get(expansion)?.truncated) $$render(consequent_12);
		});
		reset(summary);
		var div_4 = sibling(summary, 2);
		var node_17 = child(div_4);
		each(node_17, 17, () => $$props.field.sources, (source) => source.name, ($$anchor, source) => {
			var section_1 = root_20$1();
			var strong_2 = child(section_1);
			var text_12 = only_child(strong_2);
			var p_1 = sibling(strong_2, 2);
			var text_13 = only_child(sibling(child(p_1)), true);
			reset(p_1);
			var node_18 = sibling(p_1, 2);
			var consequent_13 = ($$anchor) => {
				var p_2 = root_15$1();
				var text_14 = only_child(sibling(child(p_2)), true);
				reset(p_2);
				template_effect(() => set_text(text_14, get(source).subject_fingerprint));
				append($$anchor, p_2);
			};
			if_block(node_18, ($$render) => {
				if (get(source).subject_fingerprint) $$render(consequent_13);
			});
			var node_19 = sibling(node_18, 2);
			var consequent_14 = ($$anchor) => {
				append($$anchor, root_16$1());
			};
			if_block(node_19, ($$render) => {
				if (get(source).truncated) $$render(consequent_14);
			});
			var node_20 = sibling(node_19, 2);
			var consequent_15 = ($$anchor) => {
				var p_4 = root_17$1();
				var text_15 = only_child(p_4, true);
				template_effect(() => set_text(text_15, get(source).error));
				append($$anchor, p_4);
			};
			if_block(node_20, ($$render) => {
				if (get(source).error) $$render(consequent_15);
			});
			var node_21 = sibling(node_20, 2);
			var consequent_16 = ($$anchor) => {
				var small_1 = root_18$1();
				var text_16 = only_child(small_1);
				template_effect(() => set_text(text_16, `Revision ${get(source).revision ?? ""}`));
				append($$anchor, small_1);
			};
			if_block(node_21, ($$render) => {
				if (get(source).revision) $$render(consequent_16);
			});
			var node_22 = sibling(node_21, 2);
			each(node_22, 17, () => get(source).evidence, index, ($$anchor, evidence) => {
				var p_5 = root_19$1();
				var text_17 = only_child(p_5, true);
				template_effect(() => set_text(text_17, get(evidence)));
				append($$anchor, p_5);
			});
			var node_23 = sibling(node_22, 2);
			var consequent_17 = ($$anchor) => {
				var p_6 = root_19$1();
				var text_18 = only_child(p_6);
				template_effect(() => set_text(text_18, `${get(source).evidence_omitted ?? ""} further entity evidence references omitted from this display.`));
				append($$anchor, p_6);
			};
			if_block(node_23, ($$render) => {
				if (get(source).evidence_omitted > 0) $$render(consequent_17);
			});
			reset(section_1);
			template_effect(() => {
				set_attribute(section_1, "aria-label", `${get(source).name} source evidence`);
				set_text(text_12, `${get(source).name ?? ""} · ${get(source).phase ?? ""}`);
				set_text(text_13, get(source).source_ref);
			});
			append($$anchor, section_1);
		});
		var node_24 = sibling(node_17, 2);
		var consequent_20 = ($$anchor) => {
			var section_2 = root_24$1();
			var ul = sibling(child(section_2), 2);
			each(ul, 23, () => $$props.field.subjects, (subject, index) => `${subject.source}:${subject.kind}:${subject.id}:${index}`, ($$anchor, subject) => {
				var li = root_23$1();
				var code_2 = child(li);
				var text_19 = only_child(code_2, true);
				var text_20 = sibling(code_2);
				var code_3 = sibling(text_20);
				var text_21 = only_child(code_3, true);
				var node_25 = sibling(code_3, 2);
				var consequent_18 = ($$anchor) => {
					var fragment_1 = root_21$1();
					var text_22 = only_child(sibling(first_child(fragment_1), 2), true);
					template_effect(() => set_text(text_22, get(subject).definition_ref));
					append($$anchor, fragment_1);
				};
				if_block(node_25, ($$render) => {
					if (get(subject).definition_ref) $$render(consequent_18);
				});
				var node_26 = sibling(node_25, 2);
				var consequent_19 = ($$anchor) => {
					var fragment_2 = root_22$1();
					var text_23 = only_child(sibling(first_child(fragment_2), 2), true);
					template_effect(() => set_text(text_23, get(subject).effective_definition_digest));
					append($$anchor, fragment_2);
				};
				if_block(node_26, ($$render) => {
					if (get(subject).effective_definition_digest) $$render(consequent_19);
				});
				reset(li);
				template_effect(() => {
					set_text(text_19, get(subject).source);
					set_text(text_20, ` · ${get(subject).kind ?? ""} · `);
					set_text(text_21, get(subject).id);
				});
				append($$anchor, li);
			});
			reset(ul);
			reset(section_2);
			append($$anchor, section_2);
		};
		if_block(node_24, ($$render) => {
			if ($$props.field.subjects.length) $$render(consequent_20);
		});
		var node_27 = sibling(node_24, 2);
		var consequent_22 = ($$anchor) => {
			var section_3 = root_27$1();
			var ul_1 = sibling(child(section_3), 2);
			each(ul_1, 23, () => $$props.field.metrics, (metric, index) => `${metric.id}:${index}`, ($$anchor, metric) => {
				var li_1 = root_26$1();
				var span_8 = child(li_1);
				var text_24 = only_child(span_8, true);
				var code_6 = sibling(span_8, 2);
				var text_25 = only_child(code_6, true);
				var node_28 = sibling(code_6);
				var consequent_21 = ($$anchor) => {
					var span_9 = root_25$1();
					var text_26 = only_child(span_9, true);
					template_effect(() => set_text(text_26, get(metric).unit));
					append($$anchor, span_9);
				};
				if_block(node_28, ($$render) => {
					if (get(metric).unit) $$render(consequent_21);
				});
				reset(li_1);
				template_effect(($0) => {
					set_text(text_24, get(metric).label);
					set_text(text_25, $0);
				}, [() => JSON.stringify(get(metric).value)]);
				append($$anchor, li_1);
			});
			reset(ul_1);
			reset(section_3);
			append($$anchor, section_3);
		};
		if_block(node_27, ($$render) => {
			if ($$props.field.metrics.length) $$render(consequent_22);
		});
		var node_29 = sibling(node_27, 2);
		var consequent_23 = ($$anchor) => {
			var section_4 = root_29$1();
			var ul_2 = sibling(child(section_4), 2);
			each(ul_2, 23, () => $$props.field.provenance, (sourceRef, index) => `${sourceRef}:${index}`, ($$anchor, sourceRef) => {
				var li_2 = root_28$1();
				var text_27 = only_child(child(li_2), true);
				reset(li_2);
				template_effect(() => set_text(text_27, get(sourceRef)));
				append($$anchor, li_2);
			});
			reset(ul_2);
			reset(section_4);
			append($$anchor, section_4);
		};
		if_block(node_29, ($$render) => {
			if ($$props.field.provenance.length) $$render(consequent_23);
		});
		var node_30 = sibling(node_29, 2);
		var consequent_25 = ($$anchor) => {
			var section_5 = root_31$1();
			var strong_3 = child(section_5);
			var text_28 = only_child(strong_3);
			var p_7 = sibling(strong_3, 2);
			var text_29 = only_child(p_7);
			var node_31 = sibling(p_7, 2);
			var consequent_24 = ($$anchor) => {
				append($$anchor, root_30$1());
			};
			if_block(node_31, ($$render) => {
				if (get(expansion).truncated) $$render(consequent_24);
			});
			reset(section_5);
			template_effect(() => {
				set_text(text_28, `Expansion · ${get(expansion).phase ?? ""}`);
				set_text(text_29, `Applied depth ${get(expansion).applied_depth ?? ""}; ${get(expansion).entity_count ?? ""} retained entities.`);
			});
			append($$anchor, section_5);
		};
		if_block(node_30, ($$render) => {
			if (get(expansion)) $$render(consequent_25);
		});
		var node_32 = sibling(node_30, 2);
		var consequent_26 = ($$anchor) => {
			var section_6 = root_33();
			var ul_3 = sibling(child(section_6), 4);
			each(ul_3, 23, () => $$props.field.replay.outside_cut, (event, index) => `${event}:${index}`, ($$anchor, event) => {
				var li_3 = root_32();
				var text_30 = only_child(li_3, true);
				template_effect(() => set_text(text_30, get(event)));
				append($$anchor, li_3);
			});
			reset(ul_3);
			reset(section_6);
			append($$anchor, section_6);
		};
		if_block(node_32, ($$render) => {
			if ($$props.field.replay.outside_cut.length) $$render(consequent_26);
		});
		var node_33 = sibling(node_32, 2);
		var consequent_27 = ($$anchor) => {
			append($$anchor, root_34());
		};
		if_block(node_33, ($$render) => {
			if ($$props.field.search.truncated) $$render(consequent_27);
		});
		reset(div_4);
		reset(details);
		template_effect(() => classes_1 = set_class(details, 1, "field-source-status", null, classes_1, { "has-rail": $$props.field.replay.rail.length > 0 }));
		append($$anchor, details);
	};
	if_block(node_7, ($$render) => {
		if ($$props.field.sources.length || $$props.field.subjects.length || $$props.field.metrics.length || $$props.field.provenance.length || $$props.field.replay.outside_cut.length || $$props.field.search.truncated || get(expansion)) $$render(consequent_28);
	});
	var div_5 = sibling(node_7, 2);
	each(div_5, 21, () => get(accessibilityItems), (item) => item.id, ($$anchor, item) => {
		var div_6 = root_36();
		var text_31 = only_child(div_6);
		template_effect(() => {
			set_attribute(div_6, "id", get(item).domId);
			set_attribute(div_6, "aria-selected", get(item).selected);
			set_attribute(div_6, "aria-posinset", get(item).position);
			set_attribute(div_6, "aria-setsize", get(item).size);
			set_attribute(div_6, "aria-level", get(item).level);
			set_attribute(div_6, "aria-expanded", get(item).expanded ?? void 0);
			set_text(text_31, `${get(item).groupLabel ? `Group ${get(item).groupLabel}. ` : ""}${get(item).label ?? ""}${get(item).neighbors ? `. ${get(item).neighbors}` : ""}`);
		});
		delegated("click", div_6, () => selectEntity(get(item).id));
		delegated("dblclick", div_6, () => activateEntity(get(item).id));
		delegated("keydown", div_6, (event) => {
			if (event.key === "Enter") {
				event.preventDefault();
				event.stopPropagation();
				activateEntity(get(item).id);
			}
		});
		append($$anchor, div_6);
	});
	reset(div_5);
	var node_34 = sibling(div_5, 2);
	var consequent_34 = ($$anchor) => {
		var aside = root_44();
		var node_35 = child(aside);
		var consequent_31 = ($$anchor) => {
			var fragment_3 = root_39();
			var strong_4 = first_child(fragment_3);
			var text_32 = only_child(strong_4, true);
			var small_2 = sibling(strong_4);
			var text_33 = only_child(small_2, true);
			var node_36 = sibling(small_2, 2);
			{
				let $0 = /* @__PURE__ */ user_derived(() => `${get(selected).label} details`);
				RowDetails(node_36, {
					get details() {
						return get(selected).detail;
					},
					get label() {
						return get($0);
					}
				});
			}
			var node_37 = sibling(node_36, 2);
			var consequent_29 = ($$anchor) => {
				var dl = root_37();
				var div_7 = child(dl);
				var text_34 = only_child(sibling(child(div_7)), true);
				reset(div_7);
				var div_8 = sibling(div_7, 2);
				var text_35 = only_child(sibling(child(div_8)), true);
				reset(div_8);
				var div_9 = sibling(div_8, 2);
				var text_36 = only_child(sibling(child(div_9)), true);
				reset(div_9);
				reset(dl);
				template_effect(() => {
					set_text(text_34, get(selected).artifact_ref.thread_id);
					set_text(text_35, get(selected).artifact_ref.artifact_id);
					set_text(text_36, get(selected).artifact_ref.content_hash ?? "Not recorded");
				});
				append($$anchor, dl);
			};
			if_block(node_37, ($$render) => {
				if (get(selected).artifact_ref) $$render(consequent_29);
			});
			var node_38 = sibling(node_37, 2);
			each(node_38, 17, () => get(selectedRelations), (relation) => relation.id, ($$anchor, relation) => {
				var button_11 = root$14();
				var text_37 = only_child(button_11, true);
				template_effect(() => {
					button_11.disabled = !get(relation).activate_intent;
					set_text(text_37, get(relation).label);
				});
				delegated("click", button_11, () => get(relation).activate_intent && dispatch({
					type: "activate",
					intent: get(relation).activate_intent
				}));
				append($$anchor, button_11);
			});
			each(sibling(node_38, 2), 17, () => get(previewGroups).selectedPreviews, (preview) => preview.id, ($$anchor, preview) => {
				var fragment_4 = comment();
				var node_40 = first_child(fragment_4);
				var consequent_30 = ($$anchor) => {
					GridPreview($$anchor, {
						get preview() {
							return get(preview);
						},
						get compareEnabled() {
							return get(selected).compare_available;
						},
						oncompare: () => compareEntity(get(selected).id)
					});
				};
				var alternate = ($$anchor) => {
					append($$anchor, root_38());
				};
				if_block(node_40, ($$render) => {
					if (get(preview).grid) $$render(consequent_30);
					else $$render(alternate, -1);
				});
				append($$anchor, fragment_4);
			});
			template_effect(($0) => {
				set_text(text_32, get(selected).label);
				set_text(text_33, $0);
			}, [() => [
				get(selected).kind,
				get(selected).status,
				get(selected).source
			].filter(Boolean).join(" · ")]);
			append($$anchor, fragment_3);
		};
		if_block(node_35, ($$render) => {
			if (get(selected)) $$render(consequent_31);
		});
		var node_41 = sibling(node_35, 2);
		var consequent_33 = ($$anchor) => {
			var section_8 = root_42();
			each(sibling(child(section_8), 2), 17, () => get(previewGroups).compared, (entry) => entry.entity.id, ($$anchor, entry) => {
				var article = root_41();
				var h3 = child(article);
				var text_38 = only_child(h3, true);
				var small_3 = sibling(h3);
				var code_8 = child(small_3);
				var text_39 = only_child(code_8, true);
				var text_40 = sibling(code_8);
				reset(small_3);
				each(sibling(small_3, 2), 17, () => get(entry).previews, (preview) => preview.id, ($$anchor, preview) => {
					var fragment_6 = comment();
					var node_44 = first_child(fragment_6);
					var consequent_32 = ($$anchor) => {
						GridPreview($$anchor, {
							get preview() {
								return get(preview);
							},
							get compareEnabled() {
								return get(entry).entity.compare_available;
							},
							oncompare: () => compareEntity(get(entry).entity.id)
						});
					};
					var alternate_1 = ($$anchor) => {
						append($$anchor, root_38());
					};
					if_block(node_44, ($$render) => {
						if (get(preview).grid) $$render(consequent_32);
						else $$render(alternate_1, -1);
					});
					append($$anchor, fragment_6);
				}, ($$anchor) => {
					append($$anchor, root_40());
				});
				reset(article);
				template_effect(() => {
					set_attribute(article, "aria-label", `${get(entry).entity.label} (${get(entry).entity.id}) comparison`);
					set_text(text_38, get(entry).entity.label);
					set_text(text_39, get(entry).entity.id);
					set_text(text_40, ` · ${get(entry).entity.source ?? ""}`);
				});
				append($$anchor, article);
			});
			reset(section_8);
			append($$anchor, section_8);
		};
		if_block(node_41, ($$render) => {
			if (get(previewGroups).compared.length) $$render(consequent_33);
		});
		each(sibling(node_41, 2), 17, () => $$props.field.warnings, index, ($$anchor, warning) => {
			var small_4 = root_43();
			var text_41 = only_child(small_4, true);
			template_effect(() => set_text(text_41, get(warning)));
			append($$anchor, small_4);
		});
		reset(aside);
		append($$anchor, aside);
	};
	if_block(node_34, ($$render) => {
		if (get(selected) || get(previewGroups).compared.length || $$props.field.warnings.length) $$render(consequent_34);
	});
	reset(section);
	template_effect(($0) => {
		set_attribute(section, "aria-label", $$props.field.title);
		set_text(text, $$props.field.title);
		set_text(text_1, $0);
		button.disabled = !$$props.field.replay.previous;
		button_1.disabled = !$$props.field.replay.playing && !$$props.field.replay.next;
		set_text(text_2, $$props.field.replay.playing ? "Pause" : "Play");
		button_2.disabled = !$$props.field.replay.next;
		button_3.disabled = $$props.field.replay.mode === "live";
		set_value(input, $$props.field.search.query);
		set_attribute(div_5, "aria-label", `${$$props.field.title} entities`);
		set_attribute(div_5, "aria-activedescendant", get(activeOptionId));
	}, [() => $$props.field.sources.map((source) => `${source.name}:${source.phase}`).join(" · ")]);
	delegated("click", button, () => dispatch({
		type: "step_field_cursor",
		instance_key: $$props.instanceKey,
		direction: "previous"
	}));
	delegated("click", button_1, () => dispatch({
		type: "set_field_playback",
		instance_key: $$props.instanceKey,
		playing: !$$props.field.replay.playing
	}));
	delegated("click", button_2, () => dispatch({
		type: "step_field_cursor",
		instance_key: $$props.instanceKey,
		direction: "next"
	}));
	delegated("click", button_3, () => dispatch({
		type: "step_field_cursor",
		instance_key: $$props.instanceKey,
		direction: "live"
	}));
	delegated("input", input, (event) => dispatch({
		type: "set_field_query",
		instance_key: $$props.instanceKey,
		query: event.currentTarget.value
	}));
	delegated("keydown", input, (event) => {
		if (event.key === "ArrowUp" || event.key === "ArrowDown") {
			event.preventDefault();
			dispatch({
				type: "move_field_search_match",
				instance_key: $$props.instanceKey,
				delta: event.key === "ArrowUp" ? -1 : 1
			});
		}
	});
	event("focus", div_5, () => {
		if (!$$props.field.selected && get(accessibilityItems)[0]) selectEntity(get(accessibilityItems)[0].id);
	});
	delegated("keydown", div_5, (event) => {
		const current = get(accessibilityItems).find((item) => item.selected) ?? get(accessibilityItems)[0];
		if (event.key === "ArrowUp" || event.key === "ArrowDown") {
			event.preventDefault();
			dispatch({
				type: "move_field_selection",
				instance_key: $$props.instanceKey,
				delta: event.key === "ArrowUp" ? -1 : 1
			});
		} else if (event.key === "Home" || event.key === "End") {
			event.preventDefault();
			const item = event.key === "Home" ? get(accessibilityItems)[0] : get(accessibilityItems).at(-1);
			if (item) selectEntity(item.id);
		} else if ((event.key === "ArrowLeft" || event.key === "ArrowRight") && current?.groupId) {
			const collapsed = event.key === "ArrowLeft";
			if (current.expanded === collapsed) {
				event.preventDefault();
				dispatch({
					type: "set_field_group_collapsed",
					instance_key: $$props.instanceKey,
					group_id: current.groupId,
					collapsed
				});
			}
		} else if (event.key === "Enter" && $$props.field.selected) {
			event.preventDefault();
			activateEntity($$props.field.selected);
		} else if (event.key === " " && $$props.field.selected) {
			event.preventDefault();
			compareEntity($$props.field.selected);
		}
	});
	append($$anchor, section);
	pop();
}
delegate([
	"click",
	"input",
	"keydown",
	"dblclick"
]);
//#endregion
//#region browser/views/SceneView.svelte
var root$13 = /* @__PURE__ */ from_html(`<button> </button>`);
var root_1$9 = /* @__PURE__ */ from_html(`<div class="atlas-layers"></div> <div class="atlas-lenses" aria-label="Atlas lens"></div>`, 1);
var root_2$7 = /* @__PURE__ */ from_html(`<div class="atlas-toolbar"><div class="atlas-identity"><strong> </strong><span> </span></div> <div class="atlas-projections" aria-label="Atlas projection"></div> <!></div>`);
var root_3$7 = /* @__PURE__ */ from_svg(`<line></line>`);
var root_4$5 = /* @__PURE__ */ from_svg(`<text> </text>`);
var root_5$4 = /* @__PURE__ */ from_svg(`<polygon></polygon>`);
var root_6$4 = /* @__PURE__ */ from_svg(`<rect></rect>`);
var root_7$4 = /* @__PURE__ */ from_svg(`<circle></circle>`);
var root_8$2 = /* @__PURE__ */ from_svg(`<title> </title>`);
var root_9$2 = /* @__PURE__ */ from_svg(`<g><!><!></g>`);
var root_10$2 = /* @__PURE__ */ from_svg(`<circle class="atlas-stack-item" r="0.055"><title> </title></circle>`);
var root_11$1 = /* @__PURE__ */ from_svg(`<g><circle></circle><text> </text></g><!>`, 1);
var root_12$1 = /* @__PURE__ */ from_svg(`<!><!>`, 1);
var root_13$1 = /* @__PURE__ */ from_html(`<div class="scene-view"><!> <svg role="img" preserveAspectRatio="xMidYMid meet"><!><!></svg></div>`);
function SceneView($$anchor, $$props) {
	push($$props, true);
	const dispatch = dispatchUi();
	const points = /* @__PURE__ */ user_derived(() => $$props.scene.objects.flatMap((object) => [object.position, object.end].filter((point) => point != null)));
	const bounds = /* @__PURE__ */ user_derived(() => {
		const xs = get(points).map((point) => Number(point[0] ?? 0));
		const ys = get(points).map((point) => -Number(point[1] ?? 0));
		const minX = xs.length ? Math.min(...xs) : 0;
		const maxX = xs.length ? Math.max(...xs) : 1;
		const minY = ys.length ? Math.min(...ys) : 0;
		const maxY = ys.length ? Math.max(...ys) : 1;
		const scale = Math.max(.25, $$props.scene.camera.fov_degrees / 45);
		const width = Math.max(40, maxX - minX + 40) * scale;
		const height = Math.max(30, maxY - minY + 30) * scale;
		return `${Number($$props.scene.camera.target[0] ?? 0) - width / 2} ${-Number($$props.scene.camera.target[1] ?? 0) - height / 2} ${width} ${height}`;
	});
	const accessibleLabel = /* @__PURE__ */ user_derived(() => $$props.scene.objects.flatMap((object) => object.label ? [object.label] : []).join("; ") || "Scene");
	const activate = (intent) => intent && dispatch({
		type: "activate",
		intent
	});
	const radius = (object) => Math.max(1, Number(object.scale?.[0] ?? 5));
	const x = (object) => Number(object.position?.[0] ?? 0);
	const y = (object) => -Number(object.position?.[1] ?? 0);
	const atlasNodes = /* @__PURE__ */ user_derived(() => new Map(($$props.scene.atlas?.nodes ?? []).map((node) => [node.id, node])));
	const atlasBounds = /* @__PURE__ */ user_derived(() => {
		const bounds = $$props.scene.atlas?.bounds;
		if (!bounds) return "-10 -10 20 20";
		const scale = Math.max(.25, $$props.scene.camera.fov_degrees / 45);
		const width = Math.max(4, bounds.x_max - bounds.x_min + 4) * scale;
		const height = Math.max(4, bounds.z_max - bounds.z_min + 4) * scale;
		return `${Number($$props.scene.camera.target[0] ?? 0) - width / 2} ${Number($$props.scene.camera.target[2] ?? 0) - height / 2} ${width} ${height}`;
	});
	const atlasPoint = (position) => ({
		x: Number(position[0] ?? 0),
		y: Number(position[2] ?? position[1] ?? 0)
	});
	const actionById = /* @__PURE__ */ user_derived(() => new Map($$props.scene.actions.map((action) => [action.id, action])));
	const actions = (group) => $$props.scene.actions.filter((action) => action.group === group);
	const runAction = (action) => action && dispatch(action.event);
	const keyAction = (event, action) => {
		if (event.key === "Enter" || event.key === " ") {
			event.preventDefault();
			runAction(action);
		}
	};
	var div = root_13$1();
	var node_1 = child(div);
	var consequent_1 = ($$anchor) => {
		var div_1 = root_2$7();
		var div_2 = child(div_1);
		var strong = child(div_2);
		var text = only_child(strong, true);
		var text_1 = only_child(sibling(strong));
		reset(div_2);
		var div_3 = sibling(div_2, 2);
		each(div_3, 21, () => actions("projection"), (action) => action.id, ($$anchor, action) => {
			var button = root$13();
			let classes;
			var text_2 = only_child(button, true);
			template_effect(() => {
				set_attribute(button, "aria-pressed", get(action).active);
				classes = set_class(button, 1, "", null, classes, { active: get(action).active });
				set_text(text_2, get(action).label);
			});
			delegated("click", button, () => runAction(get(action)));
			append($$anchor, button);
		});
		reset(div_3);
		var node_2 = sibling(div_3, 2);
		var consequent = ($$anchor) => {
			var fragment = root_1$9();
			var div_4 = first_child(fragment);
			each(div_4, 21, () => actions("layer"), (action) => action.id, ($$anchor, action) => {
				var button_1 = root$13();
				let classes_1;
				var text_3 = only_child(button_1, true);
				template_effect(() => {
					set_attribute(button_1, "aria-pressed", get(action).active);
					classes_1 = set_class(button_1, 1, "", null, classes_1, { active: get(action).active });
					set_text(text_3, get(action).label);
				});
				delegated("click", button_1, () => runAction(get(action)));
				append($$anchor, button_1);
			});
			reset(div_4);
			var div_5 = sibling(div_4, 2);
			each(div_5, 21, () => actions("lens"), (action) => action.id, ($$anchor, action) => {
				var button_2 = root$13();
				let classes_2;
				var text_4 = only_child(button_2, true);
				template_effect(() => {
					set_attribute(button_2, "aria-pressed", get(action).active);
					classes_2 = set_class(button_2, 1, "", null, classes_2, { active: get(action).active });
					set_text(text_4, get(action).label);
				});
				delegated("click", button_2, () => runAction(get(action)));
				append($$anchor, button_2);
			});
			reset(div_5);
			append($$anchor, fragment);
		};
		if_block(node_2, ($$render) => {
			if ($$props.scene.atlas.projection === "ai_space") $$render(consequent);
		});
		reset(div_1);
		template_effect(() => {
			set_text(text, $$props.scene.atlas.root_label);
			set_text(text_1, `${$$props.scene.atlas.nodes.length ?? ""} regions`);
		});
		append($$anchor, div_1);
	};
	if_block(node_1, ($$render) => {
		if ($$props.scene.atlas) $$render(consequent_1);
	});
	var svg = sibling(node_1, 2);
	let classes_3;
	var node_3 = child(svg);
	each(node_3, 17, () => $$props.scene.objects, (object) => object.id, ($$anchor, object) => {
		const r = /* @__PURE__ */ user_derived(() => radius(get(object)));
		const ox = /* @__PURE__ */ user_derived(() => x(get(object)));
		const oy = /* @__PURE__ */ user_derived(() => y(get(object)));
		const end = /* @__PURE__ */ user_derived(() => get(object).end);
		var g = root_9$2();
		let classes_4;
		var node_4 = child(g);
		var consequent_2 = ($$anchor) => {
			var line = root_3$7();
			template_effect(($0, $1) => {
				set_attribute(line, "x1", get(ox));
				set_attribute(line, "y1", get(oy));
				set_attribute(line, "x2", $0);
				set_attribute(line, "y2", $1);
				set_attribute(line, "stroke", get(object).color);
			}, [() => Number(get(end)[0] ?? 0), () => -Number(get(end)[1] ?? 0)]);
			append($$anchor, line);
		};
		var consequent_3 = ($$anchor) => {
			var text_5 = root_4$5();
			var text_6 = only_child(text_5, true);
			template_effect(() => {
				set_attribute(text_5, "x", get(ox));
				set_attribute(text_5, "y", get(oy));
				set_attribute(text_5, "fill", get(object).color);
				set_text(text_6, get(object).label ?? "");
			});
			append($$anchor, text_5);
		};
		var consequent_4 = ($$anchor) => {
			var polygon = root_5$4();
			template_effect(() => {
				set_attribute(polygon, "points", `${get(ox)},${get(oy) - get(r)} ${get(ox) + get(r)},${get(oy)} ${get(ox)},${get(oy) + get(r)} ${get(ox) - get(r)},${get(oy)}`);
				set_attribute(polygon, "stroke", get(object).color);
			});
			append($$anchor, polygon);
		};
		var consequent_5 = ($$anchor) => {
			var rect = root_6$4();
			template_effect(() => {
				set_attribute(rect, "x", get(ox) - get(r));
				set_attribute(rect, "y", get(oy) - get(r));
				set_attribute(rect, "width", get(r) * 2);
				set_attribute(rect, "height", get(r) * 2);
				set_attribute(rect, "stroke", get(object).color);
			});
			append($$anchor, rect);
		};
		var alternate = ($$anchor) => {
			var circle = root_7$4();
			template_effect(() => {
				set_attribute(circle, "cx", get(ox));
				set_attribute(circle, "cy", get(oy));
				set_attribute(circle, "r", get(r));
				set_attribute(circle, "stroke", get(object).color);
			});
			append($$anchor, circle);
		};
		if_block(node_4, ($$render) => {
			if (get(end)) $$render(consequent_2);
			else if (get(object).kind === "text") $$render(consequent_3, 1);
			else if (get(object).glyph === "diamond") $$render(consequent_4, 2);
			else if (get(object).glyph === "square") $$render(consequent_5, 3);
			else $$render(alternate, -1);
		});
		var node_5 = sibling(node_4);
		var consequent_6 = ($$anchor) => {
			var title = root_8$2();
			var text_7 = only_child(title, true);
			template_effect(() => set_text(text_7, get(object).label));
			append($$anchor, title);
		};
		if_block(node_5, ($$render) => {
			if (get(object).label && get(object).kind !== "text") $$render(consequent_6);
		});
		reset(g);
		template_effect(() => {
			set_attribute(g, "data-tone", get(object).tone);
			set_attribute(g, "opacity", get(object).opacity);
			set_attribute(g, "role", get(object).intent ? "button" : void 0);
			set_attribute(g, "tabindex", get(object).intent ? 0 : void 0);
			classes_4 = set_class(g, 0, "", null, classes_4, { interactive: get(object).intent != null });
		});
		delegated("click", g, () => activate(get(object).intent));
		delegated("keydown", g, (event) => {
			if (event.key === "Enter" || event.key === " ") {
				event.preventDefault();
				activate(get(object).intent);
			}
		});
		append($$anchor, g);
	});
	var node_6 = sibling(node_3);
	var consequent_8 = ($$anchor) => {
		var fragment_1 = root_12$1();
		var node_7 = first_child(fragment_1);
		each(node_7, 17, () => $$props.scene.atlas.links, (link) => link.id, ($$anchor, link) => {
			const from = /* @__PURE__ */ user_derived(() => get(atlasNodes).get(get(link).from));
			const to = /* @__PURE__ */ user_derived(() => get(atlasNodes).get(get(link).to));
			var fragment_2 = comment();
			var node_8 = first_child(fragment_2);
			var consequent_7 = ($$anchor) => {
				const start = /* @__PURE__ */ user_derived(() => atlasPoint(get(from).position));
				const end = /* @__PURE__ */ user_derived(() => atlasPoint(get(to).position));
				var line_1 = root_3$7();
				template_effect(() => {
					set_attribute(line_1, "x1", get(start).x);
					set_attribute(line_1, "y1", get(start).y);
					set_attribute(line_1, "x2", get(end).x);
					set_attribute(line_1, "y2", get(end).y);
					set_attribute(line_1, "data-kind", get(link).kind);
				});
				append($$anchor, line_1);
			};
			if_block(node_8, ($$render) => {
				if (get(from) && get(to)) $$render(consequent_7);
			});
			append($$anchor, fragment_2);
		});
		each(sibling(node_7), 17, () => $$props.scene.atlas.nodes, (node) => node.id, ($$anchor, node) => {
			const point = /* @__PURE__ */ user_derived(() => atlasPoint(get(node).position));
			const nodeAction = /* @__PURE__ */ user_derived(() => get(actionById).get(`node:${get(node).id}`));
			var fragment_3 = root_11$1();
			var g_1 = first_child(fragment_3);
			let classes_5;
			var circle_1 = child(g_1);
			var text_8 = sibling(circle_1);
			var text_9 = only_child(text_8, true);
			reset(g_1);
			each(sibling(g_1), 19, () => get(node).stack.slice(0, 4), (item) => item.id, ($$anchor, item, index) => {
				const itemAction = /* @__PURE__ */ user_derived(() => get(actionById).get(`item:${get(item).id}`));
				var circle_2 = root_10$2();
				var text_10 = only_child(child(circle_2), true);
				reset(circle_2);
				template_effect(() => {
					set_attribute(circle_2, "data-kind", get(item).kind);
					set_attribute(circle_2, "cx", get(point).x + get(index) * .11);
					set_attribute(circle_2, "cy", get(point).y - .24);
					set_attribute(circle_2, "role", get(itemAction) ? "button" : void 0);
					set_attribute(circle_2, "tabindex", get(itemAction) ? 0 : void 0);
					set_text(text_10, get(item).canonical_ref);
				});
				delegated("click", circle_2, () => runAction(get(itemAction)));
				delegated("keydown", circle_2, (event) => keyAction(event, get(itemAction)));
				append($$anchor, circle_2);
			});
			template_effect(($0) => {
				classes_5 = set_class(g_1, 0, "atlas-node", null, classes_5, {
					selected: get(node).state.selected,
					highlighted: get(node).state.highlighted,
					dimmed: get(node).state.dimmed
				});
				set_attribute(g_1, "role", get(nodeAction) ? "button" : void 0);
				set_attribute(g_1, "tabindex", get(nodeAction) ? 0 : void 0);
				set_attribute(circle_1, "cx", get(point).x);
				set_attribute(circle_1, "cy", get(point).y);
				set_attribute(circle_1, "r", $0);
				set_attribute(text_8, "x", get(point).x + .28);
				set_attribute(text_8, "y", get(point).y + .08);
				set_text(text_9, get(node).label);
			}, [() => Math.max(.18, .16 + get(node).stack.length * .045)]);
			delegated("click", g_1, () => runAction(get(nodeAction)));
			delegated("keydown", g_1, (event) => keyAction(event, get(nodeAction)));
			append($$anchor, fragment_3);
		});
		append($$anchor, fragment_1);
	};
	if_block(node_6, ($$render) => {
		if ($$props.scene.atlas) $$render(consequent_8);
	});
	reset(svg);
	reset(div);
	template_effect(() => {
		set_attribute(svg, "viewBox", $$props.scene.atlas ? get(atlasBounds) : get(bounds));
		set_attribute(svg, "aria-label", $$props.scene.atlas ? `${$$props.scene.atlas.root_label} atlas` : get(accessibleLabel));
		classes_3 = set_class(svg, 0, "", null, classes_3, { "atlas-canvas": $$props.scene.atlas != null });
	});
	append($$anchor, div);
	pop();
}
delegate(["click", "keydown"]);
//#endregion
//#region browser/views/TableHierarchy.svelte
var root$12 = /* @__PURE__ */ from_html(`<span aria-hidden="true"> </span>`);
var root_1$8 = /* @__PURE__ */ from_html(`<button class="tree-fold svelte-5rokht"> </button>`);
var root_2$6 = /* @__PURE__ */ from_html(`<span aria-hidden="true">─</span>`);
var root_3$6 = /* @__PURE__ */ from_html(`<span class="table-hierarchy svelte-5rokht"><!> <span aria-hidden="true"> </span> <!></span>`);
function TableHierarchy($$anchor, $$props) {
	push($$props, true);
	var span = root_3$6();
	var node = child(span);
	each(node, 17, () => $$props.hierarchy.ancestor_continues ?? [], index, ($$anchor, continues) => {
		var span_1 = root$12();
		var text = only_child(span_1, true);
		template_effect(() => set_text(text, get(continues) ? "│" : "\xA0"));
		append($$anchor, span_1);
	});
	var span_2 = sibling(node, 2);
	var text_1 = only_child(span_2, true);
	var node_1 = sibling(span_2, 2);
	var consequent = ($$anchor) => {
		var button = root_1$8();
		var text_2 = only_child(button, true);
		template_effect(() => {
			set_attribute(button, "data-focus-key", $$props.focusKey);
			set_attribute(button, "aria-label", `${$$props.hierarchy.collapsed ? "Expand" : "Collapse"} descendants of ${$$props.label}`);
			set_attribute(button, "aria-expanded", !$$props.hierarchy.collapsed);
			set_text(text_2, $$props.hierarchy.collapsed ? "▸" : "▾");
		});
		delegated("click", button, function(...$$args) {
			$$props.onToggle?.apply(this, $$args);
		});
		append($$anchor, button);
	};
	var alternate = ($$anchor) => {
		append($$anchor, root_2$6());
	};
	if_block(node_1, ($$render) => {
		if ($$props.hierarchy.has_children) $$render(consequent);
		else $$render(alternate, -1);
	});
	reset(span);
	template_effect(() => set_text(text_1, $$props.hierarchy.is_last ? "└" : "├"));
	append($$anchor, span);
	pop();
}
delegate(["click"]);
//#endregion
//#region browser/views/ViewRenderer.svelte
var root$11 = /* @__PURE__ */ from_html(`<div class="source-status" role="status"><!> </div>`);
var root_1$7 = /* @__PURE__ */ from_html(`<div> </div>`);
var root_2$5 = /* @__PURE__ */ from_html(`<div class="text-view"></div>`);
var root_3$5 = /* @__PURE__ */ from_html(`<small>bounded preview</small>`);
var root_4$4 = /* @__PURE__ */ from_html(`<article class="document-view"><header><span> </span><!></header> <pre> </pre></article>`);
var root_5$3 = /* @__PURE__ */ from_html(`<small> </small>`);
var root_6$3 = /* @__PURE__ */ from_html(`<span class="row-meta"> </span>`);
var root_7$3 = /* @__PURE__ */ from_html(`<button class="row-disclosure"> </button>`);
var root_8$1 = /* @__PURE__ */ from_html(`<div class="view-record-line"><button><span class="row-glyph"> </span> <span class="row-copy"><strong> </strong><!></span> <!></button> <!></div> <!>`, 1);
var root_9$1 = /* @__PURE__ */ from_html(`<div class="view-record" role="listitem"><!></div>`);
var root_10$1 = /* @__PURE__ */ from_html(`<div class="rows-view" role="list"></div>`);
var root_11 = /* @__PURE__ */ from_html(`<div class="table-coverage" role="status"> </div>`);
var root_12 = /* @__PURE__ */ from_html(`<span role="columnheader"> </span>`);
var root_13 = /* @__PURE__ */ from_html(`<div class="table-empty-page" role="status">No rows in this page.</div>`);
var root_14 = /* @__PURE__ */ from_html(`<div role="cell"><!> <button class="svelte-15ewqy0"> </button></div>`);
var root_15 = /* @__PURE__ */ from_html(`<div class="table-detail" role="row"><div role="cell"><!></div></div>`);
var root_16 = /* @__PURE__ */ from_html(`<div class="table-record" role="rowgroup"><div role="row"><!> <div class="table-action-cell" role="cell"><!></div></div> <!></div>`);
var root_17 = /* @__PURE__ */ from_html(`<div class="table-view" role="table"><!> <div class="table-head" role="row"><!><span class="table-action-head" role="columnheader">detail</span></div> <!> <!></div>`);
var root_18 = /* @__PURE__ */ from_html(`<p> </p>`);
var root_19 = /* @__PURE__ */ from_html(`<strong> </strong><!>`, 1);
var root_20 = /* @__PURE__ */ from_html(`<span> </span><!>`, 1);
var root_21 = /* @__PURE__ */ from_html(`<span class="timeline-separator"> </span>`);
var root_22 = /* @__PURE__ */ from_html(`<div class="view-record"><div class="view-record-line"><button><!></button> <!></div> <!></div>`);
var root_23 = /* @__PURE__ */ from_html(`<div class="timeline-view"></div>`);
var root_24 = /* @__PURE__ */ from_html(`<div class="view-record"><div class="view-record-line"><button><span> </span><small> </small></button><!></div> <!></div>`);
var root_25 = /* @__PURE__ */ from_html(`<section><button><span> </span><span> </span></button> <!> <!></section>`);
var root_26 = /* @__PURE__ */ from_html(`<div class="sections-view"></div>`);
var root_27 = /* @__PURE__ */ from_html(`<button> </button>`);
var root_28 = /* @__PURE__ */ from_html(`<div class="required-subject-actions" aria-label="Compatible current selections"></div>`);
var root_29 = /* @__PURE__ */ from_html(`<p class="required-subject-waiting">Select a compatible subject in another open set.</p>`);
var root_30 = /* @__PURE__ */ from_html(`<section class="required-subject" aria-live="polite"><span class="required-subject-kicker">Unresolved view</span> <h3> </h3> <p><strong> </strong> </p> <!></section>`);
var root_31 = /* @__PURE__ */ from_html(`<div class="view"><!> <!></div>`);
function ViewRenderer($$anchor, $$props) {
	push($$props, true);
	let sourceStatus = /* @__PURE__ */ user_derived(() => $$props.model.type === "rows" || $$props.model.type === "timeline" || $$props.model.type === "table" || $$props.model.type === "document" ? $$props.model.source_status : void 0);
	let tableCoverage = /* @__PURE__ */ user_derived(() => $$props.model.type === "table" ? $$props.model.coverage : void 0);
	const dispatch = dispatchUi();
	const select = (itemId) => {
		if (!itemId) return;
		dispatch({
			type: "choose_view_item",
			instance_key: $$props.instanceKey,
			item_id: itemId,
			activate: true
		});
	};
	const expand = (itemId, expanded) => dispatch({
		type: "toggle_view_item_expansion",
		instance_key: $$props.instanceKey,
		item_id: itemId,
		expand: !expanded
	});
	const foldTree = (itemId, collapsed) => dispatch({
		type: "set_view_tree_item_collapsed",
		instance_key: $$props.instanceKey,
		item_id: itemId,
		collapsed: !collapsed
	});
	const expandTimeline = (index) => {
		const itemId = $$props.model.type === "timeline" ? $$props.model.entry_ids[index] : void 0;
		if (itemId) expand(itemId, $$props.model.type === "timeline" && ($$props.model.entry_expanded?.[index] ?? false));
	};
	var div = root_31();
	var node = child(div);
	var consequent_1 = ($$anchor) => {
		var div_1 = root$11();
		var node_1 = child(div_1);
		var consequent = ($$anchor) => {
			append($$anchor, text("Last observation (stale) ·"));
		};
		if_block(node_1, ($$render) => {
			if (get(sourceStatus).stale) $$render(consequent);
		});
		var text_1 = sibling(node_1);
		reset(div_1);
		template_effect(() => set_text(text_1, `${get(sourceStatus).state ?? ""}: ${get(sourceStatus).message ?? ""}`));
		append($$anchor, div_1);
	};
	if_block(node, ($$render) => {
		if (get(sourceStatus)) $$render(consequent_1);
	});
	var node_2 = sibling(node, 2);
	var consequent_2 = ($$anchor) => {
		var div_2 = root_2$5();
		each(div_2, 21, () => $$props.model.lines, index, ($$anchor, line) => {
			var div_3 = root_1$7();
			var text_2 = only_child(div_3, true);
			template_effect(() => {
				set_attribute(div_3, "data-tone", get(line).tone);
				set_text(text_2, get(line).text);
			});
			append($$anchor, div_3);
		});
		reset(div_2);
		template_effect(() => set_attribute(div_2, "aria-label", $$props.model.title));
		append($$anchor, div_2);
	};
	var consequent_4 = ($$anchor) => {
		var article = root_4$4();
		var header = child(article);
		var span = child(header);
		var text_3 = only_child(span, true);
		var node_3 = sibling(span);
		var consequent_3 = ($$anchor) => {
			append($$anchor, root_3$5());
		};
		if_block(node_3, ($$render) => {
			if ($$props.model.truncated) $$render(consequent_3);
		});
		reset(header);
		var text_4 = only_child(sibling(header, 2), true);
		reset(article);
		template_effect(() => {
			set_attribute(article, "aria-label", $$props.model.title);
			set_text(text_3, $$props.model.path);
			set_text(text_4, $$props.model.content);
		});
		append($$anchor, article);
	};
	var consequent_10 = ($$anchor) => {
		var div_4 = root_10$1();
		each(div_4, 21, () => $$props.model.rows, (row) => row.id, ($$anchor, row) => {
			var div_5 = root_9$1();
			var node_4 = child(div_5);
			var consequent_5 = ($$anchor) => {
				{
					let $0 = /* @__PURE__ */ user_derived(() => get(row).detail ?? []);
					RowDetails($$anchor, {
						get details() {
							return get($0);
						},
						get label() {
							return get(row).primary;
						}
					});
				}
			};
			var alternate = ($$anchor) => {
				var fragment_1 = root_8$1();
				var div_6 = first_child(fragment_1);
				var button = child(div_6);
				let classes;
				var span_1 = child(button);
				var text_5 = only_child(span_1, true);
				var span_2 = sibling(span_1, 2);
				var strong = child(span_2);
				var text_6 = only_child(strong, true);
				var node_5 = sibling(strong);
				var consequent_6 = ($$anchor) => {
					var small_1 = root_5$3();
					var text_7 = only_child(small_1, true);
					template_effect(() => set_text(text_7, get(row).secondary));
					append($$anchor, small_1);
				};
				if_block(node_5, ($$render) => {
					if (get(row).secondary) $$render(consequent_6);
				});
				reset(span_2);
				var node_6 = sibling(span_2, 2);
				var consequent_7 = ($$anchor) => {
					var span_3 = root_6$3();
					var text_8 = only_child(span_3, true);
					template_effect(() => set_text(text_8, get(row).meta));
					append($$anchor, span_3);
				};
				if_block(node_6, ($$render) => {
					if (get(row).meta) $$render(consequent_7);
				});
				reset(button);
				var node_7 = sibling(button, 2);
				var consequent_8 = ($$anchor) => {
					var button_1 = root_7$3();
					var text_9 = only_child(button_1, true);
					template_effect(() => {
						set_attribute(button_1, "aria-label", `${get(row).expanded ? "Collapse" : "Expand"} ${get(row).primary}`);
						set_attribute(button_1, "aria-expanded", get(row).expanded);
						set_text(text_9, get(row).expanded ? "▾" : "▸");
					});
					delegated("click", button_1, () => expand(get(row).id, get(row).expanded));
					append($$anchor, button_1);
				};
				if_block(node_7, ($$render) => {
					if (get(row).expandable) $$render(consequent_8);
				});
				reset(div_6);
				var node_8 = sibling(div_6, 2);
				var consequent_9 = ($$anchor) => {
					{
						let $0 = /* @__PURE__ */ user_derived(() => get(row).detail ?? []);
						let $1 = /* @__PURE__ */ user_derived(() => `${get(row).primary} details`);
						RowDetails($$anchor, {
							get details() {
								return get($0);
							},
							get label() {
								return get($1);
							}
						});
					}
				};
				if_block(node_8, ($$render) => {
					if (get(row).expanded) $$render(consequent_9);
				});
				template_effect(() => {
					set_attribute(button, "data-focus-key", `view:${$$props.instanceKey}:item:${get(row).id}`);
					set_attribute(button, "data-tone", get(row).tone);
					set_attribute(button, "data-kind", get(row).kind ?? void 0);
					button.disabled = get(row).kind === "unavailable";
					set_attribute(button, "aria-disabled", get(row).kind === "unavailable");
					classes = set_class(button, 1, "", null, classes, { selected: get(row).selected });
					set_text(text_5, get(row).glyph ?? "◇");
					set_text(text_6, get(row).primary);
				});
				delegated("click", button, () => select(get(row).id));
				append($$anchor, fragment_1);
			};
			if_block(node_4, ($$render) => {
				if (get(row).kind === "detail") $$render(consequent_5);
				else $$render(alternate, -1);
			});
			reset(div_5);
			template_effect(() => set_attribute(div_5, "data-expanded", get(row).expanded));
			append($$anchor, div_5);
		});
		reset(div_4);
		template_effect(() => set_attribute(div_4, "aria-label", $$props.model.title));
		append($$anchor, div_4);
	};
	var consequent_16 = ($$anchor) => {
		var div_7 = root_17();
		var node_9 = child(div_7);
		var consequent_11 = ($$anchor) => {
			var div_8 = root_11();
			var text_10 = only_child(div_8);
			template_effect(() => set_text(text_10, `${get(tableCoverage).state ?? ""} / ${get(tableCoverage).reason ?? ""}`));
			append($$anchor, div_8);
		};
		if_block(node_9, ($$render) => {
			if (get(tableCoverage)) $$render(consequent_11);
		});
		var div_9 = sibling(node_9, 2);
		each(child(div_9), 17, () => $$props.model.columns, index, ($$anchor, column) => {
			var span_4 = root_12();
			var text_11 = only_child(span_4, true);
			template_effect(() => set_text(text_11, get(column)));
			append($$anchor, span_4);
		});
		next();
		reset(div_9);
		var node_11 = sibling(div_9, 2);
		var consequent_12 = ($$anchor) => {
			append($$anchor, root_13());
		};
		if_block(node_11, ($$render) => {
			if ($$props.model.total_rows === 0n) $$render(consequent_12);
		});
		each(sibling(node_11, 2), 17, () => $$props.model.rows, (row) => row.id, ($$anchor, row) => {
			var div_11 = root_16();
			var div_12 = child(div_11);
			let classes_1;
			var node_13 = child(div_12);
			each(node_13, 17, () => get(row).cells, index, ($$anchor, cell, index) => {
				var div_13 = root_14();
				let classes_2;
				var node_14 = child(div_13);
				var consequent_13 = ($$anchor) => {
					{
						let $0 = /* @__PURE__ */ user_derived(() => `view:${$$props.instanceKey}:tree:${get(row).id}`);
						TableHierarchy($$anchor, {
							get hierarchy() {
								return get(row).hierarchy;
							},
							get label() {
								return get(cell);
							},
							get focusKey() {
								return get($0);
							},
							onToggle: () => foldTree(get(row).id, get(row).hierarchy.collapsed)
						});
					}
				};
				if_block(node_14, ($$render) => {
					if (index === 0 && get(row).hierarchy) $$render(consequent_13);
				});
				var button_2 = sibling(node_14, 2);
				var text_12 = only_child(button_2, true);
				reset(div_13);
				template_effect(() => {
					set_attribute(div_13, "data-tone", get(row).cell_tones?.[index] ?? void 0);
					classes_2 = set_class(div_13, 1, "svelte-15ewqy0", null, classes_2, { "tree-cell": index === 0 && !!get(row).hierarchy });
					set_attribute(button_2, "data-focus-key", index === 0 ? `view:${$$props.instanceKey}:item:${get(row).id}` : void 0);
					set_attribute(button_2, "aria-label", `Select row: ${get(cell)}`);
					set_text(text_12, get(cell));
				});
				delegated("click", button_2, () => select(get(row).id));
				append($$anchor, div_13);
			});
			var div_14 = sibling(node_13, 2);
			var node_15 = child(div_14);
			var consequent_14 = ($$anchor) => {
				var button_3 = root_7$3();
				var text_13 = only_child(button_3, true);
				template_effect(() => {
					set_attribute(button_3, "aria-label", `${get(row).expanded ? "Collapse" : "Expand"} row`);
					set_attribute(button_3, "aria-expanded", get(row).expanded);
					set_text(text_13, get(row).expanded ? "▾" : "▸");
				});
				delegated("click", button_3, () => expand(get(row).id, get(row).expanded));
				append($$anchor, button_3);
			};
			if_block(node_15, ($$render) => {
				if (get(row).expandable) $$render(consequent_14);
			});
			reset(div_14);
			reset(div_12);
			var node_16 = sibling(div_12, 2);
			var consequent_15 = ($$anchor) => {
				var div_15 = root_15();
				var div_16 = child(div_15);
				var node_17 = child(div_16);
				{
					let $0 = /* @__PURE__ */ user_derived(() => get(row).detail ?? []);
					RowDetails(node_17, {
						get details() {
							return get($0);
						},
						label: "Row details"
					});
				}
				reset(div_16);
				reset(div_15);
				template_effect(() => set_attribute(div_16, "aria-colspan", $$props.model.columns.length + 1));
				append($$anchor, div_15);
			};
			if_block(node_16, ($$render) => {
				if (get(row).expanded) $$render(consequent_15);
			});
			reset(div_11);
			template_effect(() => {
				set_attribute(div_11, "data-expanded", get(row).expanded);
				classes_1 = set_class(div_12, 1, "table-record-line svelte-15ewqy0", null, classes_1, { selected: get(row).selected });
				set_attribute(div_12, "data-tone", get(row).tone);
			});
			append($$anchor, div_11);
		});
		reset(div_7);
		template_effect(() => {
			set_attribute(div_7, "aria-label", $$props.model.title);
			set_style(div_7, `--columns:${$$props.model.columns.length}`);
		});
		append($$anchor, div_7);
	};
	var consequent_24 = ($$anchor) => {
		var div_17 = root_23();
		each(div_17, 21, () => $$props.model.entries, index, ($$anchor, entry, index) => {
			var div_18 = root_22();
			var div_19 = child(div_18);
			var button_4 = child(div_19);
			let classes_3;
			var node_18 = child(button_4);
			var consequent_17 = ($$anchor) => {
				var p = root_18();
				var text_14 = only_child(p, true);
				template_effect(() => {
					set_attribute(p, "data-tone", get(entry).tone);
					set_text(text_14, get(entry).text);
				});
				append($$anchor, p);
			};
			var consequent_19 = ($$anchor) => {
				var fragment_4 = root_19();
				var strong_1 = first_child(fragment_4);
				var text_15 = only_child(strong_1, true);
				var node_19 = sibling(strong_1);
				var consequent_18 = ($$anchor) => {
					var small_2 = root_5$3();
					var text_16 = only_child(small_2, true);
					template_effect(() => set_text(text_16, get(entry).meta));
					append($$anchor, small_2);
				};
				if_block(node_19, ($$render) => {
					if (get(entry).meta) $$render(consequent_18);
				});
				template_effect(() => set_text(text_15, get(entry).primary));
				append($$anchor, fragment_4);
			};
			var consequent_21 = ($$anchor) => {
				var fragment_5 = root_20();
				var span_5 = first_child(fragment_5);
				var text_17 = only_child(span_5, true);
				var node_20 = sibling(span_5);
				var consequent_20 = ($$anchor) => {
					var small_3 = root_5$3();
					var text_18 = only_child(small_3, true);
					template_effect(() => set_text(text_18, get(entry).meta));
					append($$anchor, small_3);
				};
				if_block(node_20, ($$render) => {
					if (get(entry).meta) $$render(consequent_20);
				});
				template_effect(() => set_text(text_17, get(entry).summary));
				append($$anchor, fragment_5);
			};
			var alternate_1 = ($$anchor) => {
				var span_6 = root_21();
				var text_19 = only_child(span_6, true);
				template_effect(() => set_text(text_19, get(entry).label));
				append($$anchor, span_6);
			};
			if_block(node_18, ($$render) => {
				if (get(entry).type === "block") $$render(consequent_17);
				else if (get(entry).type === "line") $$render(consequent_19, 1);
				else if (get(entry).type === "pair") $$render(consequent_21, 2);
				else $$render(alternate_1, -1);
			});
			reset(button_4);
			var node_21 = sibling(button_4, 2);
			var consequent_22 = ($$anchor) => {
				var button_5 = root_7$3();
				var text_20 = only_child(button_5, true);
				template_effect(() => {
					set_attribute(button_5, "aria-label", `${$$props.model.entry_expanded?.[index] ? "Collapse" : "Expand"} timeline entry`);
					set_attribute(button_5, "aria-expanded", $$props.model.entry_expanded?.[index] ?? false);
					set_text(text_20, $$props.model.entry_expanded?.[index] ? "▾" : "▸");
				});
				delegated("click", button_5, () => expandTimeline(index));
				append($$anchor, button_5);
			};
			if_block(node_21, ($$render) => {
				if ($$props.model.entry_expandable?.[index]) $$render(consequent_22);
			});
			reset(div_19);
			var node_22 = sibling(div_19, 2);
			var consequent_23 = ($$anchor) => {
				{
					let $0 = /* @__PURE__ */ user_derived(() => $$props.model.entry_details?.[index] ?? []);
					RowDetails($$anchor, {
						get details() {
							return get($0);
						},
						label: "Timeline entry details"
					});
				}
			};
			if_block(node_22, ($$render) => {
				if ($$props.model.entry_expanded?.[index]) $$render(consequent_23);
			});
			reset(div_18);
			template_effect(($0) => {
				set_attribute(div_18, "data-expanded", $$props.model.entry_expanded?.[index] ?? false);
				set_attribute(button_4, "data-focus-key", `view:${$$props.instanceKey}:item:${$$props.model.entry_ids[index]}`);
				classes_3 = set_class(button_4, 1, "timeline-entry", null, classes_3, { selected: $0 });
				set_attribute(button_4, "data-kind", get(entry).type);
				set_style(button_4, `--indent:${$$props.model.entry_indents[index] ?? 0}`);
			}, [() => $$props.model.selected === BigInt(index)]);
			delegated("click", button_4, () => select($$props.model.entry_ids[index]));
			append($$anchor, div_18);
		});
		reset(div_17);
		template_effect(() => set_attribute(div_17, "aria-label", $$props.model.title));
		append($$anchor, div_17);
	};
	var consequent_30 = ($$anchor) => {
		var div_20 = root_26();
		each(div_20, 21, () => $$props.model.sections, index, ($$anchor, section) => {
			var section_1 = root_25();
			let classes_4;
			var button_6 = child(section_1);
			let classes_5;
			var span_7 = child(button_6);
			var text_21 = only_child(span_7);
			var text_22 = only_child(sibling(span_7), true);
			reset(button_6);
			var node_23 = sibling(button_6, 2);
			var consequent_26 = ($$anchor) => {
				var div_21 = root$11();
				var node_24 = child(div_21);
				var consequent_25 = ($$anchor) => {
					append($$anchor, text("Last observation (stale) ·"));
				};
				if_block(node_24, ($$render) => {
					if (get(section).source_status.stale) $$render(consequent_25);
				});
				var text_24 = sibling(node_24);
				reset(div_21);
				template_effect(() => set_text(text_24, `${get(section).source_status.state ?? ""}: ${get(section).source_status.message ?? ""}`));
				append($$anchor, div_21);
			};
			if_block(node_23, ($$render) => {
				if (get(section).source_status) $$render(consequent_26);
			});
			var node_25 = sibling(node_23, 2);
			var consequent_29 = ($$anchor) => {
				var fragment_7 = comment();
				each(first_child(fragment_7), 17, () => get(section).rows, (row) => row.id, ($$anchor, row) => {
					var div_22 = root_24();
					var div_23 = child(div_22);
					var button_7 = child(div_23);
					let classes_6;
					var span_9 = child(button_7);
					var text_25 = only_child(span_9, true);
					var text_26 = only_child(sibling(span_9), true);
					reset(button_7);
					var node_27 = sibling(button_7);
					var consequent_27 = ($$anchor) => {
						var button_8 = root_7$3();
						var text_27 = only_child(button_8, true);
						template_effect(() => {
							set_attribute(button_8, "aria-label", `${get(row).expanded ? "Collapse" : "Expand"} ${get(row).primary}`);
							set_attribute(button_8, "aria-expanded", get(row).expanded);
							set_text(text_27, get(row).expanded ? "▾" : "▸");
						});
						delegated("click", button_8, () => expand(get(row).id, get(row).expanded));
						append($$anchor, button_8);
					};
					if_block(node_27, ($$render) => {
						if (get(row).expandable) $$render(consequent_27);
					});
					reset(div_23);
					var node_28 = sibling(div_23, 2);
					var consequent_28 = ($$anchor) => {
						{
							let $0 = /* @__PURE__ */ user_derived(() => get(row).detail ?? []);
							let $1 = /* @__PURE__ */ user_derived(() => `${get(row).primary} details`);
							RowDetails($$anchor, {
								get details() {
									return get($0);
								},
								get label() {
									return get($1);
								}
							});
						}
					};
					if_block(node_28, ($$render) => {
						if (get(row).expanded) $$render(consequent_28);
					});
					reset(div_22);
					template_effect(() => {
						set_attribute(div_22, "data-expanded", get(row).expanded);
						set_attribute(button_7, "data-focus-key", `view:${$$props.instanceKey}:item:${get(row).id}`);
						classes_6 = set_class(button_7, 1, "section-row", null, classes_6, { selected: get(row).selected });
						set_attribute(button_7, "data-tone", get(row).tone);
						set_text(text_25, get(row).primary);
						set_text(text_26, get(row).meta ?? get(row).secondary ?? "");
					});
					delegated("click", button_7, () => select(get(row).id));
					append($$anchor, div_22);
				});
				append($$anchor, fragment_7);
			};
			if_block(node_25, ($$render) => {
				if (!get(section).collapsed) $$render(consequent_29);
			});
			reset(section_1);
			template_effect(($0) => {
				classes_4 = set_class(section_1, 1, "", null, classes_4, { collapsed: get(section).collapsed });
				set_attribute(button_6, "data-focus-key", `view:${$$props.instanceKey}:section:${get(section).id}`);
				classes_5 = set_class(button_6, 1, "", null, classes_5, { selected: get(section).header_selected });
				set_text(text_21, `${get(section).collapsed ? "▸" : "▾"} ${get(section).title ?? ""}`);
				set_text(text_22, $0);
			}, [() => get(section).source_status ? "—" : String(get(section).count).padStart(2, "0")]);
			delegated("click", button_6, () => dispatch({
				type: "toggle_view_section",
				instance_key: $$props.instanceKey,
				section_id: get(section).id
			}));
			append($$anchor, section_1);
		});
		reset(div_20);
		template_effect(() => set_attribute(div_20, "aria-label", $$props.model.title));
		append($$anchor, div_20);
	};
	var consequent_31 = ($$anchor) => {
		EmptyState($$anchor, {
			get title() {
				return $$props.model.title;
			},
			get message() {
				return $$props.model.message;
			}
		});
	};
	var consequent_33 = ($$anchor) => {
		var section_2 = root_30();
		var h3 = sibling(child(section_2), 2);
		var text_28 = only_child(h3, true);
		var p_1 = sibling(h3, 2);
		var strong_2 = child(p_1);
		var text_29 = only_child(strong_2, true);
		var text_30 = sibling(strong_2);
		reset(p_1);
		var node_29 = sibling(p_1, 2);
		var consequent_32 = ($$anchor) => {
			var div_24 = root_28();
			each(div_24, 21, () => $$props.model.actions, index, ($$anchor, action, index) => {
				var button_9 = root_27();
				var text_31 = only_child(button_9, true);
				template_effect(() => {
					set_attribute(button_9, "data-focus-key", `view:${$$props.instanceKey}:required-subject:${index}`);
					set_attribute(button_9, "title", get(action).title);
					set_text(text_31, get(action).label);
				});
				delegated("click", button_9, () => dispatch({
					type: "activate",
					intent: get(action).intent
				}));
				append($$anchor, button_9);
			});
			reset(div_24);
			append($$anchor, div_24);
		};
		var alternate_2 = ($$anchor) => {
			append($$anchor, root_29());
		};
		if_block(node_29, ($$render) => {
			if ($$props.model.actions.length > 0) $$render(consequent_32);
			else $$render(alternate_2, -1);
		});
		reset(section_2);
		template_effect(($0) => {
			set_attribute(section_2, "aria-labelledby", `required-subject-${$$props.tileId}`);
			set_attribute(h3, "id", `required-subject-${$$props.tileId}`);
			set_text(text_28, $$props.model.title);
			set_text(text_29, $$props.model.input);
			set_text(text_30, ` needs ${$0 ?? ""}.`);
		}, [() => $$props.model.facets.join(", ")]);
		append($$anchor, section_2);
	};
	var consequent_34 = ($$anchor) => {
		SceneView($$anchor, { get scene() {
			return $$props.model.scene;
		} });
	};
	var consequent_35 = ($$anchor) => {
		FieldView($$anchor, {
			get field() {
				return $$props.model.field;
			},
			get instanceKey() {
				return $$props.instanceKey;
			}
		});
	};
	if_block(node_2, ($$render) => {
		if ($$props.model.type === "text") $$render(consequent_2);
		else if ($$props.model.type === "document") $$render(consequent_4, 1);
		else if ($$props.model.type === "rows") $$render(consequent_10, 2);
		else if ($$props.model.type === "table") $$render(consequent_16, 3);
		else if ($$props.model.type === "timeline") $$render(consequent_24, 4);
		else if ($$props.model.type === "sections") $$render(consequent_30, 5);
		else if ($$props.model.type === "placeholder") $$render(consequent_31, 6);
		else if ($$props.model.type === "required_subject") $$render(consequent_33, 7);
		else if ($$props.model.type === "map" || $$props.model.type === "atlas") $$render(consequent_34, 8);
		else if ($$props.model.type === "field") $$render(consequent_35, 9);
	});
	reset(div);
	template_effect(() => set_attribute(div, "data-view", $$props.model.type));
	append($$anchor, div);
	pop();
}
delegate(["click"]);
//#endregion
//#region browser/runtime/dialog-focus.ts
/** DOM-only modal mechanics. Neither focus nor a dialog frame chooses a
* subject, route, grant, insertion target or semantic operation. */
function dialogFocusables(panel) {
	return [...panel.querySelectorAll("button:not([disabled]),input:not([disabled]),textarea:not([disabled]),select:not([disabled]),a[href],[tabindex=\"0\"]")].filter((element) => !element.closest("[hidden],[inert],[aria-hidden=\"true\"]") && element.getClientRects().length > 0);
}
function trapDialogTab(panel, event) {
	if (event.key !== "Tab" || event.isComposing) return;
	const controls = dialogFocusables(panel);
	event.preventDefault();
	if (!controls.length) {
		panel.focus();
		return;
	}
	const current = controls.indexOf(panel.ownerDocument.activeElement);
	(controls[event.shiftKey ? current <= 0 ? controls.length - 1 : current - 1 : current < 0 || current === controls.length - 1 ? 0 : current + 1] ?? panel).focus();
}
function restoreDialogFocus(panel, opener) {
	const active = panel.ownerDocument.activeElement;
	if (opener?.isConnected && (active === panel.ownerDocument.body || active === null || panel.contains(active))) opener.focus();
}
//#endregion
//#region browser/app/DialogLayer.svelte
var root$10 = /* @__PURE__ */ from_html(`<div class="dialog-scrim" role="presentation"><div class="dialog-panel" role="dialog" aria-modal="true" tabindex="-1"><header><h2> </h2> <button class="dialog-close">×</button></header> <div class="dialog-content"><!> <!> <!></div></div></div>`);
function DialogLayer($$anchor, $$props) {
	push($$props, true);
	let panel;
	let opener = null;
	const dispatch = dispatchUi();
	const close = () => dispatch({
		type: "close_dialog",
		instance_key: $$props.model.instance_key
	});
	onMount(() => {
		const active = panel.ownerDocument.activeElement;
		opener = active instanceof HTMLElement ? active : null;
		(panel.querySelector("textarea:not([disabled]),input:not([disabled])") ?? dialogFocusables(panel)[0] ?? panel).focus();
	});
	onDestroy(() => {
		if (panel) restoreDialogFocus(panel, opener);
	});
	function handleKey(event) {
		if (event.isComposing) return;
		if (event.key === "Escape") {
			event.preventDefault();
			event.stopPropagation();
			close();
		} else if (event.key === "Tab") trapDialogTab(panel, event);
	}
	var div = root$10();
	var div_1 = child(div);
	var header = child(div_1);
	var h2 = child(header);
	var text = only_child(h2, true);
	var button = sibling(h2, 2);
	reset(header);
	var div_2 = sibling(header, 2);
	var node = child(div_2);
	var consequent = ($$anchor) => {
		InputComposer($$anchor, { get model() {
			return $$props.model.input;
		} });
	};
	if_block(node, ($$render) => {
		if ($$props.model.input?.live_filter) $$render(consequent);
	});
	var node_1 = sibling(node, 2);
	{
		let $0 = /* @__PURE__ */ user_derived(() => String($$props.model.instance_key));
		ViewRenderer(node_1, {
			get model() {
				return $$props.model.view;
			},
			get tileId() {
				return get($0);
			},
			get instanceKey() {
				return $$props.model.instance_key;
			}
		});
	}
	var node_2 = sibling(node_1, 2);
	var consequent_1 = ($$anchor) => {
		InputComposer($$anchor, { get model() {
			return $$props.model.input;
		} });
	};
	if_block(node_2, ($$render) => {
		if ($$props.model.input && !$$props.model.input.live_filter) $$render(consequent_1);
	});
	reset(div_2);
	reset(div_1);
	bind_this(div_1, ($$value) => panel = $$value, () => panel);
	reset(div);
	template_effect(() => {
		set_attribute(div_1, "aria-labelledby", `dialog-title-${$$props.model.instance_key}`);
		set_attribute(h2, "id", `dialog-title-${$$props.model.instance_key}`);
		set_text(text, $$props.model.title);
		set_attribute(button, "data-focus-key", `dialog:${$$props.model.instance_key}:close`);
		set_attribute(button, "aria-label", `Close ${$$props.model.title}`);
	});
	delegated("click", div, (event) => event.target === event.currentTarget && close());
	delegated("keydown", div_1, handleKey);
	delegated("click", button, close);
	append($$anchor, div);
	pop();
}
delegate(["click", "keydown"]);
//#endregion
//#region \0vite/preload-helper.js
var scriptRel = "modulepreload";
var assetsURL = function(dep) {
	return "/ui/assets/" + dep;
};
var seen = {};
var __vitePreload = function preload(baseModule, deps, importerUrl) {
	let promise = Promise.resolve();
	if (deps && deps.length > 0) {
		const links = document.getElementsByTagName("link");
		const cspNonceMeta = document.querySelector("meta[property=csp-nonce]");
		const cspNonce = cspNonceMeta?.nonce || cspNonceMeta?.getAttribute("nonce");
		function allSettled(promises) {
			return Promise.all(promises.map((p) => Promise.resolve(p).then((value) => ({
				status: "fulfilled",
				value
			}), (reason) => ({
				status: "rejected",
				reason
			}))));
		}
		function importMetaResolve(specifier) {
			if (import.meta.resolve) return import.meta.resolve(specifier);
			return new URL(
				specifier,
				/** #__KEEP__ */
				import.meta.url
			).href;
		}
		promise = allSettled(deps.map((dep) => {
			dep = assetsURL(dep, importerUrl);
			dep = importMetaResolve(dep);
			if (dep in seen) return;
			seen[dep] = true;
			const isCss = dep.endsWith(".css");
			for (let i = links.length - 1; i >= 0; i--) {
				const link = links[i];
				if (link.href === dep && (!isCss || link.rel === "stylesheet")) return;
			}
			const link = document.createElement("link");
			link.rel = isCss ? "stylesheet" : scriptRel;
			if (!isCss) link.as = "script";
			link.crossOrigin = "";
			link.href = dep;
			if (cspNonce) link.setAttribute("nonce", cspNonce);
			document.head.appendChild(link);
			if (isCss) return new Promise((res, rej) => {
				link.addEventListener("load", res);
				link.addEventListener("error", () => rej(/* @__PURE__ */ new Error(`Unable to preload CSS for ${dep}`)));
			});
		}).filter((p) => p !== void 0));
	}
	function handlePreloadError(err) {
		const e = new Event("vite:preloadError", { cancelable: true });
		e.payload = err;
		window.dispatchEvent(e);
		if (!e.defaultPrevented) throw err;
	}
	return promise.then((res) => {
		for (const item of res || []) {
			if (item.status !== "rejected") continue;
			handlePreloadError(item.reason);
		}
		return baseModule().catch(handlePreloadError);
	});
};
//#endregion
//#region browser/visuals/ryeos_ambient_scene.js
var THREE = null;
var G = {
	bg: 1908769,
	orange: 14048526,
	yellow: 16432431,
	blue: 8627608,
	aqua: 9355388,
	purple: 13862555,
	fg: 15457202,
	bg2: 5261637
};
var FLAT_ATLAS_MIN_ZOOM = .35;
var FLAT_ATLAS_MAX_ZOOM = 6;
function mountRyeOsAmbientScene(canvas, sceneModel = {}, options = {}) {
	let runtime = null;
	let latestSceneModel = sceneModel;
	let latestOptions = options;
	let disposed = false;
	__vitePreload(() => import("./ryeos_three.js").then((n) => n.t).then((module) => {
		if (disposed || !canvas.isConnected) return;
		THREE = module;
		runtime = startRyeOsAmbientScene(canvas, latestSceneModel, latestOptions);
	}), []).catch((error) => {
		console.warn("RyeOS ambient scene disabled; Three.js failed to load", error);
		canvas.dataset.ambientUnavailable = "true";
	});
	return {
		update(nextSceneModel = {}, nextOptions = latestOptions) {
			latestSceneModel = nextSceneModel;
			latestOptions = nextOptions;
			runtime?.update(nextSceneModel, nextOptions);
		},
		dispose() {
			disposed = true;
			runtime?.dispose();
		}
	};
}
function startRyeOsAmbientScene(canvas, sceneModel = {}, options = {}) {
	const platform = ambientPlatform(options.platform);
	const namespaceAtlas = options.mode === "namespace_atlas" || options.mode === "atlas_2d" || options.mode === "atlas_paper_3d";
	const atlasStyle = options.atlasStyle || (options.mode === "atlas_paper_3d" ? "paper_3d" : "flat_2d");
	const renderer = new THREE.WebGLRenderer({
		canvas,
		antialias: true,
		alpha: false
	});
	renderer.setPixelRatio(Math.min(platform.devicePixelRatio(), 2));
	renderer.setClearColor(G.bg, 1);
	const scene = new THREE.Scene();
	scene.fog = new THREE.FogExp2(G.bg, namespaceAtlas ? .0018 : .004);
	const camera = namespaceAtlas ? new THREE.OrthographicCamera(-18, 18, 18, -18, .1, 1e3) : new THREE.PerspectiveCamera(55, 1, .1, 1e3);
	const root = new THREE.Group();
	scene.add(root);
	const state = {
		theta: 0,
		phi: namespaceAtlas ? .18 : Math.PI / 2.2,
		radius: namespaceAtlas ? 38 : 45,
		homeTheta: 0,
		homePhi: namespaceAtlas ? .18 : Math.PI / 2.2,
		homeRadius: namespaceAtlas ? 38 : 45,
		flatPanX: 0,
		flatPanZ: 0,
		flatZoom: 1,
		homeFlatPanX: 0,
		homeFlatPanZ: 0,
		homeFlatZoom: 1,
		spinVelTheta: namespaceAtlas ? 0 : .0012,
		spinVelPhi: 0,
		zoomVel: 0,
		resetting: false,
		dragging: false,
		didDrag: false,
		lastX: 0,
		lastY: 0,
		remotes: [],
		semanticSignature: "",
		namespaceAtlas,
		atlasStyle,
		atlasInteractive: [],
		atlasAnimated: [],
		atlasFocusables: [],
		atlasHover: null,
		atlasFocus: options.atlasFocus || null,
		latestSceneModel: sceneModel,
		canvas,
		pointerNdc: new THREE.Vector2(2, 2),
		disposed: false
	};
	const spinners = [];
	const shard = namespaceAtlas ? null : makeShard();
	if (shard) root.add(shard.group);
	if (!namespaceAtlas) addRingBands(root, spinners);
	const fragments = namespaceAtlas ? [] : makeFragments(root, platform.random);
	const streams = namespaceAtlas ? null : makeStreams(root, platform.random);
	const stars = namespaceAtlas ? null : makeStars(scene, platform.random);
	const semanticLayer = new THREE.Group();
	root.add(semanticLayer);
	const raycaster = namespaceAtlas ? new THREE.Raycaster() : null;
	if (raycaster) raycaster.domElement = canvas;
	const resize = () => {
		const rect = canvas.getBoundingClientRect();
		const viewport = platform.viewport();
		const width = Math.max(1, Math.floor(rect.width || viewport.width));
		const height = Math.max(1, Math.floor(rect.height || viewport.height));
		renderer.setSize(width, height, false);
		camera.aspect = width / height;
		if (namespaceAtlas) {
			const aspect = width / height;
			const halfHeight = 18;
			camera.left = -18 * aspect;
			camera.right = halfHeight * aspect;
			camera.top = halfHeight;
			camera.bottom = -18;
		}
		camera.updateProjectionMatrix();
	};
	const updateCamera = () => {
		if (isFlatAtlas(state)) {
			camera.position.set(state.flatPanX, 38, state.flatPanZ + .01);
			camera.up.set(0, 0, -1);
			camera.lookAt(state.flatPanX, 0, state.flatPanZ);
			camera.zoom = state.flatZoom;
			camera.updateProjectionMatrix();
			return;
		}
		if (namespaceAtlas) {
			camera.position.set(0, state.radius, .01);
			camera.up.set(0, 0, -1);
			camera.lookAt(0, 0, 0);
			camera.zoom = 38 / Math.max(12, state.radius);
			camera.updateProjectionMatrix();
			return;
		}
		const x = state.radius * Math.sin(state.phi) * Math.sin(state.theta);
		const y = state.radius * Math.cos(state.phi);
		const z = state.radius * Math.sin(state.phi) * Math.cos(state.theta);
		camera.position.set(x, y, z);
		camera.up.set(0, Math.sin(state.phi) >= 0 ? 1 : -1, 0);
		camera.lookAt(0, 0, 0);
	};
	const onPointerDown = (event) => {
		if (event.button !== 0) return;
		if (state.namespaceAtlas) updatePointerNdc(state, event);
		state.dragging = true;
		state.didDrag = false;
		state.resetting = false;
		state.lastX = event.clientX;
		state.lastY = event.clientY;
		if (isFlatAtlas(state)) {
			state.spinVelTheta = 0;
			state.spinVelPhi = 0;
			state.zoomVel = 0;
		}
		canvas.setPointerCapture?.(event.pointerId);
	};
	const onPointerMove = (event) => {
		if (state.namespaceAtlas) updatePointerNdc(state, event);
		if (!state.dragging) return;
		const dx = event.clientX - state.lastX;
		const dy = event.clientY - state.lastY;
		state.lastX = event.clientX;
		state.lastY = event.clientY;
		if (Math.abs(dx) + Math.abs(dy) > 2) state.didDrag = true;
		if (isFlatAtlas(state)) {
			dispatchAtlasNavigate(state);
			const units = atlasWorldUnitsPerPixel(camera, canvas);
			state.flatPanX -= dx * units;
			state.flatPanZ -= dy * units;
			return;
		}
		state.spinVelTheta = dx * .004;
		state.spinVelPhi = -dy * .004;
	};
	const onPointerUp = (event) => {
		if (state.namespaceAtlas && !state.didDrag && state.atlasHover) dispatchAtlasSelect(state, state.atlasHover);
		state.dragging = false;
		canvas.releasePointerCapture?.(event.pointerId);
	};
	const onPointerLeave = () => {
		if (!namespaceAtlas) return;
		state.pointerNdc.set(2, 2);
		setAtlasHover(state, null);
	};
	const onWheel = (event) => {
		event.preventDefault();
		state.resetting = false;
		if (isFlatAtlas(state)) {
			dispatchAtlasNavigate(state);
			const before = flatAtlasPointUnderCursor(state, camera, canvas, event);
			state.flatZoom = clamp(state.flatZoom * Math.exp(-event.deltaY * .0012), FLAT_ATLAS_MIN_ZOOM, FLAT_ATLAS_MAX_ZOOM);
			const after = flatAtlasPointUnderCursor(state, camera, canvas, event);
			state.flatPanX += before.x - after.x;
			state.flatPanZ += before.z - after.z;
			return;
		}
		state.zoomVel += event.deltaY * .008;
		state.spinVelTheta += namespaceAtlas ? 0 : event.deltaX * 4e-4;
	};
	const onKeyDown = (event) => {
		if (event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey) return;
		if (event.key?.toLowerCase() !== "r") return;
		if (event.target?.closest?.("input, textarea, select, [contenteditable='true']")) return;
		state.resetting = true;
		state.dragging = false;
		state.spinVelTheta = 0;
		state.spinVelPhi = 0;
		state.zoomVel = 0;
	};
	canvas.addEventListener("pointerdown", onPointerDown);
	canvas.addEventListener("pointermove", onPointerMove);
	canvas.addEventListener("pointerup", onPointerUp);
	canvas.addEventListener("pointercancel", onPointerUp);
	canvas.addEventListener("pointerleave", onPointerLeave);
	canvas.addEventListener("wheel", onWheel, { passive: false });
	platform.eventTarget.addEventListener("keydown", onKeyDown);
	platform.eventTarget.addEventListener("resize", resize);
	let frame = null;
	let last = platform.now();
	const animate = (now) => {
		if (state.disposed) return;
		const reducedMotion = platform.reducedMotion();
		frame = reducedMotion ? null : platform.requestFrame(animate);
		resize();
		Math.min((now - last) / 1e3, .05);
		last = now;
		const t = reducedMotion ? 0 : now / 1e3;
		if (state.resetting) {
			const lerpSpeed = .045;
			if (isFlatAtlas(state)) {
				state.flatPanX += (state.homeFlatPanX - state.flatPanX) * lerpSpeed;
				state.flatPanZ += (state.homeFlatPanZ - state.flatPanZ) * lerpSpeed;
				state.flatZoom += (state.homeFlatZoom - state.flatZoom) * lerpSpeed;
				if (Math.abs(state.flatPanX - state.homeFlatPanX) + Math.abs(state.flatPanZ - state.homeFlatPanZ) + Math.abs(state.flatZoom - state.homeFlatZoom) < .005) state.resetting = false;
			} else {
				state.theta += (state.homeTheta - state.theta) * lerpSpeed;
				state.phi += (state.homePhi - state.phi) * lerpSpeed;
				state.radius += (state.homeRadius - state.radius) * lerpSpeed;
				if (Math.abs(state.theta - state.homeTheta) + Math.abs(state.phi - state.homePhi) + Math.abs(state.radius - state.homeRadius) < .01) state.resetting = false;
			}
		} else if (!isFlatAtlas(state)) {
			state.theta += state.spinVelTheta;
			state.phi += state.spinVelPhi;
		}
		if (!state.dragging && !state.resetting) {
			state.spinVelTheta = state.spinVelTheta * .965 + (namespaceAtlas ? 0 : 4e-5);
			state.spinVelPhi *= .965;
		}
		if (!state.resetting) {
			state.radius = Math.max(12, Math.min(120, state.radius + state.zoomVel));
			state.zoomVel *= .9;
		}
		stars?.update(t);
		streams?.update(t);
		if (shard) {
			shard.group.rotation.y = t * .065;
			shard.group.rotation.x = Math.sin(t * .045) * .08;
			shard.group.rotation.z = Math.cos(t * .035) * .05;
			shard.group.position.y = Math.sin(t * .48) * .7;
			shard.glow.material.uniforms.time.value = t;
		}
		for (const spinner of spinners) {
			spinner.group.rotation.y += spinner.spinY * .28;
			spinner.group.position.y = (shard?.group.position.y || 0) * spinner.bob;
		}
		for (const frag of fragments) {
			const a = frag.angle + t * frag.speed + frag.phase;
			frag.group.position.set(Math.cos(a) * frag.radius, frag.height + Math.sin(t * .06 + frag.phase) * .35, Math.sin(a) * frag.radius);
			frag.group.rotation.x += frag.rx * .18;
			frag.group.rotation.y += frag.ry * .18;
			frag.group.rotation.z += frag.rz * .18;
		}
		root.position.y = namespaceAtlas ? 0 : Math.sin(t * .08) * .2;
		root.rotation.y = namespaceAtlas ? 0 : t * .0015;
		for (const remote of state.remotes) {
			remote.marker.rotation.y = -root.rotation.y;
			remote.marker.scale.setScalar(.85 + Math.sin(t * 1.8 + remote.phase) * .08);
		}
		animateAtlasMarkers(state, t);
		updateCamera();
		updateAtlasHover(state, camera, raycaster);
		renderer.render(scene, camera);
	};
	const api = {
		update(nextSceneModel = {}, nextOptions = {}) {
			state.latestSceneModel = nextSceneModel;
			if (state.namespaceAtlas) state.atlasStyle = nextOptions.atlasStyle || state.atlasStyle;
			state.atlasFocus = nextOptions.atlasFocus || null;
			updateSemanticObjects(semanticLayer, state, nextSceneModel);
			applyAtlasFocus(state);
			if (platform.reducedMotion() && frame === null) frame = platform.requestFrame(animate);
		},
		dispose() {
			state.disposed = true;
			canvas.removeEventListener("pointerdown", onPointerDown);
			canvas.removeEventListener("pointermove", onPointerMove);
			canvas.removeEventListener("pointerup", onPointerUp);
			canvas.removeEventListener("pointercancel", onPointerUp);
			canvas.removeEventListener("pointerleave", onPointerLeave);
			canvas.removeEventListener("wheel", onWheel);
			platform.eventTarget.removeEventListener("keydown", onKeyDown);
			platform.eventTarget.removeEventListener("resize", resize);
			if (frame !== null) platform.cancelFrame(frame);
			disposeObject(scene);
			renderer.dispose();
		}
	};
	resize();
	updateCamera();
	frame = platform.requestFrame(animate);
	api.update(sceneModel);
	return api;
}
function ambientPlatform(overrides = {}) {
	const eventTarget = overrides.eventTarget || window;
	return {
		random: overrides.random || Math.random,
		now: overrides.now || (() => performance.now()),
		requestFrame: overrides.requestFrame || ((callback) => requestAnimationFrame(callback)),
		cancelFrame: overrides.cancelFrame || ((frame) => cancelAnimationFrame(frame)),
		eventTarget,
		devicePixelRatio: overrides.devicePixelRatio || (() => window.devicePixelRatio || 1),
		viewport: overrides.viewport || (() => ({
			width: window.innerWidth,
			height: window.innerHeight
		})),
		reducedMotion: overrides.reducedMotion || (() => window.matchMedia?.("(prefers-reduced-motion: reduce)").matches === true)
	};
}
function makeShard() {
	const group = new THREE.Group();
	const vertices = [
		[
			0,
			2.2,
			.1
		],
		[
			.4,
			1.3,
			.3
		],
		[
			-.35,
			1.5,
			-.2
		],
		[
			.7,
			.4,
			.5
		],
		[
			-.65,
			.35,
			-.4
		],
		[
			.55,
			.2,
			-.6
		],
		[
			-.5,
			.5,
			.55
		],
		[
			.2,
			-.3,
			.7
		],
		[
			-.3,
			-.2,
			-.6
		],
		[
			.6,
			-.6,
			.1
		],
		[
			-.55,
			-.7,
			-.2
		],
		[
			.15,
			-1.8,
			.15
		],
		[
			-.25,
			-1.4,
			.3
		],
		[
			.45,
			-1.2,
			-.35
		]
	].map(([x, y, z]) => new THREE.Vector3(x, y, z));
	const faces = [
		[
			0,
			1,
			2,
			0
		],
		[
			0,
			1,
			3,
			0
		],
		[
			0,
			2,
			6,
			1
		],
		[
			0,
			2,
			4,
			1
		],
		[
			1,
			3,
			5,
			1
		],
		[
			2,
			4,
			8,
			2
		],
		[
			1,
			2,
			6,
			3
		],
		[
			3,
			5,
			9,
			1
		],
		[
			4,
			6,
			10,
			2
		],
		[
			5,
			8,
			13,
			3
		],
		[
			6,
			7,
			12,
			0
		],
		[
			7,
			9,
			11,
			0
		],
		[
			3,
			6,
			7,
			2
		],
		[
			4,
			8,
			10,
			3
		],
		[
			5,
			9,
			13,
			1
		],
		[
			8,
			10,
			12,
			2
		],
		[
			9,
			11,
			13,
			0
		],
		[
			10,
			11,
			12,
			1
		],
		[
			7,
			10,
			12,
			3
		],
		[
			9,
			10,
			11,
			2
		],
		[
			3,
			7,
			9,
			1
		],
		[
			4,
			6,
			12,
			0
		],
		[
			1,
			3,
			6,
			3
		],
		[
			2,
			4,
			6,
			2
		]
	];
	const colors = [
		new THREE.Color(G.orange),
		new THREE.Color(G.yellow),
		new THREE.Color(G.aqua),
		new THREE.Color(1709844)
	];
	const positions = [];
	const vertexColors = [];
	for (const [a, b, c, ci] of faces) {
		const color = colors[ci];
		const brightness = ci === 3 ? .025 : .09;
		for (const v of [
			vertices[a],
			vertices[b],
			vertices[c]
		]) {
			positions.push(v.x, v.y, v.z);
			vertexColors.push(color.r * brightness, color.g * brightness, color.b * brightness);
		}
	}
	const geo = new THREE.BufferGeometry();
	geo.setAttribute("position", new THREE.Float32BufferAttribute(positions, 3));
	geo.setAttribute("color", new THREE.Float32BufferAttribute(vertexColors, 3));
	group.add(new THREE.Mesh(geo, new THREE.MeshBasicMaterial({
		vertexColors: true,
		transparent: true,
		opacity: .92,
		side: THREE.DoubleSide
	})));
	const edges = /* @__PURE__ */ new Map();
	for (const [a, b, c, ci] of faces) for (const [i, j] of [
		[a, b],
		[b, c],
		[a, c]
	]) {
		const key = i < j ? `${i}_${j}` : `${j}_${i}`;
		const existing = edges.get(key);
		if (!existing || ci < existing.ci) edges.set(key, {
			i,
			j,
			ci
		});
	}
	const edgeColors = [
		G.orange,
		G.yellow,
		G.aqua,
		G.purple
	];
	for (const { i, j, ci } of edges.values()) group.add(new THREE.Line(new THREE.BufferGeometry().setFromPoints([vertices[i], vertices[j]]), lineMat(ci === 3 ? G.bg2 : edgeColors[ci], ci === 3 ? .2 : [
		1,
		.85,
		.7,
		.5
	][ci])));
	const glow = new THREE.Mesh(new THREE.SphereGeometry(2.8, 24, 24), new THREE.ShaderMaterial({
		uniforms: { time: { value: 0 } },
		transparent: true,
		depthWrite: false,
		side: THREE.BackSide,
		vertexShader: `varying vec3 vN; varying vec3 vP; void main(){vN=normalize(normalMatrix*normal);vP=position;gl_Position=projectionMatrix*modelViewMatrix*vec4(position,1.);}`,
		fragmentShader: `uniform float time; varying vec3 vN; varying vec3 vP; void main(){float rim=pow(1.-abs(dot(vN,vec3(0,0,1))),1.8);float pulse=.5+.5*sin(time*.6+vP.y*1.2);float y=clamp((vP.y+2.5)/5.,0.,1.);vec3 c=mix(vec3(.996,.502,.098),vec3(.98,.741,.184),y);gl_FragColor=vec4(c,rim*pulse*.35);}`
	}));
	glow.scale.set(1, 1.2, 1);
	group.add(glow);
	return {
		group,
		glow
	};
}
function addRingBands(root, spinners) {
	const defs = [
		[
			.4,
			.3,
			"circ",
			4.5,
			90,
			G.orange,
			.9,
			.024,
			1.4
		],
		[
			1.1,
			.5,
			"circ",
			5.2,
			90,
			G.orange,
			.75,
			-.019,
			.3
		],
		[
			.785,
			.4,
			"poly",
			5.8,
			6,
			G.yellow,
			.7,
			.015,
			.9
		],
		[
			1,
			-.8,
			"circ",
			4.8,
			90,
			G.yellow,
			.6,
			-.013,
			1.8
		],
		[
			.15,
			0,
			"circ",
			12,
			110,
			G.aqua,
			.65,
			.008,
			.5
		],
		[
			1.45,
			.3,
			"circ",
			14,
			110,
			G.aqua,
			.55,
			-.007,
			1.6
		],
		[
			.7,
			-.6,
			"poly",
			11,
			8,
			G.yellow,
			.5,
			.006,
			.2
		],
		[
			1.1,
			1,
			"circ",
			13,
			110,
			G.blue,
			.4,
			-.005,
			1.1
		],
		[
			.2,
			0,
			"circ",
			22,
			140,
			G.blue,
			.3,
			.003,
			.7
		],
		[
			.9,
			.5,
			"poly",
			28,
			8,
			G.purple,
			.22,
			-.0025,
			1.3
		],
		[
			1.4,
			-.3,
			"dash",
			25,
			90,
			G.purple,
			.18,
			.002,
			.4
		],
		[
			.4,
			1.2,
			"circ",
			35,
			160,
			G.bg2,
			.15,
			-.0015,
			1
		],
		[
			1.2,
			.8,
			"poly",
			42,
			12,
			G.bg2,
			.1,
			.001,
			.15
		]
	];
	for (const [rx, rz, shape, r, sides, color, opacity, spinY, bob] of defs) {
		const mesh = shape === "dash" ? dashedRing(r, sides, color, opacity) : ring(shape === "poly" ? polyPoints(sides, r) : circlePoints(r, sides), color, opacity);
		const group = new THREE.Group();
		group.rotation.set(rx, 0, rz);
		group.add(mesh);
		root.add(group);
		spinners.push({
			group,
			spinY,
			bob
		});
	}
}
function makeFragments(root, random) {
	return [
		[
			5,
			.8,
			3.14,
			.25,
			.018
		],
		[
			5.5,
			-.4,
			2.8,
			.18,
			.02
		],
		[
			4.8,
			.3,
			4.2,
			.3,
			.016
		],
		[
			5.2,
			-.7,
			.8,
			.2,
			.022
		],
		[
			9,
			1.5,
			2.5,
			.4,
			.012
		],
		[
			10.5,
			-1.2,
			3.8,
			.35,
			.01
		],
		[
			8.5,
			.5,
			3,
			.5,
			.011
		],
		[
			11,
			-2,
			.5,
			.3,
			.013
		],
		[
			18,
			2.5,
			.7,
			.6,
			.005
		],
		[
			22,
			-1.8,
			2,
			.55,
			.004
		],
		[
			16,
			.8,
			3.5,
			.7,
			.006
		],
		[
			20,
			-3,
			1.2,
			.45,
			.003
		]
	].map(([radius, height, phase, scale, speed], index) => {
		const group = miniShard(scale, radius > 14 ? .48 : .78);
		root.add(group);
		return {
			group,
			radius,
			height,
			phase,
			angle: index * .9,
			speed,
			rx: (random() - .5) * .012,
			ry: (random() - .5) * .016,
			rz: (random() - .5) * .01
		};
	});
}
function miniShard(scale, opacity) {
	const verts = [
		[
			0,
			.7,
			.1
		],
		[
			.5,
			.1,
			.3
		],
		[
			-.4,
			.15,
			-.2
		],
		[
			.25,
			-.4,
			.35
		],
		[
			-.3,
			-.5,
			.1
		],
		[
			.4,
			.2,
			-.4
		]
	].map(([x, y, z]) => new THREE.Vector3(x, y, z));
	const faces = [
		[
			0,
			1,
			2,
			0
		],
		[
			0,
			2,
			5,
			1
		],
		[
			1,
			3,
			4,
			2
		],
		[
			2,
			3,
			4,
			3
		],
		[
			0,
			1,
			5,
			1
		],
		[
			3,
			4,
			5,
			0
		]
	];
	const group = new THREE.Group();
	const edgeColors = [
		G.orange,
		G.yellow,
		G.aqua,
		G.purple
	];
	for (const [a, b, c, ci] of faces) group.add(new THREE.Line(new THREE.BufferGeometry().setFromPoints([
		verts[a],
		verts[b],
		verts[c],
		verts[a]
	]), lineMat(edgeColors[ci], opacity)));
	group.scale.setScalar(scale);
	return group;
}
function makeStreams(root, random) {
	const count = 480;
	const positions = new Float32Array(count * 3);
	const sizes = new Float32Array(count);
	const colors = new Float32Array(count * 3);
	const phases = new Float32Array(count);
	const defs = [
		new THREE.Color(G.orange),
		new THREE.Color(G.aqua),
		new THREE.Color(G.yellow),
		new THREE.Color(G.purple)
	];
	for (let i = 0; i < count; i++) {
		phases[i] = random();
		sizes[i] = .8 + random() * 1.5;
		const c = defs[i % defs.length];
		const fade = .3 + random() * .7;
		colors[i * 3] = c.r * fade;
		colors[i * 3 + 1] = c.g * fade;
		colors[i * 3 + 2] = c.b * fade;
	}
	const geo = new THREE.BufferGeometry();
	geo.setAttribute("position", new THREE.Float32BufferAttribute(positions, 3));
	geo.setAttribute("size", new THREE.Float32BufferAttribute(sizes, 1));
	geo.setAttribute("color", new THREE.Float32BufferAttribute(colors, 3));
	const mat = pointMaterial(.6, 150);
	const points = new THREE.Points(geo, mat);
	root.add(points);
	return { update(t) {
		for (let i = 0; i < count; i++) {
			const stream = i % 4;
			const phase = (phases[i] + t * .08) % 1;
			const spin = [
				1,
				-.8,
				.6,
				-1.1
			][stream];
			const tiltX = [
				.3,
				1.2,
				.7,
				-.4
			][stream];
			const tiltZ = [
				.2,
				-.5,
				.9,
				1.3
			][stream];
			const r = 1 + phase * 18;
			const angle = phase * Math.PI * 4 * spin + i * .5;
			const h = (phase - .5) * 6;
			let x = Math.cos(angle) * r, y = h, z = Math.sin(angle) * r;
			const cx = Math.cos(tiltX), sx = Math.sin(tiltX), cz = Math.cos(tiltZ), sz = Math.sin(tiltZ);
			const y2 = y * cx - z * sx, z2 = y * sx + z * cx;
			positions[i * 3] = x * cz - y2 * sz;
			positions[i * 3 + 1] = x * sz + y2 * cz;
			positions[i * 3 + 2] = z2;
			const life = phase < .1 ? phase / .1 : phase > .85 ? (1 - phase) / .15 : 1;
			sizes[i] = (.5 + phase * 1.5) * life;
		}
		geo.attributes.position.needsUpdate = true;
		geo.attributes.size.needsUpdate = true;
	} };
}
function makeStars(scene, random) {
	const count = 2200;
	const positions = new Float32Array(count * 3);
	const sizes = new Float32Array(count);
	const colors = new Float32Array(count * 3);
	const baseSizes = new Float32Array(count);
	const freqs = new Float32Array(count);
	const phases = new Float32Array(count);
	for (let i = 0; i < count; i++) {
		const theta = random() * Math.PI * 2;
		const phi = Math.acos(2 * random() - 1);
		const r = 60 + random() * 320;
		positions[i * 3] = r * Math.sin(phi) * Math.cos(theta);
		positions[i * 3 + 1] = r * Math.sin(phi) * Math.sin(theta);
		positions[i * 3 + 2] = r * Math.cos(phi);
		baseSizes[i] = sizes[i] = .4 + random() * 2.2;
		freqs[i] = .3 + random() * 2.5;
		phases[i] = random() * Math.PI * 2;
		const tint = random();
		if (tint < .72) {
			const b = .48 + random() * .46;
			colors[i * 3] = b;
			colors[i * 3 + 1] = b * .92;
			colors[i * 3 + 2] = b * .74;
		} else if (tint < .84) {
			colors[i * 3] = .86;
			colors[i * 3 + 1] = .55;
			colors[i * 3 + 2] = .28;
		} else if (tint < .94) {
			colors[i * 3] = .7;
			colors[i * 3 + 1] = .52;
			colors[i * 3 + 2] = .34;
		} else {
			colors[i * 3] = .42;
			colors[i * 3 + 1] = .52;
			colors[i * 3 + 2] = .56;
		}
	}
	const geo = new THREE.BufferGeometry();
	geo.setAttribute("position", new THREE.Float32BufferAttribute(positions, 3));
	geo.setAttribute("size", new THREE.Float32BufferAttribute(sizes, 1));
	geo.setAttribute("color", new THREE.Float32BufferAttribute(colors, 3));
	const points = new THREE.Points(geo, pointMaterial(.75, 200));
	scene.add(points);
	return { update(t) {
		for (let i = 0; i < count; i++) sizes[i] = baseSizes[i] * (.55 + .45 * Math.sin(t * freqs[i] + phases[i]));
		geo.attributes.size.needsUpdate = true;
		points.rotation.y = t * 3e-4;
		points.rotation.x = t * 1e-4;
	} };
}
function updateSemanticObjects(layer, state, sceneModel) {
	const signature = semanticSignature(sceneModel, state.namespaceAtlas);
	if (signature === state.semanticSignature) return;
	state.semanticSignature = signature;
	disposeLayer(layer);
	state.remotes = [];
	state.atlasInteractive = [];
	state.atlasAnimated = [];
	state.atlasFocusables = [];
	state.atlasHover = null;
	if (state.namespaceAtlas && sceneModel?.atlas) {
		updateAtlasObjects(layer, sceneModel.atlas, state);
		return;
	}
	if (state.namespaceAtlas) return;
	const objects = (sceneModel?.objects || []).filter((object) => object.kind !== "local_node");
	objects.forEach((object, index) => {
		const marker = semanticMarker(object);
		const position = object.position || [
			0,
			0,
			0
		];
		marker.position.set(position[0] || 0, position[1] || 0, position[2] || 0);
		if (object.kind === "remote_node") {
			const angle = index / Math.max(1, objects.length) * Math.PI * 2;
			marker.position.set(Math.cos(angle) * 7.5, .8 + index * .08, Math.sin(angle) * 7.5);
			state.remotes.push({
				marker,
				phase: angle
			});
		}
		layer.add(marker);
	});
}
function semanticSignature(sceneModel, namespaceAtlas = false) {
	if (namespaceAtlas && sceneModel?.atlas) {
		const nodes = sceneModel.atlas.nodes || [];
		const ui = sceneModel.atlas.ui || {};
		return `atlas:${sceneModel.atlas.selected_ref || ""}:${(ui.visible_layers || []).join(",")}:${ui.active_lens || "none"}:` + nodes.map((node) => [
			node.id,
			node.stack?.length || 0,
			node.state?.selected ? "s" : "",
			node.state?.highlighted ? "h" : "",
			node.state?.dimmed ? "d" : ""
		].join("/")).join(";") + `:${sceneModel.atlas.regions?.length || 0}:${sceneModel.atlas.links?.length || 0}`;
	}
	return (sceneModel?.objects || []).map((object) => [
		object.id,
		object.kind,
		object.color,
		object.label,
		object.scale?.[0],
		object.position?.join(":")
	].join("|")).join(";");
}
function updateAtlasObjects(layer, atlas, state = {}) {
	const group = new THREE.Group();
	group.userData.sceneObjectKind = "namespace_atlas";
	const nodes = atlas?.nodes || [];
	const nodeByKey = new Map(nodes.map((node) => [node.namespace_key || "", node]));
	const nodeById = new Map(nodes.map((node) => [node.id || "", node]));
	const radiusScale = atlas?.bounds?.radius_max ? Math.min(3.2, 14 / Math.max(1, atlas.bounds.radius_max)) : 1.6;
	for (const node of nodes) {
		if (!node.path?.length) continue;
		const anchor = atlasVec(node.position, radiusScale, .04);
		const marker = atlasFolderMarker(node);
		marker.position.copy(anchor);
		registerAtlasMarker(state, marker, node);
		registerAtlasFocusable(state, marker, {
			type: "folder",
			folderId: node.id,
			folderPath: node.namespace_key || ""
		});
		group.add(marker);
	}
	for (const node of nodes) {
		if (!node.path?.length) continue;
		const parentKey = node.path.slice(0, -1).join("/");
		const parent = nodeByKey.get(parentKey);
		if (!parent) continue;
		const line = new THREE.Line(new THREE.BufferGeometry().setFromPoints([atlasVec(parent.position, radiusScale, 0), atlasVec(node.position, radiusScale, 0)]), lineMat(node.stack?.length ? G.yellow : G.bg2, node.stack?.length ? .28 : .16));
		registerAtlasFocusable(state, line, {
			type: "branch",
			folderId: node.id,
			folderPath: node.namespace_key || "",
			parentPath: parent.namespace_key || ""
		});
		group.add(line);
	}
	const rootAnchor = atlasVec(nodeByKey.get("")?.position || [
		0,
		0,
		0
	], radiusScale, .03);
	const rootHub = new THREE.Group();
	rootHub.position.copy(rootAnchor);
	rootHub.add(new THREE.Mesh(new THREE.SphereGeometry(.18, 20, 14), new THREE.MeshBasicMaterial({
		color: G.orange,
		transparent: true,
		opacity: .82
	})));
	const rootRing = new THREE.LineLoop(new THREE.BufferGeometry().setFromPoints(circlePoints(.42, 44)), lineMat(G.yellow, .7));
	rootRing.rotation.x = Math.PI / 2;
	rootHub.add(rootRing);
	rootHub.userData.baseScale = 1;
	rootHub.userData.pulse = .18;
	rootHub.userData.phase = 0;
	state.atlasAnimated?.push(rootHub);
	group.add(rootHub);
	for (const region of atlas?.regions || []) {
		const radiusMin = Math.max(.2, (region.radius_min || 0) * radiusScale);
		const radiusMax = Math.max(radiusMin + .2, (region.radius_max || region.radius_min || 1) * radiusScale);
		const start = region.angle_start || 0;
		const end = region.angle_end || start;
		group.add(arcLine(radiusMax, start, end, G.purple, .34));
		group.add(new THREE.Line(new THREE.BufferGeometry().setFromPoints([new THREE.Vector3(Math.cos(start) * radiusMin, .02, Math.sin(start) * radiusMin), new THREE.Vector3(Math.cos(start) * radiusMax, .02, Math.sin(start) * radiusMax)]), lineMat(G.purple, .18)));
		group.add(new THREE.Line(new THREE.BufferGeometry().setFromPoints([new THREE.Vector3(Math.cos(end) * radiusMin, .02, Math.sin(end) * radiusMin), new THREE.Vector3(Math.cos(end) * radiusMax, .02, Math.sin(end) * radiusMax)]), lineMat(G.purple, .18)));
	}
	for (const link of atlas?.links || []) {
		const from = nodeById.get(link.from);
		const to = nodeById.get(link.to);
		if (!from || !to) continue;
		group.add(new THREE.Line(new THREE.BufferGeometry().setFromPoints([atlasVec(from.position, radiusScale, .42), atlasVec(to.position, radiusScale, .42)]), lineMat(G.yellow, .62)));
	}
	for (const node of nodes) {
		if (!node.path?.length && !(node.stack || []).length) continue;
		const stack = node.stack || [];
		const anchor = atlasVec(node.position, radiusScale, 0);
		if (!stack.length && !node.state?.selected && !node.state?.highlighted) continue;
		if (!stack.length) {
			const bead = new THREE.Mesh(new THREE.SphereGeometry(.05, 8, 6), new THREE.MeshBasicMaterial({
				color: G.yellow,
				transparent: true,
				opacity: node.state?.selected ? .65 : .38
			}));
			bead.position.copy(anchor);
			group.add(bead);
			continue;
		}
		const haloOpacity = node.state?.selected ? .95 : node.state?.highlighted ? .7 : node.state?.dimmed ? .16 : .42;
		const halo = new THREE.LineLoop(new THREE.BufferGeometry().setFromPoints(circlePoints(.18 + stack.length * .025, 28)), lineMat(node.state?.selected ? G.yellow : G.orange, haloOpacity));
		halo.position.copy(anchor);
		halo.rotation.x = Math.PI / 2;
		group.add(halo);
		for (const item of stack) {
			const marker = atlasStackMarker(item, node.state || {});
			const offset = (item.y_offset || 0) * .55;
			marker.position.set(anchor.x, anchor.y + .12 + offset, anchor.z);
			registerAtlasMarker(state, marker, node);
			registerAtlasFocusable(state, marker, {
				type: "item",
				ref: atlasItemRef(item),
				kind: item.kind || "other",
				folderId: node.id,
				folderPath: node.namespace_key || "",
				interaction: item.interaction || null
			});
			group.add(marker);
		}
	}
	layer.add(group);
}
function atlasVec(position = [
	0,
	0,
	0
], scale = 1, yOffset = 0) {
	return new THREE.Vector3((position[0] || 0) * scale, (position[1] || 0) + yOffset, (position[2] || 0) * scale);
}
function atlasFolderMarker(node) {
	const isRootFolder = (node.path?.length || 0) === 1;
	const hasItems = (node.stack || []).length > 0;
	const branch = (node.angle_end || 0) - (node.angle_start || 0);
	const radius = isRootFolder ? .16 : hasItems ? .105 : .07;
	const color = node.state?.selected ? G.yellow : node.state?.highlighted ? G.orange : isRootFolder ? G.aqua : hasItems ? G.fg : G.bg2;
	const opacity = node.state?.dimmed ? .18 : isRootFolder ? .76 : hasItems ? .5 : .34;
	const marker = new THREE.Mesh(new THREE.SphereGeometry(radius, isRootFolder ? 18 : 12, isRootFolder ? 12 : 8), new THREE.MeshBasicMaterial({
		color,
		transparent: true,
		opacity
	}));
	marker.userData.sceneObjectId = node.id;
	marker.userData.sceneObjectKind = isRootFolder ? "atlas_root_folder" : "atlas_folder";
	marker.userData.sceneObjectLabel = node.namespace_key || node.label || "folder";
	marker.userData.atlasInteraction = node.interaction || null;
	marker.userData.baseScale = 1;
	marker.userData.hoverScale = isRootFolder ? 1.65 : 1.9;
	marker.userData.pulse = node.state?.selected || node.state?.highlighted ? .18 : isRootFolder ? .08 : 0;
	marker.userData.phase = branch * 3;
	return marker;
}
function registerAtlasMarker(state, marker, node) {
	if (!state) return;
	state.atlasInteractive?.push(marker);
	if (marker.userData.pulse || node.state?.selected || node.state?.highlighted) state.atlasAnimated?.push(marker);
}
function registerAtlasFocusable(state, object, meta = {}) {
	if (!state || !object) return;
	object.userData.atlasFocus = meta;
	if (meta.interaction) object.userData.atlasInteraction = meta.interaction;
	object.userData.baseOpacity ??= object.material?.opacity;
	object.traverse?.((child) => {
		child.userData.atlasFocus = meta;
		child.userData.baseOpacity ??= child.material?.opacity;
	});
	state.atlasFocusables?.push(object);
}
function isFlatAtlas(state) {
	return state.namespaceAtlas && state.atlasStyle === "flat_2d";
}
function atlasWorldUnitsPerPixel(camera, canvas) {
	const rect = canvas.getBoundingClientRect();
	const height = Math.max(1, rect.height);
	return (camera.top - camera.bottom) / height / Math.max(.001, camera.zoom || 1);
}
function flatAtlasPointUnderCursor(state, camera, canvas, event) {
	const rect = canvas.getBoundingClientRect();
	const x01 = rect.width ? (event.clientX - rect.left) / rect.width : .5;
	const y01 = rect.height ? (event.clientY - rect.top) / rect.height : .5;
	const ndcX = x01 * 2 - 1;
	const ndcY = -(y01 * 2 - 1);
	const halfW = (camera.right - camera.left) / 2 / Math.max(.001, state.flatZoom);
	const halfH = (camera.top - camera.bottom) / 2 / Math.max(.001, state.flatZoom);
	return {
		x: state.flatPanX + ndcX * halfW,
		z: state.flatPanZ - ndcY * halfH
	};
}
function updatePointerNdc(state, event) {
	const rect = event.currentTarget.getBoundingClientRect();
	const x = rect.width ? (event.clientX - rect.left) / rect.width : .5;
	const y = rect.height ? (event.clientY - rect.top) / rect.height : .5;
	state.pointerNdc.set(x * 2 - 1, -(y * 2 - 1));
}
function updateAtlasHover(state, camera, raycaster) {
	if (!state.namespaceAtlas || !raycaster || !state.atlasInteractive?.length) return;
	raycaster.setFromCamera(state.pointerNdc, camera);
	setAtlasHover(state, raycaster.intersectObjects(state.atlasInteractive, true)[0]?.object || null);
}
function clamp(value, min, max) {
	return Math.min(max, Math.max(min, value));
}
function setAtlasHover(state, next) {
	if (state.atlasHover === next) return;
	if (state.atlasHover) {
		const base = state.atlasHover.userData.baseScale || 1;
		state.atlasHover.scale.setScalar(base);
		if (state.atlasHover.material?.opacity !== void 0 && state.atlasHover.userData.baseOpacity !== void 0) state.atlasHover.material.opacity = state.atlasHover.userData.baseOpacity;
	}
	state.atlasHover = next;
	dispatchAtlasHover(state, next);
	if (next) {
		next.userData.baseOpacity ??= next.material?.opacity;
		next.scale.setScalar(next.userData.hoverScale || 1.5);
		if (next.material?.opacity !== void 0) next.material.opacity = Math.min(1, next.material.opacity + .22);
	}
	applyAtlasFocus(state);
}
function dispatchAtlasHover(state, object) {
	state.canvas?.dispatchEvent(new CustomEvent("ryeos-atlas-hover", { detail: object ? {
		id: object.userData.sceneObjectId,
		kind: object.userData.sceneObjectKind,
		label: object.userData.sceneObjectLabel,
		path: object.userData.atlasFocus?.folderPath || "",
		interaction: object.userData.atlasInteraction || object.userData.atlasFocus?.interaction || null
	} : null }));
}
function dispatchAtlasSelect(state, object) {
	state.canvas?.dispatchEvent(new CustomEvent("ryeos-atlas-select", { detail: object ? {
		id: object.userData.sceneObjectId,
		kind: object.userData.sceneObjectKind,
		label: object.userData.sceneObjectLabel,
		path: object.userData.atlasFocus?.folderPath || "",
		interaction: object.userData.atlasInteraction || object.userData.atlasFocus?.interaction || null,
		sceneModel: state.latestSceneModel
	} : null }));
}
function dispatchAtlasNavigate(state) {
	state.canvas?.dispatchEvent(new CustomEvent("ryeos-atlas-navigate"));
}
function applyAtlasFocus(state) {
	if (!state.namespaceAtlas || !state.atlasFocusables?.length) return;
	const focus = state.atlasFocus;
	for (const object of state.atlasFocusables) {
		const active = !focus || atlasFocusRelated(focus, object.userData.atlasFocus || {});
		object.traverse?.((child) => applyAtlasFocusMaterial(child, active, Boolean(focus)));
		applyAtlasFocusMaterial(object, active, Boolean(focus));
	}
}
function applyAtlasFocusMaterial(object, active, hasFocus) {
	const material = object.material;
	if (!material || material.opacity === void 0) return;
	object.userData.baseOpacity ??= material.opacity;
	if (!hasFocus) material.opacity = object.userData.baseOpacity;
	else material.opacity = active ? Math.min(1, object.userData.baseOpacity + .18) : Math.max(.08, object.userData.baseOpacity * .25);
}
function atlasFocusRelated(focus, meta) {
	if (!focus) return true;
	if (focus.type === "kind") return meta.kind === focus.kind;
	if (focus.type === "item") {
		const ref = focus.ref || focus.id || "";
		return Boolean(ref) && meta.ref === ref;
	}
	if (focus.type === "folder") {
		const focusPath = focus.path || "";
		return meta.folderId === focus.id || meta.folderPath === focusPath || focusPath && meta.folderPath?.startsWith(`${focusPath}/`) || focusPath && meta.parentPath?.startsWith(focusPath);
	}
	return true;
}
function animateAtlasMarkers(state, t) {
	for (const marker of state.atlasAnimated || []) {
		if (marker === state.atlasHover) continue;
		const pulse = marker.userData.pulse || 0;
		if (!pulse) continue;
		const base = marker.userData.baseScale || 1;
		const phase = marker.userData.phase || 0;
		marker.scale.setScalar(base + Math.sin(t * 2.4 + phase) * pulse);
	}
}
function atlasStackMarker(item, state) {
	const kind = item.kind || "other";
	const color = atlasKindColor(kind);
	const scope = item.scope || "unknown";
	const scopeOpacity = scope === "system" ? .44 : scope === "user" ? .54 : .62;
	const opacity = state.selected ? .98 : state.highlighted ? .86 : state.dimmed ? .18 : scopeOpacity;
	let geometry;
	switch (kind) {
		case "directive":
			geometry = new THREE.OctahedronGeometry(.16, 0);
			break;
		case "tool":
			geometry = new THREE.TorusGeometry(.15, .018, 8, 28);
			break;
		case "knowledge":
			geometry = new THREE.DodecahedronGeometry(.14, 0);
			break;
		case "config":
			geometry = new THREE.BoxGeometry(.18, .035, .18);
			break;
		case "file":
			geometry = new THREE.BoxGeometry(.13, .02, .18);
			break;
		default: geometry = new THREE.SphereGeometry(.12, 10, 8);
	}
	const material = new THREE.MeshBasicMaterial({
		color,
		transparent: true,
		opacity,
		wireframe: kind !== "config" && kind !== "file"
	});
	const marker = new THREE.Mesh(geometry, material);
	marker.userData.sceneObjectId = atlasItemRef(item);
	marker.userData.sceneObjectKind = kind;
	marker.userData.sceneObjectLabel = item.label;
	return marker;
}
function atlasItemRef(item = {}) {
	return item.canonical_ref || item.id || item.label || "";
}
function arcLine(radius, start, end, color, opacity) {
	const points = [];
	const segments = Math.max(8, Math.ceil(Math.abs(end - start) * 24));
	for (let i = 0; i <= segments; i++) {
		const angle = start + (end - start) * (i / segments);
		points.push(new THREE.Vector3(Math.cos(angle) * radius, .02, Math.sin(angle) * radius));
	}
	return new THREE.Line(new THREE.BufferGeometry().setFromPoints(points), lineMat(color, opacity));
}
function atlasKindColor(kind) {
	switch (kind) {
		case "directive": return G.purple;
		case "tool": return G.aqua;
		case "knowledge": return G.yellow;
		case "config": return G.blue;
		case "file": return G.fg;
		default: return G.fg;
	}
}
function semanticMarker(object) {
	const color = colorValue(object.color || "#fabd2f");
	const opacity = object.opacity ?? .82;
	const size = Math.max(.35, object.scale?.[0] || .8);
	let geometry;
	let material = new THREE.MeshBasicMaterial({
		color,
		wireframe: true,
		transparent: true,
		opacity
	});
	switch (object.kind) {
		case "project_core":
			geometry = new THREE.IcosahedronGeometry(.5 * size, 1);
			material = new THREE.MeshBasicMaterial({
				color,
				transparent: true,
				opacity: .18 + opacity * .22
			});
			break;
		case "space_ring":
			geometry = new THREE.TorusGeometry(3 * size, .025, 8, 96);
			break;
		case "item_cluster":
			geometry = new THREE.DodecahedronGeometry(.48 * size, 0);
			break;
		case "thread_flow":
			geometry = new THREE.TorusKnotGeometry(.38 * size, .018, 80, 8);
			break;
		case "schedule_pulse":
			geometry = new THREE.OctahedronGeometry(.42 * size, 0);
			break;
		case "service_beacon":
			geometry = new THREE.ConeGeometry(.36 * size, .82 * size, 4);
			break;
		case "remote_node":
			geometry = new THREE.OctahedronGeometry(.38 * size, 0);
			break;
		case "link":
			geometry = new THREE.BoxGeometry(.92 * size, .025, .025);
			material = new THREE.MeshBasicMaterial({
				color,
				transparent: true,
				opacity: Math.min(opacity, .44)
			});
			break;
		default: geometry = new THREE.SphereGeometry(.32 * size, 12, 8);
	}
	const marker = new THREE.Mesh(geometry, material);
	marker.userData.sceneObjectId = object.id;
	marker.userData.sceneObjectKind = object.kind;
	marker.userData.sceneObjectLabel = object.label;
	return marker;
}
function disposeLayer(layer) {
	for (const child of layer.children) disposeObject(child);
	layer.clear();
}
function disposeObject(object) {
	object.traverse?.((child) => {
		child.geometry?.dispose?.();
		if (Array.isArray(child.material)) child.material.forEach((material) => material.dispose?.());
		else child.material?.dispose?.();
	});
}
function circlePoints(r, segs) {
	return Array.from({ length: segs + 1 }, (_, i) => {
		const a = i / segs * Math.PI * 2;
		return new THREE.Vector3(Math.cos(a) * r, 0, Math.sin(a) * r);
	});
}
function polyPoints(sides, r) {
	return circlePoints(r, sides);
}
function ring(points, color, opacity) {
	return new THREE.LineLoop(new THREE.BufferGeometry().setFromPoints(points), lineMat(color, opacity));
}
function dashedRing(r, segs, color, opacity) {
	const positions = [];
	for (let i = 0; i < segs; i++) {
		if (i % 2 === 0) continue;
		const a1 = i / segs * Math.PI * 2;
		const a2 = (i + 1) / segs * Math.PI * 2;
		for (let s = 0; s <= 4; s++) {
			const a = a1 + (a2 - a1) * (s / 4);
			positions.push(Math.cos(a) * r, 0, Math.sin(a) * r);
		}
	}
	const geo = new THREE.BufferGeometry();
	geo.setAttribute("position", new THREE.Float32BufferAttribute(positions, 3));
	return new THREE.LineSegments(geo, lineMat(color, opacity));
}
function lineMat(color, opacity) {
	return new THREE.LineBasicMaterial({
		color,
		opacity,
		transparent: true
	});
}
function pointMaterial(alpha, scale) {
	return new THREE.ShaderMaterial({
		vertexShader: `attribute float size; varying vec3 vColor; void main(){vColor=color;vec4 mvPos=modelViewMatrix*vec4(position,1.0);gl_PointSize=size*(${scale.toFixed(1)}/-mvPos.z);gl_Position=projectionMatrix*mvPos;}`,
		fragmentShader: `varying vec3 vColor; void main(){float d=length(gl_PointCoord-.5)*2.;float a=(1.-smoothstep(0.,1.,d))*${alpha.toFixed(2)};gl_FragColor=vec4(vColor,a);}`,
		transparent: true,
		depthWrite: false,
		blending: THREE.AdditiveBlending,
		vertexColors: true
	});
}
function colorValue(value) {
	if (typeof value === "string" && value.startsWith("#")) return parseInt(value.slice(1), 16);
	return G.aqua;
}
//#endregion
//#region browser/app/AmbientLayer.svelte
var root$9 = /* @__PURE__ */ from_html(`<div class="ambient-layer" aria-hidden="true"><canvas></canvas></div>`);
function AmbientLayer($$anchor, $$props) {
	push($$props, true);
	let canvas;
	let controller = null;
	const options = /* @__PURE__ */ user_derived(() => ({
		mode: $$props.ambient.mode,
		...$$props.ambient.atlas ? { atlasStyle: $$props.ambient.atlas.style } : {}
	}));
	onMount(() => {
		controller = mountRyeOsAmbientScene(canvas, $$props.scene, get(options));
		return () => {
			controller?.dispose();
			controller = null;
		};
	});
	user_effect(() => {
		controller?.update($$props.scene, get(options));
	});
	var div = root$9();
	bind_this(child(div), ($$value) => canvas = $$value, () => canvas);
	reset(div);
	template_effect(() => set_style(div, `--ambient-opacity:${$$props.ambient.opacity ?? 1}`));
	append($$anchor, div);
	pop();
}
//#endregion
//#region browser/app/StatusBar.svelte
var root$8 = /* @__PURE__ */ from_html(`<span> </span>`);
var root_1$6 = /* @__PURE__ */ from_html(`<footer class="status-bar"><!> <span class="key-hint"> </span> <button class="ryeos-launcher svelte-19xwx1o" data-focus-key="shell:launch">RyeOS</button></footer>`);
function StatusBar($$anchor, $$props) {
	push($$props, true);
	const dispatch = dispatchUi();
	var fragment = comment();
	var node = first_child(fragment);
	var consequent = ($$anchor) => {
		var footer = root_1$6();
		var node_1 = child(footer);
		each(node_1, 17, () => $$props.model.segments, index, ($$anchor, segment) => {
			var span = root$8();
			let classes;
			var text = only_child(span);
			template_effect(() => {
				set_attribute(span, "data-tone", get(segment).tone);
				classes = set_class(span, 1, "", null, classes, { "status-spacer": get(segment).grow });
				set_text(text, `${get(segment).label ? `${get(segment).label} ` : ""}${get(segment).value ?? ""}`);
			});
			append($$anchor, span);
		});
		var span_1 = sibling(node_1, 2);
		var text_1 = only_child(span_1, true);
		var button = sibling(span_1, 2);
		reset(footer);
		template_effect(() => set_text(text_1, $$props.model.key_hint));
		delegated("click", button, () => dispatch({ type: "open_launcher" }));
		append($$anchor, footer);
	};
	if_block(node, ($$render) => {
		if ($$props.model.visible) $$render(consequent);
	});
	append($$anchor, fragment);
	pop();
}
delegate(["click"]);
//#endregion
//#region browser/app/SystemBar.svelte
var root$7 = /* @__PURE__ */ from_html(`<span class="surface-number svelte-8hxs5n"> </span>`);
var root_1$5 = /* @__PURE__ */ from_html(`<form class="svelte-8hxs5n"><input aria-label="Surface name" class="svelte-8hxs5n"/> <button type="submit" class="svelte-8hxs5n">Save</button> <button type="button" class="svelte-8hxs5n">Cancel</button></form>`);
var root_2$4 = /* @__PURE__ */ from_html(`<button class="svelte-8hxs5n"><span class="surface-number svelte-8hxs5n"> </span> </button> <button class="svelte-8hxs5n">✎</button>`, 1);
var root_3$4 = /* @__PURE__ */ from_html(`<div><!> <button class="surface-close svelte-8hxs5n">×</button></div>`);
var root_4$3 = /* @__PURE__ */ from_html(`<details class="surface-picker svelte-8hxs5n"><summary aria-label="Select an open Surface" class="svelte-8hxs5n"><!> <span> </span><span class="surface-chevron svelte-8hxs5n" aria-hidden="true">⌄</span></summary> <div class="surface-choices svelte-8hxs5n" role="group" aria-label="Open Surfaces"></div></details>`);
var root_5$2 = /* @__PURE__ */ from_html(`<span> </span>`);
var root_6$2 = /* @__PURE__ */ from_html(`<button class="system-navigation" aria-controls="ryeos-navigation">Views</button>`);
var root_7$2 = /* @__PURE__ */ from_html(`<header class="system-bar svelte-8hxs5n"><div class="system-context svelte-8hxs5n" role="group" aria-label="Surface context" tabindex="-1"><!></div> <div class="system-state"><span> </span> <span class="transport"> </span> <!></div></header>`);
function SystemBar($$anchor, $$props) {
	push($$props, true);
	const dispatch = dispatchUi();
	let contextRoot;
	let renameTarget = /* @__PURE__ */ state(null);
	let renameDraft = /* @__PURE__ */ state("");
	let renameInput = /* @__PURE__ */ state(void 0);
	async function beginRename(surface) {
		set(renameTarget, surface, true);
		set(renameDraft, surface.title, true);
		await tick();
		if (get(renameTarget)?.surface_instance_id === surface.surface_instance_id) {
			get(renameInput)?.focus();
			get(renameInput)?.select();
		}
	}
	async function cancelRename() {
		const id = get(renameTarget)?.surface_instance_id;
		set(renameTarget, null);
		await tick();
		if (id) contextRoot?.querySelector(`[data-surface-select="${id}"]`)?.focus();
	}
	user_effect(() => {
		if (get(renameTarget) && !$$props.session.surfaces.some((surface) => surface.surface_instance_id === get(renameTarget)?.surface_instance_id && surface.rename_intent)) set(renameTarget, null);
	});
	function submitRename() {
		const intent = get(renameTarget)?.rename_intent;
		if (!intent || intent.type !== "rename_surface") return;
		dispatch({
			type: "activate",
			intent: {
				...intent,
				title: get(renameDraft)
			}
		});
		cancelRename();
	}
	function dismissSurfacePicker(details, restoreFocus = true) {
		details.removeAttribute("open");
		if (restoreFocus) details.querySelector("summary")?.focus();
	}
	function handleSurfacePickerKey(event) {
		if (event.key !== "Escape" || event.isComposing || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
		event.preventDefault();
		event.stopPropagation();
		const picker = event.currentTarget.closest("details");
		if (picker?.open) dismissSurfacePicker(picker);
	}
	var header = root_7$2();
	var div = child(header);
	var node = child(div);
	var consequent_2 = ($$anchor) => {
		var details_1 = root_4$3();
		var summary = child(details_1);
		var node_1 = child(summary);
		var consequent = ($$anchor) => {
			var span = root$7();
			var text = only_child(span, true);
			template_effect(($0) => set_text(text, $0), [() => String($$props.session.surface_ordinal).padStart(2, "0")]);
			append($$anchor, span);
		};
		if_block(node_1, ($$render) => {
			if ($$props.session.surface_ordinal !== null) $$render(consequent);
		});
		var text_1 = only_child(sibling(node_1, 2), true);
		next();
		reset(summary);
		var div_1 = sibling(summary, 2);
		each(div_1, 21, () => $$props.session.surfaces, (surface) => surface.surface_instance_id, ($$anchor, surface) => {
			var div_2 = root_3$4();
			let classes;
			var node_2 = child(div_2);
			var consequent_1 = ($$anchor) => {
				var form = root_1$5();
				var input = child(form);
				remove_input_defaults(input);
				bind_this(input, ($$value) => set(renameInput, $$value), () => get(renameInput));
				var button = sibling(input, 2);
				var button_1 = sibling(button, 2);
				reset(form);
				event("submit", form, (event) => {
					event.preventDefault();
					submitRename();
				});
				delegated("keydown", input, (event) => {
					event.stopPropagation();
					if (event.key === "Escape" && !event.isComposing) {
						event.preventDefault();
						cancelRename();
					}
				});
				bind_value(input, () => get(renameDraft), ($$value) => set(renameDraft, $$value));
				delegated("keydown", button, handleSurfacePickerKey);
				delegated("keydown", button_1, handleSurfacePickerKey);
				delegated("click", button_1, cancelRename);
				append($$anchor, form);
			};
			var alternate = ($$anchor) => {
				var fragment = root_2$4();
				var button_2 = first_child(fragment);
				var span_2 = child(button_2);
				var text_2 = only_child(span_2, true);
				var text_3 = sibling(span_2, 1, true);
				reset(button_2);
				var button_3 = sibling(button_2, 2);
				template_effect(($0) => {
					set_attribute(button_2, "data-surface-select", get(surface).surface_instance_id);
					set_attribute(button_2, "aria-current", get(surface).active ? "true" : void 0);
					set_text(text_2, $0);
					set_text(text_3, get(surface).title);
					button_3.disabled = !get(surface).rename_intent;
					set_attribute(button_3, "aria-label", `Rename ${get(surface).title}`);
				}, [() => String(get(surface).ordinal).padStart(2, "0")]);
				delegated("keydown", button_2, handleSurfacePickerKey);
				delegated("click", button_2, (event) => {
					const picker = event.currentTarget.closest("details");
					if (picker) dismissSurfacePicker(picker);
					dispatch({
						type: "activate",
						intent: get(surface).select_intent
					});
				});
				delegated("keydown", button_3, handleSurfacePickerKey);
				delegated("click", button_3, () => beginRename(get(surface)));
				append($$anchor, fragment);
			};
			if_block(node_2, ($$render) => {
				if (get(renameTarget)?.surface_instance_id === get(surface).surface_instance_id) $$render(consequent_1);
				else $$render(alternate, -1);
			});
			var button_4 = sibling(node_2, 2);
			reset(div_2);
			template_effect(() => {
				classes = set_class(div_2, 1, "surface-choice svelte-8hxs5n", null, classes, { active: get(surface).active });
				button_4.disabled = !get(surface).close_intent;
				set_attribute(button_4, "aria-label", `Close ${get(surface).title}; running work is unaffected`);
			});
			delegated("keydown", button_4, handleSurfacePickerKey);
			delegated("click", button_4, (event) => {
				if (!get(surface).close_intent) return;
				const picker = event.currentTarget.closest("details");
				const closingLast = $$props.session.surfaces.length === 1;
				if (picker) dismissSurfacePicker(picker, !closingLast);
				if (closingLast) contextRoot?.focus();
				dispatch({
					type: "activate",
					intent: get(surface).close_intent
				});
			});
			append($$anchor, div_2);
		});
		reset(div_1);
		reset(details_1);
		template_effect(() => set_text(text_1, $$props.session.title));
		delegated("keydown", summary, handleSurfacePickerKey);
		append($$anchor, details_1);
	};
	var alternate_1 = ($$anchor) => {
		var span_3 = root_5$2();
		var text_4 = only_child(span_3, true);
		template_effect(() => set_text(text_4, $$props.session.title));
		append($$anchor, span_3);
	};
	if_block(node, ($$render) => {
		if ($$props.session.surfaces.length > 0) $$render(consequent_2);
		else $$render(alternate_1, -1);
	});
	reset(div);
	bind_this(div, ($$value) => contextRoot = $$value, () => contextRoot);
	var div_3 = sibling(div, 2);
	var span_4 = child(div_3);
	var text_5 = only_child(span_4, true);
	var span_5 = sibling(span_4, 2);
	var text_6 = only_child(span_5, true);
	var node_3 = sibling(span_5, 2);
	var consequent_3 = ($$anchor) => {
		var button_5 = root_6$2();
		template_effect(() => set_attribute(button_5, "aria-expanded", $$props.navigationOpen));
		delegated("click", button_5, (event) => $$props.onToggleNavigation(event.currentTarget));
		append($$anchor, button_5);
	};
	if_block(node_3, ($$render) => {
		if ($$props.navigationAvailable) $$render(consequent_3);
	});
	reset(div_3);
	reset(header);
	template_effect(() => {
		set_text(text_5, $$props.chrome.health_label);
		set_attribute(span_5, "data-freshness", $$props.transport.freshness);
		set_text(text_6, $$props.transport.freshness);
	});
	append($$anchor, header);
	pop();
}
delegate(["keydown", "click"]);
//#endregion
//#region browser/app/ViewSetStrip.svelte
var root$6 = /* @__PURE__ */ from_html(`<form class="view-set-rename"><input aria-label="View set name"/> <button type="submit" aria-label="Save view set name">✓</button> <button type="button" aria-label="Cancel rename">×</button></form>`);
var root_1$4 = /* @__PURE__ */ from_html(`<button class="view-set-select"><span class="ordinal"> </span> <span> </span></button>`);
var root_2$3 = /* @__PURE__ */ from_html(`<span class="view-set-actions" role="group"><button title="Rename view set">✎</button> <button title="Duplicate view set">⧉</button> <button title="Close view set">×</button></span>`);
var root_3$3 = /* @__PURE__ */ from_html(`<div><!> <!></div>`);
var root_4$2 = /* @__PURE__ */ from_html(`<nav class="view-set-strip" aria-label="View sets"><!> <button class="new-view-set" aria-label="New view set">＋</button> <div class="view-set-context" aria-label="Focused view and layout"><span> </span> <span aria-label="Layout"> </span></div></nav>`);
function ViewSetStrip($$anchor, $$props) {
	push($$props, true);
	const dispatch = dispatchUi();
	let editingId = /* @__PURE__ */ state(null);
	let draft = /* @__PURE__ */ state("");
	let renameInput = /* @__PURE__ */ state(void 0);
	async function beginRename(id, title) {
		set(draft, title, true);
		set(editingId, id, true);
		await tick();
		if (get(editingId) === id) {
			get(renameInput)?.focus();
			get(renameInput)?.select();
		}
	}
	async function leaveRename() {
		const id = get(editingId);
		set(editingId, null);
		await tick();
		if (id !== null) document.querySelector(`[data-focus-key="view-set:${id}:select"]`)?.focus();
	}
	async function finishRename() {
		const title = get(draft).trim();
		if (get(editingId) !== null && title) dispatch({
			type: "activate",
			intent: {
				type: "rename_view_set",
				view_set_id: get(editingId),
				title
			}
		});
		await leaveRename();
	}
	user_effect(() => {
		if (get(editingId) !== null && !$$props.model.tabs.some((tab) => tab.view_set_id === get(editingId) && tab.active)) set(editingId, null);
	});
	var fragment = comment();
	var node = first_child(fragment);
	var consequent_2 = ($$anchor) => {
		var nav = root_4$2();
		var node_1 = child(nav);
		each(node_1, 17, () => $$props.model.tabs, (tab) => tab.view_set_id, ($$anchor, tab) => {
			var div = root_3$3();
			let classes;
			var node_2 = child(div);
			var consequent = ($$anchor) => {
				var form = root$6();
				var input = child(form);
				remove_input_defaults(input);
				bind_this(input, ($$value) => set(renameInput, $$value), () => get(renameInput));
				var button = sibling(input, 4);
				reset(form);
				template_effect(() => set_attribute(input, "data-focus-key", `view-set:${get(tab).view_set_id}:rename`));
				event("submit", form, (event) => {
					event.preventDefault();
					finishRename();
				});
				delegated("keydown", input, (event) => {
					event.stopPropagation();
					if (event.key === "Escape") {
						event.preventDefault();
						leaveRename();
					}
				});
				bind_value(input, () => get(draft), ($$value) => set(draft, $$value));
				delegated("click", button, leaveRename);
				append($$anchor, form);
			};
			var alternate = ($$anchor) => {
				var button_1 = root_1$4();
				var span = child(button_1);
				var text = only_child(span, true);
				var text_1 = only_child(sibling(span, 2), true);
				reset(button_1);
				template_effect(($0) => {
					set_attribute(button_1, "data-focus-key", `view-set:${get(tab).view_set_id}:select`);
					set_attribute(button_1, "aria-current", get(tab).active ? "page" : void 0);
					set_text(text, $0);
					set_text(text_1, get(tab).title);
				}, [() => String(get(tab).number).padStart(2, "0")]);
				delegated("click", button_1, () => dispatch({
					type: "activate",
					intent: {
						type: "select_view_set",
						view_set_id: get(tab).view_set_id
					}
				}));
				append($$anchor, button_1);
			};
			if_block(node_2, ($$render) => {
				if (get(editingId) === get(tab).view_set_id) $$render(consequent);
				else $$render(alternate, -1);
			});
			var node_3 = sibling(node_2, 2);
			var consequent_1 = ($$anchor) => {
				var span_2 = root_2$3();
				var button_2 = child(span_2);
				var button_3 = sibling(button_2, 2);
				var button_4 = sibling(button_3, 2);
				reset(span_2);
				template_effect(() => {
					set_attribute(span_2, "aria-label", `${get(tab).title} view-set actions`);
					set_attribute(button_2, "aria-label", `Rename ${get(tab).title}`);
					set_attribute(button_3, "aria-label", `Duplicate ${get(tab).title}`);
					set_attribute(button_4, "aria-label", `Close ${get(tab).title}`);
				});
				delegated("click", button_2, () => beginRename(get(tab).view_set_id, get(tab).title));
				delegated("click", button_3, () => dispatch({
					type: "activate",
					intent: {
						type: "duplicate_view_set",
						view_set_id: get(tab).view_set_id
					}
				}));
				delegated("click", button_4, () => dispatch({
					type: "activate",
					intent: {
						type: "close_view_set",
						view_set_id: get(tab).view_set_id
					}
				}));
				append($$anchor, span_2);
			};
			if_block(node_3, ($$render) => {
				if (get(tab).active) $$render(consequent_1);
			});
			reset(div);
			template_effect(() => classes = set_class(div, 1, "view-set-tab", null, classes, { active: get(tab).active }));
			append($$anchor, div);
		});
		var button_5 = sibling(node_1, 2);
		var div_1 = sibling(button_5, 2);
		var span_3 = child(div_1);
		var text_2 = only_child(span_3, true);
		var text_3 = only_child(sibling(span_3, 2), true);
		reset(div_1);
		reset(nav);
		template_effect(() => {
			set_attribute(span_3, "title", $$props.model.focused_title);
			set_text(text_2, $$props.model.focused_title);
			set_text(text_3, $$props.model.layout_symbol);
		});
		delegated("click", button_5, () => dispatch({
			type: "activate",
			intent: { type: "new_view_set" }
		}));
		append($$anchor, nav);
	};
	if_block(node, ($$render) => {
		if ($$props.model.visible) $$render(consequent_2);
	});
	append($$anchor, fragment);
	pop();
}
delegate(["keydown", "click"]);
//#endregion
//#region browser/components/ViewSupplement.svelte
var root$5 = /* @__PURE__ */ from_html(`<section class="view-supplement-scene" aria-label="View topology"><!></section>`);
var root_1$3 = /* @__PURE__ */ from_html(`<header> </header>`);
var root_2$2 = /* @__PURE__ */ from_html(`<div><span> </span><code> </code></div>`);
var root_3$2 = /* @__PURE__ */ from_html(`<section class="view-supplement-excerpt"><!> <div class="view-supplement-code"></div></section>`);
var root_4$1 = /* @__PURE__ */ from_html(`<strong> </strong>`);
var root_5$1 = /* @__PURE__ */ from_html(`<div><span> </span><small> </small></div>`);
var root_6$1 = /* @__PURE__ */ from_html(`<footer class="view-supplement-footer"><!> <!></footer>`);
var root_7$1 = /* @__PURE__ */ from_html(`<!> <!>`, 1);
function ViewSupplement($$anchor, $$props) {
	push($$props, true);
	var fragment = comment();
	var node = first_child(fragment);
	var consequent = ($$anchor) => {
		var section = root$5();
		SceneView(child(section), { get scene() {
			return $$props.model.scene;
		} });
		reset(section);
		append($$anchor, section);
	};
	var consequent_5 = ($$anchor) => {
		var fragment_1 = root_7$1();
		var node_2 = first_child(fragment_1);
		var consequent_2 = ($$anchor) => {
			var section_1 = root_3$2();
			var node_3 = child(section_1);
			var consequent_1 = ($$anchor) => {
				var header = root_1$3();
				var text = only_child(header, true);
				template_effect(() => set_text(text, $$props.model.excerpt_title));
				append($$anchor, header);
			};
			if_block(node_3, ($$render) => {
				if ($$props.model.excerpt_title) $$render(consequent_1);
			});
			var div = sibling(node_3, 2);
			each(div, 21, () => $$props.model.excerpt, index, ($$anchor, row) => {
				var div_1 = root_2$2();
				var span = child(div_1);
				var text_1 = only_child(span, true);
				var text_2 = only_child(sibling(span), true);
				reset(div_1);
				template_effect(() => {
					set_attribute(div_1, "data-tone", get(row).tone);
					set_text(text_1, get(row).field);
					set_text(text_2, get(row).value);
				});
				append($$anchor, div_1);
			});
			reset(div);
			reset(section_1);
			template_effect(() => set_attribute(section_1, "aria-label", $$props.model.excerpt_title || "Excerpt"));
			append($$anchor, section_1);
		};
		if_block(node_2, ($$render) => {
			if ($$props.model.excerpt.length) $$render(consequent_2);
		});
		var node_4 = sibling(node_2, 2);
		var consequent_4 = ($$anchor) => {
			var footer = root_6$1();
			var node_5 = child(footer);
			var consequent_3 = ($$anchor) => {
				var strong = root_4$1();
				var text_3 = only_child(strong, true);
				template_effect(() => set_text(text_3, $$props.model.footer));
				append($$anchor, strong);
			};
			if_block(node_5, ($$render) => {
				if ($$props.model.footer) $$render(consequent_3);
			});
			each(sibling(node_5, 2), 17, () => $$props.model.footer_rows, index, ($$anchor, row) => {
				var div_2 = root_5$1();
				var span_1 = child(div_2);
				var text_4 = only_child(span_1, true);
				var text_5 = only_child(sibling(span_1), true);
				reset(div_2);
				template_effect(() => {
					set_attribute(div_2, "data-tone", get(row).tone);
					set_text(text_4, get(row).field);
					set_text(text_5, get(row).value);
				});
				append($$anchor, div_2);
			});
			reset(footer);
			append($$anchor, footer);
		};
		if_block(node_4, ($$render) => {
			if ($$props.model.footer || $$props.model.footer_rows.length) $$render(consequent_4);
		});
		append($$anchor, fragment_1);
	};
	if_block(node, ($$render) => {
		if ($$props.phase === "content" && $$props.model.scene) $$render(consequent);
		else if ($$props.phase === "footer") $$render(consequent_5, 1);
	});
	append($$anchor, fragment);
	pop();
}
//#endregion
//#region browser/layout/DockSlot.svelte
var root$4 = /* @__PURE__ */ from_html(`<p> </p>`);
var root_1$2 = /* @__PURE__ */ from_html(`<div class="heading-meta"> </div>`);
var root_2$1 = /* @__PURE__ */ from_html(`<div class="content-heading"><small> </small><h1> </h1><!><!></div>`);
var root_3$1 = /* @__PURE__ */ from_html(`<aside><header><span> </span><span> </span></header> <!> <!> <!> <!> <!> <!></aside>`);
function DockSlot($$anchor, $$props) {
	push($$props, true);
	const dispatch = dispatchUi();
	var aside = root_3$1();
	let classes;
	var header = child(aside);
	var span = child(header);
	var text = only_child(span, true);
	var text_1 = only_child(sibling(span), true);
	reset(header);
	var node = sibling(header, 2);
	var consequent_2 = ($$anchor) => {
		var div = root_2$1();
		var small = child(div);
		var text_2 = only_child(small, true);
		var h1 = sibling(small);
		var text_3 = only_child(h1, true);
		var node_1 = sibling(h1);
		var consequent = ($$anchor) => {
			var p = root$4();
			var text_4 = only_child(p, true);
			template_effect(() => set_text(text_4, $$props.model.heading.summary));
			append($$anchor, p);
		};
		if_block(node_1, ($$render) => {
			if ($$props.model.heading.summary) $$render(consequent);
		});
		var node_2 = sibling(node_1);
		var consequent_1 = ($$anchor) => {
			var div_1 = root_1$2();
			var text_5 = only_child(div_1, true);
			template_effect(($0) => set_text(text_5, $0), [() => $$props.model.heading.metadata.join("  /  ")]);
			append($$anchor, div_1);
		};
		if_block(node_2, ($$render) => {
			if ($$props.model.heading.metadata.length) $$render(consequent_1);
		});
		reset(div);
		template_effect(() => {
			set_text(text_2, $$props.model.heading.eyebrow);
			set_text(text_3, $$props.model.heading.title);
		});
		append($$anchor, div);
	};
	if_block(node, ($$render) => {
		if ($$props.model.heading) $$render(consequent_2);
	});
	var node_3 = sibling(node, 2);
	var consequent_3 = ($$anchor) => {
		InputComposer($$anchor, { get model() {
			return $$props.model.input;
		} });
	};
	if_block(node_3, ($$render) => {
		if ($$props.model.input?.live_filter) $$render(consequent_3);
	});
	var node_4 = sibling(node_3, 2);
	var consequent_4 = ($$anchor) => {
		ViewSupplement($$anchor, {
			get model() {
				return $$props.model.supplement;
			},
			phase: "content"
		});
	};
	if_block(node_4, ($$render) => {
		if ($$props.model.supplement) $$render(consequent_4);
	});
	var node_5 = sibling(node_4, 2);
	{
		let $0 = /* @__PURE__ */ user_derived(() => String($$props.model.instance_key));
		ViewRenderer(node_5, {
			get model() {
				return $$props.model.view;
			},
			get tileId() {
				return get($0);
			},
			get instanceKey() {
				return $$props.model.instance_key;
			}
		});
	}
	var node_6 = sibling(node_5, 2);
	var consequent_5 = ($$anchor) => {
		ViewSupplement($$anchor, {
			get model() {
				return $$props.model.supplement;
			},
			phase: "footer"
		});
	};
	if_block(node_6, ($$render) => {
		if ($$props.model.supplement) $$render(consequent_5);
	});
	var node_7 = sibling(node_6, 2);
	var consequent_6 = ($$anchor) => {
		InputComposer($$anchor, { get model() {
			return $$props.model.input;
		} });
	};
	if_block(node_7, ($$render) => {
		if ($$props.model.input && !$$props.model.input.live_filter) $$render(consequent_6);
	});
	reset(aside);
	template_effect(() => {
		classes = set_class(aside, 1, "dock-slot", null, classes, { focused: $$props.model.focused });
		set_attribute(aside, "data-keyboard-focus", $$props.model.focused ? "current" : void 0);
		set_attribute(aside, "data-edge", $$props.model.edge);
		set_style(aside, `--dock-size:${$$props.model.size}`);
		set_text(text, $$props.model.supplement?.frame_label || $$props.model.title);
		set_text(text_1, $$props.model.attachment_label || $$props.model.supplement?.frame_detail || $$props.model.edge);
	});
	delegated("pointerdown", aside, () => {
		if (!$$props.model.focused) dispatch({
			type: "focus_dock",
			edge: $$props.model.edge
		});
	});
	delegated("focusin", aside, () => {
		if (!$$props.model.focused) dispatch({
			type: "focus_dock",
			edge: $$props.model.edge
		});
	});
	append($$anchor, aside);
	pop();
}
delegate(["pointerdown", "focusin"]);
//#endregion
//#region browser/layout/TileFrame.svelte
var root$3 = /* @__PURE__ */ from_html(`<span class="tile-frame-detail"> </span>`);
var root_1$1 = /* @__PURE__ */ from_html(`<span class="tile-frame-detail" title="Selection attachment"> </span>`);
var root_2 = /* @__PURE__ */ from_html(`<button> </button>`);
var root_3 = /* @__PURE__ */ from_html(`<details class="tile-action-menu"><summary title="View actions">⋮</summary> <div class="tile-action-list"></div></details>`);
var root_4 = /* @__PURE__ */ from_html(`<button role="tab"> </button>`);
var root_5 = /* @__PURE__ */ from_html(`<div class="view-tabs" role="tablist"></div>`);
var root_6 = /* @__PURE__ */ from_html(`<header tabindex="-1"><div class="tile-title-row"><div class="tile-identity"><span class="tile-signal"></span><strong> </strong><!></div> <div class="tile-tools"><!> <button class="tile-tool"> </button> <!></div></div> <!></header>`);
var root_7 = /* @__PURE__ */ from_html(`<p> </p>`);
var root_8 = /* @__PURE__ */ from_html(`<div class="heading-meta"> </div>`);
var root_9 = /* @__PURE__ */ from_html(`<div class="content-heading"><small> </small><h1> </h1><!><!></div>`);
var root_10 = /* @__PURE__ */ from_html(`<article><!> <!> <!> <!> <!> <!> <!></article>`);
function TileFrame($$anchor, $$props) {
	push($$props, true);
	const dispatch = dispatchUi();
	let actionSummary = /* @__PURE__ */ state(void 0);
	var article = root_10();
	let classes;
	var node = child(article);
	var consequent_4 = ($$anchor) => {
		var header = root_6();
		let classes_1;
		var div = child(header);
		var div_1 = child(div);
		var strong = sibling(child(div_1));
		var text = only_child(strong, true);
		var node_1 = sibling(strong);
		var consequent = ($$anchor) => {
			var span = root$3();
			var text_1 = only_child(span, true);
			template_effect(() => set_text(text_1, $$props.model.supplement.frame_detail));
			append($$anchor, span);
		};
		if_block(node_1, ($$render) => {
			if ($$props.model.supplement?.frame_detail) $$render(consequent);
		});
		reset(div_1);
		var div_2 = sibling(div_1, 2);
		var node_2 = child(div_2);
		var consequent_1 = ($$anchor) => {
			var span_1 = root_1$1();
			var text_2 = only_child(span_1, true);
			template_effect(() => set_text(text_2, $$props.model.attachment_label));
			append($$anchor, span_1);
		};
		if_block(node_2, ($$render) => {
			if ($$props.model.attachment_label) $$render(consequent_1);
		});
		var button = sibling(node_2, 2);
		var text_3 = only_child(button, true);
		var node_3 = sibling(button, 2);
		var consequent_2 = ($$anchor) => {
			var details = root_3();
			var summary = child(details);
			bind_this(summary, ($$value) => set(actionSummary, $$value), () => get(actionSummary));
			var div_3 = sibling(summary, 2);
			each(div_3, 21, () => $$props.model.intents, index, ($$anchor, action) => {
				var button_1 = root_2();
				var text_4 = only_child(button_1, true);
				template_effect(() => {
					set_attribute(button_1, "title", get(action).title);
					set_text(text_4, get(action).label);
				});
				delegated("click", button_1, (event) => {
					const menu = event.currentTarget.closest("details");
					if (menu) menu.open = false;
					get(actionSummary)?.focus();
					dispatch({
						type: "activate",
						intent: get(action).intent
					});
				});
				append($$anchor, button_1);
			});
			reset(div_3);
			reset(details);
			template_effect(() => set_attribute(summary, "aria-label", `Actions for ${$$props.model.title}`));
			append($$anchor, details);
		};
		if_block(node_3, ($$render) => {
			if ($$props.model.intents.length) $$render(consequent_2);
		});
		reset(div_2);
		reset(div);
		var node_4 = sibling(div, 2);
		var consequent_3 = ($$anchor) => {
			var div_4 = root_5();
			each(div_4, 21, () => $$props.model.tabs, (tab) => tab.tile_id, ($$anchor, tab) => {
				var button_2 = root_4();
				let classes_2;
				var text_5 = only_child(button_2, true);
				template_effect(() => {
					set_attribute(button_2, "aria-selected", get(tab).active);
					classes_2 = set_class(button_2, 1, "", null, classes_2, { active: get(tab).active });
					set_text(text_5, get(tab).title);
				});
				delegated("click", button_2, () => dispatch({
					type: "focus_changed",
					target: get(tab).tile_id
				}));
				append($$anchor, button_2);
			});
			reset(div_4);
			append($$anchor, div_4);
		};
		if_block(node_4, ($$render) => {
			if ($$props.model.tabs.length > 1) $$render(consequent_3);
		});
		reset(header);
		template_effect(() => {
			classes_1 = set_class(header, 1, "tile-header", null, classes_1, { grouped: $$props.model.group_label });
			set_attribute(header, "data-focus-key", `view:${$$props.model.instance_key}:header`);
			set_text(text, $$props.model.supplement?.frame_label || $$props.model.group_label || $$props.model.title);
			set_attribute(button, "aria-label", $$props.model.maximized ? "Restore view" : "Maximize view");
			set_attribute(button, "title", $$props.model.maximized ? "Restore view" : "Maximize view");
			set_text(text_3, $$props.model.maximized ? "↙" : "↗");
		});
		delegated("click", button, () => dispatch({
			type: "activate",
			intent: {
				type: "toggle_tile_maximized",
				tile_id: $$props.model.tile_id
			}
		}));
		append($$anchor, header);
	};
	if_block(node, ($$render) => {
		if (!$$props.model.chrome_hidden) $$render(consequent_4);
	});
	var node_5 = sibling(node, 2);
	var consequent_7 = ($$anchor) => {
		var div_5 = root_9();
		var small = child(div_5);
		var text_6 = only_child(small, true);
		var h1 = sibling(small);
		var text_7 = only_child(h1, true);
		var node_6 = sibling(h1);
		var consequent_5 = ($$anchor) => {
			var p = root_7();
			var text_8 = only_child(p, true);
			template_effect(() => set_text(text_8, $$props.model.heading.summary));
			append($$anchor, p);
		};
		if_block(node_6, ($$render) => {
			if ($$props.model.heading.summary) $$render(consequent_5);
		});
		var node_7 = sibling(node_6);
		var consequent_6 = ($$anchor) => {
			var div_6 = root_8();
			var text_9 = only_child(div_6, true);
			template_effect(($0) => set_text(text_9, $0), [() => $$props.model.heading.metadata.join("  /  ")]);
			append($$anchor, div_6);
		};
		if_block(node_7, ($$render) => {
			if ($$props.model.heading.metadata.length) $$render(consequent_6);
		});
		reset(div_5);
		template_effect(() => {
			set_text(text_6, $$props.model.heading.eyebrow);
			set_text(text_7, $$props.model.heading.title);
		});
		append($$anchor, div_5);
	};
	if_block(node_5, ($$render) => {
		if ($$props.model.heading) $$render(consequent_7);
	});
	var node_8 = sibling(node_5, 2);
	var consequent_8 = ($$anchor) => {
		InputComposer($$anchor, { get model() {
			return $$props.model.input;
		} });
	};
	if_block(node_8, ($$render) => {
		if ($$props.model.input?.live_filter) $$render(consequent_8);
	});
	var node_9 = sibling(node_8, 2);
	var consequent_9 = ($$anchor) => {
		ViewSupplement($$anchor, {
			get model() {
				return $$props.model.supplement;
			},
			phase: "content"
		});
	};
	if_block(node_9, ($$render) => {
		if ($$props.model.supplement) $$render(consequent_9);
	});
	var node_10 = sibling(node_9, 2);
	ViewRenderer(node_10, {
		get model() {
			return $$props.model.view;
		},
		get tileId() {
			return $$props.model.tile_id;
		},
		get instanceKey() {
			return $$props.model.instance_key;
		}
	});
	var node_11 = sibling(node_10, 2);
	var consequent_10 = ($$anchor) => {
		ViewSupplement($$anchor, {
			get model() {
				return $$props.model.supplement;
			},
			phase: "footer"
		});
	};
	if_block(node_11, ($$render) => {
		if ($$props.model.supplement) $$render(consequent_10);
	});
	var node_12 = sibling(node_11, 2);
	var consequent_11 = ($$anchor) => {
		InputComposer($$anchor, { get model() {
			return $$props.model.input;
		} });
	};
	if_block(node_12, ($$render) => {
		if ($$props.model.input && !$$props.model.input.live_filter) $$render(consequent_11);
	});
	reset(article);
	template_effect(() => {
		classes = set_class(article, 1, "tile-frame", null, classes, {
			focused: $$props.model.focused,
			transparent: $$props.model.background_transparent
		});
		set_attribute(article, "data-instance", $$props.model.instance_key);
		set_attribute(article, "data-keyboard-focus", $$props.model.focused ? "current" : void 0);
		set_attribute(article, "data-scroll-key", `tile:${$$props.model.instance_key}`);
	});
	delegated("pointerdown", article, () => {
		if (!$$props.model.focused) dispatch({
			type: "focus_changed",
			target: $$props.model.tile_id
		});
	});
	delegated("focusin", article, () => {
		if (!$$props.model.focused) dispatch({
			type: "focus_changed",
			target: $$props.model.tile_id
		});
	});
	append($$anchor, article);
	pop();
}
delegate([
	"pointerdown",
	"focusin",
	"click"
]);
//#endregion
//#region browser/layout/LayoutNode.svelte
var root$2 = /* @__PURE__ */ from_html(`<div class="split"><!>  <div class="split-divider" role="separator" tabindex="0" aria-label="Resize views"></div> <!></div>`);
function LayoutNode_1($$anchor, $$props) {
	push($$props, true);
	let path = prop($$props, "path", 19, () => []);
	const dispatch = dispatchUi();
	let region = /* @__PURE__ */ state(void 0);
	let drag = null;
	function resize(ratio, layoutGuard = $$props.guard, splitPath = path()) {
		dispatch({
			type: "activate",
			intent: {
				type: "resize_split",
				layout_guard: layoutGuard,
				path: splitPath,
				ratio
			}
		});
	}
	function begin(event) {
		if (event.button !== 0 || $$props.model.type !== "split" || !get(region)) return;
		const rect = get(region).getBoundingClientRect();
		const horizontal = $$props.model.axis === "horizontal";
		const styles = getComputedStyle(get(region));
		const gap = parseFloat(horizontal ? styles.columnGap : styles.rowGap) || 0;
		const divider = event.currentTarget;
		const dividerSize = horizontal ? divider.offsetWidth : divider.offsetHeight;
		drag = {
			pointer: event.pointerId,
			guard: $$props.guard,
			path: [...path()],
			start: horizontal ? event.clientX : event.clientY,
			ratio: $$props.model.ratio,
			extent: (horizontal ? rect.width : rect.height) - 2 * gap - dividerSize,
			horizontal,
			min: $$props.minRatio,
			max: $$props.maxRatio
		};
		event.currentTarget.setPointerCapture(event.pointerId);
		event.preventDefault();
	}
	function end(event) {
		if (!drag || drag.pointer !== event.pointerId) return;
		const retained = drag;
		drag = null;
		if (retained.extent <= 0) return;
		const position = retained.horizontal ? event.clientX : event.clientY;
		if (position === retained.start) return;
		resize(Math.max(retained.min, Math.min(retained.max, retained.ratio + (position - retained.start) / retained.extent)), retained.guard, retained.path);
	}
	function key$1(event) {
		if ($$props.model.type !== "split") return;
		const negative = $$props.model.axis === "horizontal" ? "ArrowLeft" : "ArrowUp";
		if (![
			negative,
			$$props.model.axis === "horizontal" ? "ArrowRight" : "ArrowDown",
			"Home",
			"End"
		].includes(event.key)) return;
		event.preventDefault();
		event.stopPropagation();
		const ratio = event.key === "Home" ? $$props.minRatio : event.key === "End" ? $$props.maxRatio : $$props.model.ratio + (event.key === negative ? -.05 : .05);
		resize(Math.max($$props.minRatio, Math.min($$props.maxRatio, ratio)));
	}
	var fragment = comment();
	var node = first_child(fragment);
	var consequent = ($$anchor) => {
		var fragment_1 = comment();
		key(first_child(fragment_1), () => $$props.model.instance_key, ($$anchor) => {
			TileFrame($$anchor, { get model() {
				return $$props.model;
			} });
		});
		append($$anchor, fragment_1);
	};
	var alternate = ($$anchor) => {
		var div = root$2();
		var node_2 = child(div);
		{
			let $0 = /* @__PURE__ */ user_derived(() => [...path(), "first"]);
			LayoutNode_1(node_2, {
				get model() {
					return $$props.model.first;
				},
				get guard() {
					return $$props.guard;
				},
				get minRatio() {
					return $$props.minRatio;
				},
				get maxRatio() {
					return $$props.maxRatio;
				},
				get path() {
					return get($0);
				}
			});
		}
		var div_1 = sibling(node_2, 2);
		var node_3 = sibling(div_1, 2);
		{
			let $0 = /* @__PURE__ */ user_derived(() => [...path(), "second"]);
			LayoutNode_1(node_3, {
				get model() {
					return $$props.model.second;
				},
				get guard() {
					return $$props.guard;
				},
				get minRatio() {
					return $$props.minRatio;
				},
				get maxRatio() {
					return $$props.maxRatio;
				},
				get path() {
					return get($0);
				}
			});
		}
		reset(div);
		bind_this(div, ($$value) => set(region, $$value), () => get(region));
		template_effect(($0, $1, $2) => {
			set_attribute(div, "data-axis", $$props.model.axis);
			set_style(div, `--split:${$$props.model.ratio}fr;--split-rest:${1 - $$props.model.ratio}fr`);
			set_attribute(div_1, "aria-valuemin", $0);
			set_attribute(div_1, "aria-valuemax", $1);
			set_attribute(div_1, "aria-valuenow", $2);
			set_attribute(div_1, "aria-orientation", $$props.model.axis === "horizontal" ? "vertical" : "horizontal");
		}, [
			() => Math.round($$props.minRatio * 100),
			() => Math.round($$props.maxRatio * 100),
			() => Math.round($$props.model.ratio * 100)
		]);
		delegated("pointerdown", div_1, begin);
		delegated("pointerup", div_1, end);
		event("pointercancel", div_1, () => drag = null);
		event("lostpointercapture", div_1, () => drag = null);
		delegated("keydown", div_1, key$1);
		append($$anchor, div);
	};
	if_block(node, ($$render) => {
		if ($$props.model.type === "tile") $$render(consequent);
		else $$render(alternate, -1);
	});
	append($$anchor, fragment);
	pop();
}
delegate([
	"pointerdown",
	"pointerup",
	"keydown"
]);
//#endregion
//#region browser/layout/ViewSet.svelte
var root$1 = /* @__PURE__ */ from_html(`<div class="view-set-backdrop" aria-label="Empty view set"></div>`);
var root_1 = /* @__PURE__ */ from_html(`<main><!> <div class="view-set-middle"><!> <section class="view-set-center"><!></section> <!></div> <!></main>`);
function ViewSet($$anchor, $$props) {
	push($$props, true);
	const maximized = /* @__PURE__ */ user_derived(() => $$props.model?.root?.type === "tile" && $$props.model.root.maximized);
	var main = root_1();
	let classes;
	var node = child(main);
	var consequent = ($$anchor) => {
		var fragment = comment();
		key(first_child(fragment), () => $$props.docks.top.instance_key, ($$anchor) => {
			DockSlot($$anchor, { get model() {
				return $$props.docks.top;
			} });
		});
		append($$anchor, fragment);
	};
	if_block(node, ($$render) => {
		if (!get(maximized) && $$props.docks.top) $$render(consequent);
	});
	var div = sibling(node, 2);
	var node_2 = child(div);
	var consequent_1 = ($$anchor) => {
		var fragment_2 = comment();
		key(first_child(fragment_2), () => $$props.docks.left.instance_key, ($$anchor) => {
			DockSlot($$anchor, { get model() {
				return $$props.docks.left;
			} });
		});
		append($$anchor, fragment_2);
	};
	if_block(node_2, ($$render) => {
		if (!get(maximized) && $$props.docks.left) $$render(consequent_1);
	});
	var section = sibling(node_2, 2);
	key(child(section), () => $$props.model?.view_set_id, ($$anchor) => {
		var fragment_4 = comment();
		var node_5 = first_child(fragment_4);
		var consequent_2 = ($$anchor) => {
			LayoutNode_1($$anchor, {
				get model() {
					return $$props.model.root;
				},
				get guard() {
					return $$props.model.layout_guard;
				},
				get minRatio() {
					return $$props.model.split_min_ratio;
				},
				get maxRatio() {
					return $$props.model.split_max_ratio;
				}
			});
		};
		var consequent_3 = ($$anchor) => {
			SceneView($$anchor, { get scene() {
				return $$props.backdrop;
			} });
		};
		var alternate = ($$anchor) => {
			append($$anchor, root$1());
		};
		if_block(node_5, ($$render) => {
			if ($$props.model?.root) $$render(consequent_2);
			else if ($$props.backdrop) $$render(consequent_3, 1);
			else $$render(alternate, -1);
		});
		append($$anchor, fragment_4);
	});
	reset(section);
	var node_6 = sibling(section, 2);
	var consequent_4 = ($$anchor) => {
		var fragment_7 = comment();
		key(first_child(fragment_7), () => $$props.docks.right.instance_key, ($$anchor) => {
			DockSlot($$anchor, { get model() {
				return $$props.docks.right;
			} });
		});
		append($$anchor, fragment_7);
	};
	if_block(node_6, ($$render) => {
		if (!get(maximized) && $$props.docks.right) $$render(consequent_4);
	});
	reset(div);
	var node_8 = sibling(div, 2);
	var consequent_5 = ($$anchor) => {
		var fragment_9 = comment();
		key(first_child(fragment_9), () => $$props.docks.bottom.instance_key, ($$anchor) => {
			DockSlot($$anchor, { get model() {
				return $$props.docks.bottom;
			} });
		});
		append($$anchor, fragment_9);
	};
	if_block(node_8, ($$render) => {
		if (!get(maximized) && $$props.docks.bottom) $$render(consequent_5);
	});
	reset(main);
	template_effect(() => classes = set_class(main, 1, "view-set", null, classes, {
		empty: !$$props.model || $$props.model.center_is_empty,
		maximized: get(maximized)
	}));
	append($$anchor, main);
	pop();
}
//#endregion
//#region browser/app/RyeOs.svelte
var root = /* @__PURE__ */ from_html(`<div><!> <!> <!> <div><!> <!></div> <!> <!> <!></div>`);
function RyeOs($$anchor, $$props) {
	push($$props, true);
	let replacement = /* @__PURE__ */ state(null);
	let navigationOpen = /* @__PURE__ */ state(false);
	let navigationOpener = null;
	let envelope = /* @__PURE__ */ user_derived(() => get(replacement) ?? $$props.initialEnvelope);
	provideDispatchUi((event) => $$props.dispatchUi(event));
	/** Replace the sole renderer-owned root cell with one committed envelope. */
	function replaceEnvelope(next) {
		set(replacement, next, true);
	}
	function toggleNavigation(opener) {
		if (get(navigationOpen)) {
			closeNavigation();
			return;
		}
		navigationOpener = opener;
		set(navigationOpen, true);
	}
	function closeNavigation() {
		const active = document.activeElement;
		const panel = document.getElementById("ryeos-navigation");
		const ownsFocus = active === document.body || active === null || active instanceof Element && (panel?.contains(active) || active.classList.contains("navigation-scrim"));
		set(navigationOpen, false);
		if (ownsFocus && navigationOpener?.isConnected) {
			const opener = navigationOpener;
			tick().then(() => opener.focus());
		}
	}
	var $$exports = { replaceEnvelope };
	var div = root();
	let classes;
	set_style(div, "", {}, { "--system-height": "24px" });
	var node = child(div);
	var consequent = ($$anchor) => {
		AmbientLayer($$anchor, {
			get ambient() {
				return get(envelope).view_model.session.ambient;
			},
			get scene() {
				return get(envelope).scene_model;
			}
		});
	};
	if_block(node, ($$render) => {
		if (get(envelope).view_model.session.ambient.show_background) $$render(consequent);
	});
	var node_1 = sibling(node, 2);
	{
		let $0 = /* @__PURE__ */ user_derived(() => get(envelope).view_model.navigation.items.length > 0);
		SystemBar(node_1, {
			get chrome() {
				return get(envelope).view_model.chrome;
			},
			get session() {
				return get(envelope).view_model.session;
			},
			get transport() {
				return get(envelope).view_model.transport;
			},
			get navigationAvailable() {
				return get($0);
			},
			get navigationOpen() {
				return get(navigationOpen);
			},
			onToggleNavigation: toggleNavigation
		});
	}
	var node_2 = sibling(node_1, 2);
	ViewSetStrip(node_2, { get model() {
		return get(envelope).view_model.presentation.chrome.top_bar;
	} });
	var div_1 = sibling(node_2, 2);
	let classes_1;
	var node_3 = child(div_1);
	var consequent_1 = ($$anchor) => {
		Navigation($$anchor, {
			get model() {
				return get(envelope).view_model.navigation;
			},
			get open() {
				return get(navigationOpen);
			},
			onClose: closeNavigation
		});
	};
	if_block(node_3, ($$render) => {
		if (get(envelope).view_model.navigation.items.length > 0) $$render(consequent_1);
	});
	ViewSet(sibling(node_3, 2), {
		get model() {
			return get(envelope).view_model.view_set;
		},
		get docks() {
			return get(envelope).view_model.docks;
		},
		get backdrop() {
			return get(envelope).view_model.backdrop;
		}
	});
	reset(div_1);
	var node_5 = sibling(div_1, 2);
	StatusBar(node_5, { get model() {
		return get(envelope).view_model.presentation.chrome.status_bar;
	} });
	var node_6 = sibling(node_5, 2);
	Notices(node_6, { get notices() {
		return get(envelope).view_model.notices;
	} });
	each(sibling(node_6, 2), 17, () => get(envelope).view_model.dialogs, (dialog) => dialog.instance_key, ($$anchor, dialog) => {
		DialogLayer($$anchor, { get model() {
			return get(dialog);
		} });
	});
	reset(div);
	template_effect(($0) => {
		classes = set_class(div, 1, "ryeos-shell", null, classes, {
			"without-set-strip": !get(envelope).view_model.presentation.chrome.top_bar.visible,
			"without-status-bar": !get(envelope).view_model.presentation.chrome.status_bar.visible
		});
		set_attribute(div, "data-generation", $0);
		set_attribute(div, "data-theme", get(envelope).view_model.presentation.theme.id);
		classes_1 = set_class(div_1, 1, "shell-body", null, classes_1, { "with-navigation": get(envelope).view_model.navigation.items.length > 0 });
	}, [() => String(get(envelope).generation)]);
	append($$anchor, div);
	return pop($$exports);
}
//#endregion
//#region browser/renderer.ts
/** Mount one presentation adapter over complete Rust-owned envelopes. */
function mountRyeOsRenderer(target, initialEnvelope, dispatchUi) {
	target.replaceChildren();
	const component = mount(RyeOs, {
		target,
		props: {
			initialEnvelope,
			dispatchUi
		}
	});
	let destroyed = false;
	return {
		replaceEnvelope(envelope) {
			if (destroyed) throw new Error("RyeOS renderer has been destroyed");
			component.replaceEnvelope(envelope);
		},
		async destroy() {
			if (destroyed) return;
			destroyed = true;
			await unmount(component);
		}
	};
}
//#endregion
//#region browser/runtime/browser-state.ts
var modalReturnFocus = /* @__PURE__ */ new WeakMap();
function captureBrowserPresentation(root) {
	const active = document.activeElement;
	const key = active instanceof HTMLElement && root.contains(active) ? active.dataset.focusKey ?? null : null;
	const focus = key === null ? null : {
		key,
		inDialog: active instanceof HTMLElement && active.closest("[role=\"dialog\"][aria-modal=\"true\"]") !== null,
		selectionStart: selection(active, "selectionStart"),
		selectionEnd: selection(active, "selectionEnd")
	};
	const scroll = /* @__PURE__ */ new Map();
	for (const node of root.querySelectorAll("[data-scroll-key]")) {
		const scrollKey = node.dataset.scrollKey;
		if (!scrollKey) continue;
		scroll.set(scrollKey, {
			top: node.scrollTop,
			left: node.scrollLeft,
			atTail: node.scrollHeight - node.scrollTop - node.clientHeight <= 24
		});
	}
	return {
		focus,
		scroll
	};
}
function restoreBrowserPresentation(root, snapshot) {
	for (const node of root.querySelectorAll("[data-scroll-key]")) {
		const state = snapshot.scroll.get(node.dataset.scrollKey ?? "");
		if (!state) continue;
		node.scrollTop = state.atTail ? node.scrollHeight : state.top;
		node.scrollLeft = state.left;
	}
	if (root.querySelector("[role=\"dialog\"][aria-modal=\"true\"]")) {
		if (snapshot.focus && !snapshot.focus.inDialog) modalReturnFocus.set(root, snapshot.focus);
		return;
	}
	let focus = snapshot.focus ?? modalReturnFocus.get(root) ?? null;
	if (!focus) return;
	let target = root.querySelector(`[data-focus-key="${cssEscape(focus.key)}"]`);
	if (!target && focus.key.includes(":required-subject:")) {
		const instance = focus.key.slice(5, focus.key.lastIndexOf(":required-subject:"));
		target = root.querySelector(`[data-focus-key="${cssEscape(`view:${instance}:header`)}"]`);
	}
	if (!target) {
		focus = modalReturnFocus.get(root) ?? null;
		if (!focus) return;
		target = root.querySelector(`[data-focus-key="${cssEscape(focus.key)}"]`);
	}
	target?.focus({ preventScroll: true });
	if (target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement) {
		const start = focus.selectionStart;
		if (start !== null) target.setSelectionRange(start, focus.selectionEnd ?? start);
	}
	if (target) modalReturnFocus.delete(root);
}
function selection(element, field) {
	if (element instanceof HTMLInputElement || element instanceof HTMLTextAreaElement) return element[field];
	return null;
}
function cssEscape(value) {
	return CSS.escape(value);
}
//#endregion
//#region browser/runtime/effects.ts
async function runEffect(effect, producer) {
	const kind = effect.kind;
	switch (kind.type) {
		case "open_new_presentation_document":
		case "show_prepared_presentation_document": throw new Error("presentation document requires its prepare-first document host");
		case "observe_transfer_destination": {
			if (!producer || producer.presentation_id !== kind.source_producer.presentation_id || producer.thread_id !== kind.source_producer.thread_id || producer.producer_incarnation !== kind.source_producer.producer_incarnation) throw new Error("transfer observation has another current source producer");
			const encoded = encodeJsonBody({
				action: "observe",
				presentation_id: kind.destination_presentation_id,
				thread_id: kind.destination_thread_id,
				chain_root_id: kind.destination_chain_root_id
			});
			if (new TextEncoder().encode(encoded).byteLength > safeBound(kind.request_bounds.max_request_bytes, "max_request_bytes")) throw new Error("transfer observation exceeds admitted request bound");
			const response = await postEncodedJson("/ui/api/session/seat/open", encoded);
			return success(effect, "transfer_destination_observed", {
				operation_id: crypto.randomUUID(),
				observation: unwrapResult$1(response)
			});
		}
		case "contact_transfer": {
			if (!producer) throw new Error("transfer contact requires an acknowledged active presentation producer");
			const request = kind.request;
			const actor = request.action === "commit" || request.action === "abort" || request.action === "cancel_freeze" ? request.transfer.source : request.transfer.destination;
			if (producer.presentation_id !== actor.presentation_id || producer.thread_id !== actor.thread_id) throw new Error("transfer contact has another active producer");
			const encoded = encodeJsonBody(request);
			if (new TextEncoder().encode(encoded).byteLength > safeBound(kind.request_bounds.max_request_bytes, "max_request_bytes")) throw new Error("transfer contact exceeds admitted request bound");
			return success(effect, "binding_invoked", unwrapResult$1(await postEncodedJson("/ui/api/session/seat/transfer", encoded)));
		}
		case "prepare_transfer": {
			if (!producer) throw new Error("transfer preparation requires an acknowledged active presentation producer");
			if (producer.presentation_id !== kind.transfer.source.presentation_id || producer.thread_id !== kind.transfer.source.thread_id || producer.producer_incarnation !== kind.transfer.source.producer_incarnation) throw new Error("transfer preparation has another active producer");
			const encoded = encodeJsonBody({
				action: "prepare",
				transfer: kind.transfer,
				export: kind.export,
				freeze: kind.freeze
			});
			if (new TextEncoder().encode(encoded).byteLength > safeBound(kind.request_bounds.max_request_bytes, "max_request_bytes")) throw new Error("transfer preparation exceeds admitted request bound");
			return success(effect, "binding_invoked", unwrapResult$1(await postEncodedJson("/ui/api/session/seat/transfer", encoded)));
		}
		case "fetch_source": return success(effect, "source_data", unwrapResult$1(await dispatchBinding(kind.request, kind.request_bounds, producer)));
		case "invoke_binding": return success(effect, "binding_invoked", unwrapResult$1(await dispatchBinding(kind.request, kind.request_bounds, producer)));
		case "invoke_presentation_operation": {
			const operation = kind.operation;
			if (operation.body.kind !== "admission") throw new Error("composition operations are not executable invocations");
			return success(effect, "binding_invoked", unwrapResult$1(await dispatchBinding(operation.body.request, operation.request_bounds, producer, operation.operation_id)));
		}
		case "set_location_hash":
			location.hash = kind.hash;
			return success(effect, "browser_only", null);
		case "copy_to_clipboard":
			await navigator.clipboard.writeText(kind.text);
			return success(effect, "browser_only", null);
		case "open_url":
			window.open(kind.url, "_blank", "noopener,noreferrer");
			return success(effect, "browser_only", {
				action: "open_url",
				dispatch: "returned",
				opened: "unknown"
			});
		case "replace_session": {
			const launch = new URL(kind.launch_url, location.href);
			if (launch.origin !== location.origin || !launch.pathname.startsWith("/ui/launch/")) throw new Error("replacement UI session has an invalid launch origin or path");
			location.assign(`${launch.pathname}${launch.search}${launch.hash}`);
			return success(effect, "browser_only", null);
		}
		case "release_binding_attachment":
			if (!producer) throw new Error("attachment release requires an acknowledged active presentation producer");
			return success(effect, "browser_only", unwrapResult$1(await postEncodedJson("/ui/api/session/attachments/detach", encodeJsonBody({
				producer,
				binding_attachment_id: kind.binding_attachment_id,
				binding_generation: kind.binding_generation,
				binding_digest: kind.binding_digest
			}))));
	}
}
function failedEffectResult(effect, error) {
	return {
		id: effect.id,
		ok: false,
		kind: resultKind(effect),
		data: null,
		error: {
			code: "platform_effect_failed",
			error: errorMessage(error),
			retryable: false,
			outcome: error instanceof UnknownDeliveryError ? "unknown" : "refused"
		}
	};
}
async function dispatchBinding(request, bounds, producer, presentationOperationId) {
	if (!producer) throw new Error("UI dispatch requires an acknowledged active presentation producer");
	const maximum = safeBound(bounds.max_request_bytes, "max_request_bytes");
	const inputMaximum = safeBound(bounds.max_input_bytes, "max_input_bytes");
	const invocation = {
		producer,
		binding: request
	};
	if (presentationOperationId !== void 0) invocation.presentation_operation_id = presentationOperationId;
	const encoded = encodeJsonBody(invocation);
	if (new TextEncoder().encode(encoded).byteLength > maximum) throw new Error("UI binding request exceeds the session bound");
	if (request.payload.kind === "input" && new TextEncoder().encode(request.payload.value).byteLength > inputMaximum) throw new Error("UI binding input exceeds the session bound");
	return postEncodedJson("/ui/api/invocations/dispatch", encoded);
}
function safeBound(value, name) {
	if (value <= 0n || value > BigInt(Number.MAX_SAFE_INTEGER)) throw new Error(`invalid UI binding ${name}`);
	return Number(value);
}
function success(effect, kind, data) {
	return {
		id: effect.id,
		ok: true,
		kind,
		data,
		error: null
	};
}
function resultKind(effect) {
	if (effect.kind.type === "observe_transfer_destination") return "transfer_destination_observed";
	if (effect.kind.type === "fetch_source") return "source_data";
	if (effect.kind.type === "invoke_binding" || effect.kind.type === "invoke_presentation_operation" || effect.kind.type === "prepare_transfer" || effect.kind.type === "contact_transfer") return "binding_invoked";
	return "browser_only";
}
function unwrapResult$1(value) {
	if (!isRecord$1(value)) return value;
	const result = value.result;
	if (!isRecord$1(result)) return result ?? value;
	return result.result ?? result;
}
function isRecord$1(value) {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}
//#endregion
//#region browser/runtime/keyboard.ts
function keyEvent(event) {
	const key = keyName(event.key);
	if (key === null) return null;
	return {
		key,
		modifiers: {
			ctrl: event.ctrlKey,
			alt: event.altKey,
			shift: event.shiftKey,
			meta: event.metaKey
		}
	};
}
function keyName(domKey) {
	switch (domKey) {
		case "ArrowUp": return "arrow_up";
		case "ArrowDown": return "arrow_down";
		case "ArrowLeft": return "arrow_left";
		case "ArrowRight": return "arrow_right";
		case "Enter": return "enter";
		case "Escape": return "escape";
		case "Backspace": return "backspace";
		case "Tab": return "tab";
		default: return [...domKey].length === 1 ? { char: domKey } : null;
	}
}
function hasModifiers(event) {
	const { ctrl, alt, shift, meta } = event.modifiers;
	return ctrl || alt || shift || meta;
}
/** The launcher is a shell control even while an editable view owns focus. */
function isLauncherChord(event) {
	return !event.isComposing && event.key.toLowerCase() === "k" && !event.shiftKey && !event.metaKey && event.ctrlKey !== event.altKey;
}
function isTypingTarget(target) {
	if (!(target instanceof Element)) return false;
	if (target.closest("input, textarea, select")) return true;
	let element = target;
	while (element) {
		if (element instanceof HTMLElement) return element.isContentEditable;
		element = element.parentElement;
	}
	return false;
}
/** Key events from an open shadow tree may be retargeted to its host. */
function isTypingEvent(event) {
	return isTypingTarget(event.composedPath().find((entry) => entry instanceof Element) ?? event.target);
}
function isNativeActivationTarget(target) {
	return target instanceof Element && target.closest("button, a, summary") !== null;
}
//#endregion
//#region browser/runtime/chrome-keyboard.js
/** Shell chrome owns an unmodified Escape; it must not close a focused View. */
function isShellChromeEscape(event) {
	if (event.key !== "Escape" || event.isComposing || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return false;
	return [...event.composedPath?.() ?? [], event.target].some((entry) => entry instanceof Element && entry.closest(".system-bar, .status-bar") !== null);
}
//#endregion
//#region browser/runtime/layout-preferences.ts
/** Adapter persistence follows the opaque scope projected by Rust. It never
* reconstructs a Surface identity or writes a new scope under an old key. */
function createScopedLayoutPreferencePersistence(options) {
	let key = null;
	let active = null;
	let switching = false;
	return {
		observeAcceptedState() {
			if (switching) return false;
			try {
				const nextKey = options.readKey() ?? null;
				if (nextKey !== key || nextKey !== null && active === null) {
					active?.flush();
					active = null;
					key = nextKey;
					if (key === null) return false;
					switching = true;
					try {
						const saved = options.storage.getItem(key);
						const capturedKey = key;
						active = createLayoutPreferencePersistence({
							...options,
							key: capturedKey,
							persisted: saved,
							readCurrent: () => {
								if (options.readKey() !== capturedKey) throw new Error("layout preference scope changed");
								return options.readCurrent();
							}
						});
					} finally {
						switching = false;
					}
				}
				return active?.observeAcceptedState() ?? false;
			} catch (error) {
				options.reportError(error);
				return false;
			}
		},
		flush() {
			return active?.flush() ?? false;
		}
	};
}
/** Persist one opaque Rust-owned presentation arrangement. */
function createLayoutPreferencePersistence(options) {
	const delay = options.delayMs ?? 400;
	let lastObserved = options.readCurrent();
	let lastPersisted = options.persisted;
	let pending = null;
	let timer = null;
	function clearPending() {
		if (timer !== null) window.clearTimeout(timer);
		timer = null;
		pending = null;
	}
	function writeCaptured(value) {
		if (pending !== value) return false;
		timer = null;
		pending = null;
		try {
			options.storage.setItem(options.key, value);
			lastPersisted = value;
			return true;
		} catch (error) {
			pending = value;
			options.reportError(error);
			return false;
		}
	}
	function queue(value) {
		if (timer !== null) window.clearTimeout(timer);
		pending = value;
		timer = window.setTimeout(() => writeCaptured(value), delay);
	}
	return {
		observeAcceptedState() {
			let current;
			try {
				current = options.readCurrent();
			} catch (error) {
				options.reportError(error);
				return false;
			}
			if (current === lastObserved) return false;
			lastObserved = current;
			if (current === lastPersisted) {
				clearPending();
				return false;
			}
			queue(current);
			return true;
		},
		flush() {
			if (pending === null) return false;
			if (timer !== null) window.clearTimeout(timer);
			timer = null;
			return writeCaptured(pending);
		}
	};
}
//#endregion
//#region browser/runtime/attachment-subject.ts
/** Mechanical public DTO shape check only. Subject metadata never grants access. */
function requireAttachmentSubject(attachment) {
	const subject = attachment.subject;
	if (typeof subject !== "object" || subject === null || Array.isArray(subject)) throw new Error("binding attachment subject is required");
	const value = subject;
	const keys = Object.keys(value);
	const validText = (text) => typeof text === "string" && text.length > 0 && text.trim() === text && !/[\u0000-\u001f\u007f-\u009f]/u.test(text);
	const project = validText(attachment.project_path);
	if (value.kind === "projectless" && keys.length === 1 && attachment.project_path === null) return;
	if (value.kind === "unregistered_project" && keys.length === 1 && project) return;
	if (value.kind === "registered_project" && keys.length === 2 && keys.includes("local_id") && validText(value.local_id) && project) return;
	throw new Error("binding attachment subject contradicts its project metadata or closed schema");
}
//#endregion
//#region browser/runtime/seat-history-stream.ts
var MAX_FRAME_BYTES = 16777216;
var CHUNK_SLICE_CHARS = 65536;
var MAX_BATCH = 64;
var U64_MAX = 18446744073709551615n;
/** A provisional proof ended without a completion fence. No staged state is authoritative. */
var SeatHistoryIncompleteError = class extends Error {};
/** A completed cut needs replacement; this wake carries no phase authority. */
var SeatHistoryReproofRequiredError = class extends Error {};
function controlTip(payload) {
	if (!record(payload.request)) throw new Error("seat history producer control has no request");
	return {
		schema_version: payload.schema_version,
		operation_id: payload.operation_id,
		producer_incarnation: payload.producer_incarnation,
		state: payload.state,
		next_engine_seq: exactU64(payload.next_engine_seq),
		request_action: payload.request.action,
		request_operation_id: payload.request.operation_id,
		request_presentation_id: payload.request.presentation_id,
		request_chain_root_id: payload.request.chain_root_id,
		request_thread_id: payload.request.thread_id
	};
}
/** A fresh POST and a fresh proof on every call; no EventSource reconnect cursor. */
async function consumeSeatHistory(coordinate, claim, signal, stage, onInvalidated, onVerifiedAppend, stageTransferProof, stageMoveFreezeCancellationProof) {
	const seat = { ...coordinate };
	const expectedClaim = { ...claim };
	const response = await fetch("/ui/api/session/seat/history", {
		method: "POST",
		credentials: "same-origin",
		headers: {
			"Content-Type": "application/json",
			"Accept": "text/event-stream"
		},
		body: encodeJsonBody({
			presentation_id: seat.presentation_id,
			chain_root_id: seat.chain_root_id
		}),
		signal
	});
	if (!response.ok) throw new HttpResponseError(response.status, `seat history refused: ${response.status}`);
	if (!response.body || !response.headers.get("content-type")?.toLowerCase().startsWith("text/event-stream")) throw new Error("seat history did not return an event stream");
	const reader = response.body.getReader();
	const decoder = new TextDecoder("utf-8", { fatal: true });
	let buffer = "";
	let bufferedBytes = 0;
	let began = false;
	let complete = null;
	let eventCount = 0n;
	let lastThreadSeq = 0n;
	let lastEventHash = null;
	let lastControl = null;
	const recentVerified = /* @__PURE__ */ new Map();
	let settled = false;
	let stopped = false;
	const stop = () => {
		stopped = true;
		reader.cancel().catch(() => {});
	};
	return new Promise((resolve, reject) => {
		const fail = (error) => {
			if (stopped) return;
			stop();
			if (settled) onInvalidated(error);
			else reject(error);
		};
		const onAbort = () => fail(/* @__PURE__ */ new Error("seat history read was aborted"));
		signal.addEventListener("abort", onAbort, { once: true });
		(async () => {
			try {
				for (;;) {
					signal.throwIfAborted();
					const chunk = await reader.read();
					const decoded = decoder.decode(chunk.value, { stream: !chunk.done });
					for (let offset = 0; offset < decoded.length; offset += CHUNK_SLICE_CHARS) {
						const scanFrom = Math.max(0, buffer.length - 3);
						const piece = decoded.slice(offset, offset + CHUNK_SLICE_CHARS);
						buffer += piece;
						bufferedBytes += new TextEncoder().encode(piece).length;
						const separator = /\r?\n\r?\n/g;
						separator.lastIndex = scanFrom;
						let boundary;
						while ((boundary = separator.exec(buffer)) !== null) {
							if (stopped) return;
							const raw = buffer.slice(0, boundary.index);
							buffer = buffer.slice(boundary.index + boundary[0].length);
							bufferedBytes = new TextEncoder().encode(buffer).length;
							separator.lastIndex = 0;
							if (new TextEncoder().encode(raw).length > MAX_FRAME_BYTES) throw new Error("seat history frame exceeds bound");
							if (!raw || raw.split(/\r?\n/).every((line) => line.startsWith(":"))) continue;
							const frame = parseFrame(raw);
							if (complete) {
								if (frame.event === "ui_seat.replay.append_verified.v1") {
									const append = verifiedAppend(frame.data);
									const duplicate = recentVerified.get(append.operation_id);
									if (duplicate && (duplicate.threadSeq !== append.thread_seq || duplicate.eventHash !== append.event_hash)) throw new Error("seat history verified append reused an operation with different testimony");
									if (append.thread_seq < lastThreadSeq && (duplicate?.threadSeq !== append.thread_seq || duplicate.eventHash !== append.event_hash)) throw new Error("seat history verified append moved backwards");
									if (append.thread_seq === lastThreadSeq && append.event_hash !== lastEventHash) throw new Error("seat history verified append changed the retained tip");
									if (append.thread_seq > lastThreadSeq) {
										lastThreadSeq = append.thread_seq;
										lastEventHash = append.event_hash;
									}
									if (!duplicate) {
										recentVerified.set(append.operation_id, {
											threadSeq: append.thread_seq,
											eventHash: append.event_hash
										});
										if (recentVerified.size > 128) recentVerified.delete(recentVerified.keys().next().value);
									}
									onVerifiedAppend(append);
									if (stopped) return;
									continue;
								}
								if (frame.event === "ui_seat.replay.reproof_required.v1" && frame.data.schema === 1n && Object.keys(frame.data).length === 1) {
									fail(new SeatHistoryReproofRequiredError("seat history requires a fresh paired proof"));
									return;
								}
								if (frame.event === "ui_seat.replay.invalidated.v1" && frame.data.schema === 1n && Object.keys(frame.data).length === 1) {
									fail(/* @__PURE__ */ new Error("seat history was invalidated"));
									return;
								}
								throw new Error("seat history emitted unsupported post-completion frame");
							}
							if (frame.event === "ui_seat.replay.begin.v1") {
								if (began || frame.data.schema !== 1n || frame.data.presentation_id !== seat.presentation_id || frame.data.chain_root_id !== seat.chain_root_id || frame.data.thread_id !== seat.thread_id) throw new Error("seat history begin does not match the claimed seat");
								began = true;
							} else if (frame.event === "ui_seat.replay.events.v1") {
								if (!began || frame.data.schema !== 1n || !Array.isArray(frame.data.events) || frame.data.events.length === 0 || frame.data.events.length > MAX_BATCH) throw new Error("seat history batch has invalid shape or order");
								const records = [];
								for (const entry of frame.data.events) {
									if (!record(entry) || entry.thread_id !== seat.thread_id || entry.chain_root_id !== seat.chain_root_id || typeof entry.event_hash !== "string" || !/^[0-9a-f]{64}$/.test(entry.event_hash)) throw new Error("seat history event has invalid retained identity");
									const sequence = exactU64(entry.thread_seq);
									if (sequence <= lastThreadSeq) throw new Error("seat history event order moved backwards");
									if (lastEventHash !== null && sequence === lastThreadSeq + 1n && entry.prev_thread_event_hash !== lastEventHash) throw new Error("seat history predecessor is discontinuous");
									lastThreadSeq = sequence;
									lastEventHash = entry.event_hash;
									if (entry.event_type === "ui_seat.producer.v3") {
										if (!record(entry.payload)) throw new Error("seat history producer control has no payload");
										lastControl = controlTip(entry.payload);
									}
									eventCount++;
									records.push(entry);
								}
								stage(records);
								if (stopped) return;
							} else if (frame.event === "ui_seat.replay.transfer_proof.v1") {
								if (!began || frame.data.schema !== 1n || Object.keys(frame.data).length !== 2 || !record(frame.data.proof)) throw new Error("seat history transfer proof has invalid shape or order");
								stageTransferProof(frame.data.proof);
								if (stopped) return;
							} else if (frame.event === "ui_seat.replay.move_freeze_cancellation_proof.v1") {
								if (!began || frame.data.schema !== 1n || Object.keys(frame.data).length !== 2 || !record(frame.data.proof)) throw new Error("seat history move freeze cancellation proof has invalid shape or order");
								stageMoveFreezeCancellationProof(frame.data.proof);
								if (stopped) return;
							} else if (frame.event === "ui_seat.replay.complete.v1") {
								if (!began || frame.data.schema !== 1n || typeof frame.data.tip_hash !== "string" || !/^[0-9a-f]{64}$/.test(frame.data.tip_hash)) throw new Error("seat history completion is invalid");
								const count = exactU64(frame.data.event_count);
								const tipSequence = exactU64(frame.data.thread_seq);
								if (count !== eventCount || tipSequence !== lastThreadSeq || lastEventHash !== null && frame.data.tip_hash !== lastEventHash) throw new Error("seat history completion does not match staged events");
								if (!lastControl || lastControl.schema_version !== "ryeos.ui.seat.producer.v3" || lastControl.operation_id !== expectedClaim.operation_id || lastControl.producer_incarnation !== expectedClaim.producer_incarnation || lastControl.state !== "active" || lastControl.next_engine_seq !== expectedClaim.next_engine_seq || lastControl.request_action !== "claim" || lastControl.request_operation_id !== expectedClaim.operation_id || lastControl.request_presentation_id !== seat.presentation_id || lastControl.request_chain_root_id !== seat.chain_root_id || lastControl.request_thread_id !== seat.thread_id) throw new Error("seat history completion does not retain the acknowledged producer claim");
								complete = {
									tip_hash: frame.data.tip_hash,
									thread_seq: tipSequence,
									event_count: count
								};
								settled = true;
								resolve({
									completion: complete,
									close: stop
								});
							} else if (frame.event === "ui_seat.replay.invalidated.v1" && began && frame.data.schema === 1n && Object.keys(frame.data).length === 1) {
								fail(new SeatHistoryIncompleteError("seat history was invalidated before completion"));
								return;
							} else throw new Error(`seat history has unknown frame ${frame.event}`);
						}
						if (bufferedBytes > MAX_FRAME_BYTES) {
							bufferedBytes = new TextEncoder().encode(buffer).length;
							if (bufferedBytes > MAX_FRAME_BYTES) throw new Error("seat history frame exceeds bound");
						}
					}
					if (chunk.done) break;
				}
				if (!stopped) fail(settled ? /* @__PURE__ */ new Error("seat history live handoff ended") : new SeatHistoryIncompleteError(buffer.trim() ? "seat history ended with an incomplete frame" : "seat history stream ended before completion"));
			} catch (error) {
				if (!stopped) fail(error instanceof Error ? error : new Error(String(error)));
				else if (!settled) reject(error);
			} finally {
				signal.removeEventListener("abort", onAbort);
				if (!stopped) stop();
			}
		})();
	});
}
function verifiedAppend(data) {
	const fields = [
		"schema",
		"operation_id",
		"producer_incarnation",
		"first_engine_seq",
		"last_engine_seq",
		"event_count",
		"payload_digest",
		"event_hash",
		"thread_seq"
	];
	if (Object.keys(data).length !== fields.length || fields.some((field) => !Object.hasOwn(data, field)) || data.schema !== 1n || typeof data.operation_id !== "string" || !/^[0-9a-f]{8}-[0-9a-f]{4}-8[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(data.operation_id) || typeof data.producer_incarnation !== "string" || !/^[0-9a-f]{64}$/.test(data.producer_incarnation) || typeof data.payload_digest !== "string" || !/^[0-9a-f]{64}$/.test(data.payload_digest) || typeof data.event_hash !== "string" || !/^[0-9a-f]{64}$/.test(data.event_hash)) throw new Error("seat history verified append has invalid shape");
	const first = exactU64(data.first_engine_seq);
	const last = exactU64(data.last_engine_seq);
	const count = exactU64(data.event_count);
	const threadSeq = exactU64(data.thread_seq);
	if (count === 0n || first > last || last - first + 1n !== count || threadSeq === 0n) throw new Error("seat history verified append has invalid interval");
	return {
		operation_id: data.operation_id,
		producer_incarnation: data.producer_incarnation,
		first_engine_seq: first,
		last_engine_seq: last,
		event_count: count,
		payload_digest: data.payload_digest,
		event_hash: data.event_hash,
		thread_seq: threadSeq
	};
}
function parseFrame(raw) {
	let event = null;
	let data = null;
	for (const line of raw.split(/\r?\n/)) {
		if (line.startsWith(":")) continue;
		if (line.startsWith("event: ") && event === null) event = line.slice(7);
		else if (line.startsWith("data: ") && data === null) data = line.slice(6);
		else throw new Error("seat history has duplicate or unsupported SSE field");
	}
	if (!event || data === null) throw new Error("seat history frame omitted event or payload");
	const decoded = decodeJsonBody(data);
	if (!record(decoded)) throw new Error("seat history frame payload is not an object");
	return {
		event,
		data: decoded
	};
}
function record(value) {
	return value !== null && typeof value === "object" && !Array.isArray(value);
}
function exactU64(value) {
	const number = typeof value === "bigint" ? value : typeof value === "number" && Number.isSafeInteger(value) ? BigInt(value) : typeof value === "string" && /^(0|[1-9][0-9]*)$/.test(value) ? BigInt(value) : null;
	if (number === null || number < 0n || number > U64_MAX) throw new Error("seat history integer is invalid");
	return number;
}
//#endregion
//#region browser/runtime/thread-tail.ts
/** A chain stream is wider than the currently focused route thread. */
function focusedThreadTailFrame(frame, chainRootId, threadId) {
	if (!chainRootId || !threadId || frame === null || typeof frame !== "object" || Array.isArray(frame)) return null;
	const envelope = frame;
	if (typeof envelope.event_type !== "string" || envelope.payload === null || typeof envelope.payload !== "object" || Array.isArray(envelope.payload)) return null;
	const record = envelope.payload;
	if (record.chain_root_id !== chainRootId || record.thread_id !== threadId || record.event_type !== envelope.event_type) return null;
	return {
		eventType: envelope.event_type,
		payload: record
	};
}
//#endregion
//#region browser/runtime/session.ts
var MAX_SEAT_BATCH_EVENTS = 64;
var MAX_COLD_REPLAY_ATTEMPTS = 3;
var MAX_U64 = 18446744073709551615n;
var MAX_SEAT_APPEND_ATTEMPTS = 5;
var MAX_PENDING_SEAT_EVENTS = 512;
var MAX_PENDING_SEAT_BYTES = 524288;
var MAX_REMEMBERED_APPENDS = 128;
function createSessionRuntime(options) {
	if (typeof options.maxSeatAppendBytes !== "bigint" || options.maxSeatAppendBytes < 1n || options.maxSeatAppendBytes > BigInt(Number.MAX_SAFE_INTEGER)) throw new Error("signed seat append byte bound is invalid");
	if (typeof options.deriveSeatAppendIdentity !== "function") throw new Error("shared Rust seat append identity bridge is required");
	if (typeof options.deriveSeatControlIdentity !== "function") throw new Error("shared Rust seat control identity bridge is required");
	if (typeof options.replaySeatTransferProof !== "function") throw new Error("shared Rust provisional transfer proof bridge is required");
	if (typeof options.replayAcknowledgedSeatEvents !== "function") throw new Error("shared Rust acknowledged seat evidence bridge is required");
	if (typeof options.replaySeatMoveFreezeCancellationProof !== "function") throw new Error("session requires move freeze cancellation proof replay bridge");
	const maxSeatAppendBytes = Number(options.maxSeatAppendBytes);
	let presentation = options.presentation ?? null;
	let pendingStorageKey = presentation ? storageKey(options.sessionId, presentation.presentation_id) : null;
	const creationRequestId = crypto.randomUUID();
	const claimIntentNonce = crypto.randomUUID();
	let claimRequest = null;
	let relinquishRequest = null;
	let relinquishing = null;
	let attaching = null;
	const controlRequests = /* @__PURE__ */ new Map();
	let seatThreadId = null;
	let seatProducer = null;
	let nextEngineSeq = 0n;
	let acknowledgedClaim = null;
	let seatObserved = 0;
	let replayHandoff = null;
	let replayAbort = null;
	let replayInvalidated = false;
	let engineBootstrapped = false;
	const pendingSeatEvents = [];
	let pendingSeatBytes = 0;
	let seatAttaching = false;
	let seatReady = false;
	let seatSyncing = false;
	let seatSyncPromise = null;
	let reproofing = null;
	let reproofPending = false;
	let seatSyncBlocked = false;
	let currentAppend = null;
	const acknowledgedAppends = /* @__PURE__ */ new Map();
	const acknowledgedReceipts = /* @__PURE__ */ new Map();
	const verifiedAppends = /* @__PURE__ */ new Map();
	let closed = false;
	let seatEpoch = 0;
	let heartbeat = null;
	let seatTouchInflight = false;
	let events = null;
	let eventsOpened = false;
	let tail = null;
	let tailUrl = null;
	let tailThreadId = null;
	let tailChainRootId = null;
	let hintTimer = null;
	const dirtyHints = /* @__PURE__ */ new Set();
	function commitTransport(channel, freshness) {
		options.commitEvent({
			type: "transport_state_changed",
			channel,
			freshness,
			observed_at_ms: BigInt(Date.now()),
			error: null
		});
	}
	function fenceSeatHistory(error) {
		replayInvalidated = true;
		seatReady = false;
		seatSyncBlocked = true;
		options.onUnavailable?.({
			kind: "writer_refused",
			error
		});
	}
	function receiveHistoryFailure(error) {
		if (!(error instanceof SeatHistoryReproofRequiredError) || !engineBootstrapped) {
			fenceSeatHistory(error);
			return;
		}
		if (closed || replayInvalidated) return;
		if (reproofing) {
			reproofPending = true;
			return;
		}
		reproofPending = false;
		capturePendingSeatEvents();
		seatReady = false;
		const taskEpoch = seatEpoch;
		const taskProducer = seatProducer;
		const task = reproveCurrentSeat();
		reproofing = task;
		task.catch((reason) => {
			if (!closed && taskEpoch === seatEpoch && taskProducer === seatProducer) fenceSeatHistory(reason instanceof Error ? reason : new Error(String(reason)));
		}).finally(() => {
			if (reproofing !== task) return;
			reproofing = null;
			if (reproofPending && !closed && !replayInvalidated && taskEpoch === seatEpoch && taskProducer === seatProducer) receiveHistoryFailure(new SeatHistoryReproofRequiredError("paired cut advanced during promotion"));
		});
	}
	async function reproveCurrentSeat() {
		const coordinate = presentation ? { ...presentation } : null;
		const claim = acknowledgedClaim;
		const epoch = seatEpoch;
		const producer = seatProducer;
		if (!coordinate || !claim || !producer || replayInvalidated || closed) throw new Error("paired reproof has no current acknowledged owner");
		const requireOwner = () => {
			if (closed || replayInvalidated || epoch !== seatEpoch || producer !== seatProducer || presentation?.presentation_id !== coordinate.presentation_id) throw new Error("paired reproof owner changed");
		};
		if (seatSyncPromise) await seatSyncPromise;
		requireOwner();
		if (currentAppend || pendingStorageKey && loadRetainedAppend(pendingStorageKey)) {
			await reconcileRetainedAppend();
			requireOwner();
		}
		capturePendingSeatEvents();
		while (pendingSeatEvents.length > 0) {
			const frontier = nextEngineSeq;
			await syncSeat();
			requireOwner();
			if (seatSyncBlocked || nextEngineSeq === frontier) throw new Error("paired reproof must retain unresolved seat mutations");
			capturePendingSeatEvents();
		}
		if (seatSyncBlocked || currentAppend) throw new Error("paired reproof append is not settled");
		replayHandoff?.close();
		replayHandoff = null;
		replayAbort?.abort();
		replayAbort = new AbortController();
		for (let attempt = 0; attempt < MAX_COLD_REPLAY_ATTEMPTS; attempt++) {
			let changed = false;
			let promoted = false;
			let handoff = null;
			try {
				await options.beginSeatReproof(coordinate, requireOwner);
				requireOwner();
				handoff = await consumeSeatHistory(coordinate, claim, replayAbort.signal, (events) => {
					requireOwner();
					options.replaySeatEvents(events);
				}, (error) => {
					if (closed || epoch !== seatEpoch) return;
					if (error instanceof SeatHistoryReproofRequiredError) {
						if (promoted) receiveHistoryFailure(error);
						else changed = true;
					} else fenceSeatHistory(error);
				}, (append) => {
					if (!closed && epoch === seatEpoch) receiveVerifiedAppend(append);
				}, (proof) => {
					requireOwner();
					options.replaySeatTransferProof(proof);
				}, (proof) => {
					requireOwner();
					options.replaySeatMoveFreezeCancellationProof(proof);
				});
				requireOwner();
				if (changed) throw new SeatHistoryIncompleteError("paired reproof cut advanced");
				const promotedCore = await options.completeSeatReproof(coordinate, handoff.completion, requireOwner);
				requireOwner();
				if (!Number.isSafeInteger(promotedCore.acknowledgedEventCount) || promotedCore.acknowledgedEventCount < 0 || promotedCore.acknowledgedEventCount > options.seatEvents().length || pendingSeatEvents.length > 0 || currentAppend) throw new Error("paired reproof has no exact acknowledged transport offset");
				seatObserved = promotedCore.acknowledgedEventCount;
				replayHandoff = handoff;
				handoff = null;
				promoted = true;
				seatReady = true;
				promotedCore.publish();
				if (changed) reproofPending = true;
				capturePendingSeatEvents();
				syncSeat();
				return;
			} catch (error) {
				handoff?.close();
				if (epoch === seatEpoch && producer === seatProducer) options.abortSeatReplay();
				if (!(error instanceof SeatHistoryIncompleteError) || attempt + 1 === MAX_COLD_REPLAY_ATTEMPTS || closed || replayInvalidated || replayAbort.signal.aborted) throw error;
			}
		}
	}
	function appendMatches(frame, append) {
		return frame.operation_id === append.operation_id && frame.producer_incarnation === append.producer_incarnation && frame.first_engine_seq === BigInt(append.first_engine_seq) && frame.last_engine_seq === BigInt(append.last_engine_seq) && frame.event_count === BigInt(append.event_count) && frame.payload_digest === append.payload_digest;
	}
	function receiveVerifiedAppend(frame) {
		const known = currentAppend?.operation_id === frame.operation_id ? currentAppend : acknowledgedAppends.get(frame.operation_id);
		if (!known || !appendMatches(frame, known)) throw new Error("verified seat append is not this producer's exact pending or acknowledged operation");
		const receipt = acknowledgedReceipts.get(frame.operation_id);
		if (receipt && (receipt.event_hash !== frame.event_hash || receipt.thread_seq !== frame.thread_seq)) throw new Error("verified seat append differs from its exact signed receipt");
		const previous = verifiedAppends.get(frame.operation_id);
		if (previous && (previous.event_hash !== frame.event_hash || previous.thread_seq !== frame.thread_seq)) throw new Error("verified seat append identity changed its retained position");
		if (!previous) verifiedAppends.set(frame.operation_id, frame);
		trimSettledAppendMemory();
	}
	function rememberAcknowledgedAppend(append, receipt) {
		const verified = verifiedAppends.get(append.operation_id);
		if (verified && (!appendMatches(verified, append) || verified.event_hash !== receipt.event_hash || verified.thread_seq !== receipt.thread_seq)) throw new UnknownDeliveryError("verified seat append differs from its exact acknowledgement");
		acknowledgedAppends.set(append.operation_id, append);
		acknowledgedReceipts.set(append.operation_id, receipt);
		trimSettledAppendMemory();
	}
	function trimSettledAppendMemory() {
		if ([...acknowledgedAppends.keys()].filter((id) => !verifiedAppends.has(id)).length > MAX_REMEMBERED_APPENDS) throw new Error("unverified seat append backlog exceeds bound; authenticated rebootstrap required");
		const settled = [...acknowledgedAppends.keys()].filter((id) => verifiedAppends.has(id));
		for (const id of settled.slice(0, Math.max(0, settled.length - MAX_REMEMBERED_APPENDS))) {
			acknowledgedAppends.delete(id);
			acknowledgedReceipts.delete(id);
			verifiedAppends.delete(id);
		}
		if (verifiedAppends.size > 256) throw new Error("verified seat append backlog exceeds bound; authenticated rebootstrap required");
	}
	function attachSeat() {
		if (replayInvalidated) return Promise.reject(/* @__PURE__ */ new Error("seat history handoff was invalidated; choose a new authenticated producer"));
		if (attaching) return attaching;
		const pending = performAttach();
		attaching = pending;
		pending.catch(() => {
			if (attaching === pending) attaching = null;
		});
		return pending;
	}
	async function performAttach() {
		if (closed || replayInvalidated) throw new Error("disposed or invalidated presentation cannot claim a producer");
		const epoch = ++seatEpoch;
		seatReady = false;
		seatAttaching = true;
		seatSyncBlocked = false;
		requireNoProvisionalEvents();
		if (!presentation) {
			if (options.claim) throw new Error("replacement claim requires an exact prepared presentation");
			const prepared = unwrapResult(await retrySeatControl("open", {
				action: "prepare",
				creation_request_id: creationRequestId,
				...options.rootAttachment
			}));
			if (closed || epoch !== seatEpoch) return;
			requireRootBinding(prepared.root_binding, options.rootAttachment);
			presentation = seatCoordinate(prepared);
			pendingStorageKey = storageKey(options.sessionId, presentation.presentation_id);
		}
		await reconcileRetainedAppend();
		if (closed || epoch !== seatEpoch) return;
		if (replayInvalidated) throw new Error("seat history was invalidated during producer reconciliation");
		if (claimRequest === null) {
			const predecessor = options.claim ? controlPredecessor(options.claim.predecessor) : (await observeProducer()).control_predecessor;
			if (closed || epoch !== seatEpoch) return;
			if (replayInvalidated) throw new Error("seat history was invalidated during producer observation");
			claimRequest = freezeControlRequest({
				action: "claim",
				schema: 2,
				operation_id: "",
				...presentation,
				predecessor,
				intent_nonce: claimIntentNonce,
				claim: options.claim ? {
					kind: options.claim.kind,
					expected_producer_incarnation: options.claim.expected_producer_incarnation
				} : { kind: "fresh" }
			});
		}
		const operationId = claimRequest.operation_id;
		const opened = unwrapResult(await retrySeatControl("open", claimRequest));
		if (closed || epoch !== seatEpoch) return;
		if (replayInvalidated) throw new Error("seat history was invalidated during producer claim");
		requireSeatCoordinate(opened, presentation);
		if (opened.operation_id !== operationId || opened.state !== "active") throw new Error("seat claim response does not match requested operation");
		seatThreadId = stringField(opened, "thread_id");
		if (!seatThreadId) throw new Error("seat/open returned no durable seat thread");
		const producer = requiredStringField(opened, "producer_incarnation");
		if (!/^[0-9a-f]{64}$/.test(producer)) throw new Error("claim returned invalid producer incarnation");
		const engineFrontier = exactU64Field(opened, "next_engine_seq");
		if (engineFrontier === MAX_U64) throw new Error("producer engine sequence is exhausted");
		seatProducer = producer;
		nextEngineSeq = engineFrontier;
		acknowledgedClaim = Object.freeze({
			operation_id: operationId,
			producer_incarnation: producer,
			next_engine_seq: engineFrontier
		});
		requireNoProvisionalEvents();
		if (heartbeat !== null) window.clearInterval(heartbeat);
		heartbeat = window.setInterval(() => {
			const touchedPresentation = presentation?.presentation_id;
			const touchedProducer = seatProducer;
			const touchedEpoch = seatEpoch;
			const touchedThreadId = seatThreadId;
			if (!closed && seatReady && !seatTouchInflight && touchedThreadId && touchedProducer && touchedPresentation) {
				seatTouchInflight = true;
				invokeSeat("touch", {
					presentation_id: touchedPresentation,
					thread_id: touchedThreadId,
					producer_incarnation: touchedProducer
				}).then((response) => handleSeatTouchOutcome(classifySeatTouchResponse(response, touchedPresentation, touchedThreadId), null), (error) => handleSeatTouchOutcome(classifySeatTouchError(error), error)).finally(() => {
					seatTouchInflight = false;
				});
			}
			function handleSeatTouchOutcome(outcome, error) {
				if (closed || touchedEpoch !== seatEpoch || touchedProducer !== seatProducer || touchedThreadId !== seatThreadId || touchedPresentation !== presentation?.presentation_id) return;
				if (outcome === "refused") reportSeatFailure(error, false);
			}
		}, 6e4);
		if (replayInvalidated) throw new Error("seat history was invalidated before replay handoff");
		replayHandoff?.close();
		replayHandoff = null;
		replayAbort?.abort();
		replayAbort = new AbortController();
		for (let attempt = 0; attempt < MAX_COLD_REPLAY_ATTEMPTS; attempt++) try {
			await options.beginSeatReplay(presentation);
			if (closed || epoch !== seatEpoch) throw new Error("seat replay owner changed before history read");
			const handoff = await consumeSeatHistory(presentation, acknowledgedClaim, replayAbort.signal, options.replaySeatEvents, (error) => {
				if (closed || epoch !== seatEpoch) return;
				receiveHistoryFailure(error);
			}, (append) => {
				if (closed || epoch !== seatEpoch) return;
				receiveVerifiedAppend(append);
			}, options.replaySeatTransferProof, options.replaySeatMoveFreezeCancellationProof);
			if (closed || epoch !== seatEpoch || replayInvalidated) {
				handoff.close();
				throw new Error("seat history changed before replay handoff");
			}
			replayHandoff = handoff;
			break;
		} catch (error) {
			options.abortSeatReplay();
			if (!(error instanceof SeatHistoryIncompleteError) || attempt + 1 === MAX_COLD_REPLAY_ATTEMPTS || closed || epoch !== seatEpoch || replayInvalidated || replayAbort.signal.aborted) throw error;
		}
		requireNoProvisionalEvents();
		if (replayInvalidated) throw new Error("seat history was invalidated before producer readiness");
		seatAttaching = false;
		seatReady = true;
		attachEvents();
		syncSeat();
	}
	async function reconcileRetainedAppend() {
		if (replayInvalidated) throw new Error("seat history was invalidated before append reconciliation");
		if (!pendingStorageKey || !presentation) throw new Error("presentation must be prepared before reconciliation");
		const retained = loadRetainedAppend(pendingStorageKey);
		if (!retained) return;
		if (retained.session_id !== options.sessionId || retained.presentation_id !== presentation.presentation_id || retained.thread_id !== presentation.thread_id || retained.chain_root_id !== presentation.chain_root_id) throw new Error("retained seat append belongs to a different browser session");
		const expected = {
			producer: retained.producer_incarnation,
			operationId: retained.operation_id,
			firstEngineSeq: BigInt(retained.first_engine_seq),
			lastEngineSeq: BigInt(retained.last_engine_seq),
			eventCount: retained.event_count,
			payloadDigest: retained.payload_digest
		};
		if (options.deriveSeatAppendIdentity({
			...presentation,
			producer_incarnation: expected.producer,
			first_engine_seq: expected.firstEngineSeq,
			last_engine_seq: expected.lastEngineSeq,
			event_count: BigInt(expected.eventCount),
			payload_digest: expected.payloadDigest
		}) !== expected.operationId) throw new Error("retained append uses an unsupported or conflicting transport identity");
		const retainedRequest = retainedAppendEvents(retained, expected, presentation);
		if (await sha256Hex(encodeSeatPayloadDigest(retainedRequest)) !== expected.payloadDigest) throw new Error("retained append payload differs from its exact commitment");
		const observed = unwrapResult(await invokeSeat("replay", {
			presentation_id: presentation.presentation_id,
			chain_root_id: presentation.chain_root_id,
			receipt: {
				thread_id: retained.thread_id,
				producer_incarnation: expected.producer,
				operation_id: expected.operationId,
				first_engine_seq: expected.firstEngineSeq,
				last_engine_seq: expected.lastEngineSeq,
				event_count: expected.eventCount,
				payload_digest: expected.payloadDigest
			}
		}));
		if (Object.keys(observed).length !== 2 || !Object.hasOwn(observed, "receipt_status") || !Object.hasOwn(observed, "receipt")) throw new UnknownDeliveryError("prior seat append receipt lookup has an unsupported response");
		let receipt;
		if (observed.receipt_status === "found" && isRecord(observed.receipt)) receipt = requireAppendAcknowledgement(observed.receipt, expected, retainedRequest, presentation);
		else if (observed.receipt_status === "not_found" && observed.receipt === null) receipt = await appendWithExactRetry(retained.encoded_request, retainedRequest, expected, options.sessionId, options.rootAttachment, presentation);
		else throw new UnknownDeliveryError("prior seat append receipt is not confirmed; replacement claim blocked");
		if (replayInvalidated) throw new Error("seat history was invalidated during append reconciliation");
		const epoch = seatEpoch;
		const current = currentAppend;
		if (seatProducer !== null) {
			if (!current || current.encoded_request !== retained.encoded_request || current.operation_id !== retained.operation_id || current.payload_digest !== retained.payload_digest || seatProducer !== retained.producer_incarnation || seatThreadId !== retained.thread_id || nextEngineSeq !== expected.firstEngineSeq || expected.lastEngineSeq !== expected.firstEngineSeq + BigInt(expected.eventCount - 1) || pendingSeatEvents.length < expected.eventCount) throw new Error("retained receipt does not match the current producer's pending interval");
			const prefix = pendingSeatEvents.slice(0, expected.eventCount);
			const wire = wireSeatEvents(prefix, expected.firstEngineSeq);
			const digest = await sha256Hex(encodeSeatPayloadDigest(wire));
			const encoded = encodeJsonBody({
				presentation_id: presentation.presentation_id,
				thread_id: seatThreadId,
				producer_incarnation: seatProducer,
				operation_id: expected.operationId,
				first_engine_seq: expected.firstEngineSeq,
				last_engine_seq: expected.lastEngineSeq,
				event_count: expected.eventCount,
				payload_digest: digest,
				events: wire
			});
			if (closed || replayInvalidated || epoch !== seatEpoch || currentAppend !== current || digest !== expected.payloadDigest || encoded !== retained.encoded_request || encodeSeatPayloadDigest(wireSeatEvents(pendingSeatEvents.slice(0, expected.eventCount), expected.firstEngineSeq)) !== encodeSeatPayloadDigest(wire)) throw new Error("retained receipt does not prove the exact queued prefix");
			rememberAcknowledgedAppend(retained, receipt);
			clearRetainedAppend(pendingStorageKey, retained.operation_id, retained.producer_incarnation, retained.encoded_request);
			pendingSeatEvents.splice(0, expected.eventCount);
			pendingSeatBytes -= prefix.reduce((total, event) => total + encodedSeatEventBytes(event), 0);
			nextEngineSeq = expected.lastEngineSeq + 1n;
			currentAppend = null;
			seatSyncBlocked = false;
			options.replayAcknowledgedSeatEvents({
				presentation_id: presentation.presentation_id,
				thread_id: retained.thread_id,
				producer_incarnation: retained.producer_incarnation
			}, prefix, receipt.events);
			return;
		}
		if (replayInvalidated) throw new Error("seat history was invalidated before clearing retained append");
		clearRetainedAppend(pendingStorageKey, retained.operation_id, retained.producer_incarnation, retained.encoded_request);
	}
	function attachEvents() {
		if (!options.eventsUrl) return;
		events?.close();
		const source = new EventSource(options.eventsUrl);
		events = source;
		const forward = (event) => {
			try {
				options.commitEvent({
					type: "tick",
					now_ms: BigInt(Date.now())
				});
				options.commitEvent({
					type: "daemon_event",
					payload: decodeJsonBody(event.data)
				});
			} catch (error) {
				console.warn("Failed to process RyeOS session event", error);
			}
		};
		source.addEventListener("message", forward);
		source.addEventListener("ui_intent.applied", forward);
		source.addEventListener("thread.hint", ((event) => {
			try {
				const payload = decodeJsonBody(event.data);
				const kind = isRecord(payload) && typeof payload.kind === "string" ? payload.kind : null;
				if (!kind) return;
				options.commitEvent({
					type: "hint_received",
					kind,
					payload
				});
				dirtyHints.add(kind);
				if (hintTimer === null) hintTimer = window.setTimeout(flushHints, 500);
			} catch (error) {
				console.warn("Failed to process RyeOS lifecycle hint", error);
			}
		}));
		const requireSnapshot = () => commitTransport("hints", "gap_resnapshot_required");
		source.addEventListener("snapshot_required", requireSnapshot);
		source.addEventListener("open", () => {
			commitTransport("hints", eventsOpened ? "gap_resnapshot_required" : "current");
			eventsOpened = true;
		});
		source.addEventListener("error", () => commitTransport("hints", "reconnecting"));
	}
	function flushHints() {
		hintTimer = null;
		const kinds = [...dirtyHints];
		dirtyHints.clear();
		if (kinds.length > 0) options.commitEvent({
			type: "hint_flush_batch",
			kinds
		});
	}
	function observe(model) {
		if (closed || !seatReady) return;
		syncTail(model);
		if (!seatSyncBlocked && (seatAttaching || seatReady)) capturePendingSeatEvents();
		syncSeat();
	}
	function capturePendingSeatEvents() {
		if (seatSyncBlocked) return;
		const all = options.seatEvents();
		if (all.length < seatObserved) {
			seatSyncBlocked = true;
			reportSeatFailure(/* @__PURE__ */ new Error("RyeOS seat event log moved backwards"), false);
			return;
		}
		while (seatObserved < all.length) {
			const event = all[seatObserved];
			if (!event || !appendPendingSeatEvent(event)) return;
			seatObserved += 1;
		}
	}
	function appendPendingSeatEvent(event) {
		let encodedBytes;
		try {
			encodedBytes = encodedSeatEventBytes(event);
		} catch (error) {
			seatSyncBlocked = true;
			reportSeatFailure(error, false);
			return false;
		}
		if (pendingSeatEvents.length >= MAX_PENDING_SEAT_EVENTS || pendingSeatBytes + encodedBytes > MAX_PENDING_SEAT_BYTES) {
			seatSyncBlocked = true;
			reportSeatFailure(/* @__PURE__ */ new Error(`unsaved seat history exceeds the pending limit (${MAX_PENDING_SEAT_EVENTS} events / ${MAX_PENDING_SEAT_BYTES} bytes)`), false);
			return false;
		}
		pendingSeatEvents.push(event);
		pendingSeatBytes += encodedBytes;
		return true;
	}
	function requireNoProvisionalEvents() {
		if (options.seatEvents().length !== seatObserved || pendingSeatEvents.length !== 0) throw new Error("provisional seat events before authoritative replay completed are refused");
	}
	function syncTail(model) {
		tailThreadId = model.tail_thread_id ?? model.tail_chain_root_id ?? null;
		tailChainRootId = model.tail_chain_root_id ?? null;
		const nextUrl = model.tail_url ?? null;
		if (nextUrl === tailUrl) return;
		tail?.close();
		tail = null;
		tailUrl = nextUrl;
		if (!nextUrl) return;
		const source = new EventSource(nextUrl);
		tail = source;
		let opened = false;
		source.addEventListener("message", (event) => {
			try {
				const frame = decodeJsonBody(event.data);
				if (source !== tail) return;
				const focused = focusedThreadTailFrame(frame, tailChainRootId, tailThreadId);
				if (!focused || !tailThreadId) return;
				options.commitEvent({
					type: "thread_tail",
					thread_id: tailThreadId,
					event_type: focused.eventType,
					payload: focused.payload
				});
			} catch (error) {
				console.warn("Failed to process RyeOS thread tail", error);
			}
		});
		source.addEventListener("open", () => {
			commitTransport("focused_tail", opened ? "gap_resnapshot_required" : "current");
			opened = true;
		});
		source.addEventListener("error", () => commitTransport("focused_tail", "reconnecting"));
	}
	async function syncSeat() {
		if (seatSyncPromise) return seatSyncPromise;
		const frontier = nextEngineSeq;
		const pending = syncSeatOnce();
		seatSyncPromise = pending;
		try {
			await pending;
		} finally {
			if (seatSyncPromise === pending) seatSyncPromise = null;
		}
		if (!closed && !seatSyncBlocked && nextEngineSeq > frontier && pendingSeatEvents.length > 0) syncSeat();
	}
	async function syncSeatOnce() {
		if (closed || !presentation || !pendingStorageKey || !seatThreadId || !seatProducer || seatSyncing || seatSyncBlocked) return;
		capturePendingSeatEvents();
		if (seatSyncBlocked) return;
		if (pendingSeatEvents.length === 0) return;
		let batchEvents = pendingSeatEvents.slice(0, MAX_SEAT_BATCH_EVENTS);
		const firstEngineSeq = nextEngineSeq;
		seatSyncing = true;
		let operationId;
		let payloadDigest;
		let encodedRequest;
		let batch = [];
		let lastEngineSeq = firstEngineSeq;
		const threadId = seatThreadId;
		const producer = seatProducer;
		const epoch = seatEpoch;
		try {
			const remainingEnginePositions = MAX_U64 - firstEngineSeq;
			if (remainingEnginePositions <= 0n) throw new Error("producer engine interval is exhausted");
			if (remainingEnginePositions < BigInt(batchEvents.length)) batchEvents = batchEvents.slice(0, Number(remainingEnginePositions));
			operationId = "00000000-0000-8000-8000-000000000000";
			while (batchEvents.length > 0) {
				const candidate = wireSeatEvents(batchEvents, firstEngineSeq);
				const candidateLast = firstEngineSeq + BigInt(candidate.length - 1);
				if (encodedByteLength(encodeJsonBody({
					presentation_id: presentation.presentation_id,
					thread_id: threadId,
					producer_incarnation: producer,
					operation_id: operationId,
					first_engine_seq: firstEngineSeq,
					last_engine_seq: candidateLast,
					event_count: candidate.length,
					payload_digest: "0".repeat(64),
					events: candidate
				})) <= maxSeatAppendBytes) break;
				batchEvents = batchEvents.slice(0, -1);
			}
			if (batchEvents.length === 0) throw new Error(`one seat event exceeds the ${maxSeatAppendBytes}-byte append route limit`);
			batch = wireSeatEvents(batchEvents, firstEngineSeq);
			lastEngineSeq = firstEngineSeq + BigInt(batch.length - 1);
			payloadDigest = await sha256Hex(encodeSeatPayloadDigest(batch));
			if (closed || epoch !== seatEpoch || threadId !== seatThreadId || producer !== seatProducer) return;
			operationId = options.deriveSeatAppendIdentity({
				...presentation,
				producer_incarnation: producer,
				first_engine_seq: firstEngineSeq,
				last_engine_seq: lastEngineSeq,
				event_count: BigInt(batch.length),
				payload_digest: payloadDigest
			});
			if (!/^[0-9a-f]{8}-[0-9a-f]{4}-8[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(operationId)) throw new Error("shared Rust seat append identity is invalid");
			encodedRequest = encodeJsonBody({
				presentation_id: presentation.presentation_id,
				thread_id: threadId,
				producer_incarnation: producer,
				operation_id: operationId,
				first_engine_seq: firstEngineSeq,
				last_engine_seq: lastEngineSeq,
				event_count: batch.length,
				payload_digest: payloadDigest,
				events: batch
			});
			if (encodedByteLength(encodedRequest) > maxSeatAppendBytes) throw new Error("seat append exceeded its route limit after final encoding");
			const retained = {
				schema_version: "ryeos.ui.seat.pending-append.v3",
				session_id: options.sessionId,
				presentation_id: presentation.presentation_id,
				chain_root_id: presentation.chain_root_id,
				thread_id: threadId,
				producer_incarnation: producer,
				operation_id: operationId,
				first_engine_seq: firstEngineSeq.toString(),
				last_engine_seq: lastEngineSeq.toString(),
				event_count: batch.length,
				payload_digest: payloadDigest,
				encoded_request: encodedRequest
			};
			retainAppend(pendingStorageKey, retained);
			currentAppend = retained;
		} catch (error) {
			if (closed || epoch !== seatEpoch || threadId !== seatThreadId || producer !== seatProducer) return;
			seatSyncBlocked = true;
			seatSyncing = false;
			reportSeatFailure(error, false);
			return;
		}
		let acknowledged = false;
		try {
			const receipt = await appendWithExactRetry(encodedRequest, batch, {
				producer,
				operationId,
				firstEngineSeq,
				lastEngineSeq,
				eventCount: batch.length,
				payloadDigest
			}, options.sessionId, options.rootAttachment, presentation);
			if (closed || epoch !== seatEpoch || threadId !== seatThreadId || producer !== seatProducer) return;
			if (!currentAppend) throw new Error("seat append lost its retained identity before acknowledgement");
			rememberAcknowledgedAppend(currentAppend, receipt);
			clearRetainedAppend(pendingStorageKey, operationId, producer);
			currentAppend = null;
			pendingSeatEvents.splice(0, batch.length);
			pendingSeatBytes -= batchEvents.reduce((total, event) => total + encodedSeatEventBytes(event), 0);
			nextEngineSeq = lastEngineSeq + 1n;
			acknowledged = true;
			options.replayAcknowledgedSeatEvents({
				presentation_id: presentation.presentation_id,
				thread_id: threadId,
				producer_incarnation: producer
			}, batchEvents, receipt.events);
		} catch (error) {
			if (closed || epoch !== seatEpoch || threadId !== seatThreadId) return;
			seatSyncBlocked = true;
			reportSeatFailure(error, error instanceof UnknownDeliveryError || isRetryableResponse(error));
			console.warn("RyeOS seat sync stopped with unsaved history", error);
		} finally {
			if (epoch === seatEpoch) seatSyncing = false;
			if (acknowledged && !closed) capturePendingSeatEvents();
		}
	}
	function reportSeatFailure(error, retryable) {
		if (closed) return;
		if (error instanceof HttpResponseError && (error.status === 403 || error.status === 404 || error.status === 409 || error.status === 410)) {
			seatReady = false;
			seatSyncBlocked = true;
		}
		const unknown = error instanceof UnknownDeliveryError;
		options.commitEvent({
			type: "transport_state_changed",
			channel: "session",
			freshness: "reconnecting",
			observed_at_ms: BigInt(Date.now()),
			error: {
				code: unknown ? "seat_append_outcome_unknown" : "seat_append_refused",
				error: `Seat history is not confirmed durable: ${errorMessage(error)}`,
				retryable,
				outcome: unknown ? "unknown" : "refused",
				remediation: "Relaunch or reconcile the UI session before submitting more seat mutations.",
				details: null
			}
		});
		if (error instanceof HttpResponseError && (error.status === 403 || error.status === 404 || error.status === 409 || error.status === 410)) options.onUnavailable?.({
			kind: "writer_refused",
			error
		});
		else if (error instanceof UnknownDeliveryError && seatSyncBlocked) options.onUnavailable?.({
			kind: "append_unknown",
			error
		});
	}
	function close() {
		if (closed) return;
		closed = true;
		seatEpoch += 1;
		replayAbort?.abort();
		replayHandoff?.close();
		options.abortSeatReplay();
		events?.close();
		tail?.close();
		if (heartbeat !== null) window.clearInterval(heartbeat);
		if (hintTimer !== null) window.clearTimeout(hintTimer);
		events = null;
		tail = null;
		heartbeat = null;
		hintTimer = null;
	}
	async function retrySeatControl(operation, body) {
		const requestKey = `${operation}:${isRecord(body) ? String(body.action) : ""}`;
		let encoded = controlRequests.get(requestKey);
		if (encoded === void 0) {
			encoded = encodeJsonBody(body);
			controlRequests.set(requestKey, encoded);
		}
		for (let attempt = 1;; attempt += 1) {
			if (closed) throw new Error("disposed presentation cannot retry seat control");
			try {
				return await postEncodedJson(`/ui/api/session/seat/${operation}`, encoded);
			} catch (error) {
				if (!isRetryableResponse(error) || attempt >= MAX_SEAT_APPEND_ATTEMPTS) throw error;
				await new Promise((resolve) => globalThis.setTimeout(resolve, Math.min(2e3, 100 * 2 ** (attempt - 1))));
			}
		}
	}
	function freezeControlRequest(request) {
		const operationId = options.deriveSeatControlIdentity(request);
		if (!/^[0-9a-f]{8}-[0-9a-f]{4}-8[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(operationId)) throw new Error("shared Rust seat control identity is invalid");
		return Object.freeze({
			...request,
			predecessor: Object.freeze({ ...request.predecessor }),
			operation_id: operationId
		});
	}
	function relinquish() {
		if (relinquishing) return relinquishing;
		const pending = performRelinquish();
		relinquishing = pending;
		pending.catch(() => {
			if (relinquishing === pending) relinquishing = null;
		});
		return pending;
	}
	async function performRelinquish() {
		if (closed || !presentation || !seatThreadId || !seatProducer || !seatReady && relinquishRequest === null) throw new Error("no active presentation producer to relinquish");
		const epoch = seatEpoch;
		const producer = seatProducer;
		if (relinquishRequest === null) {
			capturePendingSeatEvents();
			if (seatSyncing || pendingSeatEvents.length > 0 || !pendingStorageKey || loadRetainedAppend(pendingStorageKey)) throw new Error("seat mutations must settle before producer relinquishment");
			seatReady = false;
			seatSyncBlocked = true;
			let observation;
			try {
				observation = await observeProducer();
			} catch (error) {
				if (!closed && !replayInvalidated && epoch === seatEpoch && producer === seatProducer) {
					seatReady = true;
					seatSyncBlocked = false;
				}
				throw error;
			}
			if (closed || epoch !== seatEpoch || producer !== seatProducer) return;
			if (replayInvalidated) throw new Error("seat history was invalidated during relinquishment observation");
			if (observation.producer.kind !== "active" || observation.producer.producer_incarnation !== producer) throw new Error("relinquishment no longer names the observed active producer");
			if (options.seatEvents().length !== seatObserved) {
				if (!replayInvalidated) {
					seatReady = true;
					seatSyncBlocked = false;
					capturePendingSeatEvents();
					syncSeat();
				}
				throw new Error("new local mutations must settle before producer relinquishment");
			}
			relinquishRequest = freezeControlRequest({
				action: "relinquish",
				schema: 2,
				operation_id: "",
				...presentation,
				producer_incarnation: producer,
				predecessor: observation.control_predecessor,
				intent_nonce: crypto.randomUUID()
			});
		}
		const operationId = relinquishRequest.operation_id;
		seatReady = false;
		seatSyncBlocked = true;
		const response = unwrapResult(await retrySeatControl("close", relinquishRequest));
		if (closed || epoch !== seatEpoch || producer !== seatProducer) return;
		requireSeatCoordinate(response, presentation);
		if (response.operation_id !== operationId || response.producer_incarnation !== producer || response.state !== "relinquished") throw new UnknownDeliveryError("producer relinquishment acknowledgement is invalid");
		close();
	}
	async function reconnect(choice) {
		if (!presentation) throw new Error("reconnect requires an exact prepared presentation");
		if (choice.kind !== "independent" && !/^[0-9a-f]{64}$/.test(choice.expected_producer_incarnation)) throw new Error("reconnect requires an exact predecessor producer incarnation");
		const selectedClaim = choice.kind === "independent" ? void 0 : {
			kind: choice.kind,
			expected_producer_incarnation: choice.expected_producer_incarnation,
			predecessor: controlPredecessor(choice.predecessor)
		};
		if (selectedClaim?.predecessor.kind === "initialization") throw new Error("replacement requires an observed control predecessor");
		if (seatSyncing) throw new Error("seat append must settle before reconnect");
		await reconcileRetainedAppend();
		if (options.seatEvents().length !== seatObserved) throw new Error("uncaptured seat mutations must settle before reconnect");
		if (!closed && !seatSyncBlocked) capturePendingSeatEvents();
		if (pendingSeatEvents.length > 0 || currentAppend !== null || pendingStorageKey !== null && loadRetainedAppend(pendingStorageKey)) throw new Error("queued seat mutations must settle before reconnect");
		const { presentation: _previousPresentation, claim: _previousClaim, ...sharedOptions } = options;
		return createSessionRuntime({
			...sharedOptions,
			...selectedClaim === void 0 ? {} : {
				presentation: { ...presentation },
				claim: selectedClaim
			}
		});
	}
	async function observeProducer() {
		if (closed || !presentation) throw new Error("producer observation requires a retained presentation");
		const coordinate = { ...presentation };
		const observation = unwrapResult(await invokeSeat("open", {
			action: "observe",
			...coordinate
		}));
		if (closed || !presentation || presentation.presentation_id !== coordinate.presentation_id) throw new Error("producer observation belongs to a disposed document");
		requireSeatCoordinate(observation, coordinate);
		requireRootBinding(observation.root_binding, options.rootAttachment);
		const revision = exactIntegerField(observation, "revision");
		const control_predecessor = controlPredecessor(observation.control_predecessor);
		const producer = observation.producer;
		if (!isRecord(producer)) throw new Error("producer observation state is invalid");
		if (producer.kind === "unclaimed") {
			if (Object.keys(producer).some((key) => key !== "kind") || control_predecessor.kind !== "initialization") throw new Error("unclaimed producer observation contains unexpected fields");
			return {
				...coordinate,
				revision,
				root_binding: { ...options.rootAttachment },
				control_predecessor,
				producer: { kind: "unclaimed" }
			};
		}
		if (producer.kind !== "active" && producer.kind !== "relinquished" && producer.kind !== "closed") throw new Error("producer observation state is invalid");
		if (Object.keys(producer).some((key) => key !== "kind" && key !== "producer_incarnation") || control_predecessor.kind !== "control" || typeof producer.producer_incarnation !== "string" || !/^[0-9a-f]{64}$/.test(producer.producer_incarnation)) throw new Error("producer observation incarnation is invalid");
		return {
			...coordinate,
			revision,
			root_binding: { ...options.rootAttachment },
			control_predecessor,
			producer: {
				kind: producer.kind,
				producer_incarnation: producer.producer_incarnation
			}
		};
	}
	return {
		attachSeat,
		observe,
		relinquish,
		reconnect,
		close,
		observeProducer,
		async settle() {
			if (!closed) throw new Error("session settlement requires document disposal");
			await Promise.allSettled([
				attaching,
				reproofing,
				seatSyncPromise,
				relinquishing
			]);
		},
		currentReplayCompletion: () => !closed && seatReady && !seatSyncBlocked && !replayInvalidated && replayHandoff ? { ...replayHandoff.completion } : null,
		preparedPresentation: () => presentation ? { ...presentation } : null,
		async reconcilePendingAppend() {
			if (seatSyncing) throw new Error("seat append is still in flight");
			await reconcileRetainedAppend();
			if (!closed && !seatSyncBlocked) {
				capturePendingSeatEvents();
				syncSeat();
			}
		},
		canAcceptMutations: () => !closed && seatReady && !seatSyncBlocked && engineBootstrapped,
		currentProducer: () => !closed && seatReady && !seatSyncBlocked && presentation && seatThreadId && seatProducer ? {
			presentation_id: presentation.presentation_id,
			thread_id: seatThreadId,
			producer_incarnation: seatProducer
		} : null,
		currentEngineFrontier: () => !closed && seatReady && !seatSyncBlocked ? nextEngineSeq : null,
		currentPresentation: () => !closed && seatReady && presentation ? { ...presentation } : null,
		completeEngineBootstrap() {
			if (closed || !seatReady || engineBootstrapped) throw new Error("engine bootstrap requires one acknowledged presentation producer");
			engineBootstrapped = true;
			seatObserved = options.seatEvents().length;
		}
	};
}
async function invokeSeat(operation, body) {
	return postJson(`/ui/api/session/seat/${operation}`, body);
}
function classifySeatTouchResponse(value, presentationId, threadId) {
	try {
		const acknowledgement = unwrapResult(value);
		return Object.keys(acknowledgement).length === 3 && acknowledgement.presentation_id === presentationId && acknowledgement.thread_id === threadId && acknowledgement.touched === true ? "renewed" : "retry";
	} catch {
		return "retry";
	}
}
function classifySeatTouchError(error) {
	return error instanceof HttpResponseError && [
		403,
		404,
		409,
		410
	].includes(error.status) ? "refused" : "retry";
}
function storageKey(sessionId, presentationId) {
	return `ryeos.ui.seat.pending.v2:${encodeURIComponent(sessionId)}:${encodeURIComponent(presentationId)}`;
}
function seatCoordinate(value) {
	return {
		presentation_id: requiredStringField(value, "presentation_id"),
		thread_id: requiredStringField(value, "thread_id"),
		chain_root_id: requiredStringField(value, "chain_root_id")
	};
}
function requireSeatCoordinate(value, expected) {
	const actual = seatCoordinate(value);
	if (actual.presentation_id !== expected.presentation_id || actual.thread_id !== expected.thread_id || actual.chain_root_id !== expected.chain_root_id) throw new Error("seat response changed the retained presentation coordinate");
}
function requireRootBinding(value, expected) {
	if (!isRecord(value) || value.binding_attachment_id !== expected.binding_attachment_id || exactIntegerField(value, "binding_generation") !== expected.binding_generation || value.binding_digest !== expected.binding_digest) throw new Error("prepared seat root binding differs from admitted attachment");
}
async function appendWithExactRetry(encodedRequest, stagedEvents, expected, sessionId, rootAttachment, presentation) {
	const url = "/ui/api/session/seat/append";
	let authenticationReconciled = false;
	for (let attempt = 1;; attempt += 1) try {
		let response;
		try {
			response = unwrapResult(await postEncodedJson(url, encodedRequest));
		} catch (error) {
			if (error instanceof UnknownDeliveryError || error instanceof HttpResponseError) throw error;
			throw new UnknownDeliveryError(`seat append acknowledgement is unreadable: ${errorMessage(error)}`);
		}
		try {
			return requireAppendAcknowledgement(response, expected, stagedEvents, presentation);
		} catch (error) {
			if (error instanceof UnknownDeliveryError) throw error;
			throw new UnknownDeliveryError(`seat append acknowledgement is invalid: ${errorMessage(error)}`);
		}
	} catch (error) {
		if (isAuthenticationResponse(error) && !authenticationReconciled) {
			await requireCurrentSession(sessionId, rootAttachment, presentation);
			authenticationReconciled = true;
			continue;
		}
		const outcomeError = error instanceof HttpResponseError && (error.status === 408 || error.status === 429 || error.status >= 500) ? new UnknownDeliveryError(`seat append response leaves delivery unknown: ${errorMessage(error)}`) : error;
		if (!isRetryableResponse(outcomeError) || attempt >= MAX_SEAT_APPEND_ATTEMPTS) throw outcomeError;
		await new Promise((resolve) => {
			globalThis.setTimeout(resolve, Math.min(2e3, 100 * 2 ** (attempt - 1)));
		});
	}
}
function isAuthenticationResponse(error) {
	return error instanceof HttpResponseError && (error.status === 401 || error.status === 403);
}
async function requireCurrentSession(expectedSessionId, expectedAttachment, expectedPresentation) {
	const current = unwrapResult(await getJson("/ui/api/session/current"));
	if (requiredStringField(current, "session_id") !== expectedSessionId) throw new Error("browser session changed while reconciling a seat append");
	const attachments = current.binding_attachments;
	if (!Array.isArray(attachments)) throw new Error("browser session attachments changed while reconciling a seat append");
	const matches = attachments.filter((value) => isRecord(value) && value.binding_attachment_id === expectedAttachment.binding_attachment_id && exactIntegerField(value, "binding_generation") === BigInt(expectedAttachment.binding_generation) && value.binding_digest === expectedAttachment.binding_digest);
	if (matches.length !== 1) throw new Error("browser root attachment changed while reconciling a seat append");
	requireAttachmentSubject(matches[0]);
	const memberships = current.presentations;
	const matching = Array.isArray(memberships) ? memberships.filter((value) => isRecord(value) && value.presentation_id === expectedPresentation.presentation_id && value.thread_id === expectedPresentation.thread_id && value.chain_root_id === expectedPresentation.chain_root_id) : [];
	if (matching.length !== 1 || !isRecord(matching[0])) throw new Error("browser presentation membership changed while reconciling a seat append");
	requireRootBinding(matching[0].root_binding, expectedAttachment);
}
function isRetryableResponse(error) {
	return error instanceof UnknownDeliveryError || error instanceof HttpResponseError && error.status >= 500;
}
async function sha256Hex(encoded) {
	const bytes = new TextEncoder().encode(encoded);
	const digest = await crypto.subtle.digest("SHA-256", bytes);
	return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}
function retainAppend(key, retained) {
	localStorage.setItem(key, JSON.stringify(retained));
}
function clearRetainedAppend(key, operationId, producer, expectedEncodedRequest) {
	const retained = loadRetainedAppend(key);
	if (retained && retained.operation_id === operationId && retained.producer_incarnation === producer && (expectedEncodedRequest === void 0 || retained.encoded_request === expectedEncodedRequest)) localStorage.removeItem(key);
	else if (retained) throw new Error("pending seat append changed during acknowledgement reconciliation");
}
function loadRetainedAppend(key) {
	const encoded = localStorage.getItem(key);
	if (encoded === null) return null;
	let value;
	try {
		value = JSON.parse(encoded);
	} catch (error) {
		throw new Error(`retained seat append is malformed: ${errorMessage(error)}`);
	}
	if (!isRecord(value) || value.schema_version !== "ryeos.ui.seat.pending-append.v3" || typeof value.session_id !== "string" || typeof value.presentation_id !== "string" || typeof value.chain_root_id !== "string" || typeof value.thread_id !== "string" || typeof value.producer_incarnation !== "string" || typeof value.operation_id !== "string" || typeof value.first_engine_seq !== "string" || !/^(0|[1-9][0-9]*)$/.test(value.first_engine_seq) || typeof value.last_engine_seq !== "string" || !/^(0|[1-9][0-9]*)$/.test(value.last_engine_seq) || typeof value.event_count !== "number" || !Number.isSafeInteger(value.event_count) || value.event_count <= 0 || typeof value.payload_digest !== "string" || typeof value.encoded_request !== "string") throw new Error("retained seat append has an invalid schema");
	return value;
}
function requireAppendAcknowledgement(response, expected, stagedEvents, coordinate) {
	const acknowledgementFields = [
		"producer_incarnation",
		"operation_id",
		"first_engine_seq",
		"last_engine_seq",
		"event_count",
		"payload_digest",
		"appended",
		"chain_seq",
		"receipt_thread_seq",
		"receipt_event_hash",
		"events_authority",
		"events"
	];
	if (Object.keys(response).some((key) => !acknowledgementFields.includes(key)) || requiredStringField(response, "producer_incarnation") !== expected.producer || requiredStringField(response, "operation_id") !== expected.operationId || exactIntegerField(response, "first_engine_seq") !== expected.firstEngineSeq || exactIntegerField(response, "last_engine_seq") !== expected.lastEngineSeq || exactIntegerField(response, "event_count") !== BigInt(expected.eventCount) || exactIntegerField(response, "appended") !== BigInt(expected.eventCount) || requiredStringField(response, "payload_digest") !== expected.payloadDigest) throw new UnknownDeliveryError("seat append acknowledgement does not match the submitted operation");
	if (response.events_authority !== "authoritative" || !Array.isArray(response.events) || response.events.length !== expected.eventCount || stagedEvents.length !== expected.eventCount) throw new UnknownDeliveryError("seat append acknowledgement omitted its authoritative event interval");
	const receiptChainSeq = exactIntegerField(response, "chain_seq");
	const receiptThreadSeq = exactIntegerField(response, "receipt_thread_seq");
	const receiptHash = requiredStringField(response, "receipt_event_hash");
	if (!/^[0-9a-f]{64}$/.test(receiptHash)) throw new UnknownDeliveryError("seat append acknowledgement receipt hash is invalid");
	const firstChainSeq = receiptChainSeq - BigInt(expected.eventCount);
	if (firstChainSeq < 0n) throw new UnknownDeliveryError("seat append acknowledgement event interval underflows");
	let previous = null;
	const hashes = /* @__PURE__ */ new Set();
	for (let index = 0; index < stagedEvents.length; index += 1) {
		const acknowledged = response.events[index];
		const staged = stagedEvents[index];
		if (staged === void 0 || !isRecord(acknowledged) || Object.keys(acknowledged).some((key) => ![
			"event_hash",
			"chain_root_id",
			"chain_seq",
			"thread_id",
			"thread_seq",
			"event_type",
			"durability",
			"ts",
			"prev_chain_event_hash",
			"prev_thread_event_hash",
			"payload"
		].includes(key)) || typeof acknowledged.event_hash !== "string" || !/^[0-9a-f]{64}$/.test(acknowledged.event_hash) || hashes.has(acknowledged.event_hash) || acknowledged.chain_root_id !== coordinate.chain_root_id || acknowledged.thread_id !== coordinate.thread_id || exactIntegerField(acknowledged, "chain_seq") !== firstChainSeq + BigInt(index) || acknowledged.event_type !== staged.event_type || acknowledged.durability !== "durable" || typeof acknowledged.ts !== "string" || acknowledged.ts.length === 0 || !canonicalOptionalHash(acknowledged.prev_chain_event_hash) || !canonicalOptionalHash(acknowledged.prev_thread_event_hash) || !isRecord(acknowledged.payload) || exactIntegerField(acknowledged.payload, "seq") !== staged.engine_seq || encodeSeatPayloadDigest(acknowledged.payload.payload) !== encodeSeatPayloadDigest(staged.payload)) throw new UnknownDeliveryError("seat append acknowledgement changed its authoritative event interval");
		hashes.add(acknowledged.event_hash);
		const threadSeq = exactIntegerField(acknowledged, "thread_seq");
		if (previous !== null && (threadSeq !== exactIntegerField(previous, "thread_seq") + 1n || acknowledged.prev_chain_event_hash !== previous.event_hash || acknowledged.prev_thread_event_hash !== previous.event_hash)) throw new UnknownDeliveryError("seat append acknowledgement event links are not contiguous");
		previous = acknowledged;
	}
	if (previous === null || receiptThreadSeq !== exactIntegerField(previous, "thread_seq") + 1n) throw new UnknownDeliveryError("seat append acknowledgement receipt is not adjacent to its event interval");
	return {
		event_hash: receiptHash,
		thread_seq: receiptThreadSeq,
		events: response.events
	};
}
function retainedAppendEvents(retained, expected, coordinate) {
	const request = decodeJsonBody(retained.encoded_request);
	if (!isRecord(request) || Object.keys(request).some((key) => ![
		"presentation_id",
		"thread_id",
		"producer_incarnation",
		"operation_id",
		"first_engine_seq",
		"last_engine_seq",
		"event_count",
		"payload_digest",
		"events"
	].includes(key)) || request.presentation_id !== coordinate.presentation_id || request.thread_id !== coordinate.thread_id || request.producer_incarnation !== expected.producer || request.operation_id !== expected.operationId || exactIntegerField(request, "first_engine_seq") !== expected.firstEngineSeq || exactIntegerField(request, "last_engine_seq") !== expected.lastEngineSeq || exactIntegerField(request, "event_count") !== BigInt(expected.eventCount) || request.payload_digest !== expected.payloadDigest || !Array.isArray(request.events) || request.events.length !== expected.eventCount) throw new Error("retained append request differs from its exact staged interval");
	return request.events.map((event, index) => {
		if (!isRecord(event) || exactIntegerField(event, "engine_seq") !== expected.firstEngineSeq + BigInt(index) || typeof event.event_type !== "string" || event.event_type.length === 0 || !("payload" in event)) throw new Error("retained append contains an invalid staged event interval");
		return {
			engine_seq: exactIntegerField(event, "engine_seq"),
			event_type: event.event_type,
			payload: event.payload
		};
	});
}
function canonicalOptionalHash(value) {
	return value === null || typeof value === "string" && /^[0-9a-f]{64}$/.test(value);
}
function unwrapResult(value) {
	if (!isRecord(value)) throw new Error("seat service returned a non-object response");
	const first = isRecord(value.result) ? value.result : value;
	return isRecord(first.result) ? first.result : first;
}
function stringField(value, name) {
	return typeof value[name] === "string" ? value[name] : null;
}
function requiredStringField(value, name) {
	const field = stringField(value, name);
	if (field === null || field.length === 0) throw new Error(`seat service returned no ${name}`);
	return field;
}
function exactU64Field(value, name) {
	const integer = exactIntegerField(value, name);
	if (integer > MAX_U64) throw new Error(`seat service returned an out-of-range u64 for ${name}`);
	return integer;
}
function exactIntegerField(value, name) {
	const field = value[name];
	if (field === null || field === void 0) throw new Error(`seat service returned no ${name}`);
	return parseExactInteger(field, name);
}
function parseExactInteger(value, name) {
	if (typeof value === "bigint" && value >= 0n) return value;
	if (typeof value === "number" && Number.isSafeInteger(value) && value >= 0) return BigInt(value);
	if (typeof value === "string" && /^(0|[1-9][0-9]*)$/.test(value)) return BigInt(value);
	throw new Error(`seat service returned an invalid exact integer for ${name}`);
}
function eventType(event) {
	return event.event_type;
}
function eventPayload(event) {
	return event.payload;
}
function wireSeatEvents(events, firstEngineSeq) {
	return events.map((event, offset) => {
		const engineSeq = firstEngineSeq + BigInt(offset);
		if (engineSeq < 0n || engineSeq >= MAX_U64) throw new Error("producer engine interval is exhausted");
		const payload = eventPayload(event);
		if (event.producer_engine_seq !== void 0 && event.producer_engine_seq !== null && parseExactInteger(event.producer_engine_seq, "producer_engine_seq") !== engineSeq) throw new Error("captured event cannot be rebased to another producer interval");
		if (eventType(event) === "seat.presentation_operation.prepared" && (!isRecord(payload) || payload.schema !== 2 && payload.schema !== 2n || exactIntegerField(payload, "prepared_engine_seq") !== engineSeq)) throw new Error("prepared operation differs from its reserved producer position");
		return {
			engine_seq: engineSeq,
			event_type: eventType(event),
			payload
		};
	});
}
function encodedSeatEventBytes(event) {
	return encodedByteLength(encodeJsonBody({
		engine_seq: event.seq,
		event_type: eventType(event),
		payload: eventPayload(event)
	}));
}
function encodedByteLength(encoded) {
	return new TextEncoder().encode(encoded).byteLength;
}
function isRecord(value) {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}
function controlPredecessor(value) {
	if (!isRecord(value) || value.kind !== "initialization" && value.kind !== "control" || Object.keys(value).some((key) => key !== "kind" && key !== "event_hash") || typeof value.event_hash !== "string" || !/^[0-9a-f]{64}$/.test(value.event_hash)) throw new Error("producer observation omitted its exact control predecessor");
	return {
		kind: value.kind,
		event_hash: value.event_hash
	};
}
//#endregion
//#region browser/runtime/reconnect-controls.ts
/** Mechanical choices from the authenticated owner; never diagnostic-text inference. */
function replacementChoices(observation) {
	const producer = observation.producer;
	if (producer.kind === "relinquished") return [{
		kind: "resume",
		expected_producer_incarnation: producer.producer_incarnation,
		predecessor: { ...observation.control_predecessor }
	}, {
		kind: "takeover",
		expected_producer_incarnation: producer.producer_incarnation,
		predecessor: { ...observation.control_predecessor }
	}];
	if (producer.kind === "active") return [{
		kind: "takeover",
		expected_producer_incarnation: producer.producer_incarnation,
		predecessor: { ...observation.control_predecessor }
	}];
	return [];
}
/**
* Reconcile the retained unknown append before observing its owner. This does
* not await newly queued suffix writes: observation is read-only and may
* coexist with sync. An explicit reconnect still refuses until that queue
* settles; these choices are not evidence that replacement is already safe.
*/
async function observeReconnectChoices(runtime) {
	await runtime.reconcilePendingAppend();
	return runtime.observeProducer();
}
/** One failed bootstrap's mechanical decision frame, disposed with its document. */
function awaitBootRecovery(root, runtime, failure, signal, options = {}) {
	return new Promise((resolve, reject) => {
		let settled = false;
		let busy = false;
		let inFlightAction = null;
		const listeners = [];
		const main = document.createElement("main");
		main.className = "ryeos-boot ryeos-boot-failed";
		main.setAttribute("role", "alert");
		const title = document.createElement("h1");
		title.textContent = "Presentation needs your decision";
		const diagnostic = document.createElement("pre");
		diagnostic.className = "ryeos-boot-error-detail";
		diagnostic.textContent = failure instanceof Error ? failure.message : String(failure);
		const guidance = document.createElement("p");
		guidance.textContent = options.allowExactRetry === false ? "This live presentation is fenced. Reconcile retained receipts, inspect the writer, then explicitly choose a replacement. No writer death is inferred." : "Retry keeps this document’s exact request. Reconcile checks retained receipts before showing explicit writer choices.";
		main.append(title, diagnostic, guidance);
		const cleanup = () => {
			listeners.splice(0).forEach((remove) => remove());
			signal?.removeEventListener("abort", abort);
		};
		const finish = (next) => {
			if (settled) {
				if (next !== runtime) next.close();
				return;
			}
			settled = true;
			cleanup();
			resolve(next);
		};
		const abort = () => {
			if (settled) return;
			settled = true;
			cleanup();
			const reason = signal?.reason ?? /* @__PURE__ */ new Error("document disposed");
			(inFlightAction ?? Promise.resolve()).then(() => reject(reason), () => reject(reason));
		};
		const button = (label, action) => {
			const control = document.createElement("button");
			control.type = "button";
			control.className = "ryeos-boot-retry";
			control.textContent = label;
			const click = () => {
				if (busy || settled) return;
				busy = true;
				const running = Promise.resolve().then(() => {
					if (settled) return;
					return action();
				}).catch((error) => {
					if (!settled) diagnostic.textContent = error instanceof Error ? error.message : String(error);
				}).finally(() => {
					busy = false;
				});
				inFlightAction = running;
				running.then(() => {
					if (inFlightAction === running) inFlightAction = null;
				}, () => {
					if (inFlightAction === running) inFlightAction = null;
				});
				return running;
			};
			control.addEventListener("click", click);
			listeners.push(() => control.removeEventListener("click", click));
			main.append(control);
			return control;
		};
		if (options.allowExactRetry !== false) button("Retry exact request", async () => {
			finish(runtime);
		});
		if (runtime.preparedPresentation()) {
			button("Reconcile and inspect writer", async () => {
				main.querySelectorAll("[data-replacement-choice]").forEach((node) => node.remove());
				const observation = await observeReconnectChoices(runtime);
				if (settled) return;
				guidance.textContent = `Observed writer: ${observation.producer.kind}. Replacement requires an explicit choice; the node checks the predecessor again.`;
				for (const choice of replacementChoices(observation)) {
					const control = button(choice.kind === "resume" ? "Resume relinquished presentation" : "Take over this presentation", async () => {
						finish(await runtime.reconnect(choice));
					});
					control.dataset.replacementChoice = "true";
				}
			});
			button("Open independent presentation", async () => {
				if (!window.confirm("Open a distinct presentation? This does not resume or take over the existing writer. Unresolved local writes still must reconcile.")) return;
				finish(await runtime.reconnect({ kind: "independent" }));
			});
		}
		root.replaceChildren(main);
		signal?.addEventListener("abort", abort, { once: true });
		if (signal?.aborted) abort();
	});
}
//#endregion
//#region browser/runtime/wasm-api.ts
async function loadWasm() {
	const specifier = "/ui/assets/ryeos_web.js";
	const module = await __vitePreload(() => import(
		/* @vite-ignore */
		specifier
), []);
	if (!isWasmApi(module)) throw new Error("installed RyeOS WASM module has an invalid export surface");
	return module;
}
function isWasmApi(value) {
	if (typeof value !== "object" || value === null) return false;
	const exports = value;
	return [
		"default",
		"ryeos_start",
		"ryeos_dispatch",
		"ryeos_apply_effect_result",
		"ryeos_key",
		"ryeos_seat_events",
		"ryeos_seat_append_identity",
		"ryeos_seat_control_identity",
		"ryeos_replay_acknowledged_seat_events",
		"ryeos_begin_seat_replay",
		"ryeos_stage_seat_replay_events",
		"ryeos_stage_seat_transfer_proof",
		"ryeos_stage_seat_move_freeze_cancellation_proof",
		"ryeos_commit_seat_replay",
		"ryeos_abort_seat_replay",
		"ryeos_require_presentation_producer_ready",
		"ryeos_begin_seat_reproof",
		"ryeos_commit_seat_reproof",
		"ryeos_acknowledged_seat_prefix_len",
		"ryeos_layout_preference_key",
		"ryeos_export_layout_preferences",
		"ryeos_restore_layout_preferences",
		"ryeos_export_active_view_set_template",
		"ryeos_open_saved_view_set_template"
	].every((name) => typeof exports[name] === "function");
}
//#endregion
//#region browser/runtime/presentation-document.js
function preparedDocumentUrl(current, presentationId) {
	if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(presentationId)) throw new Error("prepared presentation identity is invalid");
	const next = new URL("/ui/", current);
	next.searchParams.set("presentation_id", presentationId);
	return next;
}
/** One platform-effect frame. Retry repeats the exact supplied request.
* Dismissal removes framing only, never the retained seat or its obligations. */
function createPresentationDocumentHandoff(document, parent, current, prepare, onDismiss = () => {}) {
	const frame = document.createElement("section");
	frame.className = "ryeos-document-handoff";
	frame.setAttribute("role", "status");
	frame.setAttribute("aria-label", "Prepared presentation");
	const status = document.createElement("p");
	const link = document.createElement("a");
	link.textContent = "Open Surface";
	link.target = "_blank";
	link.rel = "noopener noreferrer";
	link.hidden = true;
	const retry = document.createElement("button");
	retry.type = "button";
	retry.textContent = "Retry exact preparation";
	retry.hidden = true;
	const dismiss = document.createElement("button");
	dismiss.type = "button";
	dismiss.textContent = "Dismiss link";
	frame.append(status, link, retry, dismiss);
	parent.append(frame);
	let disposed = false;
	let inFlight = null;
	function start() {
		if (disposed || inFlight) return inFlight ?? Promise.resolve();
		retry.hidden = true;
		status.textContent = "Preparing an independent presentation. No producer is claimed.";
		const running = Promise.resolve().then(prepare).then((presentationId) => {
			const url = preparedDocumentUrl(current, presentationId);
			if (disposed) return;
			link.href = url.href;
			link.hidden = false;
			status.textContent = "Ready. Open the prepared Surface when you choose. The link remains available if opening is blocked.";
		}).catch((error) => {
			if (disposed) return;
			status.textContent = `Preparation is not confirmed: ${error instanceof Error ? error.message : String(error)}. Retry reconciles the same request.`;
			retry.hidden = false;
		});
		inFlight = running;
		running.finally(() => {
			if (inFlight === running) inFlight = null;
		});
		return running;
	}
	const retryClick = () => {
		start();
	};
	const dismissClick = () => {
		dispose();
		onDismiss();
	};
	retry.addEventListener("click", retryClick);
	dismiss.addEventListener("click", dismissClick);
	function dispose() {
		if (disposed) return;
		disposed = true;
		retry.removeEventListener("click", retryClick);
		dismiss.removeEventListener("click", dismissClick);
		frame.remove();
	}
	return {
		ready: start(),
		dispose,
		settle: () => inFlight ?? Promise.resolve()
	};
}
//#endregion
//#region browser/runtime/boot.ts
/** Enter the installed browser surface from the generated index document. */
async function bootRyeOsDocument(signal, predecessorSettlement) {
	const root = document.getElementById("app");
	if (!root) throw new Error("RyeOS browser root #app is missing");
	setBootStage(predecessorSettlement ? "Settling previous presentation" : "Joining node", predecessorSettlement ? "Waiting for the previous document to release its runtime before attaching." : "Opening the authenticated browser session and durable seat.");
	try {
		await predecessorSettlement;
		signal?.throwIfAborted();
		setBootStage("Joining node", "Opening the authenticated browser session and durable seat.");
		return await bootRyeOs(root, signal);
	} catch (error) {
		if (!signal?.aborted) renderBootFailure(root, error);
		throw error;
	}
}
async function bootRyeOs(root, signal) {
	signal?.throwIfAborted();
	const wasm = await loadWasm();
	signal?.throwIfAborted();
	await wasm.default({ module_or_path: "/ui/assets/ryeos_web_bg.wasm" });
	signal?.throwIfAborted();
	const sessionUnknown = await getJson("/ui/api/session/current");
	signal?.throwIfAborted();
	const selection = preparedPresentationSelection(sessionUnknown, new URL(location.href));
	const rootAttachment = selection?.rootAttachment ?? sessionRootAttachment(sessionUnknown);
	let coreStarted = false;
	let replaySessionIdentity = null;
	let runtime = null;
	let sessionRuntime = null;
	let preferences = null;
	let renderer = null;
	let detachBrowserEvents = null;
	let closePromise = null;
	let closed = false;
	const pendingDocuments = /* @__PURE__ */ new Set();
	const documentSettlements = /* @__PURE__ */ new Set();
	let recoveryTask = null;
	const recoveryAbort = new AbortController();
	let seatAttached = false;
	const queuedEvents = [];
	let releaseEffects = () => {};
	let documentEpoch = 0;
	let recovering = false;
	let effectsReady = new Promise((resolve) => {
		releaseEffects = resolve;
	});
	const dispatchEvent = (event) => {
		if (closed) return;
		if (recovering) return;
		if (!seatAttached) {
			queuedEvents.push(event);
			return;
		}
		runtime?.enqueueEvent(event);
	};
	const dispatchUi = (event) => {
		if (!sessionRuntime?.canAcceptMutations()) return;
		dispatchEvent({
			type: "ui",
			event
		});
	};
	const abortBoot = () => {
		close().catch((error) => console.error("RyeOS boot cleanup failed", error));
	};
	signal?.addEventListener("abort", abortBoot, { once: true });
	function disposePendingDocuments() {
		for (const frame of pendingDocuments) {
			frame.dispose();
			const settlement = frame.settle();
			documentSettlements.add(settlement);
			settlement.finally(() => documentSettlements.delete(settlement));
		}
		pendingDocuments.clear();
	}
	function close() {
		if (closePromise) return closePromise;
		closed = true;
		disposePendingDocuments();
		recoveryAbort.abort(/* @__PURE__ */ new Error("document disposed during presentation recovery"));
		documentEpoch += 1;
		signal?.removeEventListener("abort", abortBoot);
		detachBrowserEvents?.();
		queuedEvents.length = 0;
		releaseEffects(false);
		runtime?.close();
		const closingSession = sessionRuntime;
		closingSession?.close();
		wasm.ryeos_abort_seat_replay();
		const cleanup = (async () => {
			try {
				preferences?.flush();
			} finally {
				await renderer?.destroy();
			}
		})();
		closePromise = (async () => {
			const outcomes = await Promise.allSettled([
				cleanup,
				recoveryTask ?? Promise.resolve(),
				closingSession?.settle() ?? Promise.resolve(),
				...documentSettlements
			]);
			for (const outcome of outcomes) if (outcome.status === "rejected") throw outcome.reason;
		})();
		return closePromise;
	}
	async function admitCurrent() {
		if (!sessionRuntime) throw new Error("presentation transport is unavailable");
		if (closed) throw new Error("document disposed during presentation admission");
		signal?.throwIfAborted();
		await sessionRuntime.attachSeat();
		signal?.throwIfAborted();
		const presentation = sessionRuntime.currentPresentation();
		if (!presentation) throw new Error("presentation claim did not acknowledge a current producer");
		const admittedSession = await getJson("/ui/api/session/current");
		signal?.throwIfAborted();
		if (closed) throw new Error("document disposed during presentation admission");
		if (replaySessionIdentity !== encodeCanonicalJsonBody(admittedSession)) throw new Error("authenticated session changed after candidate history reconstruction");
		const currentProducer = sessionRuntime.currentProducer();
		if (!currentProducer || currentProducer.presentation_id !== presentation.presentation_id || currentProducer.thread_id !== presentation.thread_id) throw new Error("presentation producer became unavailable during core bootstrap");
		if (sessionId(admittedSession) !== sessionId(sessionUnknown)) throw new Error("authenticated session changed during presentation admission");
		const membershipUrl = new URL(location.href);
		membershipUrl.searchParams.set("presentation_id", presentation.presentation_id);
		const admitted = preparedPresentationSelection(admittedSession, membershipUrl);
		if (!admitted || admitted.presentation.thread_id !== presentation.thread_id || admitted.presentation.chain_root_id !== presentation.chain_root_id || admitted.rootAttachment.binding_attachment_id !== rootAttachment.binding_attachment_id || admitted.rootAttachment.binding_generation !== rootAttachment.binding_generation || admitted.rootAttachment.binding_digest !== rootAttachment.binding_digest) throw new Error("acknowledged presentation membership changed before core construction");
		return {
			admittedSession,
			presentation
		};
	}
	function installCore(admittedPresentation) {
		const activeSession = sessionRuntime;
		if (!activeSession) throw new Error("presentation transport is unavailable");
		disposePendingDocuments();
		const installedEpoch = ++documentEpoch;
		effectsReady = new Promise((resolve) => {
			releaseEffects = resolve;
		});
		const completion = activeSession.currentReplayCompletion();
		if (!completion) throw new Error("presentation replay has no authenticated completion fence");
		const initial = wasm.ryeos_commit_seat_replay(completion);
		coreStarted = true;
		renderer = mountRyeOsRenderer(root, initial, dispatchUi);
		runtime = createCommitRuntime({
			reduce: (event) => wasm.ryeos_dispatch(event),
			applyEffectResult: (result) => wasm.ryeos_apply_effect_result(result),
			async startEffect(effect) {
				if (effect.kind.type === "show_prepared_presentation_document") {
					if (!await effectsReady || closed || installedEpoch !== documentEpoch) throw new Error("presentation document owner is obsolete");
					const presentationId = effect.kind.presentation_id;
					const frame = createPresentationDocumentHandoff(document, document.body, new URL(location.href), async () => presentationId, () => pendingDocuments.delete(frame));
					pendingDocuments.add(frame);
					await frame.ready;
					return {
						id: effect.id,
						kind: "browser_only",
						ok: true,
						data: null,
						error: null
					};
				}
				if (effect.kind.type === "open_new_presentation_document") {
					if (closed || installedEpoch !== documentEpoch) throw new Error("presentation document owner is obsolete");
					const kind = effect.kind;
					const encoded = encodeCanonicalJsonBody({
						action: "prepare",
						creation_request_id: crypto.randomUUID(),
						...kind.root_binding
					});
					const frame = createPresentationDocumentHandoff(document, document.body, new URL(location.href), async () => {
						if (!await effectsReady || closed || installedEpoch !== documentEpoch) throw new Error("presentation document owner was disposed");
						const root = verifiedDocumentRoot(await getJson("/ui/api/session/current"), kind.expected_session_id, kind.root_binding);
						if (closed || installedEpoch !== documentEpoch) throw new Error("presentation document owner was disposed");
						const response = sessionRecord(await postEncodedJson("/ui/api/session/seat/open", encoded));
						const prepared = sessionRecord(response.result ?? response);
						const actual = sessionRecord(prepared.root_binding);
						if (actual.binding_attachment_id !== root.binding_attachment_id || actual.binding_generation !== root.binding_generation || actual.binding_digest !== root.binding_digest) throw new Error("prepared document root changed");
						const id = requiredNonEmptyString(prepared, "presentation_id");
						requiredNonEmptyString(prepared, "thread_id");
						requiredNonEmptyString(prepared, "chain_root_id");
						return id;
					}, () => {
						pendingDocuments.delete(frame);
						const settling = frame.settle();
						documentSettlements.add(settling);
						settling.finally(() => documentSettlements.delete(settling));
					});
					pendingDocuments.add(frame);
					await frame.ready;
					return {
						id: effect.id,
						kind: "browser_only",
						ok: true,
						data: null,
						error: null
					};
				}
				if (!await effectsReady || closed || installedEpoch !== documentEpoch) throw new Error("presentation bootstrap did not admit platform effects");
				return runEffect(effect, sessionRuntime?.currentProducer() ?? null);
			},
			failedEffectResult,
			async render(envelope) {
				if (closed || installedEpoch !== documentEpoch) return;
				const browserState = captureBrowserPresentation(root);
				renderer?.replaceEnvelope(envelope);
				await Promise.resolve();
				if (closed || installedEpoch !== documentEpoch) return;
				restoreBrowserPresentation(root, browserState);
				preferences?.observeAcceptedState();
				sessionRuntime?.observe(envelope.view_model);
			}
		});
		runtime.enqueueEnvelope(initial);
		activeSession.completeEngineBootstrap();
		const readyProducer = activeSession.currentProducer();
		const nextEngineSeq = activeSession.currentEngineFrontier();
		if (!readyProducer || nextEngineSeq === null) throw new Error("presentation producer became unavailable before initial effects");
		runtime.enqueueEvent({
			type: "presentation_producer_ready",
			producer: readyProducer,
			next_engine_seq: nextEngineSeq
		});
		wasm.ryeos_require_presentation_producer_ready(readyProducer, nextEngineSeq);
		seatAttached = true;
		releaseEffects(true);
		for (const event of queuedEvents.splice(0)) runtime.enqueueEvent(event);
		configurePreferences(wasm, (next) => {
			preferences = next;
		});
		if (location.hash && activeSession.canAcceptMutations()) runtime.enqueueEvent({
			type: "route_changed",
			route: location.hash.replace(/^#/, "")
		});
		detachBrowserEvents = attachBrowserEvents(wasm, runtime, () => sessionRuntime?.canAcceptMutations() ?? false);
		history.replaceState(history.state, "", retainedPresentationUrl(new URL(location.href), admittedPresentation.presentation_id).href);
	}
	async function disposeInstalledCore() {
		detachBrowserEvents?.();
		detachBrowserEvents = null;
		runtime?.close();
		runtime = null;
		releaseEffects(false);
		try {
			preferences?.flush();
		} finally {
			preferences = null;
			await renderer?.destroy();
			renderer = null;
		}
		coreStarted = false;
	}
	async function recoverLive(failure) {
		if (closed || recovering || !seatAttached || !sessionRuntime) return;
		recovering = true;
		seatAttached = false;
		disposePendingDocuments();
		documentEpoch += 1;
		detachBrowserEvents?.();
		detachBrowserEvents = null;
		runtime?.close();
		runtime = null;
		releaseEffects(false);
		try {
			preferences?.flush();
			preferences = null;
			await renderer?.destroy();
			renderer = null;
			if (closed) return;
			const previous = sessionRuntime;
			const replacement = await awaitBootRecovery(root, previous, failure, recoveryAbort.signal, { allowExactRetry: false });
			if (closed || signal?.aborted) {
				if (replacement !== previous) replacement.close();
				return;
			}
			previous.close();
			sessionRuntime = replacement;
			coreStarted = false;
			wasm.ryeos_abort_seat_replay();
			replaySessionIdentity = null;
			queuedEvents.length = 0;
			for (;;) {
				let attemptedInstall = false;
				try {
					const admitted = await admitCurrent();
					if (closed) return;
					attemptedInstall = true;
					installCore(admitted.presentation);
					break;
				} catch (error) {
					signal?.throwIfAborted();
					if (closed) throw error;
					seatAttached = false;
					disposePendingDocuments();
					documentEpoch += 1;
					await disposeInstalledCore();
					const current = sessionRuntime;
					if (!current) throw new Error("presentation recovery lost its exact session owner");
					const replacement = await awaitBootRecovery(root, current, error, recoveryAbort.signal, { allowExactRetry: !attemptedInstall });
					if (closed || signal?.aborted) {
						if (replacement !== current) replacement.close();
						return;
					}
					sessionRuntime = replacement;
					if (sessionRuntime !== current) {
						current.close();
						wasm.ryeos_abort_seat_replay();
						replaySessionIdentity = null;
						queuedEvents.length = 0;
					}
				}
			}
		} catch (error) {
			close().catch((cleanupError) => console.error("RyeOS recovery cleanup failed", cleanupError));
			if (!closed && !signal?.aborted) renderBootFailure(root, error);
		} finally {
			recovering = false;
		}
	}
	async function reproofSession(presentation) {
		const admittedSession = await getJson("/ui/api/session/current");
		if (closed || signal?.aborted || sessionId(admittedSession) !== sessionId(sessionUnknown)) throw new Error("authenticated session changed during paired reproof");
		const membershipUrl = new URL(location.href);
		membershipUrl.searchParams.set("presentation_id", presentation.presentation_id);
		const admitted = preparedPresentationSelection(admittedSession, membershipUrl);
		if (!admitted || admitted.presentation.thread_id !== presentation.thread_id || admitted.presentation.chain_root_id !== presentation.chain_root_id || admitted.rootAttachment.binding_attachment_id !== rootAttachment.binding_attachment_id || admitted.rootAttachment.binding_generation !== rootAttachment.binding_generation || admitted.rootAttachment.binding_digest !== rootAttachment.binding_digest) throw new Error("paired reproof has no exact current presentation membership");
		return admittedSession;
	}
	try {
		sessionRuntime = createSessionRuntime({
			sessionId: sessionId(sessionUnknown),
			eventsUrl: sessionEventsUrl(sessionUnknown),
			rootAttachment,
			maxSeatAppendBytes: sessionSeatAppendBound(sessionUnknown, rootAttachment.binding_attachment_id),
			onUnavailable: (failure) => {
				if (closed || recoveryTask) return;
				const task = recoverLive(failure.error);
				recoveryTask = task;
				task.then(() => {
					if (recoveryTask === task) recoveryTask = null;
				}, () => {
					if (recoveryTask === task) recoveryTask = null;
				});
			},
			...selection ? { presentation: selection.presentation } : {},
			seatEvents: () => coreStarted ? wasm.ryeos_seat_events() : [],
			deriveSeatAppendIdentity: (identity) => wasm.ryeos_seat_append_identity(identity),
			deriveSeatControlIdentity: (request) => wasm.ryeos_seat_control_identity(request),
			commitEvent: dispatchEvent,
			replayAcknowledgedSeatEvents: (producer, captured, events) => {
				if (!coreStarted || !runtime || closed) throw new Error("acknowledged seat evidence has no live Core owner");
				runtime.enqueueEnvelope(wasm.ryeos_replay_acknowledged_seat_events(producer, captured, events));
			},
			async beginSeatReplay(presentation) {
				const admittedSession = await getJson("/ui/api/session/current");
				if (closed || signal?.aborted || sessionId(admittedSession) !== sessionId(sessionUnknown)) throw new Error("authenticated session changed before seat history");
				const membershipUrl = new URL(location.href);
				membershipUrl.searchParams.set("presentation_id", presentation.presentation_id);
				const admitted = preparedPresentationSelection(admittedSession, membershipUrl);
				if (!admitted || admitted.presentation.thread_id !== presentation.thread_id || admitted.presentation.chain_root_id !== presentation.chain_root_id || admitted.rootAttachment.binding_attachment_id !== rootAttachment.binding_attachment_id || admitted.rootAttachment.binding_generation !== rootAttachment.binding_generation || admitted.rootAttachment.binding_digest !== rootAttachment.binding_digest) throw new Error("seat history has no exact authenticated presentation membership");
				wasm.ryeos_begin_seat_replay(admittedSession, presentation.presentation_id, viewport(), BigInt(Date.now()));
				replaySessionIdentity = encodeCanonicalJsonBody(admittedSession);
			},
			async beginSeatReproof(presentation, requireOwner) {
				if (!coreStarted || !runtime || closed || recovering) throw new Error("paired reproof has no live Core owner");
				const admittedSession = await reproofSession(presentation);
				requireOwner();
				if (closed || recovering) throw new Error("paired reproof owner changed before staging");
				wasm.ryeos_begin_seat_reproof(admittedSession, presentation.presentation_id, viewport(), BigInt(Date.now()));
			},
			async completeSeatReproof(presentation, completion, requireOwner) {
				await reproofSession(presentation);
				requireOwner();
				if (!coreStarted || !runtime || closed || recovering) throw new Error("paired reproof owner changed before promotion");
				const envelope = wasm.ryeos_commit_seat_reproof(completion);
				const acknowledgedPrefix = wasm.ryeos_acknowledged_seat_prefix_len();
				if (acknowledgedPrefix < 0n || acknowledgedPrefix > BigInt(Number.MAX_SAFE_INTEGER)) throw new Error("paired reproof acknowledged prefix cannot be represented exactly");
				return {
					acknowledgedEventCount: Number(acknowledgedPrefix),
					publish: () => {
						requireOwner();
						if (!runtime || closed || recovering) throw new Error("paired reproof document changed before rendering");
						runtime.enqueueEnvelope(envelope);
					}
				};
			},
			replaySeatEvents: (events) => wasm.ryeos_stage_seat_replay_events(events),
			replaySeatTransferProof: (proof) => wasm.ryeos_stage_seat_transfer_proof(proof),
			replaySeatMoveFreezeCancellationProof: (proof) => wasm.ryeos_stage_seat_move_freeze_cancellation_proof(proof),
			abortSeatReplay: () => wasm.ryeos_abort_seat_replay()
		});
		let admittedPresentation;
		for (;;) try {
			admittedPresentation = (await admitCurrent()).presentation;
			break;
		} catch (error) {
			signal?.throwIfAborted();
			if (closed) throw error;
			const previous = sessionRuntime;
			if (!previous) throw new Error("presentation admission lost its exact session owner");
			sessionRuntime = await awaitBootRecovery(root, previous, error, signal);
			if (sessionRuntime !== previous) {
				previous.close();
				wasm.ryeos_abort_seat_replay();
				replaySessionIdentity = null;
				queuedEvents.length = 0;
			}
			signal?.throwIfAborted();
			setBootStage("Joining node", "Claiming the selected presentation and replaying its retained history.");
		}
		signal?.throwIfAborted();
		installCore(admittedPresentation);
		return { close };
	} catch (error) {
		try {
			await close();
		} catch (cleanupError) {
			console.error("RyeOS boot cleanup failed", cleanupError);
		}
		throw error;
	}
}
function retainedPresentationUrl(current, presentationId) {
	if (!presentationId || presentationId.trim() !== presentationId) throw new Error("installed presentation identity is invalid");
	const next = preparedDocumentUrl(current, presentationId);
	next.hash = current.hash;
	return next;
}
function verifiedDocumentRoot(session, expectedSessionId, expected) {
	if (sessionId(session) !== expectedSessionId) throw new Error("fresh presentation belongs to another authenticated session");
	const actual = sessionAttachment(sessionRecord(session), expected.binding_attachment_id);
	if (actual.binding_generation !== expected.binding_generation || actual.binding_digest !== expected.binding_digest) throw new Error("fresh presentation root attachment changed");
	return actual;
}
/** Read only the selected retained signed root's projected append route bound. */
function sessionSeatAppendBound(session, attachmentId) {
	const record = sessionRecord(session);
	sessionAttachment(record, attachmentId);
	const bound = sessionRecord(record.binding_attachments.find((entry) => entry.binding_attachment_id === attachmentId).binding_request_bounds).max_seat_append_bytes;
	if (typeof bound !== "bigint" || bound < 1n || bound > BigInt(Number.MAX_SAFE_INTEGER)) throw new Error("authenticated browser session signed seat append byte bound is invalid");
	return bound;
}
/** Resolve the immutable authored launch attachment without treating array order as authority. */
function sessionRootAttachment(session) {
	const record = sessionRecord(session);
	return sessionAttachment(record, requiredNonEmptyString(record, "root_attachment_id"));
}
/**
* Prepared links select retained membership under the existing cookie. They
* contain no bearer grant and never request resume/takeover automatically.
* Copying a live presentation link therefore reaches the owner's fresh-claim
* conflict instead of rotating its producer.
*/
function preparedPresentationSelection(session, url) {
	const selections = url.searchParams.getAll("presentation_id");
	if (selections.length === 0) return null;
	const selected = selections[0];
	if (selections.length !== 1 || selected === void 0 || selected.length === 0) throw new Error("prepared presentation selection is ambiguous or empty");
	const record = sessionRecord(session);
	if (!Array.isArray(record.presentations)) throw new Error("authenticated presentation memberships are invalid");
	const matches = record.presentations.filter((membership) => typeof membership === "object" && membership !== null && !Array.isArray(membership) && membership.presentation_id === selected);
	if (matches.length !== 1) throw new Error("prepared presentation is not an exact authenticated membership");
	const membership = matches[0];
	const coordinate = sessionRecord(membership.root_binding);
	const rootAttachment = sessionAttachment(record, requiredNonEmptyString(coordinate, "binding_attachment_id"));
	if (coordinate.binding_generation !== rootAttachment.binding_generation || coordinate.binding_digest !== rootAttachment.binding_digest) throw new Error("prepared presentation root binding is stale or invalid");
	return {
		rootAttachment,
		presentation: {
			presentation_id: requiredNonEmptyString(membership, "presentation_id"),
			thread_id: requiredNonEmptyString(membership, "thread_id"),
			chain_root_id: requiredNonEmptyString(membership, "chain_root_id")
		}
	};
}
function sessionAttachment(record, attachmentId) {
	if (!Array.isArray(record.binding_attachments)) throw new Error("authenticated browser session binding_attachments is invalid");
	const matches = record.binding_attachments.filter((attachment) => typeof attachment === "object" && attachment !== null && !Array.isArray(attachment) && attachment.binding_attachment_id === attachmentId);
	if (matches.length !== 1) throw new Error("authenticated browser session does not contain exactly one root attachment");
	const attachment = matches[0];
	requireAttachmentSubject(attachment);
	const generation = attachment.binding_generation;
	if (typeof generation !== "bigint" || generation < 1n || generation > 18446744073709551615n) throw new Error("authenticated browser session root attachment generation is invalid");
	return {
		binding_attachment_id: requiredNonEmptyString(attachment, "binding_attachment_id"),
		binding_generation: generation,
		binding_digest: requiredNonEmptyString(attachment, "binding_digest")
	};
}
function sessionRecord(session) {
	if (typeof session !== "object" || session === null || Array.isArray(session)) throw new Error("authenticated browser session is not an object");
	return session;
}
function requiredNonEmptyString(record, field) {
	const value = record[field];
	if (typeof value !== "string" || value.length === 0) throw new Error(`authenticated browser session ${field} is invalid`);
	return value;
}
function sessionEventsUrl(session) {
	if (typeof session !== "object" || session === null || Array.isArray(session)) throw new Error("authenticated browser session is not an object");
	const value = session.events_url;
	if (value === void 0 || value === null) return null;
	if (typeof value !== "string") throw new Error("authenticated browser session events_url is invalid");
	return value;
}
function sessionId(session) {
	const value = sessionRecord(session).session_id;
	if (typeof value !== "string" || value.length === 0) throw new Error("authenticated browser session has no session_id");
	return value;
}
function configurePreferences(wasm, assign) {
	try {
		const reportError = (error) => console.warn("RyeOS arrangement cache was not saved", error);
		const preferences = createScopedLayoutPreferencePersistence({
			readKey: wasm.ryeos_layout_preference_key,
			readCurrent: wasm.ryeos_export_layout_preferences,
			storage: localStorage,
			reportError
		});
		assign(preferences);
		preferences.observeAcceptedState();
	} catch (error) {
		console.warn("RyeOS layout preference storage is unavailable", error);
		assign(null);
	}
}
function attachBrowserEvents(wasm, runtime, canAcceptMutations) {
	const abort = new AbortController();
	const options = { signal: abort.signal };
	window.addEventListener("keydown", (event) => {
		if (!canAcceptMutations()) return;
		if (event.defaultPrevented) return;
		if (event.isComposing) return;
		if (isShellChromeEscape(event)) {
			event.preventDefault();
			return;
		}
		if (isTypingEvent(event) && !isLauncherChord(event)) return;
		const key = keyEvent(event);
		if (!key) return;
		if ((event.key === "Enter" || event.key === " ") && !hasModifiers(key) && isNativeActivationTarget(event.target)) return;
		let handled = false;
		runtime.commitMutation(() => {
			const outcome = wasm.ryeos_key(key);
			handled = outcome.handled;
			return outcome.envelope;
		});
		if (handled) event.preventDefault();
	}, options);
	let resizeTimer = null;
	window.addEventListener("resize", () => {
		if (resizeTimer !== null) window.clearTimeout(resizeTimer);
		resizeTimer = window.setTimeout(() => {
			resizeTimer = null;
			runtime.enqueueEvent({
				type: "resize",
				viewport: viewport()
			});
		}, 120);
	}, options);
	window.addEventListener("hashchange", () => {
		if (!canAcceptMutations()) return;
		runtime.enqueueEvent({
			type: "route_changed",
			route: location.hash.replace(/^#/, "")
		});
	}, options);
	return () => {
		abort.abort();
		if (resizeTimer !== null) window.clearTimeout(resizeTimer);
	};
}
function viewport() {
	return {
		width: window.innerWidth,
		height: window.innerHeight,
		device_pixel_ratio: window.devicePixelRatio || 1
	};
}
function setBootStage(stage, detail) {
	const stageNode = document.getElementById("ryeos-boot-stage");
	const detailNode = document.getElementById("ryeos-boot-detail");
	if (stageNode) stageNode.textContent = stage;
	if (detailNode) detailNode.textContent = detail;
}
function renderBootFailure(root, error) {
	console.error("RyeOS boot failed", error);
	const main = document.createElement("main");
	main.className = "ryeos-boot ryeos-boot-failed";
	main.setAttribute("role", "alert");
	const kicker = document.createElement("p");
	kicker.className = "ryeos-boot-kicker";
	kicker.textContent = "RyeOS / connection interrupted";
	const title = document.createElement("h1");
	title.textContent = "The node surface did not open";
	const diagnostic = document.createElement("pre");
	diagnostic.className = "ryeos-boot-error-detail";
	diagnostic.textContent = error instanceof Error ? error.message : String(error);
	const guidance = document.createElement("p");
	guidance.className = "ryeos-boot-detail";
	guidance.textContent = "Run ryeos web again to mint a fresh one-time launch, or check ryeos node status if the node is offline.";
	const retry = document.createElement("button");
	retry.className = "ryeos-boot-retry";
	retry.type = "button";
	retry.textContent = "Retry this session";
	retry.addEventListener("click", () => location.reload());
	main.append(kicker, title, diagnostic, guidance, retry);
	root.replaceChildren(main);
}
//#endregion
//#region browser/runtime/document-lifetime.ts
function createDocumentLifetime(start) {
	const abort = new AbortController();
	let running = null;
	let disposal = null;
	let markDisposalRequested = () => {};
	const disposalRequested = new Promise((resolve) => {
		markDisposalRequested = resolve;
	});
	const ready = Promise.resolve().then(async () => {
		if (abort.signal.aborted) return;
		const runtime = await start(abort.signal);
		if (abort.signal.aborted) await runtime.close();
		else running = runtime;
	});
	return {
		ready,
		settled: (async () => {
			await disposalRequested;
			const outcomes = await Promise.allSettled([ready, disposal ?? Promise.resolve()]);
			for (const outcome of outcomes) if (outcome.status === "rejected") throw outcome.reason;
		})(),
		dispose() {
			abort.abort();
			if (!disposal) {
				const runtime = running;
				running = null;
				disposal = runtime ? Promise.resolve().then(() => runtime.close()) : Promise.resolve();
				markDisposalRequested();
			}
			return disposal;
		}
	};
}
//#endregion
//#region browser/entry.ts
var predecessorSettlement = void 0;
var lifetime = createDocumentLifetime((signal) => bootRyeOsDocument(signal, predecessorSettlement));
lifetime.ready.catch(() => {});
lifetime.settled.catch((error) => console.error("RyeOS document settlement failed", error));
//#endregion
export { bootRyeOs, bootRyeOsDocument, captureBrowserPresentation, createCommitRuntime, decodeJsonBody, encodeCanonicalJsonBody, encodeJsonBody, encodeSeatPayloadDigest, mountRyeOsRenderer, restoreBrowserPresentation };
