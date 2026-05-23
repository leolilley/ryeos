/* tslint:disable */
/* eslint-disable */

/**
 * Dispatch a keyboard event.
 */
export function on_key(key_code: number, shift: boolean, ctrl: boolean, alt: boolean): void;

/**
 * Resize the viewport.
 */
export function on_resize(width: number, height: number): void;

/**
 * Initialize the app with viewport dimensions.
 */
export function start(width: number, height: number): void;

/**
 * Advance animation by dt milliseconds.
 */
export function tick(dt_ms: number): void;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly on_key: (a: number, b: number, c: number, d: number) => void;
    readonly on_resize: (a: number, b: number) => void;
    readonly start: (a: number, b: number) => void;
    readonly tick: (a: number) => void;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
