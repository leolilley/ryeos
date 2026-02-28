// rye:signed:2026-02-28T00:36:04Z:45d3134a798551dabac5e66abdb742dec2c2c689aa790be282c2ace939d709cd:dP-9O72JGGri5xA3ezy4wR3NrMkdC7beZBS4cWzT0hB-FqqDsYwxaun1TMeOt33iC5i_MGUPTprGoJF5jTI0Cg==:4b987fd4e40303ac
// rye:unsigned
import { parseArgs } from "node:util";
import { execSync } from "node:child_process";
import { resolve, isAbsolute, relative } from "node:path";
import { existsSync } from "node:fs";

export const __version__ = "1.0.0";
export const __tool_type__ = "javascript";
export const __executor_id__ = "rye/core/runtimes/node/node";
export const __category__ = "rye/code/typescript";
export const __tool_description__ =
  "TypeScript type checker — run tsc --noEmit for type checking without build";

export const CONFIG_SCHEMA = {
  type: "object",
  properties: {
    action: {
      type: "string",
      enum: ["check", "check-file"],
      description: "Type check entire project or a single file",
    },
    file_path: {
      type: "string",
      description: "File to check (for check-file action)",
    },
    working_dir: {
      type: "string",
      description: "Directory containing tsconfig.json",
    },
    strict: {
      type: "boolean",
      default: false,
      description: "Enable strict mode",
    },
    timeout: {
      type: "integer",
      default: 60,
      description: "Timeout in seconds",
    },
  },
  required: ["action"],
};

const MAX_OUTPUT_BYTES = 51200;
const DEFAULT_TIMEOUT = 60;

interface Params {
  action: string;
  file_path?: string;
  working_dir?: string;
  strict?: boolean;
  timeout?: number;
}

interface Diagnostic {
  file: string;
  line: number;
  column: number;
  severity: string;
  message: string;
  code: string;
}

interface Result {
  success: boolean;
  output?: string;
  error?: string;
  diagnostics?: Diagnostic[];
  error_count?: number;
  command?: string;
}

function parseTscOutput(output: string, projectPath: string): Diagnostic[] {
  const diagnostics: Diagnostic[] = [];
  const pattern = /^(.+?)\((\d+),(\d+)\): (error|warning) (TS\d+): (.+)/;

  for (const line of output.split("\n")) {
    const match = line.match(pattern);
    if (match) {
      let file = match[1];
      try {
        file = relative(projectPath, file);
      } catch {
        // keep absolute
      }
      diagnostics.push({
        file,
        line: parseInt(match[2], 10),
        column: parseInt(match[3], 10),
        severity: match[4],
        message: match[6],
        code: match[5],
      });
    }
  }

  return diagnostics;
}

function execute(params: Params, projectPath: string): Result {
  const project = resolve(projectPath);

  if (!params.action) {
    return { success: false, error: "Missing required parameter: action" };
  }

  if (params.action === "check-file" && !params.file_path) {
    return {
      success: false,
      error: "check-file action requires file_path parameter",
    };
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

  const cmdParts = ["tsc", "--noEmit", "--pretty", "false"];

  if (params.strict) {
    cmdParts.push("--strict");
  }

  if (params.action === "check-file" && params.file_path) {
    const filePath = isAbsolute(params.file_path)
      ? params.file_path
      : resolve(project, params.file_path);

    if (!existsSync(filePath)) {
      return { success: false, error: `File not found: ${filePath}` };
    }

    cmdParts.push(filePath);
  }

  const cmdStr = cmdParts.join(" ");

  try {
    const output = execSync(cmdStr, {
      cwd,
      timeout,
      encoding: "utf-8",
      stdio: ["pipe", "pipe", "pipe"],
    });

    return {
      success: true,
      output: output?.trim() || "No type errors found.",
      diagnostics: [],
      error_count: 0,
      command: cmdStr,
    };
  } catch (e: any) {
    if (e.killed) {
      return {
        success: false,
        error: `tsc timed out after ${params.timeout ?? DEFAULT_TIMEOUT} seconds`,
        command: cmdStr,
      };
    }

    const combined = (e.stdout ?? "") + (e.stderr ?? "");
    const diagnostics = parseTscOutput(combined, project);
    const errorCount = diagnostics.filter(
      (d) => d.severity === "error",
    ).length;

    let output = diagnostics
      .map(
        (d) =>
          `${d.file}(${d.line},${d.column}): ${d.severity} ${d.code}: ${d.message}`,
      )
      .join("\n");

    if (!output) output = combined.trim();

    if (output.length > MAX_OUTPUT_BYTES) {
      output = output.slice(0, MAX_OUTPUT_BYTES) + "\n... [output truncated]";
    }

    return {
      success: errorCount === 0,
      output,
      diagnostics,
      error_count: errorCount,
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

async function main() {
  let paramsJson: string;
  if (values.params) {
    paramsJson = values.params;
  } else {
    const chunks: Buffer[] = [];
    for await (const chunk of process.stdin) chunks.push(chunk);
    paramsJson = Buffer.concat(chunks).toString();
  }
  const result = execute(
    JSON.parse(paramsJson) as Params,
    values["project-path"]!,
  );
  console.log(JSON.stringify(result));
}

if (values["project-path"]) main();
