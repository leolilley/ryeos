# ryeos:signed:2026-11-16T00:19:46Z:a436402564a4cbe55f2023a4c77589795b5f269d30bded1dd5394c6d2d6c735d:cU1ATC9YAff5AiglEpcH8eerF09tpt+r3lGDyq+U017EYBdqRSeuCB7XqdyzLH1Fusid42EclIa6oeI//OL0Cg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: central-auth/lib
#   version: "0.1.0"
#   tool_type: python
#   executor_id: null
#   description: "Shared central-auth implementation for the sibling RyeOS tool descriptors. Not directly invokable."
"""Shared central-auth implementation for the sibling RyeOS tool descriptors."""

from __future__ import annotations

import argparse
import base64
import contextlib
import datetime as dt
import fcntl
import hashlib
import hmac
import json
import os
import re
import secrets
import sys
import tempfile
from pathlib import Path
from typing import Any


REALM_RE = re.compile(r"^[a-z0-9][a-z0-9._-]{0,127}$")
PRINCIPAL_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._@-]{0,127}$")
CAPABILITY_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._:-]{0,255}$")
REQUEST_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$")
SESSION_BYTES = 32
PBKDF2_ITERATIONS = 210_000
# Verified against when the principal is missing, disabled or has no usable
# passphrase record, so a failed login costs one KDF run whatever the cause.
DUMMY_KDF_SALT = b"central-auth-dummy-salt"
# Expired sessions/invites/failure records examined per login or verify.
OPPORTUNISTIC_GC_SAMPLE = 16
# The node owns central-auth state: the daemon mounts one bundle state home per
# realm and names it in RYEOSD_BUNDLE_STATE_HOME, never at a caller-chosen
# location. Under enforced isolation the daemon mounts the realm's directory
# at this fixed destination.
STATE_HOME_ENV = "RYEOSD_BUNDLE_STATE_HOME"
ENFORCED_STATE_MOUNT = "/run/ryeos/bundle-state"
CALLER_STATE_LOCATION_KEYS = ("runtime_state_dir", "project_path")
LOGIN_THROTTLE_FIELDS = ("max_failures", "window_secs", "lockout_secs")
PRINCIPAL_MANAGEMENT_FIELDS = ("operator_capability", "managed_roles", "protected_capabilities")
STATUS_OPERATION_KIND = "principal.status_change"


class AuthError(Exception):
    def __init__(self, code: str, message: str | None = None):
        self.code = code
        self.message = message or code
        super().__init__(self.message)


def now_unix() -> int:
    return int(dt.datetime.now(dt.UTC).timestamp())


def iso(ts: int | None = None) -> str:
    if ts is None:
        ts = now_unix()
    return dt.datetime.fromtimestamp(ts, dt.UTC).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def token_urlsafe() -> str:
    return base64.urlsafe_b64encode(secrets.token_bytes(SESSION_BYTES)).rstrip(b"=").decode("ascii")


def sha256_hex(value: str) -> str:
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def read_stdin_json() -> dict[str, Any]:
    raw = sys.stdin.read()
    if not raw.strip():
        return {}
    data = json.loads(raw)
    if not isinstance(data, dict):
        raise AuthError("invalid_request", "stdin JSON must be an object")
    return data


def write_json(value: dict[str, Any]) -> None:
    print(json.dumps(value, ensure_ascii=False, separators=(",", ":")))


def require_str(data: dict[str, Any], key: str) -> str:
    value = data.get(key)
    if not isinstance(value, str) or not value.strip():
        raise AuthError("invalid_request", f"{key} must be a non-empty string")
    return value.strip()


def optional_str(data: dict[str, Any], key: str) -> str | None:
    value = data.get(key)
    if value is None:
        return None
    if not isinstance(value, str) or not value.strip():
        raise AuthError("invalid_request", f"{key} must be a non-empty string when present")
    return value.strip()


def require_secret(data: dict[str, Any], key: str, min_len: int = 8) -> str:
    value = data.get(key)
    if not isinstance(value, str) or value == "":
        raise AuthError("invalid_request", f"{key} must be a non-empty string")
    if len(value) < min_len:
        raise AuthError("invalid_request", f"{key} must be at least {min_len} characters")
    return value


def optional_secret(data: dict[str, Any], key: str, min_len: int = 8) -> str | None:
    if data.get(key) is None:
        return None
    return require_secret(data, key, min_len)


def optional_int(
    data: dict[str, Any],
    key: str,
    default: int,
    *,
    minimum: int | None = None,
    maximum: int | None = None,
) -> int:
    value = data.get(key, default)
    if isinstance(value, bool):
        raise AuthError("invalid_request", f"{key} must be an integer")
    try:
        out = int(value)
    except Exception:
        raise AuthError("invalid_request", f"{key} must be an integer")
    if minimum is not None and out < minimum:
        raise AuthError("invalid_request", f"{key} must be >= {minimum}")
    if maximum is not None and out > maximum:
        raise AuthError("invalid_request", f"{key} must be <= {maximum}")
    return out


def require_bool(data: dict[str, Any], key: str) -> bool:
    value = data.get(key)
    if not isinstance(value, bool):
        raise AuthError("invalid_request", f"{key} must be a boolean")
    return value


def string_list(data: dict[str, Any], key: str) -> list[str]:
    value = data.get(key, [])
    if isinstance(value, str):
        items = [value]
    elif isinstance(value, list):
        items = value
    else:
        raise AuthError("invalid_request", f"{key} must be a string or array of strings")
    out: list[str] = []
    for item in items:
        if not isinstance(item, str) or not item.strip():
            raise AuthError("invalid_request", f"{key} contains an empty/non-string value")
        out.append(item.strip())
    return sorted(set(out))


def validate_realm(realm_id: str) -> str:
    if not REALM_RE.match(realm_id):
        raise AuthError("invalid_realm", "realm_id must match [a-z0-9][a-z0-9._-]{0,127}")
    return realm_id


def validate_principal_id(principal_id: str) -> str:
    if not PRINCIPAL_RE.match(principal_id):
        raise AuthError("invalid_principal", "principal_id contains unsupported characters")
    return principal_id


def validate_request_id(request_id: str) -> str:
    if not REQUEST_ID_RE.match(request_id):
        raise AuthError("invalid_request", "request_id must match [A-Za-z0-9][A-Za-z0-9._-]{0,127}")
    return request_id


def validate_capability(capability: str) -> str:
    if "*" in capability or not CAPABILITY_RE.match(capability):
        raise AuthError("invalid_capability", f"invalid capability: {capability}")
    return capability


def state_home() -> Path:
    raw = os.environ.get(STATE_HOME_ENV, "")
    if not raw:
        raise AuthError(
            "state_home_unavailable",
            "the daemon did not grant a bundle state home: check the tool's state_home declaration",
        )
    home = Path(raw)
    if not home.is_absolute():
        raise AuthError("state_home_unavailable", f"{STATE_HOME_ENV} must be absolute, saw {raw!r}")
    return home


def resolve_realm_dir(data: dict[str, Any]) -> Path:
    for key in CALLER_STATE_LOCATION_KEYS:
        if key in data:
            raise AuthError(
                "caller_state_location_refused",
                f"{key} is not accepted: central-auth state lives in the node-owned "
                "bundle state home",
            )
    realm_id = validate_realm(require_str(data, "realm_id"))
    home = state_home()
    # The daemon mounted the namespace named by the realm_id launch parameter, so
    # the home is the realm directory. Without a mount (isolation disabled) the
    # variable names the real directory, whose name must be that realm.
    if str(home) != ENFORCED_STATE_MOUNT and os.path.basename(os.path.realpath(home)) != realm_id:
        raise AuthError(
            "state_home_unavailable",
            f"the granted bundle state home is not the namespace for realm {realm_id!r}",
        )
    return home


def ensure_dirs(realm_dir: Path) -> None:
    for path in [
        realm_dir,
        realm_dir / "principals",
        realm_dir / "sessions",
        realm_dir / "invites",
        realm_dir / "failures",
        realm_dir / "operations",
    ]:
        path.mkdir(parents=True, exist_ok=True, mode=0o700)
        with contextlib.suppress(PermissionError):
            os.chmod(path, 0o700)


@contextlib.contextmanager
def realm_lock(realm_dir: Path):
    ensure_dirs(realm_dir)
    lock_path = realm_dir / "lock"
    with open(lock_path, "a+", encoding="utf-8") as fh:
        with contextlib.suppress(PermissionError):
            os.chmod(lock_path, 0o600)
        fcntl.flock(fh.fileno(), fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(fh.fileno(), fcntl.LOCK_UN)


def read_json_file(path: Path) -> dict[str, Any] | None:
    if not path.exists():
        return None
    with open(path, "r", encoding="utf-8") as fh:
        data = json.load(fh)
    if not isinstance(data, dict):
        raise AuthError("state_corrupt", f"{path} does not contain a JSON object")
    return data


def atomic_write_json(path: Path, value: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd, tmp_name = tempfile.mkstemp(prefix=f".{path.name}.", suffix=".tmp", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            os.chmod(tmp_name, 0o600)
            json.dump(value, fh, ensure_ascii=False, indent=2, sort_keys=True)
            fh.write("\n")
            fh.flush()
            os.fsync(fh.fileno())
        os.replace(tmp_name, path)
        dir_fd = os.open(path.parent, os.O_DIRECTORY)
        try:
            os.fsync(dir_fd)
        finally:
            os.close(dir_fd)
    finally:
        with contextlib.suppress(FileNotFoundError):
            os.unlink(tmp_name)


def append_audit(realm_dir: Path, event: str, fields: dict[str, Any]) -> None:
    line = {"ts": iso(), "event": event, **fields}
    path = realm_dir / "audit.jsonl"
    with open(path, "a", encoding="utf-8") as fh:
        with contextlib.suppress(PermissionError):
            os.chmod(path, 0o600)
        fh.write(json.dumps(line, ensure_ascii=False, separators=(",", ":")) + "\n")


def policy_path(realm_dir: Path) -> Path:
    return realm_dir / "policy.json"


def principal_path(realm_dir: Path, principal_id: str) -> Path:
    return realm_dir / "principals" / f"{validate_principal_id(principal_id)}.json"


def session_path(realm_dir: Path, session_token: str) -> Path:
    return realm_dir / "sessions" / f"{sha256_hex(session_token)}.json"


def invite_path(realm_dir: Path, invite_code: str) -> Path:
    return realm_dir / "invites" / f"{sha256_hex(invite_code)}.json"


def operation_path(realm_dir: Path, request_id: str) -> Path:
    return realm_dir / "operations" / f"{validate_request_id(request_id)}.json"


def failure_path(realm_dir: Path, subject: str) -> Path:
    return realm_dir / "failures" / f"{sha256_hex(subject)}.json"


def load_policy(realm_dir: Path) -> dict[str, Any]:
    policy = read_json_file(policy_path(realm_dir))
    if policy is None:
        return {"roles": {}, "allowed_capabilities": []}
    return validate_policy(policy)


def validate_login_throttle(value: Any) -> dict[str, int]:
    if value is None:
        raise AuthError(
            "policy_missing_login_throttle",
            "policy requires login_throttle {max_failures, window_secs, lockout_secs}",
        )
    if not isinstance(value, dict):
        raise AuthError("invalid_policy", "login_throttle must be an object")
    unknown = sorted(set(value) - set(LOGIN_THROTTLE_FIELDS))
    if unknown:
        raise AuthError("invalid_policy", f"login_throttle has unknown fields: {unknown}")
    out: dict[str, int] = {}
    for field in LOGIN_THROTTLE_FIELDS:
        raw = value.get(field)
        if isinstance(raw, bool) or not isinstance(raw, int) or raw < 1:
            raise AuthError("invalid_policy", f"login_throttle.{field} must be an integer >= 1")
        out[field] = raw
    return out


def require_login_throttle(policy: dict[str, Any]) -> dict[str, int]:
    return validate_login_throttle(policy.get("login_throttle"))


def validate_principal_management(value: Any, roles: dict[str, Any], allowed: set[str]) -> dict[str, Any]:
    """Which principals an operator may manage, as realm data.

    `operator_capability` marks an operator. A principal is manageable only when
    its roles are all in `managed_roles`, it holds no direct capability, and
    none of its effective capabilities is protected. The operator capability is
    always protected, so no operator can manage another operator."""
    if not isinstance(value, dict):
        raise AuthError("invalid_policy", "principal_management must be an object")
    unknown = sorted(set(value) - set(PRINCIPAL_MANAGEMENT_FIELDS))
    if unknown:
        raise AuthError("invalid_policy", f"principal_management has unknown fields: {unknown}")
    operator = value.get("operator_capability")
    if not isinstance(operator, str):
        raise AuthError("invalid_policy", "principal_management.operator_capability must be a string")
    operator = validate_capability(operator)
    if operator not in allowed:
        raise AuthError("invalid_policy", f"principal_management.operator_capability is outside allowed_capabilities: {operator}")
    managed = value.get("managed_roles")
    if not isinstance(managed, list) or not managed:
        raise AuthError("invalid_policy", "principal_management.managed_roles must be a non-empty array")
    for role in managed:
        if not isinstance(role, str) or role not in roles:
            raise AuthError("invalid_policy", f"principal_management.managed_roles names an unknown role: {role}")
    protected = value.get("protected_capabilities", [])
    if not isinstance(protected, list):
        raise AuthError("invalid_policy", "principal_management.protected_capabilities must be an array")
    protected_set = {operator}
    for cap in protected:
        if not isinstance(cap, str):
            raise AuthError("invalid_policy", "principal_management.protected_capabilities must contain strings")
        cap = validate_capability(cap)
        if cap not in allowed:
            raise AuthError("invalid_policy", f"principal_management.protected_capabilities is outside allowed_capabilities: {cap}")
        protected_set.add(cap)
    return {
        "operator_capability": operator,
        "managed_roles": sorted(set(managed)),
        "protected_capabilities": sorted(protected_set),
    }


def validate_policy(policy: Any) -> dict[str, Any]:
    if not isinstance(policy, dict):
        raise AuthError("invalid_policy", "policy must be an object")
    roles = policy.get("roles")
    allowed = policy.get("allowed_capabilities")
    if not isinstance(roles, dict) or not isinstance(allowed, list):
        raise AuthError("invalid_policy", "policy requires roles and allowed_capabilities")
    allowed_set = set()
    for cap in allowed:
        if not isinstance(cap, str):
            raise AuthError("invalid_policy", "allowed_capabilities must contain strings")
        allowed_set.add(validate_capability(cap))
    normalized_roles: dict[str, dict[str, list[str]]] = {}
    for role, spec in roles.items():
        if not isinstance(role, str) or not role:
            raise AuthError("invalid_policy", "role names must be non-empty strings")
        if not isinstance(spec, dict) or not isinstance(spec.get("capabilities"), list):
            raise AuthError("invalid_policy", f"role {role} requires capabilities array")
        caps = []
        for cap in spec["capabilities"]:
            if not isinstance(cap, str):
                raise AuthError("invalid_policy", f"role {role} has non-string capability")
            cap = validate_capability(cap)
            if cap not in allowed_set:
                raise AuthError("invalid_policy", f"role {role} references capability outside allowed_capabilities: {cap}")
            caps.append(cap)
        normalized_roles[role] = {"capabilities": sorted(set(caps))}
    out = {
        "roles": normalized_roles,
        "allowed_capabilities": sorted(allowed_set),
        "login_throttle": validate_login_throttle(policy.get("login_throttle")),
    }
    if policy.get("principal_management") is not None:
        out["principal_management"] = validate_principal_management(
            policy["principal_management"], normalized_roles, allowed_set
        )
    return out


def require_principal_management(policy: dict[str, Any]) -> dict[str, Any]:
    management = policy.get("principal_management")
    if management is None:
        raise AuthError(
            "principal_management_unconfigured",
            "the realm policy has no principal_management section; rerun set-policy with "
            "principal_management {operator_capability, managed_roles, protected_capabilities}",
        )
    return management


def validate_grants(policy: dict[str, Any], roles: list[str], capabilities: list[str]) -> None:
    role_map = policy["roles"]
    allowed = set(policy["allowed_capabilities"])
    for role in roles:
        if role not in role_map:
            raise AuthError("invalid_grant", f"unknown role: {role}")
    for cap in capabilities:
        validate_capability(cap)
        if cap not in allowed:
            raise AuthError("invalid_grant", f"capability outside allowed_capabilities: {cap}")


def effective_capabilities(policy: dict[str, Any], principal: dict[str, Any]) -> list[str]:
    roles = principal.get("roles", [])
    direct = principal.get("capabilities", [])
    if not isinstance(roles, list) or not isinstance(direct, list):
        raise AuthError("state_corrupt", "principal grants must be arrays")
    validate_grants(policy, roles, direct)
    caps = set(direct)
    for role in roles:
        caps.update(policy["roles"][role]["capabilities"])
    return sorted(caps)


def principal_session_generation(principal: dict[str, Any]) -> int:
    """Sessions record this at issue; a disable raises it, which invalidates
    every session issued before. A principal never disabled is at 0."""
    value = principal.get("session_generation", 0)
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise AuthError("state_corrupt", "principal session_generation is invalid")
    return value


def validate_principal_document(path: Path, principal: dict[str, Any], policy: dict[str, Any]) -> dict[str, Any]:
    """Check what security decisions read: the file name and the embedded id
    agree, and the status, generation and grants are well formed. A failure is
    state corruption, never a reason to leave the account out of a roster."""
    embedded_id = principal.get("id")
    if not isinstance(embedded_id, str) or validate_principal_id(embedded_id) != path.stem:
        raise AuthError("state_corrupt", f"principal identity mismatch: {path.name}")
    display_name = principal.get("display_name", embedded_id)
    if not isinstance(display_name, str) or not display_name.strip():
        raise AuthError("state_corrupt", f"principal display_name is invalid: {path.name}")
    if not isinstance(principal.get("disabled", False), bool):
        raise AuthError("state_corrupt", f"principal disabled is invalid: {path.name}")
    principal_session_generation(principal)
    effective_capabilities(policy, principal)
    return principal


def load_principal_strict(realm_dir: Path, principal_id: str, policy: dict[str, Any]) -> dict[str, Any] | None:
    path = principal_path(realm_dir, principal_id)
    principal = read_json_file(path)
    if principal is None:
        return None
    return validate_principal_document(path, principal, policy)


def strict_principal_roster(realm_dir: Path, policy: dict[str, Any]) -> list[dict[str, Any]]:
    principals: list[dict[str, Any]] = []
    for path in sorted((realm_dir / "principals").glob("*.json")):
        principal = read_json_file(path)
        if principal is None:
            # The caller holds the realm lock, so a vanished file is not a
            # reason to return a silently incomplete roster.
            raise AuthError("state_corrupt", f"principal disappeared while reading: {path.name}")
        principals.append(validate_principal_document(path, principal, policy))
    return principals


def is_operator(policy: dict[str, Any], management: dict[str, Any], principal: dict[str, Any]) -> bool:
    return not principal.get("disabled") and management["operator_capability"] in effective_capabilities(policy, principal)


def principal_manageability_reason(
    policy: dict[str, Any], management: dict[str, Any], actor: dict[str, Any], target: dict[str, Any]
) -> str | None:
    """A public-safe reason the actor cannot manage the target, or None."""
    if actor["id"] == target["id"]:
        return "self"
    if not is_operator(policy, management, actor):
        return "actor_not_authorized"
    roles = target.get("roles")
    if not isinstance(roles, list) or not roles or not set(roles) <= set(management["managed_roles"]):
        return "privileged_or_nonstandard_account"
    if target.get("capabilities") != []:
        return "privileged_or_nonstandard_account"
    if set(effective_capabilities(policy, target)) & set(management["protected_capabilities"]):
        return "privileged_or_nonstandard_account"
    return None


def load_operator(realm_dir: Path, policy: dict[str, Any], management: dict[str, Any], actor_id: str) -> dict[str, Any]:
    actor = load_principal_strict(realm_dir, actor_id, policy)
    if actor is None or not is_operator(policy, management, actor):
        raise AuthError("actor_not_authorized", "the actor is not an enabled operator of this realm")
    return actor


def public_principal(policy: dict[str, Any], principal: dict[str, Any]) -> dict[str, Any]:
    return {
        "id": principal["id"],
        "display_name": principal.get("display_name", principal["id"]),
        "roles": principal.get("roles", []),
        "capabilities": effective_capabilities(policy, principal),
        "disabled": bool(principal.get("disabled", False)),
    }


def hash_passphrase(passphrase: str) -> dict[str, Any]:
    salt = secrets.token_bytes(16)
    digest = hashlib.pbkdf2_hmac("sha256", passphrase.encode("utf-8"), salt, PBKDF2_ITERATIONS)
    return {
        "kdf": "pbkdf2_hmac_sha256",
        "iterations": PBKDF2_ITERATIONS,
        "salt_b64": base64.b64encode(salt).decode("ascii"),
        "hash_b64": base64.b64encode(digest).decode("ascii"),
    }


def dummy_verify(passphrase: str) -> bool:
    """Spend the same KDF work as a real check, then fail."""
    hashlib.pbkdf2_hmac("sha256", passphrase.encode("utf-8"), DUMMY_KDF_SALT, PBKDF2_ITERATIONS)
    return False


def verify_passphrase(passphrase: str, stored: dict[str, Any] | None) -> bool:
    """Constant-work check: every path that cannot match still runs one KDF."""
    if not isinstance(stored, dict) or stored.get("kdf") != "pbkdf2_hmac_sha256":
        return dummy_verify(passphrase)
    try:
        iterations = int(stored["iterations"])
        salt = base64.b64decode(stored["salt_b64"])
        expected = base64.b64decode(stored["hash_b64"])
    except Exception:
        return dummy_verify(passphrase)
    if iterations < 1:
        return dummy_verify(passphrase)
    actual = hashlib.pbkdf2_hmac("sha256", passphrase.encode("utf-8"), salt, iterations)
    return hmac.compare_digest(actual, expected)


def throttle_subjects(principal_id: str, client_address: str | None) -> list[str]:
    subjects = [f"principal:{principal_id}"]
    if client_address:
        subjects.append(f"address:{client_address}")
    return subjects


def locked_retry_after(realm_dir: Path, subjects: list[str], n: int) -> int | None:
    retry_after = 0
    for subject in subjects:
        record = read_json_file(failure_path(realm_dir, subject))
        if record and int(record.get("locked_until", 0)) > n:
            retry_after = max(retry_after, int(record["locked_until"]) - n)
    return retry_after or None


def record_login_failure(realm_dir: Path, subject: str, throttle: dict[str, int], n: int) -> None:
    path = failure_path(realm_dir, subject)
    record = read_json_file(path)
    if not record or n - int(record.get("first_at", 0)) >= throttle["window_secs"]:
        record = {"version": 1, "count": 0, "first_at": n, "locked_until": 0}
    record["count"] = int(record.get("count", 0)) + 1
    if record["count"] >= throttle["max_failures"]:
        record = {"version": 1, "count": 0, "first_at": n, "locked_until": n + throttle["lockout_secs"]}
    atomic_write_json(path, record)


def clear_login_failures(realm_dir: Path, subject: str) -> None:
    with contextlib.suppress(FileNotFoundError):
        failure_path(realm_dir, subject).unlink()


def session_expired(doc: dict[str, Any], n: int) -> bool:
    return doc.get("expires_at_unix", 0) <= n


def invite_expired(doc: dict[str, Any], n: int) -> bool:
    return doc.get("expires_at_unix", 0) <= n or doc.get("uses", 0) >= doc.get("max_uses", 1)


def failure_expired(doc: dict[str, Any], n: int, throttle: dict[str, int] | None) -> bool:
    if int(doc.get("locked_until", 0)) > n:
        return False
    if throttle is None:
        return False
    return n - int(doc.get("first_at", 0)) >= throttle["window_secs"]


def purge_expired(realm_dir: Path, policy: dict[str, Any], n: int, sample: int | None) -> dict[str, int]:
    """Remove expired sessions, spent invites and stale failure records.

    With `sample`, examines at most that many random entries per directory, so
    login and verify keep the realm bounded without walking it every call. The
    caller holds the realm lock."""
    throttle = policy.get("login_throttle")
    checks = {
        "sessions": session_expired,
        "invites": invite_expired,
        "failures": lambda doc, now: failure_expired(doc, now, throttle),
    }
    removed = {name: 0 for name in checks}
    for name, expired in checks.items():
        directory = realm_dir / name
        try:
            entries = [e for e in os.listdir(directory) if e.endswith(".json")]
        except FileNotFoundError:
            continue
        if sample is not None and len(entries) > sample:
            entries = secrets.SystemRandom().sample(entries, sample)
        for entry in entries:
            path = directory / entry
            try:
                doc = read_json_file(path)
            except (AuthError, ValueError, OSError):
                continue
            if doc and expired(doc, n):
                with contextlib.suppress(FileNotFoundError):
                    path.unlink()
                removed[name] += 1
    return removed


def issue_session(realm_dir: Path, policy: dict[str, Any], principal: dict[str, Any], ttl_secs: int) -> dict[str, Any]:
    ttl_secs = max(60, min(ttl_secs, 30 * 24 * 3600))
    token = token_urlsafe()
    expires_at_unix = now_unix() + ttl_secs
    session = {
        "version": 1,
        "token_hash": sha256_hex(token),
        "principal_id": principal["id"],
        "session_generation": principal_session_generation(principal),
        "created_at": iso(),
        "expires_at_unix": expires_at_unix,
        "expires_at": iso(expires_at_unix),
        "revoked_at": None,
    }
    atomic_write_json(session_path(realm_dir, token), session)
    append_audit(realm_dir, "session.issue", {"principal_id": principal["id"], "expires_at": session["expires_at"]})
    purge_expired(realm_dir, policy, now_unix(), OPPORTUNISTIC_GC_SAMPLE)
    return {
        "ok": True,
        "session_token": token,
        "expires_at": session["expires_at"],
        "principal": public_principal(policy, principal),
    }


def command_set_policy(data: dict[str, Any]) -> dict[str, Any]:
    realm_dir = resolve_realm_dir(data)
    policy = validate_policy(data.get("policy"))
    with realm_lock(realm_dir):
        atomic_write_json(policy_path(realm_dir), policy)
        append_audit(realm_dir, "policy.set", {"roles": sorted(policy["roles"].keys())})
    return {"ok": True, "policy": policy}


def command_create_principal(data: dict[str, Any]) -> dict[str, Any]:
    realm_dir = resolve_realm_dir(data)
    principal_id = validate_principal_id(require_str(data, "principal_id"))
    display_name = require_str(data, "display_name")
    roles = string_list(data, "roles")
    capabilities = string_list(data, "capabilities")
    passphrase = optional_secret(data, "passphrase")
    bootstrap = bool(data.get("bootstrap", False))
    with realm_lock(realm_dir):
        existing = list((realm_dir / "principals").glob("*.json"))
        if bootstrap and existing:
            raise AuthError("bootstrap_forbidden", "bootstrap is only allowed before principals exist")
        if not bootstrap and principal_path(realm_dir, principal_id).exists() and not data.get("replace", False):
            raise AuthError("principal_exists", "principal already exists")
        policy = load_policy(realm_dir)
        validate_grants(policy, roles, capabilities)
        principal = {
            "version": 1,
            "id": principal_id,
            "display_name": display_name,
            "roles": roles,
            "capabilities": capabilities,
            "disabled": bool(data.get("disabled", False)),
            "session_generation": 0,
            "created_at": iso(),
            "updated_at": iso(),
        }
        if passphrase:
            principal["passphrase"] = hash_passphrase(passphrase)
        atomic_write_json(principal_path(realm_dir, principal_id), principal)
        append_audit(realm_dir, "principal.upsert", {"principal_id": principal_id, "roles": roles})
    return {"ok": True, "principal": public_principal(policy, principal)}


def command_create_invite(data: dict[str, Any]) -> dict[str, Any]:
    realm_dir = resolve_realm_dir(data)
    roles = string_list(data, "roles")
    capabilities = string_list(data, "capabilities")
    ttl_secs = optional_int(data, "ttl_secs", 86400, minimum=1, maximum=30 * 24 * 3600)
    max_uses = optional_int(data, "max_uses", 1, minimum=1)
    with realm_lock(realm_dir):
        policy = load_policy(realm_dir)
        validate_grants(policy, roles, capabilities)
        invite_code = token_urlsafe()
        expires_at_unix = now_unix() + ttl_secs
        invite = {
            "version": 1,
            "code_hash": sha256_hex(invite_code),
            "roles": roles,
            "capabilities": capabilities,
            "max_uses": max_uses,
            "uses": 0,
            "created_at": iso(),
            "expires_at_unix": expires_at_unix,
            "expires_at": iso(expires_at_unix),
            "disabled": False,
        }
        atomic_write_json(invite_path(realm_dir, invite_code), invite)
        append_audit(realm_dir, "invite.create", {"roles": roles, "expires_at": invite["expires_at"]})
    return {"ok": True, "invite_code": invite_code, "expires_at": invite["expires_at"], "max_uses": max_uses}


def load_principal_for_login(realm_dir: Path, principal_id: str) -> dict[str, Any] | None:
    return read_json_file(principal_path(realm_dir, principal_id))


def command_login(data: dict[str, Any]) -> dict[str, Any]:
    method = data.get("method", "passphrase")
    if method not in {"passphrase", "invite"}:
        raise AuthError("invalid_request", "method must be passphrase or invite")
    realm_dir = resolve_realm_dir(data)
    ttl_secs = optional_int(data, "ttl_secs", 8 * 3600, minimum=60, maximum=30 * 24 * 3600)
    with realm_lock(realm_dir):
        policy = load_policy(realm_dir)
        if method == "passphrase":
            throttle = require_login_throttle(policy)
            principal_id = validate_principal_id(require_str(data, "principal_id"))
            client_address = optional_str(data, "client_address")
            subjects = throttle_subjects(principal_id, client_address)
            passphrase = require_secret(data, "passphrase")
            n = now_unix()
            retry_after = locked_retry_after(realm_dir, subjects, n)
            if retry_after is not None:
                append_audit(realm_dir, "login.locked", {"principal_id": principal_id})
                return {"ok": False, "error": {"code": "locked", "retry_after_secs": retry_after}}
            principal = load_principal_for_login(realm_dir, principal_id)
            stored = principal.get("passphrase") if principal and not principal.get("disabled") else None
            # Always run the KDF (verify_passphrase falls back to a dummy), so a
            # missing or disabled principal costs the same as a wrong passphrase.
            valid = verify_passphrase(passphrase, stored)
            if not principal or principal.get("disabled") or not valid:
                for subject in subjects:
                    record_login_failure(realm_dir, subject, throttle, n)
                append_audit(realm_dir, "login.failure", {"principal_id": principal_id})
                return {"ok": False, "error": {"code": "invalid_credentials"}}
            effective_capabilities(policy, principal)
            clear_login_failures(realm_dir, subjects[0])
            return issue_session(realm_dir, policy, principal, ttl_secs)

        invite_code = require_str(data, "invite_code")
        invite = read_json_file(invite_path(realm_dir, invite_code))
        if not invite or invite.get("disabled") or invite.get("expires_at_unix", 0) <= now_unix() or invite.get("uses", 0) >= invite.get("max_uses", 1):
            return {"ok": False, "error": {"code": "invalid_credentials"}}
        principal_id = validate_principal_id(require_str(data, "principal_id"))
        if principal_path(realm_dir, principal_id).exists():
            return {"ok": False, "error": {"code": "invalid_credentials"}}
        display_name = require_str(data, "display_name")
        passphrase = require_secret(data, "passphrase")
        roles = list(invite.get("roles", []))
        capabilities = list(invite.get("capabilities", []))
        validate_grants(policy, roles, capabilities)
        principal = {
            "version": 1,
            "id": principal_id,
            "display_name": display_name,
            "roles": sorted(set(roles)),
            "capabilities": sorted(set(capabilities)),
            "disabled": False,
            "session_generation": 0,
            "created_at": iso(),
            "updated_at": iso(),
            "passphrase": hash_passphrase(passphrase),
        }
        invite["uses"] = int(invite.get("uses", 0)) + 1
        atomic_write_json(invite_path(realm_dir, invite_code), invite)
        atomic_write_json(principal_path(realm_dir, principal_id), principal)
        append_audit(realm_dir, "invite.redeem", {"principal_id": principal_id})
        return issue_session(realm_dir, policy, principal, ttl_secs)


def verify_session(data: dict[str, Any]) -> dict[str, Any]:
    realm_dir = resolve_realm_dir(data)
    session_token = require_str(data, "session_token")
    required_capability = optional_str(data, "required_capability")
    with realm_lock(realm_dir):
        policy = load_policy(realm_dir)
        n = now_unix()
        touched = session_path(realm_dir, session_token)
        session = read_json_file(touched)
        if session and session_expired(session, n):
            touched.unlink()
        purge_expired(realm_dir, policy, n, OPPORTUNISTIC_GC_SAMPLE)
        if not session or session.get("revoked_at") or session_expired(session, n):
            return {"valid": False, "allowed": False, "error": {"code": "invalid_session"}}
        principal_id = session.get("principal_id")
        if not isinstance(principal_id, str):
            raise AuthError("state_corrupt", "session principal_id is invalid")
        principal = read_json_file(principal_path(realm_dir, principal_id))
        if not principal or principal.get("disabled"):
            return {"valid": False, "allowed": False, "error": {"code": "invalid_session"}}
        generation = session.get("session_generation", 0)
        if (
            isinstance(generation, bool)
            or not isinstance(generation, int)
            or generation != principal_session_generation(principal)
        ):
            return {"valid": False, "allowed": False, "error": {"code": "invalid_session"}}
        try:
            public = public_principal(policy, principal)
        except AuthError as exc:
            if exc.code in {"invalid_grant", "invalid_policy"}:
                return {"valid": False, "allowed": False, "error": {"code": "invalid_session_grants"}}
            raise
        allowed = True
        if required_capability:
            validate_capability(required_capability)
            if required_capability not in policy["allowed_capabilities"]:
                allowed = False
            elif required_capability not in public["capabilities"]:
                allowed = False
        out = {
            "valid": True,
            "allowed": allowed,
            "principal": public,
            "session": {"expires_at": session.get("expires_at"), "expires_at_unix": session.get("expires_at_unix")},
        }
        if required_capability and not allowed:
            out["error"] = {"code": "missing_capability"}
        return out


def command_check_capability(data: dict[str, Any]) -> dict[str, Any]:
    require_str(data, "required_capability")
    return verify_session(data)


def command_revoke_session(data: dict[str, Any]) -> dict[str, Any]:
    realm_dir = resolve_realm_dir(data)
    session_token = require_str(data, "session_token")
    with realm_lock(realm_dir):
        path = session_path(realm_dir, session_token)
        session = read_json_file(path)
        if not session:
            return {"ok": True, "revoked": False}
        if not session.get("revoked_at"):
            session["revoked_at"] = iso()
            atomic_write_json(path, session)
            append_audit(realm_dir, "session.revoke", {"principal_id": session.get("principal_id")})
    return {"ok": True, "revoked": True}


def command_gc(data: dict[str, Any]) -> dict[str, Any]:
    """Full sweep for operators. Login and verify also purge a bounded sample
    on every call, so a realm stays bounded without anything scheduling this."""
    realm_dir = resolve_realm_dir(data)
    with realm_lock(realm_dir):
        policy = load_policy(realm_dir)
        removed = purge_expired(realm_dir, policy, now_unix(), None)
        fields = {
            "removed_sessions": removed["sessions"],
            "removed_invites": removed["invites"],
            "removed_failures": removed["failures"],
        }
        append_audit(realm_dir, "gc", fields)
    return {"ok": True, **fields}


def command_list_principals(data: dict[str, Any]) -> dict[str, Any]:
    """Secret-free roster of the realm's accounts, read under the realm lock.
    An unreadable or inconsistent principal fails the listing (`state_corrupt`)
    rather than being skipped, so `complete` is always true on success.

    With `actor_principal_id`, the actor must be an enabled operator under the
    policy's principal_management, and each entry also says whether that actor
    may manage it (`manageable`, else `protected_reason`)."""
    realm_dir = resolve_realm_dir(data)
    requested_actor = optional_str(data, "actor_principal_id")
    with realm_lock(realm_dir):
        policy = load_policy(realm_dir)
        actor: dict[str, Any] | None = None
        management: dict[str, Any] | None = None
        if requested_actor is not None:
            management = require_principal_management(policy)
            actor = load_operator(realm_dir, policy, management, validate_principal_id(requested_actor))
        principals: list[dict[str, Any]] = []
        for principal in strict_principal_roster(realm_dir, policy):
            public = {
                "id": principal["id"],
                "display_name": principal.get("display_name", principal["id"]),
                "roles": principal.get("roles", []),
                "disabled": bool(principal.get("disabled", False)),
            }
            if actor is not None and management is not None:
                reason = principal_manageability_reason(policy, management, actor, principal)
                public["manageable"] = reason is None
                if reason is not None:
                    public["protected_reason"] = reason
            principals.append(public)
    return {"ok": True, "complete": True, "principals": principals}


def sweep_principal_sessions(realm_dir: Path, principal_id: str) -> tuple[int, bool, list[str]]:
    """Mark every readable session of the principal revoked. The raised
    session generation already invalidates them; this leaves durable revocation
    records. An unreadable unrelated session does not undo the disable."""
    revoked = 0
    errors: list[str] = []
    for path in sorted((realm_dir / "sessions").glob("*.json")):
        try:
            session = read_json_file(path)
            if session is None:
                raise AuthError("state_corrupt", "session disappeared while reading")
            if session.get("principal_id") != principal_id or session.get("revoked_at"):
                continue
            session["revoked_at"] = iso()
            atomic_write_json(path, session)
            revoked += 1
        except Exception as exc:
            errors.append(f"{path.name}:{type(exc).__name__}")
    return revoked, not errors, errors[:20]


def load_status_operation(realm_dir: Path, request_id: str) -> dict[str, Any] | None:
    operation = read_json_file(operation_path(realm_dir, request_id))
    if operation is None:
        return None
    if operation.get("version") != 1 or operation.get("kind") != STATUS_OPERATION_KIND:
        raise AuthError("state_corrupt", f"operation {request_id} is not an account status change")
    return operation


def status_operation_result(operation: dict[str, Any]) -> dict[str, Any]:
    return {
        "ok": True,
        "request_id": operation["request_id"],
        "principal": {"id": operation["principal_id"], "disabled": operation["disabled"]},
        "changed": bool(operation.get("changed", False)),
        "session_generation": operation.get("session_generation") or 0,
        "revoked_sessions": int(operation.get("revoked_sessions", 0)),
        "session_sweep_complete": bool(operation.get("session_sweep_complete", False)),
        "audit_recorded": bool(operation.get("audit_recorded", False)),
        "complete": operation.get("status") == "complete",
    }


def command_set_principal_disabled(data: dict[str, Any]) -> dict[str, Any]:
    """Disable or re-enable one manageable account, idempotent per request_id.

    Only the disabled flag changes; passphrase, display name, roles and
    capabilities are untouched. Disabling raises the principal's session
    generation before anything else, so every earlier session stops verifying
    even if the session sweep or the audit write does not finish; a retry with
    the same request_id resumes those steps. A completed request replays its
    result and is never applied again."""
    realm_dir = resolve_realm_dir(data)
    actor_id = validate_principal_id(require_str(data, "actor_principal_id"))
    principal_id = validate_principal_id(require_str(data, "principal_id"))
    disabled = require_bool(data, "disabled")
    request_id = validate_request_id(require_str(data, "request_id"))
    with realm_lock(realm_dir):
        policy = load_policy(realm_dir)
        management = require_principal_management(policy)
        actor = load_operator(realm_dir, policy, management, actor_id)
        target = load_principal_strict(realm_dir, principal_id, policy)
        if target is None:
            raise AuthError("principal_not_found", "principal was not found")

        operation = load_status_operation(realm_dir, request_id)
        if operation is not None:
            if (
                operation.get("actor_principal_id") != actor_id
                or operation.get("principal_id") != principal_id
                or operation.get("disabled") is not disabled
            ):
                raise AuthError("request_id_conflict", "request_id was already used for another operation")
            if operation.get("status") == "complete":
                return status_operation_result(operation)
        # Checked again when a pending request resumes: the target may have
        # gained a protected role since.
        reason = principal_manageability_reason(policy, management, actor, target)
        if reason is not None:
            raise AuthError("principal_not_manageable", f"principal cannot be managed: {reason}")
        if operation is None:
            operation = {
                "version": 1,
                "kind": STATUS_OPERATION_KIND,
                "request_id": request_id,
                "actor_principal_id": actor_id,
                "principal_id": principal_id,
                "disabled": disabled,
                "created_at": iso(),
                "status": "pending",
                "changed": None,
                "session_generation": None,
                "revoked_sessions": 0,
                "session_sweep_complete": False,
                "session_sweep_errors": [],
                "audit_recorded": False,
            }
            atomic_write_json(operation_path(realm_dir, request_id), operation)

        if operation.get("status") == "pending":
            was_disabled = bool(target.get("disabled", False))
            generation = principal_session_generation(target)
            if disabled and not was_disabled:
                generation += 1
            target["disabled"] = disabled
            target["session_generation"] = generation
            target["updated_at"] = iso()
            atomic_write_json(principal_path(realm_dir, principal_id), target)
            operation.update({
                "status": "state_applied",
                "changed": was_disabled != disabled,
                "session_generation": generation,
                "state_applied_at": iso(),
            })
            atomic_write_json(operation_path(realm_dir, request_id), operation)

        if disabled and not operation.get("session_sweep_complete", False):
            revoked, complete, errors = sweep_principal_sessions(realm_dir, principal_id)
            operation["revoked_sessions"] = int(operation.get("revoked_sessions", 0)) + revoked
            operation["session_sweep_complete"] = complete
            operation["session_sweep_errors"] = errors
            atomic_write_json(operation_path(realm_dir, request_id), operation)
            if not complete:
                return status_operation_result(operation)
        elif not disabled and not operation.get("session_sweep_complete", False):
            operation["session_sweep_complete"] = True
            atomic_write_json(operation_path(realm_dir, request_id), operation)

        if not operation.get("audit_recorded", False):
            try:
                append_audit(realm_dir, "principal.status_changed", {
                    "request_id": request_id,
                    "actor_principal_id": actor_id,
                    "principal_id": principal_id,
                    "disabled": disabled,
                    "changed": bool(operation.get("changed", False)),
                    "session_generation": operation.get("session_generation") or 0,
                    "revoked_sessions": operation.get("revoked_sessions", 0),
                })
            except OSError as exc:
                operation["audit_error"] = type(exc).__name__
                atomic_write_json(operation_path(realm_dir, request_id), operation)
                return status_operation_result(operation)
            operation["audit_recorded"] = True
        operation["status"] = "complete"
        operation["completed_at"] = iso()
        atomic_write_json(operation_path(realm_dir, request_id), operation)
        return status_operation_result(operation)


COMMANDS = {
    "set-policy": command_set_policy,
    "create-principal": command_create_principal,
    "list-principals": command_list_principals,
    "create-invite": command_create_invite,
    "login": command_login,
    "verify-session": verify_session,
    "check-capability": command_check_capability,
    "revoke-session": command_revoke_session,
    "set-principal-disabled": command_set_principal_disabled,
    "gc": command_gc,
}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="RyeOS central-auth tool")
    parser.add_argument("command", choices=sorted(COMMANDS))
    args = parser.parse_args(argv)
    try:
        data = read_stdin_json()
        result = COMMANDS[args.command](data)
        write_json(result)
        return 0
    except AuthError as exc:
        write_json({"ok": False, "error": {"code": exc.code, "message": exc.message}})
        return 1 if exc.code not in {"invalid_credentials"} else 0
    except Exception as exc:
        write_json({"ok": False, "error": {"code": "internal_error", "message": str(exc)}})
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
