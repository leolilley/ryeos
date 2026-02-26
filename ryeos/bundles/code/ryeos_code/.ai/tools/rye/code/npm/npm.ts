// rye:signed:2026-02-26T05:02:48Z:3bee8b7d861a79339f8fd009ef561e11abb40d1c87b08db49028242be4970d0f:l_RNuXCcUdnwzJU40bUEV63ZRYoAH58hrzrHCCFDncyjHOx48PPwnkGOOHbNHK4oC6cLOXxHE6seKufELtxEAw==:4b987fd4e40303ac
import { parseArgs } from "node:util";
import { execSync } from "node:child_process";
import { resolve, isAbsolute } from "node:path";
import { existsSync } from "node:fs";

export const __version__ = "1.0.0";
export const __tool_type__ = "javascript";
export const __executor_id__ = "rye/core/runtimes/node/node";
export const __category__ = "rye/code/npm";
export const __tool_description__ =
  "NPM operations tool - install, run scripts, exec commands";

export const CONFIG_SCHEMA = {
  type: "object",
  properties: {
    action: {
      type: "string",
      enum: ["install", "run", "build", "test", "init", "exec"],
      description: "NPM action to perform",
    },
    args: {
      type: "array",
      items: { type: "string" },
      default: [],
      description:
        "Arguments for the action (package names for install, script name for run, command for exec, etc.)",
    },
    flags: {
      type: "object",
      default: {},
      description:
        "Flags to pass (e.g. { save_dev: true, force: true, global: true })",
    },
    working_dir: {
      type: "string",
      description: "Working directory (relative to project root or absolute)",
    },
    timeout: {
      type: "integer",
      default: 120,
      description: "Timeout in seconds",
    },
  },
  required: ["action"],
};

const MAX_OUTPUT_BYTES = 51200;
const DEFAULT_TIMEOUT = 120;

interface Params {
  action: string;
  args?: string[];
  flags?: Record<string, boolean | string>;
  working_dir?: string;
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

function buildCommand(params: Params): string[] {
  const args = params.args ?? [];
  const flags = params.flags ? buildFlags(params.flags) : [];

  switch (params.action) {
    case "install":
      return ["npm", "install", ...args, ...flags];
    case "run":
      if (args.length === 0) return ["npm", "run", ...flags];
      return ["npm", "run", args[0], ...flags, ...args.slice(1)];
    case "build":
      return ["npm", "run", "build", ...flags];
    case "test":
      return ["npm", "test", ...flags];
    case "init":
      return ["npm", "init", "-y", ...flags];
    case "exec":
      if (args.length === 0)
        throw new Error("exec action requires at least one arg (the command)");
      return ["npx", ...args, ...flags];
    default:
      throw new Error(`Unknown action: ${params.action}`);
  }
}

function execute(params: Params, projectPath: string): Result {
  const project = resolve(projectPath);

  if (!params.action) {
    return { success: false, error: "Missing required parameter: action" };
  }

  const timeout = (params.timeout ?? DEFAULT_TIMEOUT) * 1000;

  let cwd = project;
  if (params.working_dir) {
    cwd = isAbsolute(params.working_dir)
      ? resolve(params.working_dir)
      : resolve(project, params.working_dir);

    if (!existsSync(cwd)) {
      return { success: false, error: `Working directory not found: ${cwd}` };
    }
  }

  let cmd: string[];
  try {
    cmd = buildCommand(params);
  } catch (e: any) {
    return { success: false, error: e.message };
  }

  const cmdStr = cmd.join(" ");

  try {
    const output = execSync(cmdStr, {
      cwd,
      timeout,
      encoding: "utf-8",
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env },
    });

    const [stdout, truncated] = truncateOutput(output ?? "", MAX_OUTPUT_BYTES);

    return {
      success: true,
      output: stdout,
      stdout,
      stderr: "",
      exit_code: 0,
      truncated,
      command: cmdStr,
    };
  } catch (e: any) {
    if (e.killed) {
      return {
        success: false,
        error: `Command timed out after ${params.timeout ?? DEFAULT_TIMEOUT} seconds`,
        command: cmdStr,
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
    };
  }
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
