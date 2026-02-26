// rye:signed:2026-02-26T05:52:24Z:f3a7c36f42b746420bbc4829c50e3da78e3826332d10c0113a48b3ae5f3dee49:oHPe9QU81VgfWVYnX5JUzkywQ5Fz-N-Kt5QasrccwkcgP36RLwpolBW_qMz7mnK7Y_mPeJ4BPVBf2yrlP8OoAQ==:4b987fd4e40303ac
// rye:unsigned
import { parseArgs } from "node:util";
import { execSync } from "node:child_process";
import { resolve, isAbsolute, join, dirname } from "node:path";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { parse as parseYaml } from "node:querystring";

const __tooldir = dirname(fileURLToPath(import.meta.url));

export const __version__ = "1.0.0";
export const __tool_type__ = "javascript";
export const __executor_id__ = "rye/core/runtimes/node/node";
export const __category__ = "rye/web/browser";
export const __tool_description__ =
  "Browser automation via playwright-cli — open pages, screenshot, interact with elements, manage sessions";

export const CONFIG_SCHEMA = {
  type: "object",
  properties: {
    command: {
      type: "string",
      enum: [
        "open",
        "goto",
        "screenshot",
        "snapshot",
        "click",
        "fill",
        "type",
        "select",
        "hover",
        "resize",
        "console",
        "network",
        "eval",
        "press",
        "tab-list",
        "tab-new",
        "tab-select",
        "tab-close",
        "close",
        "close-all",
      ],
      description: "playwright-cli command to execute",
    },
    args: {
      type: "array",
      items: { type: "string" },
      default: [],
      description:
        "Positional arguments for the command (URL for open/goto, element ref for click, etc.)",
    },
    flags: {
      type: "object",
      default: {},
      description:
        "Named flags (e.g. { headed: true, filename: 'page.png' })",
    },
    session: {
      type: "string",
      default: "rye",
      description: "Named session for browser isolation between directive threads",
    },
    timeout: {
      type: "integer",
      default: 30,
      description: "Command timeout in seconds",
    },
  },
  required: ["command"],
};

const MAX_OUTPUT_BYTES = 51200;
const DEFAULT_TIMEOUT = 30;
const DEFAULT_SESSION = "rye";

const VALID_COMMANDS = new Set([
  "open",
  "goto",
  "screenshot",
  "snapshot",
  "click",
  "fill",
  "type",
  "select",
  "hover",
  "resize",
  "console",
  "network",
  "eval",
  "press",
  "tab-list",
  "tab-new",
  "tab-select",
  "tab-close",
  "close",
  "close-all",
]);

// Default browser config — uses Playwright's bundled chromium (no channel)
const DEFAULT_BROWSER_CONFIG = {
  browser: {
    browserName: "chromium",
    launchOptions: {
      channel: "chromium",
      headless: true,
    },
  },
};

interface Params {
  command: string;
  args?: string[];
  flags?: Record<string, boolean | string>;
  session?: string;
  timeout?: number;
}

interface Result {
  success: boolean;
  output?: string;
  stdout?: string;
  stderr?: string;
  exit_code?: number;
  error?: string;
  truncated?: boolean;
  command?: string;
  session?: string;
  screenshot_path?: string;
  snapshot_path?: string;
}

function shellQuote(arg: string): string {
  if (/^[a-zA-Z0-9_./:=@-]+$/.test(arg)) return arg;
  return `'${arg.replace(/'/g, "'\\''")}'`;
}

function truncateOutput(output: string, maxBytes: number): [string, boolean] {
  const encoded = Buffer.from(output, "utf-8");
  if (encoded.length <= maxBytes) return [output, false];

  const truncated = encoded.subarray(0, maxBytes).toString("utf-8");
  return [
    truncated + `\n... [output truncated, ${encoded.length} bytes total]`,
    true,
  ];
}

function buildFlags(flags: Record<string, boolean | string>): string[] {
  const result: string[] = [];
  for (const [key, value] of Object.entries(flags)) {
    const flag = key.length === 1 ? `-${key}` : `--${key.replace(/_/g, "-")}`;
    if (value === true) {
      result.push(flag);
    } else if (typeof value === "string") {
      result.push(flag, value);
    }
  }
  return result;
}

function ensureCacheDir(projectPath: string, subdir: string): string {
  const dir = join(
    projectPath,
    ".ai",
    "cache",
    "tools",
    "rye",
    "web",
    "browser",
    subdir,
  );
  mkdirSync(dir, { recursive: true });
  return dir;
}

/**
 * Resolve browser config from project → user → system (default).
 * Follows the same pattern as websearch.yaml config resolution.
 * Config file: .ai/config/web/browser.json
 */
function resolveBrowserConfig(projectPath: string): Record<string, any> {
  const userSpace = process.env.USER_SPACE || process.env.HOME || "";
  const configPaths = [
    join(projectPath, ".ai", "config", "web", "browser.json"),
    join(userSpace, ".ai", "config", "web", "browser.json"),
  ];

  for (const configPath of configPaths) {
    if (existsSync(configPath)) {
      try {
        return JSON.parse(readFileSync(configPath, "utf-8"));
      } catch {
        continue;
      }
    }
  }

  return DEFAULT_BROWSER_CONFIG;
}

/**
 * Ensure playwright-cli config is written to the project cache dir.
 * Returns the path to the config file for --config flag.
 * 
 * Config is written to .ai/cache/tools/rye/web/browser/.playwright/cli.config.json
 * so that playwright-cli's .playwright-cli/ output also lands in the cache dir
 * (not inside the tool dir, where it would trip integrity checks).
 */
function ensurePlaywrightConfig(projectPath: string): string {
  const config = resolveBrowserConfig(projectPath);
  const cacheDir = ensureCacheDir(projectPath, ".playwright");
  const configPath = join(cacheDir, "cli.config.json");
  writeFileSync(configPath, JSON.stringify(config, null, 2));
  return configPath;
}

/**
 * Get the browser cache working directory.
 * playwright-cli uses cwd as the workspace root for .playwright-cli/ output.
 */
function getBrowserCacheDir(projectPath: string): string {
  return ensureCacheDir(projectPath, "");
}

function buildCommand(params: Params, projectPath: string): string[] {
  const session = params.session ?? DEFAULT_SESSION;
  const args = params.args ?? [];
  const userFlags = params.flags ? { ...params.flags } : {};

  // For screenshot command, auto-generate filename in cache dir if not specified
  if (params.command === "screenshot" && !userFlags["filename"]) {
    const screenshotsDir = ensureCacheDir(projectPath, "screenshots");
    const timestamp = Math.floor(Date.now() / 1000);
    userFlags["filename"] = join(screenshotsDir, `screenshot-${timestamp}.png`);
  }

  // For snapshot command, auto-generate filename in cache dir if not specified
  if (params.command === "snapshot" && !userFlags["filename"]) {
    const snapshotsDir = ensureCacheDir(projectPath, "snapshots");
    const timestamp = Math.floor(Date.now() / 1000);
    userFlags["filename"] = join(snapshotsDir, `snapshot-${timestamp}.yaml`);
  }

  const flags = buildFlags(userFlags);

  return [
    "playwright-cli",
    `-s=${session}`,
    params.command,
    ...args,
    ...flags,
  ];
}

function execute(params: Params, projectPath: string): Result {
  const project = resolve(projectPath);
  const session = params.session ?? DEFAULT_SESSION;

  if (!params.command) {
    return { success: false, error: "Missing required parameter: command" };
  }

  if (!VALID_COMMANDS.has(params.command)) {
    return {
      success: false,
      error: `Unknown command: ${params.command}. Valid commands: ${[...VALID_COMMANDS].join(", ")}`,
    };
  }

  const timeout = (params.timeout ?? DEFAULT_TIMEOUT) * 1000;

  // Write config to project cache dir and get path for --config flag
  const configPath = ensurePlaywrightConfig(project);
  const browserCacheDir = getBrowserCacheDir(project);

  let cmd: string[];
  try {
    cmd = buildCommand(params, project);
  } catch (e: any) {
    return { success: false, error: e.message };
  }

  // Insert --config flag after session flag
  cmd.splice(2, 0, `--config=${configPath}`);

  const cmdStr = cmd.map(shellQuote).join(" ");

  try {
    // Run from browser cache dir so playwright-cli output lands there
    const output = execSync(cmdStr, {
      cwd: browserCacheDir,
      timeout,
      encoding: "utf-8",
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env },
    });

    const [stdout, truncated] = truncateOutput(output ?? "", MAX_OUTPUT_BYTES);

    const result: Result = {
      success: true,
      output: stdout,
      stdout,
      stderr: "",
      exit_code: 0,
      truncated,
      command: cmdStr,
      session,
    };

    // Include artifact paths in result
    const flags = params.flags ?? {};
    if (params.command === "screenshot") {
      result.screenshot_path =
        (flags["filename"] as string) ??
        buildScreenshotPath(project);
    }
    if (params.command === "snapshot") {
      result.snapshot_path =
        (flags["filename"] as string) ??
        buildSnapshotPath(project);
    }

    return result;
  } catch (e: any) {
    if (e.killed) {
      return {
        success: false,
        error: `Command timed out after ${params.timeout ?? DEFAULT_TIMEOUT} seconds`,
        command: cmdStr,
        session,
      };
    }

    const stdout = e.stdout ?? "";
    const stderr = e.stderr ?? "";
    const [outTrunc, outWasTrunc] = truncateOutput(stdout, MAX_OUTPUT_BYTES);
    const [errTrunc, errWasTrunc] = truncateOutput(stderr, MAX_OUTPUT_BYTES);

    const outputParts: string[] = [];
    if (outTrunc) outputParts.push(outTrunc);
    if (errTrunc) outputParts.push(`[stderr]\n${errTrunc}`);

    return {
      success: false,
      output: outputParts.join("\n"),
      stdout: outTrunc,
      stderr: errTrunc,
      exit_code: e.status ?? 1,
      truncated: outWasTrunc || errWasTrunc,
      command: cmdStr,
      session,
    };
  }
}

function buildScreenshotPath(projectPath: string): string {
  const dir = ensureCacheDir(projectPath, "screenshots");
  const timestamp = Math.floor(Date.now() / 1000);
  return join(dir, `screenshot-${timestamp}.png`);
}

function buildSnapshotPath(projectPath: string): string {
  const dir = ensureCacheDir(projectPath, "snapshots");
  const timestamp = Math.floor(Date.now() / 1000);
  return join(dir, `snapshot-${timestamp}.yaml`);
}

// CLI entry point
const { values } = parseArgs({
  options: {
    params: { type: "string" },
    "project-path": { type: "string" },
  },
});

if (values.params && values["project-path"]) {
  const result = execute(
    JSON.parse(values.params) as Params,
    values["project-path"],
  );
  console.log(JSON.stringify(result));
}
