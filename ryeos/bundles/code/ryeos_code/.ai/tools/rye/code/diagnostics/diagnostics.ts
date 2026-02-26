// rye:signed:2026-02-26T06:42:43Z:0270efac388ac0fcc3d6512757396ba070ee053f7db245f39c5f51fbb2c7b823:ZkvQdTVc2N6bnS60a-x8O0Kye0h6jkDPyNtLGM-zystKRikA-efau7d-QZasMlt-R_ZYRzkMgs0XoryVijBaCw==:4b987fd4e40303ac
// rye:unsigned
import { parseArgs } from "node:util";
import { execSync } from "node:child_process";
import { resolve, isAbsolute, relative, extname } from "node:path";
import { existsSync } from "node:fs";

export const __version__ = "1.0.0";
export const __tool_type__ = "javascript";
export const __executor_id__ = "rye/core/runtimes/node/node";
export const __category__ = "rye/code/diagnostics";
export const __tool_description__ =
  "Run linters and type checkers on source files — ruff, mypy, eslint, tsc, clippy, etc.";

export const CONFIG_SCHEMA = {
  type: "object",
  properties: {
    file_path: {
      type: "string",
      description: "Path to file to get diagnostics for",
    },
    linters: {
      type: "array",
      items: { type: "string" },
      description:
        "Linters to use (ruff, mypy, pylint for Python; eslint, tsc for JS/TS). Auto-detected if not specified.",
    },
    timeout: {
      type: "integer",
      default: 30,
      description: "Timeout per linter in seconds",
    },
  },
  required: ["file_path"],
};

const MAX_OUTPUT_BYTES = 32768;
const DEFAULT_TIMEOUT = 30;

interface Params {
  file_path: string;
  linters?: string[];
  timeout?: number;
}

interface Diagnostic {
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
  linters_checked?: string[];
  file_type?: string | null;
}

const FILE_TYPE_MAP: Record<string, string> = {
  ".py": "python",
  ".js": "javascript",
  ".jsx": "javascript",
  ".ts": "typescript",
  ".tsx": "typescript",
  ".mjs": "javascript",
  ".cjs": "javascript",
  ".go": "go",
  ".rs": "rust",
  ".rb": "ruby",
  ".java": "java",
  ".kt": "kotlin",
  ".c": "c",
  ".cpp": "cpp",
  ".h": "c",
  ".hpp": "cpp",
};

const LINTERS_BY_TYPE: Record<string, string[]> = {
  python: ["ruff", "mypy", "pylint", "flake8"],
  javascript: ["eslint", "tsc"],
  typescript: ["eslint", "tsc"],
  go: ["go vet"],
  rust: ["cargo clippy"],
};

function detectFileType(filePath: string): string | null {
  return FILE_TYPE_MAP[extname(filePath).toLowerCase()] ?? null;
}

function isAvailable(cmd: string): boolean {
  try {
    execSync(`which ${cmd.split(" ")[0]}`, {
      encoding: "utf-8",
      stdio: ["pipe", "pipe", "pipe"],
    });
    return true;
  } catch {
    return false;
  }
}

function runLinter(
  linter: string,
  filePath: string,
  cwd: string,
  timeout: number,
): Diagnostic[] {
  const diagnostics: Diagnostic[] = [];

  let cmd: string;
  switch (linter) {
    case "ruff":
      cmd = `ruff check --output-format=json ${filePath}`;
      break;
    case "mypy":
      cmd = `mypy --no-error-summary --no-color-output ${filePath}`;
      break;
    case "pylint":
      cmd = `pylint --output-format=json ${filePath}`;
      break;
    case "flake8":
      cmd = `flake8 --format=default ${filePath}`;
      break;
    case "eslint":
      cmd = `eslint --format=json ${filePath}`;
      break;
    case "tsc":
      cmd = `tsc --noEmit --pretty false ${filePath}`;
      break;
    case "go vet":
      cmd = `go vet ${filePath}`;
      break;
    case "cargo clippy":
      cmd = `cargo clippy --message-format=json ${filePath}`;
      break;
    default:
      return [];
  }

  let stdout = "";
  let stderr = "";
  try {
    stdout = execSync(cmd, {
      cwd,
      timeout: timeout * 1000,
      encoding: "utf-8",
      stdio: ["pipe", "pipe", "pipe"],
    });
  } catch (e: any) {
    if (e.killed) {
      return [
        {
          line: 0,
          column: 0,
          severity: "error",
          message: `${linter} timed out`,
          code: "timeout",
        },
      ];
    }
    stdout = e.stdout ?? "";
    stderr = e.stderr ?? "";
  }

  try {
    if (linter === "ruff" && stdout) {
      const issues = JSON.parse(stdout);
      for (const issue of issues) {
        diagnostics.push({
          line: issue.location?.row ?? 0,
          column: issue.location?.column ?? 0,
          severity: issue.severity === "error" ? "error" : "warning",
          message: issue.message ?? "",
          code: issue.code ?? "",
        });
      }
    } else if (linter === "mypy") {
      const pattern = /^(.+?):(\d+): (error|warning|note): (.+)/;
      for (const line of (stdout + stderr).split("\n")) {
        const match = line.match(pattern);
        if (match) {
          diagnostics.push({
            line: parseInt(match[2], 10),
            column: 0,
            severity: match[3] === "note" ? "info" : match[3],
            message: match[4],
            code: "",
          });
        }
      }
    } else if (linter === "pylint" && stdout) {
      const issues = JSON.parse(stdout);
      for (const issue of issues) {
        diagnostics.push({
          line: issue.line ?? 0,
          column: issue.column ?? 0,
          severity: issue.type === "error" ? "error" : "warning",
          message: issue.message ?? "",
          code: issue.symbol ?? "",
        });
      }
    } else if (linter === "flake8") {
      const pattern = /^(.+?):(\d+):(\d+): ([A-Z]\d+) (.+)/;
      for (const line of stdout.split("\n")) {
        const match = line.match(pattern);
        if (match) {
          diagnostics.push({
            line: parseInt(match[2], 10),
            column: parseInt(match[3], 10),
            severity: match[4].startsWith("E") ? "error" : "warning",
            message: match[5],
            code: match[4],
          });
        }
      }
    } else if (linter === "eslint" && stdout) {
      const data = JSON.parse(stdout);
      for (const fileResult of data) {
        for (const msg of fileResult.messages ?? []) {
          diagnostics.push({
            line: msg.line ?? 0,
            column: msg.column ?? 0,
            severity: msg.severity === 2 ? "error" : "warning",
            message: msg.message ?? "",
            code: msg.ruleId ?? "",
          });
        }
      }
    } else if (linter === "tsc") {
      const pattern = /^(.+?)\((\d+),(\d+)\): (error|warning) (TS\d+): (.+)/;
      for (const line of (stdout + stderr).split("\n")) {
        const match = line.match(pattern);
        if (match) {
          diagnostics.push({
            line: parseInt(match[2], 10),
            column: parseInt(match[3], 10),
            severity: match[4],
            message: match[6],
            code: match[5],
          });
        }
      }
    }
  } catch {
    // JSON parse failures are silently ignored (linter produced no valid output)
  }

  return diagnostics;
}

function formatDiagnostics(diagnostics: Diagnostic[], filePath: string): string {
  if (diagnostics.length === 0) return `No issues found in ${filePath}`;

  return diagnostics
    .sort((a, b) => a.line - b.line || a.column - b.column)
    .map((d) => {
      const col = d.column ? `:${d.column}` : "";
      const code = d.code ? ` [${d.code}]` : "";
      return `${filePath}:${d.line}${col}: ${d.severity}: ${d.message}${code}`;
    })
    .join("\n");
}

function execute(params: Params, projectPath: string): Result {
  const project = resolve(projectPath);

  let filePath = params.file_path;
  if (!isAbsolute(filePath)) {
    filePath = resolve(project, filePath);
  }

  if (!existsSync(filePath)) {
    return { success: false, error: `File not found: ${filePath}` };
  }

  const fileType = detectFileType(filePath);
  const timeout = params.timeout ?? DEFAULT_TIMEOUT;

  const candidateLinters = params.linters ?? LINTERS_BY_TYPE[fileType ?? ""] ?? [];
  const availableLinters = candidateLinters.filter((l) =>
    isAvailable(l.split(" ")[0]),
  );

  if (availableLinters.length === 0) {
    return {
      success: true,
      output: `No linters available for ${fileType ?? "unknown"} files`,
      diagnostics: [],
      linters_checked: [],
      file_type: fileType,
    };
  }

  const allDiagnostics: Diagnostic[] = [];
  for (const linter of availableLinters) {
    allDiagnostics.push(...runLinter(linter, filePath, project, timeout));
  }

  // Deduplicate
  const seen = new Set<string>();
  const unique = allDiagnostics.filter((d) => {
    const key = `${d.line}:${d.column}:${d.message}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });

  let relPath: string;
  try {
    relPath = relative(project, filePath);
  } catch {
    relPath = filePath;
  }

  let output = formatDiagnostics(unique, relPath);
  if (output.length > MAX_OUTPUT_BYTES) {
    output = output.slice(0, MAX_OUTPUT_BYTES) + "\n... [output truncated]";
  }

  return {
    success: true,
    output,
    diagnostics: unique,
    linters_checked: availableLinters,
    file_type: fileType,
  };
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
