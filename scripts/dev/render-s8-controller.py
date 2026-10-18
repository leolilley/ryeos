#!/usr/bin/env python3
"""Operator script for the F5R S8 Render deployment (deploy, status, teardown)."""
import argparse
import datetime
import json
import os
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

DEFAULT_ENV_FILE = Path(__file__).resolve().parents[2] / ".env"
API = "https://api.render.com/v1"
FARM_SERVICE_ID = "srv-datiea7avr4c73dm14fg"
SERVICE_PREFIX = "ryeos-f5r-s9-"
FORBIDDEN_WORDS = ("farm", "snap-track")
IMAGE_RE = re.compile(r"^(?P<path>ghcr\.io/leolilley/ryeos-f5r-controller)@(?P<digest>sha256:[0-9a-f]{64})$")
DIGEST_RE = re.compile(r"^sha256:[0-9a-f]{64}$")
TERMINAL_OK = {"live"}
TERMINAL_BAD = {"build_failed", "update_failed", "canceled", "pre_deploy_failed", "deactivated"}


class Refusal(Exception):
    pass


class Failure(Exception):
    pass


class NoRedirect(urllib.request.HTTPRedirectHandler):
    """Refuses every 3xx so the bearer header never follows a redirect."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def http_open(request, timeout):
    return urllib.request.build_opener(NoRedirect).open(request, timeout=timeout)


class Api:
    """Thin Render client; `opener` and `log` are injectable seams."""

    def __init__(self, key, opener=http_open, log=None):
        self._key = key
        self._secrets = [key]
        self._opener = opener
        self._log = log or (lambda line: print(line, file=sys.stderr))

    def add_secret(self, value):
        self._secrets.append(value)

    def scrub(self, text):
        for secret in self._secrets:
            if secret:
                text = text.replace(secret, "***")
        return text

    def _error_message(self, raw):
        try:
            message = json.loads(raw).get("message")
        except (ValueError, AttributeError):
            return ""
        if not isinstance(message, str):
            return ""
        return self.scrub(message[:200])

    def call(self, method, path, body=None, base=API, ok404=False):
        data = None if body is None else json.dumps(body).encode()
        headers = {"Accept": "application/json", "Authorization": "Bearer " + self._key}
        if data is not None:
            headers["Content-Type"] = "application/json"
        request = urllib.request.Request(base + path, data=data, method=method, headers=headers)
        message = ""
        try:
            with self._opener(request, 60) as resp:
                status, raw = resp.status, resp.read()
        except urllib.error.HTTPError as err:
            status, raw = err.code, err.read()
            message = self._error_message(raw)
        except (urllib.error.URLError, OSError) as err:
            self._log(f"{method} {path} -> error {type(err).__name__}")
            raise Failure(f"{method} {path}: {type(err).__name__}")
        self._log(f"{method} {path} -> {status}")
        if status == 404 and ok404:
            return status, None
        if status >= 400:
            raise Failure(f"{method} {path}: HTTP {status}" + (f" message={message}" if message else ""))
        try:
            return status, (json.loads(raw) if raw else None)
        except ValueError:
            return status, None


def clean_secret(text):
    text = text.strip()
    if not text or any(c.isspace() or ord(c) < 32 or ord(c) == 127 for c in text):
        raise Failure("secret is empty or contains whitespace or control characters")
    return text


def parse_env_file(text):
    values = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        name, value = line.split("=", 1)
        name, value = name.strip(), value.strip()
        if name.startswith("export "):
            name = name[len("export "):].strip()
        if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
            value = value[1:-1]
        values[name] = value
    return values


def read_env_file_key(path):
    path = Path(path)
    if not path.is_file():
        raise Failure(f"no render api key: {path} does not exist")
    if path.stat().st_mode & 0o077:
        raise Refusal(f"env file {path} is group or world readable; run chmod 600 on it")
    key = parse_env_file(path.read_text()).get("RENDER_API_KEY")
    if not key:
        raise Failure(f"no render api key: RENDER_API_KEY missing from {path}")
    return clean_secret(key)


def read_key(args):
    if args.api_key_file:
        return clean_secret(Path(args.api_key_file).read_text())
    if os.environ.get("RENDER_API_KEY"):
        return clean_secret(os.environ["RENDER_API_KEY"])
    return read_env_file_key(args.env_file)


def guard(name, payload):
    lowered = name.lower()
    for word in FORBIDDEN_WORDS:
        if word in lowered:
            raise Refusal(f"service name contains forbidden word {word!r}")
    blob = json.dumps(payload).lower()
    if FARM_SERVICE_ID in blob:
        raise Refusal("payload references the farm service id")
    for word in FORBIDDEN_WORDS:
        if word in blob:
            raise Refusal(f"payload contains forbidden word {word!r}")


def guard_id(service_id):
    if service_id.strip().lower() == FARM_SERVICE_ID:
        raise Refusal("refusing to touch the farm service id")


def require_prefix(name):
    if not name.startswith(SERVICE_PREFIX):
        raise Refusal(f"service name must start with {SERVICE_PREFIX}")


def build_payload(args, owner_id, credential_id):
    match = IMAGE_RE.match(args.image)
    if not match:
        raise Refusal("--image must be ghcr.io/leolilley/ryeos-f5r-controller@sha256:<64hex>")
    if not DIGEST_RE.match(args.substrate_digest):
        raise Refusal("--substrate-digest must be sha256:<64hex>")
    image_path = match.group("path")
    env = [
        ("RYEOS_INIT_HOST_CLASS", "hosted"),
        ("RYEOS_SUBSTRATE_IMAGE", f"{image_path}@{args.substrate_digest}"),
        ("RYEOS_SUBSTRATE_PROTOCOL", str(args.substrate_protocol)),
        ("RYEOS_TRUST_BAKED_PUBLISHERS", "1"),
        ("PORT", "8000"),
    ]
    if args.operator_hold:
        env.append(("RYEOS_OPERATOR_HOLD", "1"))
    image = {"ownerId": owner_id, "imagePath": args.image}
    if credential_id:
        image["registryCredentialId"] = credential_id
    return {
        "type": "web_service",
        "name": args.name,
        "ownerId": owner_id,
        "image": image,
        "serviceDetails": {
            "env": "image",
            "autoDeploy": "no",
            "plan": args.plan,
            "region": args.region,
            "healthCheckPath": "/_ryeos/ready",
            "numInstances": 1,
            "disk": {"name": "ryeos-data", "mountPath": "/data", "sizeGB": args.disk_gb},
        },
        "envVars": [{"key": k, "value": v} for k, v in env],
    }


def now():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def unwrap(item):
    """Strips the list wrapper ({"<singular>": {...}, "cursor": ...}) when present."""
    if isinstance(item, dict) and "id" not in item:
        inner = [v for k, v in item.items() if k != "cursor" and isinstance(v, dict)]
        if len(inner) == 1:
            return inner[0]
    return item


def list_all(api, path):
    items, cursor = [], None
    while True:
        query = "limit=100" + (f"&cursor={urllib.parse.quote(cursor)}" if cursor else "")
        _, page = api.call("GET", f"{path}?{query}")
        page = page or []
        items.extend(page)
        cursor = page[-1].get("cursor") if page else None
        if len(page) < 100 or not cursor:
            return [unwrap(i) for i in items]


OWNER_ID_RE = re.compile(r"^(tea|usr)-[a-z0-9]+$")


def resolve_owner(api, email, owner_id=None):
    if owner_id:
        return owner_id
    _, owners = api.call("GET", "/owners?limit=20")
    items = [unwrap(o) for o in (owners or [])]
    if email:
        items = [o for o in items if (o.get("email") or "").lower() == email.lower()]
    if len(items) != 1:
        raise Failure(f"expected exactly one owner, found {len(items)}")
    return items[0]["id"]


def service_url(service):
    details = service.get("serviceDetails") or {}
    return details.get("url") or service.get("url")


def ready_probe(api, url):
    if not url:
        return {"url": None, "status": None}
    target = url if url.startswith("http") else "https://" + url
    request = urllib.request.Request(target + "/_ryeos/ready", method="GET")
    try:
        with api._opener(request, 30) as resp:
            status = resp.status
    except urllib.error.HTTPError as err:
        status = err.code
    except (urllib.error.URLError, OSError) as err:
        api._log(f"GET /_ryeos/ready -> error {type(err).__name__}")
        return {"url": target, "status": None}
    api._log(f"GET /_ryeos/ready -> {status}")
    return {"url": target, "status": status}


def write_json(out, name, record):
    out = Path(out)
    out.mkdir(parents=True, exist_ok=True)
    (out / name).write_text(json.dumps(record, indent=2) + "\n")


def cmd_deploy(args, api, sleep=time.sleep, clock=time.monotonic):
    require_prefix(args.name)
    guard(args.name, build_payload(args, "own-placeholder", "rcred-placeholder"))
    owner_id = resolve_owner(api, args.owner_email, args.owner_id)
    guard(args.name, build_payload(args, owner_id, "rcred-placeholder"))
    query = urllib.parse.urlencode({"name": args.name, "ownerId": owner_id})
    _, existing = api.call("GET", f"/services?{query}")
    for item in existing or []:
        found = unwrap(item)
        if found.get("name") == args.name:
            raise Refusal(f"a service named {args.name} already exists: {found.get('id')}")
    credential_id = args.registry_credential_id
    payload = build_payload(args, owner_id, credential_id)
    guard(args.name, payload)
    record = {
        "name": args.name,
        "image": args.image,
        "substrateDigest": args.substrate_digest,
        "registryCredentialId": credential_id,
        "registryCredentialCreated": False,
        "envKeys": [e["key"] for e in payload["envVars"]],
        "state": "credential-ready" if credential_id else "starting",
        "startedAt": now(),
    }
    write_json(args.out, "s8-service.json", record)
    try:
        _, created = api.call("POST", "/services", payload)
    except Failure as err:
        record.update(state="create-failed", error=str(err), finishedAt=now())
        write_json(args.out, "s8-service.json", record)
        raise
    service = created["service"]
    service_id, deploy_id = service["id"], created.get("deployId")
    guard_id(service_id)
    record.update(serviceId=service_id, deployId=deploy_id, state="created", url=service_url(service))
    write_json(args.out, "s8-service.json", record)
    print(f"created service {service_id} deploy {deploy_id}", flush=True)
    deadline = clock() + args.timeout
    state = "timeout"
    try:
        while True:
            _, deploy = api.call("GET", f"/services/{service_id}/deploys/{deploy_id}")
            record["deployStatus"] = deploy.get("status")
            if record["deployStatus"] in TERMINAL_OK:
                state = "live"
                break
            if record["deployStatus"] in TERMINAL_BAD:
                state = "failed"
                break
            if clock() >= deadline:
                break
            sleep(args.poll_interval)
        _, current = api.call("GET", f"/services/{service_id}")
        record["url"] = service_url(current) or record["url"]
        record["ready"] = ready_probe(api, record["url"]) if state == "live" else {"url": record["url"], "status": None}
        if state == "live" and record["ready"]["status"] != 200:
            print(f"warning: live but readiness probe returned {record['ready']['status']}", file=sys.stderr)
        record["state"] = state
    except BaseException as err:
        record["state"] = "interrupted" if isinstance(err, KeyboardInterrupt) else "poll-error"
        record["error"] = api.scrub(f"{type(err).__name__}: {err}")
        raise
    finally:
        record["finishedAt"] = now()
        write_json(args.out, "s8-service.json", record)
    print(f"deploy {state} service={service_id} url={record['url']} deploy={record['deployStatus']} "
          f"ready={record['ready']['status']}")
    return 0 if state == "live" else 1


def cmd_status(args, api):
    guard_id(args.service_id)
    _, service = api.call("GET", f"/services/{args.service_id}")
    guard(service.get("name") or "", {"name": service.get("name")})
    _, deploys = api.call("GET", f"/services/{args.service_id}/deploys?limit=1")
    deploy = unwrap(deploys[0]) if deploys else {}
    url = service_url(service)
    print(json.dumps({
        "serviceId": args.service_id,
        "name": service.get("name"),
        "suspended": service.get("suspended"),
        "url": url,
        "deployStatus": deploy.get("status"),
        "ready": ready_probe(api, url),
    }))
    return 0


def cmd_teardown(args, api, sleep=time.sleep):
    guard_id(args.service_id)
    if not args.confirm:
        raise Refusal("teardown requires --confirm")
    record_path = Path(args.out) / "s8-service.json"
    recorded = json.loads(record_path.read_text()) if record_path.exists() else {}
    _, service = api.call("GET", f"/services/{args.service_id}", ok404=True)
    if service is not None:
        name = service.get("name") or ""
        guard(name, {"name": name})
        require_prefix(name)
    elif recorded.get("serviceId") == args.service_id:
        name = recorded.get("name") or ""
        guard(name, {"name": name})
        require_prefix(name)
    else:
        raise Refusal("service is gone and no matching record names it")
    cred_id = args.registry_credential_id
    if cred_id:
        recorded_ok = (recorded.get("serviceId") == args.service_id
                       and recorded.get("registryCredentialCreated") is True
                       and recorded.get("registryCredentialId") == cred_id)
        named_ok = recorded_ok or any(c.get("id") == cred_id and c.get("name") == name + "-ghcr"
                                      for c in list_all(api, "/registrycredentials"))
        if not named_ok:
            raise Refusal("registry credential is not recorded as created by this deploy")
    if service is not None:
        api.call("DELETE", f"/services/{args.service_id}", ok404=True)
    if cred_id:
        api.call("DELETE", f"/registrycredentials/{cred_id}", ok404=True)
    verified = False
    for attempt in range(3):
        status, _ = api.call("GET", f"/services/{args.service_id}", ok404=True)
        if status == 404:
            verified = True
            break
        sleep(2)
    write_json(args.out, "s8-teardown.json", {
        "serviceId": args.service_id,
        "registryCredentialDeleted": bool(cred_id),
        "verifiedGone": verified,
        "finishedAt": now(),
    })
    print(f"teardown service={args.service_id} verifiedGone={verified}")
    return 0 if verified else 1


def parser():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--api-key-file")
    p.add_argument("--env-file", default=str(DEFAULT_ENV_FILE))
    sub = p.add_subparsers(dest="cmd", required=True)
    d = sub.add_parser("deploy")
    d.add_argument("--image", required=True)
    d.add_argument("--substrate-digest", required=True)
    d.add_argument("--substrate-protocol", type=int, default=1)
    d.add_argument("--name", default=SERVICE_PREFIX + datetime.date.today().strftime("%Y%m%d"))
    d.add_argument("--region", default="oregon")
    d.add_argument("--plan", default="starter")
    d.add_argument("--disk-gb", type=int, default=1)
    d.add_argument("--owner-email")
    d.add_argument("--owner-id")
    d.add_argument("--registry-credential-id")
    d.add_argument("--operator-hold", action="store_true")
    d.add_argument("--timeout", type=int, default=1200)
    d.add_argument("--poll-interval", type=int, default=10)
    d.add_argument("--out", required=True)
    s = sub.add_parser("status")
    s.add_argument("--service-id", required=True)
    t = sub.add_parser("teardown")
    t.add_argument("--service-id", required=True)
    t.add_argument("--registry-credential-id")
    t.add_argument("--confirm", action="store_true")
    t.add_argument("--out", default=".")
    return p


def main(argv=None, opener=http_open, sleep=time.sleep, clock=time.monotonic, key_reader=read_key):
    args = parser().parse_args(argv)
    try:
        if args.cmd == "deploy":
            # Static refusals run before the key is read or any request is made.
            guard(args.name, {"name": args.name, "image": args.image})
            build_payload(args, "own-placeholder", args.registry_credential_id)
            if args.owner_id and not OWNER_ID_RE.match(args.owner_id):
                raise Refusal("--owner-id must match (tea|usr)-[a-z0-9]+")
        elif args.cmd in ("status", "teardown"):
            guard_id(args.service_id)
    except Refusal as err:
        print(f"refused: {err}", file=sys.stderr)
        return 2
    api = None
    try:
        api = Api(key_reader(args), opener=opener)
        if args.cmd == "deploy":
            return cmd_deploy(args, api, sleep=sleep, clock=clock)
        if args.cmd == "status":
            return cmd_status(args, api)
        return cmd_teardown(args, api, sleep=sleep)
    except Refusal as err:
        print(f"refused: {err}", file=sys.stderr)
        return 2
    except Failure as err:
        print(f"failed: {err}", file=sys.stderr)
        return 1
    except Exception as err:
        print(f"failed: {type(err).__name__}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
