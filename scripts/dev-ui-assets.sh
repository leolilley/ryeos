#!/usr/bin/env bash
# Serve RyeOS browser UI assets directly from this checkout.
#
# Default mode starts a small local HTTP proxy: /ui and /ui/assets/* come from
# crates/clients/web/pkg, while /ui/api/* and launch/session routes proxy to the
# already-running daemon. This avoids stopping or rebuilding the active daemon.

set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/dev-ui-assets.sh [options]

Serve RyeOS browser UI assets from crates/clients/web/pkg without touching the
active daemon.

Options:
  --port PORT       Local dev UI port (default: 7411)
  --upstream URL    Active daemon URL to proxy API/session requests to
                    (default: http://127.0.0.1:7400)
  --asset-dir DIR   Override the asset directory
  --background      Run the proxy in the background
  --open            Mint a normal RyeOS web launch URL, rewrite it through the
                    dev proxy, and open it in the browser. Implies --background
  --pid-file PATH   PID file for --background (default: /tmp/ryeos-ui-assets.pid)
  --stop            Stop the background proxy from --pid-file
  --print-env       Print the daemon-side RYEOS_UI_ASSET_DIR env var and exit
  --direct-start    Start ryeos with RYEOS_UI_ASSET_DIR directly. This is not
                    the default because it touches the daemon lifecycle.
  --restart         With --direct-start only: stop current daemon before start
  -h, --help        Show this help

Examples:
  scripts/dev-ui-assets.sh --background --open
  scripts/dev-ui-assets.sh --stop
  scripts/dev-ui-assets.sh --print-env

After startup, open http://127.0.0.1:7411/ui, edit files under
crates/clients/web/pkg, and refresh the browser.
EOF
}

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"
asset_dir="$repo_root/crates/clients/web/pkg"
port=7411
upstream="http://127.0.0.1:7400"
pid_file="/tmp/ryeos-ui-assets.pid"
restart=0
print_env=0
background=0
stop=0
direct_start=0
open_ui=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --port)
            [[ $# -ge 2 ]] || { echo "--port requires PORT" >&2; exit 2; }
            port="$2"
            shift 2
            ;;
        --upstream)
            [[ $# -ge 2 ]] || { echo "--upstream requires URL" >&2; exit 2; }
            upstream="${2%/}"
            shift 2
            ;;
        --background)
            background=1
            shift
            ;;
        --open)
            open_ui=1
            background=1
            shift
            ;;
        --pid-file)
            [[ $# -ge 2 ]] || { echo "--pid-file requires PATH" >&2; exit 2; }
            pid_file="$2"
            shift 2
            ;;
        --stop)
            stop=1
            shift
            ;;
        --direct-start)
            direct_start=1
            shift
            ;;
        --restart)
            restart=1
            shift
            ;;
        --print-env)
            print_env=1
            shift
            ;;
        --asset-dir)
            [[ $# -ge 2 ]] || { echo "--asset-dir requires DIR" >&2; exit 2; }
            asset_dir="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "dev-ui-assets.sh: unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [[ $stop -eq 1 ]]; then
    if [[ -f "$pid_file" ]]; then
        pid="$(cat "$pid_file")"
        if [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid"
            echo "[dev-ui-assets] stopped background proxy pid $pid"
        fi
        rm -f "$pid_file"
    else
        echo "[dev-ui-assets] no pid file: $pid_file"
    fi
    exit 0
fi

open_dev_ui() {
    launch_url="$(ryeos web --no-open --print-url)"
    dev_launch_url="$($PYTHON_BIN - "$launch_url" "$port" <<'PY'
import sys
import urllib.parse

url = sys.argv[1]
port = int(sys.argv[2])
parts = urllib.parse.urlsplit(url)
netloc = f"127.0.0.1:{port}"
print(urllib.parse.urlunsplit((parts.scheme, netloc, parts.path, parts.query, parts.fragment)))
PY
)"
    echo "[dev-ui-assets] opening: $dev_launch_url"
    if command -v xdg-open >/dev/null 2>&1; then
        xdg-open "$dev_launch_url" >/dev/null 2>&1 &
    else
        echo "$dev_launch_url"
    fi
}

PYTHON_BIN="${PYTHON:-python}"

if [[ ! -d "$asset_dir" ]]; then
    echo "dev-ui-assets.sh: asset dir does not exist: $asset_dir" >&2
    exit 1
fi

if [[ $print_env -eq 1 ]]; then
    printf 'export RYEOS_UI_ASSET_DIR=%q\n' "$asset_dir"
    exit 0
fi

if [[ $direct_start -eq 1 && $restart -eq 1 ]]; then
    ryeos stop --force >/dev/null 2>&1 || true
fi

if [[ $direct_start -eq 1 ]]; then
    echo "[dev-ui-assets] starting ryeos with RYEOS_UI_ASSET_DIR=$asset_dir"
    RYEOS_UI_ASSET_DIR="$asset_dir" ryeos start
    exit 0
fi

run_proxy() {
    exec env \
    RYEOS_UI_ASSET_DIR="$asset_dir" \
    RYEOS_UI_DEV_PORT="$port" \
    RYEOS_UI_DEV_UPSTREAM="$upstream" \
    python - <<'PY'
import http.server
import mimetypes
import os
import pathlib
import socketserver
import sys
import urllib.error
import urllib.parse
import urllib.request

asset_dir = pathlib.Path(os.environ["RYEOS_UI_ASSET_DIR"]).resolve()
port = int(os.environ["RYEOS_UI_DEV_PORT"])
upstream = os.environ["RYEOS_UI_DEV_UPSTREAM"].rstrip("/")

class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None

opener = urllib.request.build_opener(NoRedirect)

class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_HEAD(self):
        if self.path == "/" or self.path == "/ui" or self.path.startswith("/ui?"):
            return self.serve_asset("index.html", head_only=True)
        if self.path.startswith("/ui/assets/"):
            path = urllib.parse.urlsplit(self.path).path.removeprefix("/ui/assets/")
            return self.serve_asset(path, head_only=True)
        return self.proxy(head_only=True)

    def do_GET(self):
        if self.path == "/" or self.path == "/ui" or self.path.startswith("/ui?"):
            return self.serve_asset("index.html")
        if self.path.startswith("/ui/assets/"):
            path = urllib.parse.urlsplit(self.path).path.removeprefix("/ui/assets/")
            return self.serve_asset(path)
        return self.proxy()

    def do_POST(self):
        return self.proxy()

    def do_PUT(self):
        return self.proxy()

    def do_DELETE(self):
        return self.proxy()

    def serve_asset(self, relative, head_only=False):
        candidate = (asset_dir / relative).resolve()
        try:
            candidate.relative_to(asset_dir)
        except ValueError:
            return self.send_error(403)
        if not candidate.is_file():
            return self.send_error(404)
        data = candidate.read_bytes()
        content_type = mimetypes.guess_type(candidate.name)[0] or "application/octet-stream"
        if candidate.suffix == ".js":
            content_type = "application/javascript; charset=utf-8"
        elif candidate.suffix == ".css":
            content_type = "text/css; charset=utf-8"
        elif candidate.suffix == ".html":
            content_type = "text/html; charset=utf-8"
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        if not head_only:
            self.wfile.write(data)

    def proxy(self, head_only=False):
        body = None
        length = self.headers.get("Content-Length")
        if length:
            body = self.rfile.read(int(length))
        target = upstream + self.path
        headers = {k: v for k, v in self.headers.items() if k.lower() not in {"host", "content-length"}}
        req = urllib.request.Request(target, data=body, headers=headers, method=self.command)
        try:
            with opener.open(req, timeout=60) as resp:
                self.send_response(resp.status)
                excluded = {"transfer-encoding", "connection", "content-encoding"}
                for key, value in resp.headers.items():
                    if key.lower() not in excluded:
                        self.send_header(key, value)
                self.end_headers()
                if not head_only:
                    while True:
                        chunk = resp.read(65536)
                        if not chunk:
                            break
                        self.wfile.write(chunk)
        except urllib.error.HTTPError as err:
            data = err.read()
            self.send_response(err.code)
            for key, value in err.headers.items():
                if key.lower() not in {"transfer-encoding", "connection", "content-encoding", "content-length"}:
                    self.send_header(key, value)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            if not head_only:
                self.wfile.write(data)
        except Exception as err:
            message = f"dev-ui-assets proxy error: {err}\n".encode()
            self.send_response(502)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", str(len(message)))
            self.end_headers()
            self.wfile.write(message)

    def log_message(self, fmt, *args):
        sys.stderr.write("[dev-ui-assets] " + fmt % args + "\n")

class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True

print(f"[dev-ui-assets] serving UI assets from: {asset_dir}")
print(f"[dev-ui-assets] proxying API/session requests to: {upstream}")
print(f"[dev-ui-assets] open: http://127.0.0.1:{port}/ui")
Server(("127.0.0.1", port), Handler).serve_forever()
PY
}

if [[ $background -eq 1 ]]; then
    if [[ -f "$pid_file" ]]; then
        old_pid="$(cat "$pid_file")"
        if [[ "$old_pid" =~ ^[0-9]+$ ]] && kill -0 "$old_pid" 2>/dev/null; then
            echo "[dev-ui-assets] already running pid $old_pid (pid file: $pid_file)"
            echo "[dev-ui-assets] open: http://127.0.0.1:$port/ui"
            if [[ $open_ui -eq 1 ]]; then
                open_dev_ui
            fi
            exit 0
        fi
        rm -f "$pid_file"
    fi
    log_file="${pid_file%.pid}.log"
    run_proxy >"$log_file" 2>&1 &
    pid=$!
    echo "$pid" >"$pid_file"
    echo "[dev-ui-assets] background proxy pid $pid"
    echo "[dev-ui-assets] log: $log_file"
    echo "[dev-ui-assets] open: http://127.0.0.1:$port/ui"
    if [[ $open_ui -eq 1 ]]; then
        open_dev_ui
    fi
else
    run_proxy
fi
