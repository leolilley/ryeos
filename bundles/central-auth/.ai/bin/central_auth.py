# rye:unsigned
"""Central web-app auth primitives for RyeOS projects."""

from __future__ import annotations

__version__ = "0.1.0"
__category__ = "ryeos/central-auth"
__description__ = "Shared implementation for central-auth tool descriptors"

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
SESSION_BYTES = 32
PBKDF2_ITERATIONS = 210_000


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


def validate_capability(capability: str) -> str:
    if "*" in capability or not CAPABILITY_RE.match(capability):
        raise AuthError("invalid_capability", f"invalid capability: {capability}")
    return capability


def resolve_realm_dir(data: dict[str, Any]) -> Path:
    realm_id = validate_realm(require_str(data, "realm_id"))
    runtime_state_dir = optional_str(data, "runtime_state_dir")
    if runtime_state_dir:
        root = Path(runtime_state_dir)
        if not root.is_absolute():
            raise AuthError("invalid_request", "runtime_state_dir must be absolute")
    else:
        project_path = Path(require_str(data, "project_path"))
        if not project_path.is_absolute():
            raise AuthError("invalid_request", "project_path must be absolute")
        root = project_path / ".ai" / "state" / "central-auth"
    return root / "realms" / realm_id


def ensure_dirs(realm_dir: Path) -> None:
    for path in [realm_dir, realm_dir / "principals", realm_dir / "sessions", realm_dir / "invites"]:
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


def load_policy(realm_dir: Path) -> dict[str, Any]:
    policy = read_json_file(policy_path(realm_dir))
    if policy is None:
        return {"roles": {}, "allowed_capabilities": []}
    validate_policy(policy)
    return policy


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
    return {"roles": normalized_roles, "allowed_capabilities": sorted(allowed_set)}


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


def verify_passphrase(passphrase: str, stored: dict[str, Any] | None) -> bool:
    if not stored or stored.get("kdf") != "pbkdf2_hmac_sha256":
        return False
    try:
        iterations = int(stored["iterations"])
        salt = base64.b64decode(stored["salt_b64"])
        expected = base64.b64decode(stored["hash_b64"])
    except Exception:
        return False
    actual = hashlib.pbkdf2_hmac("sha256", passphrase.encode("utf-8"), salt, iterations)
    return hmac.compare_digest(actual, expected)


def issue_session(realm_dir: Path, policy: dict[str, Any], principal: dict[str, Any], ttl_secs: int) -> dict[str, Any]:
    ttl_secs = max(60, min(ttl_secs, 30 * 24 * 3600))
    token = token_urlsafe()
    expires_at_unix = now_unix() + ttl_secs
    session = {
        "version": 1,
        "token_hash": sha256_hex(token),
        "principal_id": principal["id"],
        "created_at": iso(),
        "expires_at_unix": expires_at_unix,
        "expires_at": iso(expires_at_unix),
        "revoked_at": None,
    }
    atomic_write_json(session_path(realm_dir, token), session)
    append_audit(realm_dir, "session.issue", {"principal_id": principal["id"], "expires_at": session["expires_at"]})
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
            principal_id = validate_principal_id(require_str(data, "principal_id"))
            principal = load_principal_for_login(realm_dir, principal_id)
            passphrase = require_secret(data, "passphrase")
            if not principal or principal.get("disabled") or not verify_passphrase(passphrase, principal.get("passphrase")):
                return {"ok": False, "error": {"code": "invalid_credentials"}}
            effective_capabilities(policy, principal)
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
        session = read_json_file(session_path(realm_dir, session_token))
        if not session or session.get("revoked_at") or session.get("expires_at_unix", 0) <= now_unix():
            return {"valid": False, "allowed": False, "error": {"code": "invalid_session"}}
        principal_id = session.get("principal_id")
        if not isinstance(principal_id, str):
            raise AuthError("state_corrupt", "session principal_id is invalid")
        principal = read_json_file(principal_path(realm_dir, principal_id))
        if not principal or principal.get("disabled"):
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
    realm_dir = resolve_realm_dir(data)
    removed_sessions = 0
    removed_invites = 0
    with realm_lock(realm_dir):
        n = now_unix()
        for path in (realm_dir / "sessions").glob("*.json"):
            doc = read_json_file(path)
            if doc and doc.get("expires_at_unix", 0) <= n:
                path.unlink()
                removed_sessions += 1
        for path in (realm_dir / "invites").glob("*.json"):
            doc = read_json_file(path)
            if doc and (doc.get("expires_at_unix", 0) <= n or doc.get("uses", 0) >= doc.get("max_uses", 1)):
                path.unlink()
                removed_invites += 1
        append_audit(realm_dir, "gc", {"removed_sessions": removed_sessions, "removed_invites": removed_invites})
    return {"ok": True, "removed_sessions": removed_sessions, "removed_invites": removed_invites}


COMMANDS = {
    "set-policy": command_set_policy,
    "create-principal": command_create_principal,
    "create-invite": command_create_invite,
    "login": command_login,
    "verify-session": verify_session,
    "check-capability": command_check_capability,
    "revoke-session": command_revoke_session,
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
