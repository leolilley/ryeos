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
	let renderScheduled = false;
	let renderRunning = false;
	let pendingEffects = 0;
	let latestRenderable = null;
	let acceptedSequence = 0;
	let settledSequence = 0;
	const idleWaiters = [];
	function enqueueMutation(mutation) {
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
			options.startEffect(effect).then((result) => deliverEffectResult(result), (error) => deliverEffectResult(options.failedEffectResult(effect, error)));
			function deliverEffectResult(result) {
				if (delivered) return;
				delivered = true;
				pendingEffects -= 1;
				enqueueMutation(() => options.applyEffectResult(result));
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
		idle() {
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
var root$17 = /* @__PURE__ */ from_html(`<button><span class="navigation-glyph" aria-hidden="true">◇</span> <span class="navigation-copy"><strong> </strong></span></button>`);
var root_1$9 = /* @__PURE__ */ from_html(`<aside class="navigation" aria-label="RyeOS navigation"><div class="navigation-heading">Explorer <span> </span></div> <nav></nav></aside>`);
function Navigation($$anchor, $$props) {
	push($$props, true);
	const dispatch = dispatchUi();
	var aside = root_1$9();
	var div = child(aside);
	var text = only_child(sibling(child(div)), true);
	reset(div);
	var nav = sibling(div, 2);
	each(nav, 21, () => $$props.model.items, (item) => item.id, ($$anchor, item) => {
		var button = root$17();
		let classes;
		var span_1 = sibling(child(button), 2);
		var text_1 = only_child(child(span_1), true);
		reset(span_1);
		reset(button);
		template_effect(() => {
			classes = set_class(button, 1, "", null, classes, { active: get(item).selected });
			set_text(text_1, get(item).label);
		});
		delegated("click", button, () => dispatch({
			type: "activate",
			intent: get(item).intent
		}));
		append($$anchor, button);
	});
	reset(nav);
	reset(aside);
	template_effect(($0) => set_text(text, $0), [() => String($$props.model.items.length).padStart(2, "0")]);
	append($$anchor, aside);
	pop();
}
delegate(["click"]);
//#endregion
//#region browser/app/Notices.svelte
var root$16 = /* @__PURE__ */ from_html(`<div class="notice"><span> </span> <button aria-label="Dismiss notice">×</button></div>`);
var root_1$8 = /* @__PURE__ */ from_html(`<aside class="notice-stack" aria-label="RyeOS notices" aria-live="polite" aria-atomic="false"></aside>`);
function Notices($$anchor, $$props) {
	push($$props, true);
	const dispatch = dispatchUi();
	var fragment = comment();
	var node = first_child(fragment);
	var consequent = ($$anchor) => {
		var aside = root_1$8();
		each(aside, 21, () => $$props.notices, (notice) => notice.id, ($$anchor, notice) => {
			var div = root$16();
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
//#region browser/app/OverlayLayer.svelte
var root$15 = /* @__PURE__ */ from_html(`<span> </span>`);
var root_1$7 = /* @__PURE__ */ from_html(`<div class="overlay-columns"></div>`);
var root_2$5 = /* @__PURE__ */ from_html(`<span class="overlay-secondary"> </span>`);
var root_3$4 = /* @__PURE__ */ from_html(`<span class="overlay-secondary overlay-disabled-reason"> </span>`);
var root_4$4 = /* @__PURE__ */ from_html(`<small> </small>`);
var root_5$4 = /* @__PURE__ */ from_html(`<button class="overlay-secondary-action">↗</button>`);
var root_6$4 = /* @__PURE__ */ from_html(`<div><button role="option"><span class="overlay-primary"><!> </span> <!> <!> <!></button> <!></div>`);
var root_7$3 = /* @__PURE__ */ from_html(`<p class="overlay-empty">No matching entries</p>`);
var root_8$3 = /* @__PURE__ */ from_html(`<footer> </footer>`);
var root_9$2 = /* @__PURE__ */ from_html(`<div class="overlay-scrim" role="presentation"><div class="overlay-panel" role="dialog" aria-modal="true" tabindex="-1"><header><div><small> </small><h2> </h2></div> <button class="overlay-close">×</button></header> <input class="overlay-query" type="search" autocomplete="off" spellcheck="false"/> <!> <div class="overlay-items" role="listbox"></div> <!></div></div>`);
function OverlayLayer($$anchor, $$props) {
	push($$props, true);
	let queryInput;
	let panel;
	const dispatch = dispatchUi();
	onMount(() => queryInput?.focus());
	function select(itemId) {
		dispatch({
			type: "set_overlay_selection",
			item_id: itemId
		});
	}
	function choose(itemId, secondary) {
		dispatch({
			type: "choose_overlay_at",
			item_id: itemId,
			secondary
		});
	}
	function handleKey(event) {
		if (event.isComposing) return;
		if (event.key === "Escape") {
			event.preventDefault();
			dispatch({ type: "close_overlay" });
		} else if (event.key === "ArrowDown" || event.key === "ArrowUp") {
			event.preventDefault();
			dispatch({
				type: "move_overlay_selection",
				delta: event.key === "ArrowDown" ? 1 : -1
			});
		} else if (event.key === "Enter") {
			event.preventDefault();
			dispatch({
				type: "choose_overlay",
				secondary: event.shiftKey || event.altKey
			});
		} else if (event.key === "Tab" && panel) {
			const focusable = [...panel.querySelectorAll("input,button:not([disabled])")];
			if (focusable.length === 0) return;
			const current = focusable.indexOf(document.activeElement);
			const next = event.shiftKey ? current <= 0 ? focusable.length - 1 : current - 1 : current >= focusable.length - 1 ? 0 : current + 1;
			event.preventDefault();
			focusable[next]?.focus();
		}
	}
	var div = root_9$2();
	var div_1 = child(div);
	var header = child(div_1);
	var div_2 = child(header);
	var small = child(div_2);
	var text$1 = only_child(small, true);
	var h2 = sibling(small);
	var text_1 = only_child(h2, true);
	reset(div_2);
	var button = sibling(div_2, 2);
	reset(header);
	var input = sibling(header, 2);
	remove_input_defaults(input);
	bind_this(input, ($$value) => queryInput = $$value, () => queryInput);
	var node = sibling(input, 2);
	var consequent = ($$anchor) => {
		var div_3 = root_1$7();
		each(div_3, 21, () => $$props.model.columns, index, ($$anchor, column) => {
			var span = root$15();
			var text_2 = only_child(span, true);
			template_effect(() => set_text(text_2, get(column)));
			append($$anchor, span);
		});
		reset(div_3);
		template_effect(() => set_style(div_3, `--columns:${$$props.model.columns.length}`));
		append($$anchor, div_3);
	};
	if_block(node, ($$render) => {
		if ($$props.model.columns.length > 0) $$render(consequent);
	});
	var div_4 = sibling(node, 2);
	each(div_4, 23, () => $$props.model.items, (item, index) => `${item.category}:${item.primary}:${index}`, ($$anchor, item, index) => {
		var div_5 = root_6$4();
		let classes;
		var button_1 = child(div_5);
		var span_1 = child(button_1);
		var node_1 = child(span_1);
		var consequent_1 = ($$anchor) => {
			var text_3 = text();
			template_effect(() => set_text(text_3, get(item).expanded ? "▾" : "▸"));
			append($$anchor, text_3);
		};
		if_block(node_1, ($$render) => {
			if (get(item).header) $$render(consequent_1);
		});
		var text_4 = sibling(node_1, 1, true);
		reset(span_1);
		var node_2 = sibling(span_1, 2);
		var consequent_2 = ($$anchor) => {
			var span_2 = root_2$5();
			var text_5 = only_child(span_2, true);
			template_effect(() => set_text(text_5, get(item).secondary));
			append($$anchor, span_2);
		};
		if_block(node_2, ($$render) => {
			if (get(item).secondary && !get(item).disabled_reason) $$render(consequent_2);
		});
		var node_3 = sibling(node_2, 2);
		var consequent_3 = ($$anchor) => {
			var span_3 = root_3$4();
			var text_6 = only_child(span_3, true);
			template_effect(() => {
				set_attribute(span_3, "id", `overlay-reason-${get(item).id}`);
				set_text(text_6, get(item).disabled_reason);
			});
			append($$anchor, span_3);
		};
		if_block(node_3, ($$render) => {
			if (get(item).disabled_reason) $$render(consequent_3);
		});
		var node_4 = sibling(node_3, 2);
		var consequent_4 = ($$anchor) => {
			var small_1 = root_4$4();
			var text_7 = only_child(small_1, true);
			template_effect(() => set_text(text_7, get(item).meta));
			append($$anchor, small_1);
		};
		if_block(node_4, ($$render) => {
			if (get(item).meta) $$render(consequent_4);
		});
		reset(button_1);
		var node_5 = sibling(button_1, 2);
		var consequent_5 = ($$anchor) => {
			var button_2 = root_5$4();
			template_effect(() => {
				set_attribute(button_2, "data-focus-key", `overlay:${$$props.model.id}:item:${get(item).id}:alternate`);
				set_attribute(button_2, "aria-label", `Alternate action for ${get(item).primary}`);
			});
			delegated("click", button_2, () => choose(get(item).id, true));
			append($$anchor, button_2);
		};
		if_block(node_5, ($$render) => {
			if (get(item).secondary_intent && get(item).enabled) $$render(consequent_5);
		});
		reset(div_5);
		template_effect(($0, $1) => {
			classes = set_class(div_5, 1, "overlay-item", null, classes, {
				selected: $0,
				header: get(item).header
			});
			set_style(div_5, `--depth:${get(item).depth}`);
			set_attribute(button_1, "aria-selected", $1);
			set_attribute(button_1, "aria-describedby", get(item).disabled_reason ? `overlay-reason-${get(item).id}` : void 0);
			set_attribute(button_1, "data-focus-key", `overlay:${$$props.model.id}:item:${get(item).id}`);
			button_1.disabled = !get(item).enabled;
			set_text(text_4, get(item).primary);
		}, [() => $$props.model.selected === BigInt(get(index)), () => $$props.model.selected === BigInt(get(index))]);
		delegated("click", button_1, () => choose(get(item).id, false));
		event("pointerenter", button_1, () => select(get(item).id));
		append($$anchor, div_5);
	}, ($$anchor) => {
		append($$anchor, root_7$3());
	});
	reset(div_4);
	var node_6 = sibling(div_4, 2);
	var consequent_6 = ($$anchor) => {
		var footer = root_8$3();
		var text_8 = only_child(footer, true);
		template_effect(() => set_text(text_8, $$props.model.hint));
		append($$anchor, footer);
	};
	if_block(node_6, ($$render) => {
		if ($$props.model.hint) $$render(consequent_6);
	});
	reset(div_1);
	bind_this(div_1, ($$value) => panel = $$value, () => panel);
	reset(div);
	template_effect(() => {
		set_attribute(div_1, "aria-labelledby", `overlay-title-${$$props.model.id}`);
		set_text(text$1, $$props.model.widget);
		set_attribute(h2, "id", `overlay-title-${$$props.model.id}`);
		set_text(text_1, $$props.model.title);
		set_attribute(button, "aria-label", `Close ${$$props.model.title}`);
		set_attribute(input, "data-focus-key", `overlay:${$$props.model.id}:query`);
		set_value(input, $$props.model.query);
		set_attribute(input, "aria-label", `Filter ${$$props.model.title}`);
		set_attribute(div_4, "aria-label", $$props.model.title);
	});
	delegated("click", div, (event) => event.target === event.currentTarget && dispatch({ type: "close_overlay" }));
	delegated("keydown", div_1, handleKey);
	delegated("click", button, () => dispatch({ type: "close_overlay" }));
	delegated("input", input, (event) => dispatch({
		type: "set_overlay_query",
		query: event.currentTarget.value
	}));
	append($$anchor, div);
	pop();
}
delegate([
	"click",
	"keydown",
	"input"
]);
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
var root$14 = /* @__PURE__ */ from_html(`<div class="ambient-layer" aria-hidden="true"><canvas></canvas></div>`);
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
	var div = root$14();
	bind_this(child(div), ($$value) => canvas = $$value, () => canvas);
	reset(div);
	template_effect(() => set_style(div, `--ambient-opacity:${$$props.ambient.opacity ?? 1}`));
	append($$anchor, div);
	pop();
}
//#endregion
//#region browser/app/StatusBar.svelte
var root$13 = /* @__PURE__ */ from_html(`<span> </span>`);
var root_1$6 = /* @__PURE__ */ from_html(`<footer class="status-bar"><!> <span class="key-hint"> </span></footer>`);
function StatusBar($$anchor, $$props) {
	push($$props, true);
	var fragment = comment();
	var node = first_child(fragment);
	var consequent = ($$anchor) => {
		var footer = root_1$6();
		var node_1 = child(footer);
		each(node_1, 17, () => $$props.model.segments, index, ($$anchor, segment) => {
			var span = root$13();
			let classes;
			var text = only_child(span);
			template_effect(() => {
				set_attribute(span, "data-tone", get(segment).tone);
				classes = set_class(span, 1, "", null, classes, { "status-spacer": get(segment).grow });
				set_text(text, `${get(segment).label ? `${get(segment).label} ` : ""}${get(segment).value ?? ""}`);
			});
			append($$anchor, span);
		});
		var text_1 = only_child(sibling(node_1, 2), true);
		reset(footer);
		template_effect(() => set_text(text_1, $$props.model.key_hint));
		append($$anchor, footer);
	};
	if_block(node, ($$render) => {
		if ($$props.model.visible) $$render(consequent);
	});
	append($$anchor, fragment);
	pop();
}
//#endregion
//#region browser/app/SystemBar.svelte
var root$12 = /* @__PURE__ */ from_html(`<header class="system-bar"><div class="brand" aria-label="RyeOS"><span class="brand-mark" aria-hidden="true">◇</span> <span> </span></div> <div class="system-context"><span class="presence">●</span> <span> </span> <span class="separator">/</span> <span> </span></div> <div class="system-state"><span> </span> <span class="transport"> </span></div></header>`);
function SystemBar($$anchor, $$props) {
	push($$props, true);
	var header = root$12();
	var div = child(header);
	var text = only_child(sibling(child(div), 2), true);
	reset(div);
	var div_1 = sibling(div, 2);
	var span_1 = child(div_1);
	var span_2 = sibling(span_1, 2);
	var text_1 = only_child(span_2, true);
	var text_2 = only_child(sibling(span_2, 4), true);
	reset(div_1);
	var div_2 = sibling(div_1, 2);
	var span_4 = child(div_2);
	var text_3 = only_child(span_4, true);
	var span_5 = sibling(span_4, 2);
	var text_4 = only_child(span_5, true);
	reset(div_2);
	reset(header);
	template_effect(() => {
		set_text(text, $$props.chrome.title);
		set_attribute(span_1, "data-tone", $$props.chrome.health_tone);
		set_text(text_1, $$props.session.user_principal_id ?? "local");
		set_text(text_2, $$props.session.project_path ?? $$props.session.surface_ref);
		set_text(text_3, $$props.chrome.health_label);
		set_attribute(span_5, "data-freshness", $$props.transport.freshness);
		set_text(text_4, $$props.transport.freshness);
	});
	append($$anchor, header);
	pop();
}
//#endregion
//#region browser/app/ViewSetStrip.svelte
var root$11 = /* @__PURE__ */ from_html(`<span class="view-set-actions" role="group"><button title="Duplicate view set">⧉</button> <button title="Close view set">×</button></span>`);
var root_1$5 = /* @__PURE__ */ from_html(`<div><button class="view-set-select"><span class="ordinal"> </span> <span> </span></button> <!></div>`);
var root_2$4 = /* @__PURE__ */ from_html(`<nav class="view-set-strip" aria-label="View sets"><!> <button class="new-view-set" aria-label="New view set">＋</button></nav>`);
function ViewSetStrip($$anchor, $$props) {
	push($$props, true);
	const dispatch = dispatchUi();
	var fragment = comment();
	var node = first_child(fragment);
	var consequent_1 = ($$anchor) => {
		var nav = root_2$4();
		var node_1 = child(nav);
		each(node_1, 17, () => $$props.model.tabs, (tab) => tab.view_set_id, ($$anchor, tab) => {
			var div = root_1$5();
			let classes;
			var button = child(div);
			var span = child(button);
			var text = only_child(span, true);
			var text_1 = only_child(sibling(span, 2), true);
			reset(button);
			var node_2 = sibling(button, 2);
			var consequent = ($$anchor) => {
				var span_2 = root$11();
				var button_1 = child(span_2);
				var button_2 = sibling(button_1, 2);
				reset(span_2);
				template_effect(() => {
					set_attribute(span_2, "aria-label", `${get(tab).title} view-set actions`);
					set_attribute(button_1, "aria-label", `Duplicate ${get(tab).title}`);
					set_attribute(button_2, "aria-label", `Close ${get(tab).title}`);
				});
				delegated("click", button_1, () => dispatch({
					type: "activate",
					intent: {
						type: "duplicate_view_set",
						view_set_id: get(tab).view_set_id
					}
				}));
				delegated("click", button_2, () => dispatch({
					type: "activate",
					intent: {
						type: "close_view_set",
						view_set_id: get(tab).view_set_id
					}
				}));
				append($$anchor, span_2);
			};
			if_block(node_2, ($$render) => {
				if (get(tab).active) $$render(consequent);
			});
			reset(div);
			template_effect(($0) => {
				classes = set_class(div, 1, "view-set-tab", null, classes, { active: get(tab).active });
				set_attribute(button, "aria-current", get(tab).active ? "page" : void 0);
				set_text(text, $0);
				set_text(text_1, get(tab).title);
			}, [() => String(get(tab).number).padStart(2, "0")]);
			delegated("click", button, () => dispatch({
				type: "activate",
				intent: {
					type: "select_view_set",
					view_set_id: get(tab).view_set_id
				}
			}));
			append($$anchor, div);
		});
		var button_3 = sibling(node_1, 2);
		reset(nav);
		delegated("click", button_3, () => dispatch({
			type: "activate",
			intent: { type: "new_view_set" }
		}));
		append($$anchor, nav);
	};
	if_block(node, ($$render) => {
		if ($$props.model.visible) $$render(consequent_1);
	});
	append($$anchor, fragment);
	pop();
}
delegate(["click"]);
//#endregion
//#region browser/components/InputComposer.svelte
var root$10 = /* @__PURE__ */ from_html(`<section class="composer"><div class="composer-route"><span class="route-state">●</span><span> </span><span class="draft-state">DRAFT</span></div> <textarea></textarea> <div class="composer-actions"><span> </span><button>↑</button></div></section>`);
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
	var section = root$10();
	var div = child(section);
	var text_1 = only_child(sibling(child(div)), true);
	next();
	reset(div);
	var textarea = sibling(div, 2);
	remove_textarea_child(textarea);
	var div_1 = sibling(textarea, 2);
	var span_1 = child(div_1);
	var text_2 = only_child(span_1, true);
	var button = sibling(span_1);
	reset(div_1);
	reset(section);
	template_effect(() => {
		set_attribute(section, "aria-label", $$props.model.route_label);
		set_text(text_1, $$props.model.route_label);
		set_attribute(textarea, "data-focus-key", `input:${$$props.model.address.buffer.view_instance_key}:${$$props.model.address.buffer.input_id}`);
		set_value(textarea, $$props.model.text);
		set_attribute(textarea, "placeholder", $$props.model.placeholder);
		set_attribute(textarea, "aria-label", $$props.model.route_label);
		set_text(text_2, $$props.model.hint);
		button.disabled = !$$props.model.submit_enabled;
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
		if (event.key === "Enter" && !event.shiftKey && !composing && $$props.model.submit_enabled) {
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
	delegated("click", button, () => dispatch({
		type: "input_at",
		address: $$props.model.address,
		action: {
			type: "submit",
			interrupt: false
		}
	}));
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
var root$9 = /* @__PURE__ */ from_html(`<div class="empty-state"><span>◇</span><strong> </strong><p> </p></div>`);
function EmptyState($$anchor, $$props) {
	var div = root$9();
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
//#region browser/visuals/ryeos_field_accessibility.js
function fieldAccessibilityModel(vm) {
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
			domId: `ryeos-field-option-${safeId(entity.id)}`,
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
	return [...String(value)].map((character) => /[a-zA-Z0-9-]/.test(character) ? character : `_${character.codePointAt(0).toString(16)}_`).join("");
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
	const suffix = (grid.cells || []).length > cells.length ? "; remaining cells omitted" : "";
	return `${label}; ${grid.width} by ${grid.height} indexed grid; cells ${cells.join(" ")}${suffix}`;
}
function contrastingTextColor(color) {
	const match = /^#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/i.exec(color || "");
	if (!match) return "CanvasText";
	return (Number.parseInt(match[1], 16) * 299 + Number.parseInt(match[2], 16) * 587 + Number.parseInt(match[3], 16) * 114) / 1e3 >= 140 ? "#111111" : "#ffffff";
}
//#endregion
//#region browser/views/GridPreview.svelte
var root$8 = /* @__PURE__ */ from_html(`<figure class="field-preview"><figcaption> </figcaption> <canvas></canvas></figure>`);
function GridPreview($$anchor, $$props) {
	push($$props, true);
	let canvas;
	const label = /* @__PURE__ */ user_derived(() => indexedGridAccessibilityLabel($$props.preview, $$props.preview.label));
	onMount(() => drawIndexedGrid(canvas, $$props.preview, { scale: 9 }));
	user_effect(() => {
		if (canvas) drawIndexedGrid(canvas, $$props.preview, { scale: 9 });
	});
	var figure = root$8();
	var figcaption = child(figure);
	var text = only_child(figcaption, true);
	var canvas_1 = sibling(figcaption, 2);
	bind_this(canvas_1, ($$value) => canvas = $$value, () => canvas);
	reset(figure);
	template_effect(() => {
		set_text(text, $$props.preview.label);
		set_attribute(canvas_1, "aria-label", get(label));
		set_attribute(canvas_1, "title", $$props.compareEnabled ? "Shift-click to compare" : void 0);
	});
	delegated("click", canvas_1, (event) => {
		if (event.shiftKey && $$props.compareEnabled) $$props.oncompare();
	});
	append($$anchor, figure);
	pop();
}
delegate(["click"]);
//#endregion
//#region browser/views/FieldView.svelte
var root$7 = /* @__PURE__ */ from_html(`<button> </button>`);
var root_1$4 = /* @__PURE__ */ from_html(`<button>Clear</button>`);
var root_2$3 = /* @__PURE__ */ from_html(`<button> </button> <button> </button> <!>`, 1);
var root_3$3 = /* @__PURE__ */ from_html(`<nav class="field-rail" aria-label="Durable execution events"></nav>`);
var root_4$3 = /* @__PURE__ */ from_html(`<div role="treeitem" tabindex="-1"> </div>`);
var root_5$3 = /* @__PURE__ */ from_html(`<strong> </strong><small> </small> <!> <!>`, 1);
var root_6$3 = /* @__PURE__ */ from_html(`<small class="field-warning"> </small>`);
var root_7$2 = /* @__PURE__ */ from_html(`<aside class="field-detail"><!> <!></aside>`);
var root_8$2 = /* @__PURE__ */ from_html(`<section class="field-view"><header class="field-toolbar"><div class="field-identity"><strong> </strong><span> </span></div> <div class="field-controls"><button>◀</button> <button> </button> <button>▶</button> <button>Live</button> <!> <!> <!></div> <input type="search" placeholder="Search field" aria-label="Search field entities"/></header> <!> <div class="field-stage"><canvas aria-hidden="true"></canvas></div> <div class="field-accessibility" role="tree" tabindex="0"></div> <!></section>`);
function FieldView($$anchor, $$props) {
	push($$props, true);
	const dispatch = dispatchUi();
	let canvas;
	let controller = null;
	const selected = /* @__PURE__ */ user_derived(() => $$props.field.entities.find((entity) => entity.id === $$props.field.selected) ?? null);
	const selectedRelations = /* @__PURE__ */ user_derived(() => get(selected) ? $$props.field.relations.filter((relation) => relation.source_id === get(selected).id || relation.target_id === get(selected).id) : []);
	const previewIds = /* @__PURE__ */ user_derived(() => {
		const ids = new Set(get(selected)?.preview_ids ?? []);
		for (const id of $$props.field.compare) for (const preview of $$props.field.entities.find((entity) => entity.id === id)?.preview_ids ?? []) ids.add(preview);
		return ids;
	});
	const previews = /* @__PURE__ */ user_derived(() => $$props.field.previews.filter((preview) => get(previewIds).has(preview.id)));
	const expansion = /* @__PURE__ */ user_derived(() => get(selected) ? $$props.field.expansions.find((item) => item.source === get(selected).source && item.root_id === get(selected).id) ?? null : null);
	const accessibilityItems = /* @__PURE__ */ user_derived(() => fieldAccessibilityModel($$props.field));
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
	var section = root_8$2();
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
		var button_4 = root$7();
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
		var button_5 = root$7();
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
	var consequent_1 = ($$anchor) => {
		var fragment = root_2$3();
		var button_6 = first_child(fragment);
		var text_5 = only_child(button_6, true);
		var button_7 = sibling(button_6, 2);
		var text_6 = only_child(button_7, true);
		var node_3 = sibling(button_7, 2);
		var consequent = ($$anchor) => {
			var button_8 = root_1$4();
			delegated("click", button_8, () => dispatch({
				type: "clear_field_expansion",
				instance_key: $$props.instanceKey,
				source: get(selected).source,
				root_id: get(selected).id
			}));
			append($$anchor, button_8);
		};
		if_block(node_3, ($$render) => {
			if (get(expansion)) $$render(consequent);
		});
		template_effect(($0) => {
			button_6.disabled = !get(selected).compare_available;
			set_text(text_5, $0);
			button_7.disabled = !!get(expansion) && !get(expansion).can_continue;
			set_text(text_6, get(expansion)?.can_continue ? "Continue" : get(expansion) ? "Expanded" : "Expand");
		}, [() => $$props.field.compare.includes(get(selected).id) ? "Uncompare" : "Compare"]);
		delegated("click", button_6, () => compareEntity(get(selected).id));
		delegated("click", button_7, () => dispatch({
			type: get(expansion)?.can_continue ? "continue_field_expansion" : "request_field_expansion",
			instance_key: $$props.instanceKey,
			source: get(selected).source,
			root_id: get(selected).id
		}));
		append($$anchor, fragment);
	};
	if_block(node_2, ($$render) => {
		if (get(selected)) $$render(consequent_1);
	});
	reset(div_1);
	var input = sibling(div_1, 2);
	remove_input_defaults(input);
	reset(header);
	var node_4 = sibling(header, 2);
	var consequent_2 = ($$anchor) => {
		var nav = root_3$3();
		each(nav, 21, () => $$props.field.replay.rail, index, ($$anchor, entry) => {
			var button_9 = root$7();
			let classes;
			var text_7 = only_child(button_9, true);
			template_effect(() => {
				set_attribute(button_9, "aria-pressed", get(entry).selected);
				classes = set_class(button_9, 1, "", null, classes, { selected: get(entry).selected });
				set_text(text_7, get(entry).label);
			});
			delegated("click", button_9, () => dispatch({
				type: "set_field_cursor",
				instance_key: $$props.instanceKey,
				cursor: {
					mode: "braid_cut",
					anchor: get(entry).event
				}
			}));
			append($$anchor, button_9);
		});
		reset(nav);
		append($$anchor, nav);
	};
	if_block(node_4, ($$render) => {
		if ($$props.field.replay.rail.length) $$render(consequent_2);
	});
	var div_2 = sibling(node_4, 2);
	bind_this(child(div_2), ($$value) => canvas = $$value, () => canvas);
	reset(div_2);
	var div_3 = sibling(div_2, 2);
	each(div_3, 21, () => get(accessibilityItems), (item) => item.id, ($$anchor, item) => {
		var div_4 = root_4$3();
		var text_8 = only_child(div_4);
		template_effect(() => {
			set_attribute(div_4, "id", get(item).domId);
			set_attribute(div_4, "aria-selected", get(item).selected);
			set_attribute(div_4, "aria-posinset", get(item).position);
			set_attribute(div_4, "aria-setsize", get(item).size);
			set_attribute(div_4, "aria-level", get(item).level);
			set_attribute(div_4, "aria-expanded", get(item).expanded ?? void 0);
			set_text(text_8, `${get(item).groupLabel ? `Group ${get(item).groupLabel}. ` : ""}${get(item).label ?? ""}${get(item).neighbors ? `. ${get(item).neighbors}` : ""}`);
		});
		delegated("click", div_4, () => selectEntity(get(item).id));
		delegated("dblclick", div_4, () => activateEntity(get(item).id));
		delegated("keydown", div_4, (event) => {
			if (event.key === "Enter") activateEntity(get(item).id);
		});
		append($$anchor, div_4);
	});
	reset(div_3);
	var node_5 = sibling(div_3, 2);
	var consequent_4 = ($$anchor) => {
		var aside = root_7$2();
		var node_6 = child(aside);
		var consequent_3 = ($$anchor) => {
			var fragment_1 = root_5$3();
			var strong_1 = first_child(fragment_1);
			var text_9 = only_child(strong_1, true);
			var small = sibling(strong_1);
			var text_10 = only_child(small, true);
			var node_7 = sibling(small, 2);
			each(node_7, 17, () => get(selectedRelations), (relation) => relation.id, ($$anchor, relation) => {
				var button_10 = root$7();
				var text_11 = only_child(button_10, true);
				template_effect(() => {
					button_10.disabled = !get(relation).activate_intent;
					set_text(text_11, get(relation).label);
				});
				delegated("click", button_10, () => get(relation).activate_intent && dispatch({
					type: "activate",
					intent: get(relation).activate_intent
				}));
				append($$anchor, button_10);
			});
			each(sibling(node_7, 2), 17, () => get(previews), (preview) => preview.id, ($$anchor, preview) => {
				GridPreview($$anchor, {
					get preview() {
						return get(preview);
					},
					get compareEnabled() {
						return get(selected).compare_available;
					},
					oncompare: () => compareEntity(get(selected).id)
				});
			});
			template_effect(($0) => {
				set_text(text_9, get(selected).label);
				set_text(text_10, $0);
			}, [() => [
				get(selected).kind,
				get(selected).status,
				get(selected).source
			].filter(Boolean).join(" · ")]);
			append($$anchor, fragment_1);
		};
		if_block(node_6, ($$render) => {
			if (get(selected)) $$render(consequent_3);
		});
		each(sibling(node_6, 2), 17, () => $$props.field.warnings, index, ($$anchor, warning) => {
			var small_1 = root_6$3();
			var text_12 = only_child(small_1, true);
			template_effect(() => set_text(text_12, get(warning)));
			append($$anchor, small_1);
		});
		reset(aside);
		append($$anchor, aside);
	};
	if_block(node_5, ($$render) => {
		if (get(selected) || $$props.field.warnings.length) $$render(consequent_4);
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
		set_attribute(div_3, "aria-label", `${$$props.field.title} entities`);
		set_attribute(div_3, "aria-activedescendant", get(activeOptionId));
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
	event("focus", div_3, () => {
		if (!$$props.field.selected && get(accessibilityItems)[0]) selectEntity(get(accessibilityItems)[0].id);
	});
	delegated("keydown", div_3, (event) => {
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
var root$6 = /* @__PURE__ */ from_html(`<button> </button>`);
var root_1$3 = /* @__PURE__ */ from_html(`<div class="atlas-layers"></div> <div class="atlas-lenses" aria-label="Atlas lens"></div>`, 1);
var root_2$2 = /* @__PURE__ */ from_html(`<div class="atlas-toolbar"><div class="atlas-identity"><strong> </strong><span> </span></div> <div class="atlas-projections" aria-label="Atlas projection"></div> <!></div>`);
var root_3$2 = /* @__PURE__ */ from_svg(`<line></line>`);
var root_4$2 = /* @__PURE__ */ from_svg(`<text> </text>`);
var root_5$2 = /* @__PURE__ */ from_svg(`<polygon></polygon>`);
var root_6$2 = /* @__PURE__ */ from_svg(`<rect></rect>`);
var root_7$1 = /* @__PURE__ */ from_svg(`<circle></circle>`);
var root_8$1 = /* @__PURE__ */ from_svg(`<title> </title>`);
var root_9$1 = /* @__PURE__ */ from_svg(`<g><!><!></g>`);
var root_10$1 = /* @__PURE__ */ from_svg(`<circle class="atlas-stack-item" r="0.055"><title> </title></circle>`);
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
		var div_1 = root_2$2();
		var div_2 = child(div_1);
		var strong = child(div_2);
		var text = only_child(strong, true);
		var text_1 = only_child(sibling(strong));
		reset(div_2);
		var div_3 = sibling(div_2, 2);
		each(div_3, 21, () => actions("projection"), (action) => action.id, ($$anchor, action) => {
			var button = root$6();
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
			var fragment = root_1$3();
			var div_4 = first_child(fragment);
			each(div_4, 21, () => actions("layer"), (action) => action.id, ($$anchor, action) => {
				var button_1 = root$6();
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
				var button_2 = root$6();
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
		var g = root_9$1();
		let classes_4;
		var node_4 = child(g);
		var consequent_2 = ($$anchor) => {
			var line = root_3$2();
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
			var text_5 = root_4$2();
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
			var polygon = root_5$2();
			template_effect(() => {
				set_attribute(polygon, "points", `${get(ox)},${get(oy) - get(r)} ${get(ox) + get(r)},${get(oy)} ${get(ox)},${get(oy) + get(r)} ${get(ox) - get(r)},${get(oy)}`);
				set_attribute(polygon, "stroke", get(object).color);
			});
			append($$anchor, polygon);
		};
		var consequent_5 = ($$anchor) => {
			var rect = root_6$2();
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
			var circle = root_7$1();
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
			var title = root_8$1();
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
				var line_1 = root_3$2();
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
				var circle_2 = root_10$1();
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
//#region browser/views/ViewRenderer.svelte
var root$5 = /* @__PURE__ */ from_html(`<div> </div>`);
var root_1$2 = /* @__PURE__ */ from_html(`<div class="text-view"></div>`);
var root_2$1 = /* @__PURE__ */ from_html(`<small>bounded preview</small>`);
var root_3$1 = /* @__PURE__ */ from_html(`<article class="document-view"><header><span> </span><!></header> <pre> </pre></article>`);
var root_4$1 = /* @__PURE__ */ from_html(`<small> </small>`);
var root_5$1 = /* @__PURE__ */ from_html(`<span class="row-meta"> </span>`);
var root_6$1 = /* @__PURE__ */ from_html(`<button><span class="row-glyph"> </span> <span class="row-copy"><strong> </strong><!></span> <!></button>`);
var root_7 = /* @__PURE__ */ from_html(`<div class="rows-view" role="list"></div>`);
var root_8 = /* @__PURE__ */ from_html(`<span role="columnheader"> </span>`);
var root_9 = /* @__PURE__ */ from_html(`<span role="cell"> </span>`);
var root_10 = /* @__PURE__ */ from_html(`<button role="row"></button>`);
var root_11 = /* @__PURE__ */ from_html(`<div class="table-view" role="table"><div class="table-head" role="row"></div> <!></div>`);
var root_12 = /* @__PURE__ */ from_html(`<p> </p>`);
var root_13 = /* @__PURE__ */ from_html(`<strong> </strong><!>`, 1);
var root_14 = /* @__PURE__ */ from_html(`<span> </span><!>`, 1);
var root_15 = /* @__PURE__ */ from_html(`<span class="timeline-separator"> </span>`);
var root_16 = /* @__PURE__ */ from_html(`<button><!></button>`);
var root_17 = /* @__PURE__ */ from_html(`<div class="timeline-view"></div>`);
var root_18 = /* @__PURE__ */ from_html(`<button><span> </span><small> </small></button>`);
var root_19 = /* @__PURE__ */ from_html(`<section><button><span> </span><span> </span></button> <!></section>`);
var root_20 = /* @__PURE__ */ from_html(`<div class="sections-view"></div>`);
var root_21 = /* @__PURE__ */ from_html(`<div class="view"><!></div>`);
function ViewRenderer($$anchor, $$props) {
	push($$props, true);
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
	var div = root_21();
	var node = child(div);
	var consequent = ($$anchor) => {
		var div_1 = root_1$2();
		each(div_1, 21, () => $$props.model.lines, index, ($$anchor, line) => {
			var div_2 = root$5();
			var text = only_child(div_2, true);
			template_effect(() => {
				set_attribute(div_2, "data-tone", get(line).tone);
				set_text(text, get(line).text);
			});
			append($$anchor, div_2);
		});
		reset(div_1);
		template_effect(() => set_attribute(div_1, "aria-label", $$props.model.title));
		append($$anchor, div_1);
	};
	var consequent_2 = ($$anchor) => {
		var article = root_3$1();
		var header = child(article);
		var span = child(header);
		var text_1 = only_child(span, true);
		var node_1 = sibling(span);
		var consequent_1 = ($$anchor) => {
			append($$anchor, root_2$1());
		};
		if_block(node_1, ($$render) => {
			if ($$props.model.truncated) $$render(consequent_1);
		});
		reset(header);
		var text_2 = only_child(sibling(header, 2), true);
		reset(article);
		template_effect(() => {
			set_attribute(article, "aria-label", $$props.model.title);
			set_text(text_1, $$props.model.path);
			set_text(text_2, $$props.model.content);
		});
		append($$anchor, article);
	};
	var consequent_5 = ($$anchor) => {
		var div_3 = root_7();
		each(div_3, 23, () => $$props.model.rows, (row) => row.id, ($$anchor, row) => {
			var button = root_6$1();
			let classes;
			var span_1 = child(button);
			var text_3 = only_child(span_1, true);
			var span_2 = sibling(span_1, 2);
			var strong = child(span_2);
			var text_4 = only_child(strong, true);
			var node_2 = sibling(strong);
			var consequent_3 = ($$anchor) => {
				var small_1 = root_4$1();
				var text_5 = only_child(small_1, true);
				template_effect(() => set_text(text_5, get(row).secondary));
				append($$anchor, small_1);
			};
			if_block(node_2, ($$render) => {
				if (get(row).secondary) $$render(consequent_3);
			});
			reset(span_2);
			var node_3 = sibling(span_2, 2);
			var consequent_4 = ($$anchor) => {
				var span_3 = root_5$1();
				var text_6 = only_child(span_3, true);
				template_effect(() => set_text(text_6, get(row).meta));
				append($$anchor, span_3);
			};
			if_block(node_3, ($$render) => {
				if (get(row).meta) $$render(consequent_4);
			});
			reset(button);
			template_effect(() => {
				set_attribute(button, "data-focus-key", `view:${$$props.instanceKey}:item:${get(row).id}`);
				set_attribute(button, "data-tone", get(row).tone);
				classes = set_class(button, 1, "", null, classes, { selected: get(row).selected });
				set_text(text_3, get(row).glyph ?? "◇");
				set_text(text_4, get(row).primary);
			});
			delegated("click", button, () => select(get(row).id));
			append($$anchor, button);
		});
		reset(div_3);
		template_effect(() => set_attribute(div_3, "aria-label", $$props.model.title));
		append($$anchor, div_3);
	};
	var consequent_6 = ($$anchor) => {
		var div_4 = root_11();
		var div_5 = child(div_4);
		each(div_5, 21, () => $$props.model.columns, index, ($$anchor, column) => {
			var span_4 = root_8();
			var text_7 = only_child(span_4, true);
			template_effect(() => set_text(text_7, get(column)));
			append($$anchor, span_4);
		});
		reset(div_5);
		each(sibling(div_5, 2), 19, () => $$props.model.rows, (row) => row.id, ($$anchor, row) => {
			var button_1 = root_10();
			let classes_1;
			each(button_1, 21, () => get(row).cells, index, ($$anchor, cell, index, $$array) => {
				var span_5 = root_9();
				var text_8 = only_child(span_5, true);
				template_effect(() => {
					set_attribute(span_5, "data-tone", get(row).cell_tones?.[index] ?? void 0);
					set_text(text_8, get(cell));
				});
				append($$anchor, span_5);
			});
			reset(button_1);
			template_effect(() => {
				set_attribute(button_1, "data-focus-key", `view:${$$props.instanceKey}:item:${get(row).id}`);
				set_attribute(button_1, "data-tone", get(row).tone);
				classes_1 = set_class(button_1, 1, "", null, classes_1, { selected: get(row).selected });
			});
			delegated("click", button_1, () => select(get(row).id));
			append($$anchor, button_1);
		});
		reset(div_4);
		template_effect(() => {
			set_attribute(div_4, "aria-label", $$props.model.title);
			set_style(div_4, `--columns:${$$props.model.columns.length}`);
		});
		append($$anchor, div_4);
	};
	var consequent_12 = ($$anchor) => {
		var div_6 = root_17();
		each(div_6, 21, () => $$props.model.entries, index, ($$anchor, entry, index) => {
			var button_2 = root_16();
			let classes_2;
			var node_5 = child(button_2);
			var consequent_7 = ($$anchor) => {
				var p = root_12();
				var text_9 = only_child(p, true);
				template_effect(() => {
					set_attribute(p, "data-tone", get(entry).tone);
					set_text(text_9, get(entry).text);
				});
				append($$anchor, p);
			};
			var consequent_9 = ($$anchor) => {
				var fragment = root_13();
				var strong_1 = first_child(fragment);
				var text_10 = only_child(strong_1, true);
				var node_6 = sibling(strong_1);
				var consequent_8 = ($$anchor) => {
					var small_2 = root_4$1();
					var text_11 = only_child(small_2, true);
					template_effect(() => set_text(text_11, get(entry).meta));
					append($$anchor, small_2);
				};
				if_block(node_6, ($$render) => {
					if (get(entry).meta) $$render(consequent_8);
				});
				template_effect(() => set_text(text_10, get(entry).primary));
				append($$anchor, fragment);
			};
			var consequent_11 = ($$anchor) => {
				var fragment_1 = root_14();
				var span_6 = first_child(fragment_1);
				var text_12 = only_child(span_6, true);
				var node_7 = sibling(span_6);
				var consequent_10 = ($$anchor) => {
					var small_3 = root_4$1();
					var text_13 = only_child(small_3, true);
					template_effect(() => set_text(text_13, get(entry).meta));
					append($$anchor, small_3);
				};
				if_block(node_7, ($$render) => {
					if (get(entry).meta) $$render(consequent_10);
				});
				template_effect(() => set_text(text_12, get(entry).summary));
				append($$anchor, fragment_1);
			};
			var alternate = ($$anchor) => {
				var span_7 = root_15();
				var text_14 = only_child(span_7, true);
				template_effect(() => set_text(text_14, get(entry).label));
				append($$anchor, span_7);
			};
			if_block(node_5, ($$render) => {
				if (get(entry).type === "block") $$render(consequent_7);
				else if (get(entry).type === "line") $$render(consequent_9, 1);
				else if (get(entry).type === "pair") $$render(consequent_11, 2);
				else $$render(alternate, -1);
			});
			reset(button_2);
			template_effect(($0) => {
				set_attribute(button_2, "data-focus-key", `view:${$$props.instanceKey}:item:${$$props.model.entry_ids[index]}`);
				classes_2 = set_class(button_2, 1, "timeline-entry", null, classes_2, { selected: $0 });
				set_attribute(button_2, "data-kind", get(entry).type);
				set_style(button_2, `--indent:${$$props.model.entry_indents[index] ?? 0}`);
			}, [() => $$props.model.selected === BigInt(index)]);
			delegated("click", button_2, () => select($$props.model.entry_ids[index]));
			append($$anchor, button_2);
		});
		reset(div_6);
		template_effect(() => set_attribute(div_6, "aria-label", $$props.model.title));
		append($$anchor, div_6);
	};
	var consequent_14 = ($$anchor) => {
		var div_7 = root_20();
		each(div_7, 21, () => $$props.model.sections, index, ($$anchor, section) => {
			var section_1 = root_19();
			let classes_3;
			var button_3 = child(section_1);
			let classes_4;
			var span_8 = child(button_3);
			var text_15 = only_child(span_8);
			var text_16 = only_child(sibling(span_8), true);
			reset(button_3);
			var node_8 = sibling(button_3, 2);
			var consequent_13 = ($$anchor) => {
				var fragment_2 = comment();
				each(first_child(fragment_2), 17, () => get(section).rows, (row) => row.id, ($$anchor, row) => {
					var button_4 = root_18();
					let classes_5;
					var span_10 = child(button_4);
					var text_17 = only_child(span_10, true);
					var text_18 = only_child(sibling(span_10), true);
					reset(button_4);
					template_effect(() => {
						set_attribute(button_4, "data-focus-key", `view:${$$props.instanceKey}:item:${get(row).id}`);
						classes_5 = set_class(button_4, 1, "section-row", null, classes_5, { selected: get(row).selected });
						set_text(text_17, get(row).primary);
						set_text(text_18, get(row).meta ?? get(row).secondary ?? "");
					});
					delegated("click", button_4, () => select(get(row).id));
					append($$anchor, button_4);
				});
				append($$anchor, fragment_2);
			};
			if_block(node_8, ($$render) => {
				if (!get(section).collapsed) $$render(consequent_13);
			});
			reset(section_1);
			template_effect(($0) => {
				classes_3 = set_class(section_1, 1, "", null, classes_3, { collapsed: get(section).collapsed });
				set_attribute(button_3, "data-focus-key", `view:${$$props.instanceKey}:section:${get(section).id}`);
				classes_4 = set_class(button_3, 1, "", null, classes_4, { selected: get(section).header_selected });
				set_text(text_15, `${get(section).collapsed ? "▸" : "▾"} ${get(section).title ?? ""}`);
				set_text(text_16, $0);
			}, [() => String(get(section).count).padStart(2, "0")]);
			delegated("click", button_3, () => dispatch({
				type: "toggle_view_section",
				instance_key: $$props.instanceKey,
				section_id: get(section).id
			}));
			append($$anchor, section_1);
		});
		reset(div_7);
		template_effect(() => set_attribute(div_7, "aria-label", $$props.model.title));
		append($$anchor, div_7);
	};
	var consequent_15 = ($$anchor) => {
		EmptyState($$anchor, {
			get title() {
				return $$props.model.title;
			},
			get message() {
				return $$props.model.message;
			}
		});
	};
	var consequent_16 = ($$anchor) => {
		SceneView($$anchor, { get scene() {
			return $$props.model.scene;
		} });
	};
	var consequent_17 = ($$anchor) => {
		FieldView($$anchor, {
			get field() {
				return $$props.model.field;
			},
			get instanceKey() {
				return $$props.instanceKey;
			}
		});
	};
	if_block(node, ($$render) => {
		if ($$props.model.type === "text") $$render(consequent);
		else if ($$props.model.type === "document") $$render(consequent_2, 1);
		else if ($$props.model.type === "rows") $$render(consequent_5, 2);
		else if ($$props.model.type === "table") $$render(consequent_6, 3);
		else if ($$props.model.type === "timeline") $$render(consequent_12, 4);
		else if ($$props.model.type === "sections") $$render(consequent_14, 5);
		else if ($$props.model.type === "placeholder") $$render(consequent_15, 6);
		else if ($$props.model.type === "map" || $$props.model.type === "atlas") $$render(consequent_16, 7);
		else if ($$props.model.type === "field") $$render(consequent_17, 8);
	});
	reset(div);
	template_effect(() => set_attribute(div, "data-view", $$props.model.type));
	append($$anchor, div);
	pop();
}
delegate(["click"]);
//#endregion
//#region browser/layout/DockSlot.svelte
var root$4 = /* @__PURE__ */ from_html(`<aside><header><span> </span><span> </span></header> <!> <!></aside>`);
function DockSlot($$anchor, $$props) {
	push($$props, true);
	const dispatch = dispatchUi();
	var aside = root$4();
	let classes;
	var header = child(aside);
	var span = child(header);
	var text = only_child(span, true);
	var text_1 = only_child(sibling(span), true);
	reset(header);
	var node = sibling(header, 2);
	{
		let $0 = /* @__PURE__ */ user_derived(() => String($$props.model.instance_key));
		ViewRenderer(node, {
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
	var node_1 = sibling(node, 2);
	var consequent = ($$anchor) => {
		InputComposer($$anchor, { get model() {
			return $$props.model.input;
		} });
	};
	if_block(node_1, ($$render) => {
		if ($$props.model.input) $$render(consequent);
	});
	reset(aside);
	template_effect(() => {
		classes = set_class(aside, 1, "dock-slot", null, classes, { focused: $$props.model.focused });
		set_attribute(aside, "data-edge", $$props.model.edge);
		set_style(aside, `--dock-size:${$$props.model.size}`);
		set_text(text, $$props.model.title);
		set_text(text_1, $$props.model.edge);
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
var root$3 = /* @__PURE__ */ from_html(`<button role="tab"> </button>`);
var root_1$1 = /* @__PURE__ */ from_html(`<div class="view-tabs" role="tablist"></div>`);
var root_2 = /* @__PURE__ */ from_html(`<header><div class="tile-title-row"><div class="tile-identity"><span class="tile-signal"></span><strong> </strong></div> <div class="tile-tools"><button class="tile-tool"> </button></div></div> <!></header>`);
var root_3 = /* @__PURE__ */ from_html(`<p> </p>`);
var root_4 = /* @__PURE__ */ from_html(`<div class="heading-meta"> </div>`);
var root_5 = /* @__PURE__ */ from_html(`<div class="content-heading"><small> </small><h1> </h1><!><!></div>`);
var root_6 = /* @__PURE__ */ from_html(`<article><!> <!> <!> <!></article>`);
function TileFrame($$anchor, $$props) {
	push($$props, true);
	const dispatch = dispatchUi();
	var article = root_6();
	let classes;
	var node = child(article);
	var consequent_1 = ($$anchor) => {
		var header = root_2();
		let classes_1;
		var div = child(header);
		var div_1 = child(div);
		var text = only_child(sibling(child(div_1)), true);
		reset(div_1);
		var div_2 = sibling(div_1, 2);
		var button = child(div_2);
		var text_1 = only_child(button, true);
		reset(div_2);
		reset(div);
		var node_1 = sibling(div, 2);
		var consequent = ($$anchor) => {
			var div_3 = root_1$1();
			each(div_3, 21, () => $$props.model.tabs, (tab) => tab.tile_id, ($$anchor, tab) => {
				var button_1 = root$3();
				let classes_2;
				var text_2 = only_child(button_1, true);
				template_effect(() => {
					set_attribute(button_1, "aria-selected", get(tab).active);
					classes_2 = set_class(button_1, 1, "", null, classes_2, { active: get(tab).active });
					set_text(text_2, get(tab).title);
				});
				delegated("click", button_1, () => dispatch({
					type: "focus_changed",
					target: get(tab).tile_id
				}));
				append($$anchor, button_1);
			});
			reset(div_3);
			append($$anchor, div_3);
		};
		if_block(node_1, ($$render) => {
			if ($$props.model.tabs.length > 1) $$render(consequent);
		});
		reset(header);
		template_effect(() => {
			classes_1 = set_class(header, 1, "tile-header", null, classes_1, { grouped: $$props.model.group_label });
			set_text(text, $$props.model.group_label ?? $$props.model.title);
			set_attribute(button, "aria-label", $$props.model.maximized ? "Restore view" : "Maximize view");
			set_attribute(button, "title", $$props.model.maximized ? "Restore view" : "Maximize view");
			set_text(text_1, $$props.model.maximized ? "↙" : "↗");
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
		if (!$$props.model.chrome_hidden) $$render(consequent_1);
	});
	var node_2 = sibling(node, 2);
	var consequent_4 = ($$anchor) => {
		var div_4 = root_5();
		var small = child(div_4);
		var text_3 = only_child(small, true);
		var h1 = sibling(small);
		var text_4 = only_child(h1, true);
		var node_3 = sibling(h1);
		var consequent_2 = ($$anchor) => {
			var p = root_3();
			var text_5 = only_child(p, true);
			template_effect(() => set_text(text_5, $$props.model.heading.summary));
			append($$anchor, p);
		};
		if_block(node_3, ($$render) => {
			if ($$props.model.heading.summary) $$render(consequent_2);
		});
		var node_4 = sibling(node_3);
		var consequent_3 = ($$anchor) => {
			var div_5 = root_4();
			var text_6 = only_child(div_5, true);
			template_effect(($0) => set_text(text_6, $0), [() => $$props.model.heading.metadata.join("  /  ")]);
			append($$anchor, div_5);
		};
		if_block(node_4, ($$render) => {
			if ($$props.model.heading.metadata.length) $$render(consequent_3);
		});
		reset(div_4);
		template_effect(() => {
			set_text(text_3, $$props.model.heading.eyebrow);
			set_text(text_4, $$props.model.heading.title);
		});
		append($$anchor, div_4);
	};
	if_block(node_2, ($$render) => {
		if ($$props.model.heading) $$render(consequent_4);
	});
	var node_5 = sibling(node_2, 2);
	ViewRenderer(node_5, {
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
	var node_6 = sibling(node_5, 2);
	var consequent_5 = ($$anchor) => {
		InputComposer($$anchor, { get model() {
			return $$props.model.input;
		} });
	};
	if_block(node_6, ($$render) => {
		if ($$props.model.input) $$render(consequent_5);
	});
	reset(article);
	template_effect(() => {
		classes = set_class(article, 1, "tile-frame", null, classes, {
			focused: $$props.model.focused,
			transparent: $$props.model.background_transparent
		});
		set_attribute(article, "data-instance", $$props.model.instance_key);
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
var root$2 = /* @__PURE__ */ from_html(`<div class="split"><!> <div class="split-divider" role="separator"></div> <!></div>`);
function LayoutNode_1($$anchor, $$props) {
	push($$props, true);
	var fragment = comment();
	var node = first_child(fragment);
	var consequent = ($$anchor) => {
		TileFrame($$anchor, { get model() {
			return $$props.model;
		} });
	};
	var alternate = ($$anchor) => {
		var div = root$2();
		var node_1 = child(div);
		LayoutNode_1(node_1, { get model() {
			return $$props.model.first;
		} });
		var div_1 = sibling(node_1, 2);
		LayoutNode_1(sibling(div_1, 2), { get model() {
			return $$props.model.second;
		} });
		reset(div);
		template_effect(() => {
			set_attribute(div, "data-axis", $$props.model.axis);
			set_style(div, `--split:${$$props.model.ratio}`);
			set_attribute(div_1, "aria-orientation", $$props.model.axis === "horizontal" ? "vertical" : "horizontal");
		});
		append($$anchor, div);
	};
	if_block(node, ($$render) => {
		if ($$props.model.type === "tile") $$render(consequent);
		else $$render(alternate, -1);
	});
	append($$anchor, fragment);
	pop();
}
//#endregion
//#region browser/layout/ViewSet.svelte
var root$1 = /* @__PURE__ */ from_html(`<div class="view-set-backdrop" aria-label="Empty view set"></div>`);
var root_1 = /* @__PURE__ */ from_html(`<main><!> <div class="view-set-middle"><!> <section class="view-set-center"><!></section> <!></div> <!></main>`);
function ViewSet($$anchor, $$props) {
	push($$props, true);
	const maximized = /* @__PURE__ */ user_derived(() => $$props.model.root?.type === "tile" && $$props.model.root.maximized);
	var main = root_1();
	let classes;
	var node = child(main);
	var consequent = ($$anchor) => {
		DockSlot($$anchor, { get model() {
			return $$props.model.docks.top;
		} });
	};
	if_block(node, ($$render) => {
		if (!get(maximized) && $$props.model.docks.top) $$render(consequent);
	});
	var div = sibling(node, 2);
	var node_1 = child(div);
	var consequent_1 = ($$anchor) => {
		DockSlot($$anchor, { get model() {
			return $$props.model.docks.left;
		} });
	};
	if_block(node_1, ($$render) => {
		if (!get(maximized) && $$props.model.docks.left) $$render(consequent_1);
	});
	var section = sibling(node_1, 2);
	var node_2 = child(section);
	var consequent_2 = ($$anchor) => {
		LayoutNode_1($$anchor, { get model() {
			return $$props.model.root;
		} });
	};
	var consequent_3 = ($$anchor) => {
		SceneView($$anchor, { get scene() {
			return $$props.model.backdrop;
		} });
	};
	var alternate = ($$anchor) => {
		append($$anchor, root$1());
	};
	if_block(node_2, ($$render) => {
		if ($$props.model.root) $$render(consequent_2);
		else if ($$props.model.backdrop) $$render(consequent_3, 1);
		else $$render(alternate, -1);
	});
	reset(section);
	var node_3 = sibling(section, 2);
	var consequent_4 = ($$anchor) => {
		DockSlot($$anchor, { get model() {
			return $$props.model.docks.right;
		} });
	};
	if_block(node_3, ($$render) => {
		if (!get(maximized) && $$props.model.docks.right) $$render(consequent_4);
	});
	reset(div);
	var node_4 = sibling(div, 2);
	var consequent_5 = ($$anchor) => {
		DockSlot($$anchor, { get model() {
			return $$props.model.docks.bottom;
		} });
	};
	if_block(node_4, ($$render) => {
		if (!get(maximized) && $$props.model.docks.bottom) $$render(consequent_5);
	});
	reset(main);
	template_effect(() => classes = set_class(main, 1, "view-set", null, classes, {
		empty: $$props.model.center_is_empty,
		maximized: get(maximized)
	}));
	append($$anchor, main);
	pop();
}
//#endregion
//#region browser/app/RyeOs.svelte
var root = /* @__PURE__ */ from_html(`<div class="ryeos-shell"><!> <!> <!> <div><!> <!></div> <!> <!> <!></div>`);
function RyeOs($$anchor, $$props) {
	push($$props, true);
	let replacement = /* @__PURE__ */ state(null);
	let envelope = /* @__PURE__ */ user_derived(() => get(replacement) ?? $$props.initialEnvelope);
	provideDispatchUi((event) => $$props.dispatchUi(event));
	/** Replace the sole renderer-owned root cell with one committed envelope. */
	function replaceEnvelope(next) {
		set(replacement, next, true);
	}
	var $$exports = { replaceEnvelope };
	var div = root();
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
	SystemBar(node_1, {
		get chrome() {
			return get(envelope).view_model.chrome;
		},
		get session() {
			return get(envelope).view_model.session;
		},
		get transport() {
			return get(envelope).view_model.transport;
		}
	});
	var node_2 = sibling(node_1, 2);
	ViewSetStrip(node_2, { get model() {
		return get(envelope).view_model.presentation.chrome.top_bar;
	} });
	var div_1 = sibling(node_2, 2);
	let classes;
	var node_3 = child(div_1);
	var consequent_1 = ($$anchor) => {
		Navigation($$anchor, { get model() {
			return get(envelope).view_model.navigation;
		} });
	};
	if_block(node_3, ($$render) => {
		if (get(envelope).view_model.navigation.items.length > 0) $$render(consequent_1);
	});
	ViewSet(sibling(node_3, 2), { get model() {
		return get(envelope).view_model.view_set;
	} });
	reset(div_1);
	var node_5 = sibling(div_1, 2);
	StatusBar(node_5, { get model() {
		return get(envelope).view_model.presentation.chrome.status_bar;
	} });
	var node_6 = sibling(node_5, 2);
	Notices(node_6, { get notices() {
		return get(envelope).view_model.notices;
	} });
	each(sibling(node_6, 2), 17, () => get(envelope).view_model.overlays, (overlay) => overlay.id, ($$anchor, overlay) => {
		OverlayLayer($$anchor, { get model() {
			return get(overlay);
		} });
	});
	reset(div);
	template_effect(($0) => {
		set_attribute(div, "data-generation", $0);
		set_attribute(div, "data-theme", get(envelope).view_model.presentation.theme.id);
		classes = set_class(div_1, 1, "shell-body", null, classes, { "with-navigation": get(envelope).view_model.navigation.items.length > 0 });
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
		if (snapshot.focus && !snapshot.focus.key.startsWith("overlay:")) modalReturnFocus.set(root, snapshot.focus);
		return;
	}
	let focus = snapshot.focus ?? modalReturnFocus.get(root) ?? null;
	if (!focus) return;
	let target = root.querySelector(`[data-focus-key="${cssEscape(focus.key)}"]`);
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
async function runEffect(effect) {
	const kind = effect.kind;
	switch (kind.type) {
		case "fetch_source": return success(effect, "source_data", unwrapResult$1(await dispatchBinding(kind.request, kind.request_bounds)));
		case "invoke_binding": return success(effect, "binding_invoked", unwrapResult$1(await dispatchBinding(kind.request, kind.request_bounds)));
		case "set_location_hash":
			location.hash = kind.hash;
			return success(effect, "browser_only", null);
		case "copy_to_clipboard":
			await navigator.clipboard.writeText(kind.text);
			return success(effect, "browser_only", null);
		case "open_url":
			window.open(kind.url, "_blank", "noopener,noreferrer");
			return success(effect, "browser_only", null);
		case "replace_session": {
			const launch = new URL(kind.launch_url, location.href);
			if (launch.origin !== location.origin || !launch.pathname.startsWith("/ui/launch/")) throw new Error("replacement UI session has an invalid launch origin or path");
			location.assign(`${launch.pathname}${launch.search}${launch.hash}`);
			return success(effect, "browser_only", null);
		}
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
async function dispatchBinding(request, bounds) {
	const maximum = safeBound(bounds.max_request_bytes, "max_request_bytes");
	const inputMaximum = safeBound(bounds.max_input_bytes, "max_input_bytes");
	const encoded = encodeJsonBody(request);
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
	if (effect.kind.type === "fetch_source") return "source_data";
	if (effect.kind.type === "invoke_binding") return "binding_invoked";
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
function isTypingTarget(target) {
	return target instanceof Element && target.closest("input, textarea, select, [contenteditable='true']") !== null;
}
function isNativeActivationTarget(target) {
	return target instanceof Element && target.closest("button, a, summary") !== null;
}
//#endregion
//#region browser/runtime/layout-preferences.ts
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
	if (options.persisted !== null && lastObserved !== options.persisted) queue(lastObserved);
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
//#region browser/runtime/session.ts
var MAX_SEAT_BATCH_EVENTS = 64;
var MAX_SEAT_APPEND_ATTEMPTS = 5;
var MAX_SEAT_APPEND_BODY_BYTES = 65536;
var MAX_PENDING_SEAT_EVENTS = 512;
var MAX_PENDING_SEAT_BYTES = 524288;
function createSessionRuntime(options) {
	const pendingStorageKey = `ryeos.ui.seat.pending.v1:${options.sessionId}`;
	let seatThreadId = null;
	let seatProducer = null;
	let nextEngineSeq = 0n;
	let seatObserved = 0;
	const pendingSeatEvents = [];
	let pendingSeatBytes = 0;
	let seatAttaching = false;
	let seatReady = false;
	let seatSyncing = false;
	let seatSyncBlocked = false;
	let closed = false;
	let seatEpoch = 0;
	let heartbeat = null;
	let events = null;
	let eventsOpened = false;
	let tail = null;
	let tailUrl = null;
	let tailThreadId = null;
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
	async function attachSeat() {
		const epoch = ++seatEpoch;
		seatAttaching = true;
		seatSyncBlocked = false;
		const startupBaseline = options.seatEvents().slice();
		seatObserved = startupBaseline.length;
		await reconcileRetainedAppend();
		if (closed || epoch !== seatEpoch) return;
		const opened = unwrapResult(await invokeSeat("open", {}));
		if (closed || epoch !== seatEpoch) return;
		seatThreadId = stringField(opened, "thread_id");
		if (!seatThreadId) throw new Error("seat/open returned no durable seat thread");
		seatProducer = requiredStringField(opened, "producer_incarnation");
		nextEngineSeq = exactIntegerField(opened, "next_engine_seq");
		capturePendingSeatEvents();
		if (heartbeat !== null) window.clearInterval(heartbeat);
		heartbeat = window.setInterval(() => {
			if (seatThreadId) invokeSeat("touch", { thread_id: seatThreadId });
		}, 6e4);
		if (opened.reattached === true) {
			let cursor = null;
			do {
				const replay = unwrapResult(await invokeSeat("replay", {
					chain_root_id: seatThreadId,
					after_chain_seq: cursor,
					limit: 500
				}));
				if (closed || epoch !== seatEpoch) return;
				capturePendingSeatEvents();
				const replayEvents = Array.isArray(replay.events) ? replay.events : [];
				if (replayEvents.length > 0) options.replaySeatEvents(replayEvents);
				seatObserved = options.seatEvents().length;
				cursor = optionalExactIntegerField(replay, "next_cursor");
			} while (cursor !== null);
			if (nextEngineSeq === 0n) prependPendingSeatEvents(startupBaseline);
			else if (pendingSeatEvents.length > 0) {
				const rebased = wireSeatEvents(pendingSeatEvents, nextEngineSeq).map((event) => ({
					event_type: event.event_type,
					payload: {
						seq: event.engine_seq,
						payload: event.payload
					}
				}));
				options.replaySeatEvents(rebased);
				seatObserved = options.seatEvents().length;
			}
		} else prependPendingSeatEvents(startupBaseline);
		capturePendingSeatEvents();
		seatAttaching = false;
		seatReady = true;
		syncSeat();
	}
	async function reconcileRetainedAppend() {
		const retained = loadRetainedAppend(pendingStorageKey);
		if (!retained) return;
		if (retained.session_id !== options.sessionId) throw new Error("retained seat append belongs to a different browser session");
		await appendWithExactRetry(retained.encoded_request, {
			producer: retained.producer_incarnation,
			operationId: retained.operation_id,
			firstEngineSeq: BigInt(retained.first_engine_seq),
			lastEngineSeq: BigInt(retained.last_engine_seq),
			eventCount: retained.event_count,
			payloadDigest: retained.payload_digest
		}, options.sessionId);
		clearRetainedAppend(pendingStorageKey);
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
	function prependPendingSeatEvents(events) {
		let encodedBytes;
		try {
			encodedBytes = events.reduce((total, event) => total + encodedSeatEventBytes(event), 0);
		} catch (error) {
			seatSyncBlocked = true;
			reportSeatFailure(error, false);
			return;
		}
		if (pendingSeatEvents.length + events.length > MAX_PENDING_SEAT_EVENTS || pendingSeatBytes + encodedBytes > MAX_PENDING_SEAT_BYTES) {
			seatSyncBlocked = true;
			reportSeatFailure(/* @__PURE__ */ new Error(`unsaved seat history exceeds the pending limit (${MAX_PENDING_SEAT_EVENTS} events / ${MAX_PENDING_SEAT_BYTES} bytes)`), false);
			return;
		}
		pendingSeatEvents.unshift(...events);
		pendingSeatBytes += encodedBytes;
	}
	function syncTail(model) {
		tailThreadId = model.tail_thread_id ?? model.tail_chain_root_id ?? null;
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
				if (!isRecord(frame) || typeof frame.event_type !== "string" || !tailThreadId) return;
				options.commitEvent({
					type: "thread_tail",
					thread_id: tailThreadId,
					event_type: frame.event_type,
					payload: frame.payload ?? null
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
		if (closed || !seatThreadId || !seatProducer || seatSyncing || seatSyncBlocked) return;
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
			operationId = crypto.randomUUID();
			while (batchEvents.length > 0) {
				const candidate = wireSeatEvents(batchEvents, firstEngineSeq);
				const candidateLast = firstEngineSeq + BigInt(candidate.length - 1);
				if (encodedByteLength(encodeJsonBody({
					thread_id: threadId,
					producer_incarnation: producer,
					operation_id: operationId,
					first_engine_seq: firstEngineSeq,
					last_engine_seq: candidateLast,
					event_count: candidate.length,
					payload_digest: "0".repeat(64),
					events: candidate
				})) <= MAX_SEAT_APPEND_BODY_BYTES) break;
				batchEvents = batchEvents.slice(0, -1);
			}
			if (batchEvents.length === 0) throw new Error(`one seat event exceeds the ${MAX_SEAT_APPEND_BODY_BYTES}-byte append route limit`);
			batch = wireSeatEvents(batchEvents, firstEngineSeq);
			lastEngineSeq = firstEngineSeq + BigInt(batch.length - 1);
			payloadDigest = await sha256Hex(encodeSeatPayloadDigest(batch));
			encodedRequest = encodeJsonBody({
				thread_id: threadId,
				producer_incarnation: producer,
				operation_id: operationId,
				first_engine_seq: firstEngineSeq,
				last_engine_seq: lastEngineSeq,
				event_count: batch.length,
				payload_digest: payloadDigest,
				events: batch
			});
			if (encodedByteLength(encodedRequest) > MAX_SEAT_APPEND_BODY_BYTES) throw new Error("seat append exceeded its route limit after final encoding");
			retainAppend(pendingStorageKey, {
				schema_version: "ryeos.ui.seat.pending-append.v1",
				session_id: options.sessionId,
				thread_id: threadId,
				producer_incarnation: producer,
				operation_id: operationId,
				first_engine_seq: firstEngineSeq.toString(),
				last_engine_seq: lastEngineSeq.toString(),
				event_count: batch.length,
				payload_digest: payloadDigest,
				encoded_request: encodedRequest
			});
		} catch (error) {
			seatSyncBlocked = true;
			seatSyncing = false;
			reportSeatFailure(error, false);
			return;
		}
		let acknowledged = false;
		try {
			await appendWithExactRetry(encodedRequest, {
				producer,
				operationId,
				firstEngineSeq,
				lastEngineSeq,
				eventCount: batch.length,
				payloadDigest
			}, options.sessionId);
			if (closed || epoch !== seatEpoch || threadId !== seatThreadId || producer !== seatProducer) return;
			clearRetainedAppend(pendingStorageKey);
			pendingSeatEvents.splice(0, batch.length);
			pendingSeatBytes -= batchEvents.reduce((total, event) => total + encodedSeatEventBytes(event), 0);
			nextEngineSeq = lastEngineSeq + 1n;
			acknowledged = true;
		} catch (error) {
			if (closed || epoch !== seatEpoch || threadId !== seatThreadId) return;
			seatSyncBlocked = true;
			reportSeatFailure(error, error instanceof UnknownDeliveryError || isRetryableResponse(error));
			console.warn("RyeOS seat sync stopped with unsaved history", error);
		} finally {
			if (epoch === seatEpoch) seatSyncing = false;
			if (acknowledged && !closed) {
				capturePendingSeatEvents();
				if (pendingSeatEvents.length > 0) syncSeat();
			}
		}
	}
	function reportSeatFailure(error, retryable) {
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
	}
	function close() {
		if (closed) return;
		closed = true;
		seatEpoch += 1;
		events?.close();
		tail?.close();
		if (heartbeat !== null) window.clearInterval(heartbeat);
		if (hintTimer !== null) window.clearTimeout(hintTimer);
		if (seatThreadId && !seatSyncing && pendingSeatEvents.length === 0 && !hasRetainedAppend(pendingStorageKey)) {
			const body = JSON.stringify({ thread_id: seatThreadId });
			navigator.sendBeacon?.("/ui/api/session/seat/close", new Blob([body], { type: "application/json" }));
		}
		events = null;
		tail = null;
		heartbeat = null;
		hintTimer = null;
	}
	attachEvents();
	return {
		attachSeat,
		observe,
		close
	};
}
async function invokeSeat(operation, body) {
	return postJson(`/ui/api/session/seat/${operation}`, body);
}
async function appendWithExactRetry(encodedRequest, expected, sessionId) {
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
		requireAppendAcknowledgement(response, expected);
		return;
	} catch (error) {
		if (isAuthenticationResponse(error) && !authenticationReconciled) {
			await requireCurrentSession(sessionId);
			authenticationReconciled = true;
			continue;
		}
		if (!isRetryableResponse(error) || attempt >= MAX_SEAT_APPEND_ATTEMPTS) throw error;
		await new Promise((resolve) => {
			globalThis.setTimeout(resolve, Math.min(2e3, 100 * 2 ** (attempt - 1)));
		});
	}
}
function isAuthenticationResponse(error) {
	return error instanceof HttpResponseError && (error.status === 401 || error.status === 403);
}
async function requireCurrentSession(expectedSessionId) {
	if (requiredStringField(unwrapResult(await getJson("/ui/api/session/current")), "session_id") !== expectedSessionId) throw new Error("browser session changed while reconciling a seat append");
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
function clearRetainedAppend(key) {
	localStorage.removeItem(key);
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
	if (!isRecord(value) || value.schema_version !== "ryeos.ui.seat.pending-append.v1" || typeof value.session_id !== "string" || typeof value.thread_id !== "string" || typeof value.producer_incarnation !== "string" || typeof value.operation_id !== "string" || typeof value.first_engine_seq !== "string" || !/^(0|[1-9][0-9]*)$/.test(value.first_engine_seq) || typeof value.last_engine_seq !== "string" || !/^(0|[1-9][0-9]*)$/.test(value.last_engine_seq) || typeof value.event_count !== "number" || !Number.isSafeInteger(value.event_count) || value.event_count <= 0 || typeof value.payload_digest !== "string" || typeof value.encoded_request !== "string") throw new Error("retained seat append has an invalid schema");
	return value;
}
function hasRetainedAppend(key) {
	try {
		return loadRetainedAppend(key) !== null;
	} catch {
		return true;
	}
}
function requireAppendAcknowledgement(response, expected) {
	if (requiredStringField(response, "producer_incarnation") !== expected.producer || requiredStringField(response, "operation_id") !== expected.operationId || exactIntegerField(response, "first_engine_seq") !== expected.firstEngineSeq || exactIntegerField(response, "last_engine_seq") !== expected.lastEngineSeq || exactIntegerField(response, "event_count") !== BigInt(expected.eventCount) || exactIntegerField(response, "appended") !== BigInt(expected.eventCount) || requiredStringField(response, "payload_digest") !== expected.payloadDigest) throw new UnknownDeliveryError("seat append acknowledgement does not match the submitted operation");
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
function optionalExactIntegerField(value, name) {
	const field = value[name];
	if (field === null || field === void 0) return null;
	return parseExactInteger(field, name);
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
	return events.map((event, offset) => ({
		engine_seq: firstEngineSeq + BigInt(offset),
		event_type: eventType(event),
		payload: eventPayload(event)
	}));
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
		"ryeos_replay_seat_events",
		"ryeos_layout_preference_key",
		"ryeos_export_layout_preferences",
		"ryeos_restore_layout_preferences",
		"ryeos_export_active_view_set_template",
		"ryeos_open_saved_view_set_template"
	].every((name) => typeof exports[name] === "function");
}
//#endregion
//#region browser/runtime/boot.ts
/** Enter the installed browser surface from the generated index document. */
async function bootRyeOsDocument() {
	const root = document.getElementById("app");
	if (!root) throw new Error("RyeOS browser root #app is missing");
	setBootStage("Joining node", "Opening the authenticated browser session and durable seat.");
	try {
		return await bootRyeOs(root);
	} catch (error) {
		renderBootFailure(root, error);
		throw error;
	}
}
async function bootRyeOs(root) {
	const wasm = await loadWasm();
	await wasm.default({ module_or_path: "/ui/assets/ryeos_web_bg.wasm" });
	const sessionUnknown = await getJson("/ui/api/session/current");
	const initial = wasm.ryeos_start(sessionUnknown, viewport(), BigInt(Date.now()));
	let runtime;
	let sessionRuntime = null;
	let preferences = null;
	let renderer;
	let closed = false;
	let seatAttached = false;
	const queuedUiEvents = [];
	const dispatchUi = (event) => {
		if (!seatAttached) {
			queuedUiEvents.push(event);
			return;
		}
		runtime.enqueueEvent({
			type: "ui",
			event
		});
	};
	renderer = mountRyeOsRenderer(root, initial, dispatchUi);
	runtime = createCommitRuntime({
		reduce: (event) => wasm.ryeos_dispatch(event),
		applyEffectResult: (result) => wasm.ryeos_apply_effect_result(result),
		startEffect: runEffect,
		failedEffectResult,
		async render(envelope) {
			const browserState = captureBrowserPresentation(root);
			renderer.replaceEnvelope(envelope);
			await Promise.resolve();
			restoreBrowserPresentation(root, browserState);
			preferences?.observeAcceptedState();
			sessionRuntime?.observe(envelope.view_model);
		}
	});
	sessionRuntime = createSessionRuntime({
		sessionId: sessionId(sessionUnknown),
		eventsUrl: sessionEventsUrl(sessionUnknown),
		seatEvents: () => wasm.ryeos_seat_events(),
		commitEvent: (event) => runtime.enqueueEvent(event),
		replaySeatEvents: (events) => runtime.commitMutation(() => wasm.ryeos_replay_seat_events(events))
	});
	runtime.enqueueEnvelope(initial);
	await sessionRuntime.attachSeat();
	seatAttached = true;
	for (const event of queuedUiEvents.splice(0)) runtime.enqueueEvent({
		type: "ui",
		event
	});
	configurePreferences(wasm, runtime, (next) => {
		preferences = next;
	});
	if (location.hash) runtime.enqueueEvent({
		type: "route_changed",
		route: location.hash.replace(/^#/, "")
	});
	const detachBrowserEvents = attachBrowserEvents(wasm, runtime);
	return { async close() {
		if (closed) return;
		closed = true;
		detachBrowserEvents();
		preferences?.flush();
		sessionRuntime?.close();
		await runtime.idle();
		await renderer.destroy();
	} };
}
function sessionEventsUrl(session) {
	if (typeof session !== "object" || session === null || Array.isArray(session)) throw new Error("authenticated browser session is not an object");
	const value = session.events_url;
	if (value === void 0 || value === null) return null;
	if (typeof value !== "string") throw new Error("authenticated browser session events_url is invalid");
	return value;
}
function sessionId(session) {
	if (typeof session !== "object" || session === null || Array.isArray(session)) throw new Error("authenticated browser session is not an object");
	const value = session.session_id;
	if (typeof value !== "string" || value.length === 0) throw new Error("authenticated browser session has no session_id");
	return value;
}
function configurePreferences(wasm, runtime, assign) {
	try {
		const key = wasm.ryeos_layout_preference_key();
		const saved = localStorage.getItem(key);
		const reportError = (error) => console.warn("RyeOS layout preference was not applied", error);
		if (saved !== null) try {
			runtime.commitMutation(() => wasm.ryeos_restore_layout_preferences(saved));
		} catch (error) {
			reportError(error);
		}
		assign(createLayoutPreferencePersistence({
			key,
			persisted: saved,
			readCurrent: wasm.ryeos_export_layout_preferences,
			storage: localStorage,
			reportError
		}));
	} catch (error) {
		console.warn("RyeOS layout preference storage is unavailable", error);
		assign(null);
	}
}
function attachBrowserEvents(wasm, runtime) {
	const abort = new AbortController();
	const options = { signal: abort.signal };
	window.addEventListener("keydown", (event) => {
		if (event.defaultPrevented) return;
		if (isTypingTarget(event.target)) return;
		const key = keyEvent(event);
		if (!key) return;
		if (key.key === "enter" && !hasModifiers(key) && isNativeActivationTarget(event.target)) return;
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
//#region browser/entry.ts
bootRyeOsDocument();
//#endregion
export { bootRyeOs, bootRyeOsDocument, captureBrowserPresentation, createCommitRuntime, decodeJsonBody, encodeCanonicalJsonBody, encodeJsonBody, encodeSeatPayloadDigest, mountRyeOsRenderer, restoreBrowserPresentation };
