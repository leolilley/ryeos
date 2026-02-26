# rye:signed:2026-02-26T05:52:24Z:6dfef20f9c49624b0ebfdd1d3671f9edaf8fd385452b694bd8391e0045b0dbd9:hvcbUfMrm6zjXCOaS_n1LTQStg8db4MedCoHr-gIw9uOMikdnOxaAC9CKj8Ft-SyJ0dMJIenNHAPDW_Sy6O2Bw==:4b987fd4e40303ac
# PROTECTED: Core RYE tool - do not override
"""
Capability Token System

Provides capability tokens for permission enforcement in the safety harness.
Tokens are signed using Ed25519 for cryptographic verification.
"""

__tool_type__ = "python"
__version__ = "1.0.0"
__category__ = "rye/agent/permissions/capability_tokens"
__tool_description__ = "Capability token management"

import base64
import hashlib
import json
import os
import uuid
from dataclasses import dataclass, field, asdict
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional, Set, Tuple

# Try to import cryptography for Ed25519 signing
try:
    from cryptography.hazmat.primitives import serialization
    from cryptography.hazmat.primitives.asymmetric.ed25519 import (
        Ed25519PrivateKey,
        Ed25519PublicKey,
    )
    CRYPTOGRAPHY_AVAILABLE = True
except ImportError:
    CRYPTOGRAPHY_AVAILABLE = False


# Default key paths — unified under USER_SPACE / AI_DIR / keys/
try:
    from rye.constants import AI_DIR
    from rye.utils.path_utils import get_user_space
    DEFAULT_KEY_DIR = get_user_space() / AI_DIR / "keys"
except ImportError:
    from rye.constants import AI_DIR
    DEFAULT_KEY_DIR = Path.home() / AI_DIR / "keys"
PRIVATE_KEY_FILE = "private_key.pem"
PUBLIC_KEY_FILE = "public_key.pem"


@dataclass
class CapabilityToken:
    """
    Capability token for permission enforcement.
    
    Attributes:
        caps: List of granted capabilities (e.g., ["fs.read", "tool.bash"])
        aud: Audience identifier (prevents cross-service replay)
        exp: Expiry time (UTC)
        parent_id: Parent token ID for delegation chains
        directive_id: Source directive that minted this token
        thread_id: Thread this token belongs to
        signature: Ed25519 signature (set after signing)
        token_id: Unique token identifier
    """
    
    caps: List[str]
    aud: str
    exp: datetime
    directive_id: str
    thread_id: str
    parent_id: Optional[str] = None
    signature: Optional[str] = None
    token_id: str = field(default_factory=lambda: str(uuid.uuid4()))
    
    def to_dict(self) -> Dict[str, Any]:
        """Convert token to dictionary for serialization."""
        return {
            "token_id": self.token_id,
            "caps": self.caps,
            "aud": self.aud,
            "exp": self.exp.isoformat(),
            "parent_id": self.parent_id,
            "directive_id": self.directive_id,
            "thread_id": self.thread_id,
            "signature": self.signature,
        }
    
    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> "CapabilityToken":
        """Create token from dictionary."""
        exp = data["exp"]
        if isinstance(exp, str):
            exp = datetime.fromisoformat(exp)
        
        return cls(
            token_id=data.get("token_id", str(uuid.uuid4())),
            caps=data["caps"],
            aud=data["aud"],
            exp=exp,
            parent_id=data.get("parent_id"),
            directive_id=data["directive_id"],
            thread_id=data["thread_id"],
            signature=data.get("signature"),
        )
    
    def to_jwt(self) -> str:
        """Serialize token to JWT-like base64 string."""
        data = self.to_dict()
        json_bytes = json.dumps(data, sort_keys=True).encode("utf-8")
        return base64.urlsafe_b64encode(json_bytes).decode("ascii")
    
    @classmethod
    def from_jwt(cls, token_str: str) -> "CapabilityToken":
        """Deserialize token from JWT-like base64 string."""
        json_bytes = base64.urlsafe_b64decode(token_str.encode("ascii"))
        data = json.loads(json_bytes.decode("utf-8"))
        return cls.from_dict(data)
    
    def is_expired(self) -> bool:
        """Check if token has expired."""
        now = datetime.now(timezone.utc)
        # Handle naive datetimes by assuming UTC
        exp = self.exp
        if exp.tzinfo is None:
            exp = exp.replace(tzinfo=timezone.utc)
        return now > exp
    
    def has_capability(self, capability: str) -> bool:
        """Check if token grants a specific capability.
        
        Uses prefix/glob matching and structural implication.
        """
        return check_capability(self.caps, capability)
    
    def has_any_capability(self, capabilities: List[str]) -> bool:
        """Check if token grants any of the specified capabilities."""
        return any(check_capability(self.caps, cap) for cap in capabilities)
    
    def has_all_capabilities(self, capabilities: List[str]) -> bool:
        """Check if token grants all of the specified capabilities."""
        all_ok, _ = check_all_capabilities(self.caps, capabilities)
        return all_ok
    
    def get_expanded_capabilities(self) -> List[str]:
        """Get all capabilities including implied ones from hierarchy."""
        return sorted(expand_capabilities(self.caps))
    
    def get_payload_for_signing(self) -> bytes:
        """Get the token payload for signing (excludes signature)."""
        data = {
            "token_id": self.token_id,
            "caps": sorted(self.caps),  # Sort for deterministic output
            "aud": self.aud,
            "exp": self.exp.isoformat(),
            "parent_id": self.parent_id,
            "directive_id": self.directive_id,
            "thread_id": self.thread_id,
        }
        return json.dumps(data, sort_keys=True).encode("utf-8")


def _get_key_path(key_type: str = "private") -> Path:
    """Get path to key file."""
    filename = PRIVATE_KEY_FILE if key_type == "private" else PUBLIC_KEY_FILE
    return DEFAULT_KEY_DIR / filename


def _ensure_key_directory() -> None:
    """Ensure key directory exists with proper permissions."""
    DEFAULT_KEY_DIR.mkdir(parents=True, exist_ok=True)
    # Set directory permissions to 700 (owner only)
    os.chmod(DEFAULT_KEY_DIR, 0o700)


def generate_keypair() -> tuple[bytes, bytes]:
    """Generate a new Ed25519 keypair.
    
    Returns:
        Tuple of (private_key_pem, public_key_pem)
    """
    if not CRYPTOGRAPHY_AVAILABLE:
        raise RuntimeError("cryptography library required for key generation")
    
    private_key = Ed25519PrivateKey.generate()
    public_key = private_key.public_key()
    
    private_pem = private_key.private_bytes(
        encoding=serialization.Encoding.PEM,
        format=serialization.PrivateFormat.PKCS8,
        encryption_algorithm=serialization.NoEncryption(),
    )
    
    public_pem = public_key.public_bytes(
        encoding=serialization.Encoding.PEM,
        format=serialization.PublicFormat.SubjectPublicKeyInfo,
    )
    
    return private_pem, public_pem


def save_keypair(private_key: bytes, public_key: bytes) -> None:
    """Save keypair to default location."""
    _ensure_key_directory()
    
    private_path = _get_key_path("private")
    public_path = _get_key_path("public")
    
    private_path.write_bytes(private_key)
    os.chmod(private_path, 0o600)  # Owner read/write only
    
    public_path.write_bytes(public_key)
    os.chmod(public_path, 0o644)  # Owner read/write, others read


def load_private_key() -> bytes:
    """Load private key from default location."""
    key_path = _get_key_path("private")
    if not key_path.exists():
        raise FileNotFoundError(f"Private key not found at {key_path}")
    return key_path.read_bytes()


def load_public_key() -> bytes:
    """Load public key from default location."""
    key_path = _get_key_path("public")
    if not key_path.exists():
        raise FileNotFoundError(f"Public key not found at {key_path}")
    return key_path.read_bytes()


def ensure_keypair() -> tuple[bytes, bytes]:
    """Ensure a keypair exists, generating one if needed.
    
    Returns:
        Tuple of (private_key_pem, public_key_pem)
    """
    try:
        private_key = load_private_key()
        public_key = load_public_key()
        return private_key, public_key
    except FileNotFoundError:
        private_key, public_key = generate_keypair()
        save_keypair(private_key, public_key)
        return private_key, public_key


def sign_token(token: CapabilityToken, private_key: bytes) -> str:
    """Sign a capability token using Ed25519.
    
    Args:
        token: The token to sign
        private_key: Ed25519 private key in PEM format
        
    Returns:
        Base64-encoded signature string
    """
    if not CRYPTOGRAPHY_AVAILABLE:
        raise RuntimeError("cryptography library required for signing")
    
    key = serialization.load_pem_private_key(private_key, password=None)
    payload = token.get_payload_for_signing()
    signature = key.sign(payload)
    
    return base64.urlsafe_b64encode(signature).decode("ascii")


def verify_token(token: CapabilityToken, public_key: bytes) -> Optional[CapabilityToken]:
    """Verify a capability token signature.
    
    Args:
        token: The token to verify
        public_key: Ed25519 public key in PEM format
        
    Returns:
        The token if valid, None if invalid or expired
    """
    if not CRYPTOGRAPHY_AVAILABLE:
        return None
    
    if token.is_expired():
        return None
    
    if not token.signature:
        return None
    
    try:
        key = serialization.load_pem_public_key(public_key)
        
        payload = token.get_payload_for_signing()
        signature = base64.urlsafe_b64decode(token.signature.encode("ascii"))
        
        try:
            key.verify(signature, payload)
            return token
        except Exception:
            return None
            
    except Exception:
        return None


def mint_token(
    caps: List[str],
    directive_id: str,
    thread_id: str,
    parent_id: Optional[str] = None,
    exp_hours: int = 1,
    aud: str = "rye",
) -> CapabilityToken:
    """Mint a new capability token.
    
    Args:
        caps: List of capabilities to grant
        directive_id: ID of the directive minting this token
        thread_id: ID of the thread this token is for
        parent_id: Optional parent token ID for delegation chains
        exp_hours: Token validity in hours (default 1)
        aud: Audience identifier (default "rye")
        
    Returns:
        Unsigned CapabilityToken (call sign_token to sign)
    """
    exp = datetime.now(timezone.utc) + timedelta(hours=exp_hours)
    
    return CapabilityToken(
        caps=list(caps),
        aud=aud,
        exp=exp,
        parent_id=parent_id,
        directive_id=directive_id,
        thread_id=thread_id,
    )


def attenuate_token(
    parent_token: CapabilityToken,
    child_declared_caps: List[str],
) -> CapabilityToken:
    """Attenuate a parent token for a child thread.
    
    Implements capability intersection: child only gets capabilities
    that BOTH the parent has AND the child declares it needs.
    
    Args:
        parent_token: The parent thread's token
        child_declared_caps: Capabilities the child directive declares
        
    Returns:
        New token with attenuated capabilities
    """
    # Intersection: child gets only what parent has AND child declares
    parent_caps = set(parent_token.caps)
    child_caps = set(child_declared_caps)
    attenuated_caps = list(parent_caps & child_caps)
    
    # Create new token with attenuated caps
    return CapabilityToken(
        caps=sorted(attenuated_caps),  # Sort for consistency
        aud=parent_token.aud,
        exp=parent_token.exp,  # Inherit expiry from parent
        parent_id=parent_token.token_id,
        directive_id=parent_token.directive_id,
        thread_id=parent_token.thread_id,
    )


# ---------------------------------------------------------------------------
# Capability format: rye.{primary}.{item_type}.{specifics...}
#   primary:   execute | search | load | sign
#   item_type: tool | directive | knowledge
#   specifics: item_id with / converted to .
#
# Examples:
#   rye.execute.tool.rye.file-system.fs_write
#   rye.execute.knowledge.rye-architecture
#   rye.search.directive.*
#   rye.execute.*  (can execute anything)
#   rye.*  (god mode)
# ---------------------------------------------------------------------------

PRIMARY_TOOLS = ("execute", "search", "load", "sign")
ITEM_TYPES = ("tool", "directive", "knowledge")

PRIMARY_IMPLIES = {
    "execute": ["search", "load"],
    "sign": ["load"],
}


def item_id_to_cap(primary: str, item_type: str, item_id: str) -> str:
    """Convert item_id to capability string.

    Args:
        primary: Primary tool (execute, search, load, sign)
        item_type: Item type (tool, directive, knowledge)
        item_id: Item ID with / separators (e.g., "rye/file-system/fs_write")

    Returns:
        Capability string (e.g., "rye.execute.tool.rye.file-system.fs_write")
    """
    segments = item_id.replace("/", ".")
    return f"rye.{primary}.{item_type}.{segments}"


def parse_capability(cap: str) -> Optional[Dict[str, Any]]:
    """Parse a capability string into its components.

    Returns dict with keys: primary, item_type, specifics, is_wildcard
    Returns None if not a valid rye capability.
    """
    if not cap.startswith("rye."):
        return None

    parts = cap[4:].split(".", 2)  # After "rye."

    if len(parts) == 0:
        return None

    # rye.* — god mode
    if parts[0] == "*":
        return {"primary": "*", "item_type": "*", "specifics": "*", "is_wildcard": True}

    primary = parts[0]
    if primary not in PRIMARY_TOOLS:
        return None

    if len(parts) == 1:
        return {"primary": primary, "item_type": "*", "specifics": "*", "is_wildcard": True}

    item_type = parts[1]
    if item_type == "*":
        return {"primary": primary, "item_type": "*", "specifics": "*", "is_wildcard": True}

    if item_type not in ITEM_TYPES:
        return None

    if len(parts) == 2:
        return {"primary": primary, "item_type": item_type, "specifics": "*", "is_wildcard": True}

    specifics = parts[2]
    is_wildcard = specifics.endswith("*")

    return {"primary": primary, "item_type": item_type, "specifics": specifics, "is_wildcard": is_wildcard}


def cap_matches(granted: str, required: str) -> bool:
    """Check if a granted capability satisfies a required capability.

    Uses prefix/glob matching:
    - rye.* matches everything
    - rye.execute.* matches rye.execute.tool.anything
    - rye.execute.tool.* matches rye.execute.tool.rye.file-system.fs_write
    - rye.execute.tool.rye.file-system.* matches rye.execute.tool.rye.file-system.fs_write
    - Exact match always works
    """
    if granted == required:
        return True

    # Wildcard matching: strip trailing .* and check prefix
    if granted.endswith(".*"):
        prefix = granted[:-2]
        return required.startswith(prefix + ".") or required == prefix

    # Implicit wildcard: rye.execute (no trailing segments) implies rye.execute.*
    g_parsed = parse_capability(granted)
    r_parsed = parse_capability(required)
    if not g_parsed or not r_parsed:
        return False

    if g_parsed["is_wildcard"] and g_parsed["specifics"] == "*":
        if g_parsed["primary"] == "*":
            return True
        if g_parsed["primary"] == r_parsed["primary"]:
            if g_parsed["item_type"] == "*":
                return True
            if g_parsed["item_type"] == r_parsed["item_type"]:
                return True

    return False


def expand_capabilities(caps) -> Set[str]:
    """Expand capabilities using structural implication.

    rye.execute.* implies rye.search.* + rye.load.*
    rye.sign.* implies rye.load.*

    Also: rye.execute.tool.* implies rye.search.tool.* + rye.load.tool.*
    (implication preserves item_type specificity)
    """
    expanded = set(caps)

    changed = True
    while changed:
        changed = False
        for cap in list(expanded):
            parsed = parse_capability(cap)
            if not parsed:
                continue

            primary = parsed["primary"]
            item_type = parsed["item_type"]
            specifics = parsed["specifics"]

            # God mode implies everything
            if primary == "*":
                for p in PRIMARY_TOOLS:
                    new_cap = f"rye.{p}.*"
                    if new_cap not in expanded:
                        expanded.add(new_cap)
                        changed = True
                continue

            # Structural implication
            implied_primaries = PRIMARY_IMPLIES.get(primary, [])
            for implied_p in implied_primaries:
                if item_type == "*":
                    new_cap = f"rye.{implied_p}.*"
                else:
                    if specifics == "*":
                        new_cap = f"rye.{implied_p}.{item_type}.*"
                    else:
                        new_cap = f"rye.{implied_p}.{item_type}.{specifics}"

                if new_cap not in expanded:
                    expanded.add(new_cap)
                    changed = True

    return expanded


def check_capability(granted_caps, required_cap: str) -> bool:
    """Check if granted capabilities satisfy a required capability."""
    expanded = expand_capabilities(granted_caps)
    for granted in expanded:
        if cap_matches(granted, required_cap):
            return True
    return False


def check_all_capabilities(granted_caps, required_caps) -> Tuple[bool, List[str]]:
    """Check if all required capabilities are satisfied.

    Returns:
        Tuple of (all_satisfied, missing_caps)
    """
    expanded = expand_capabilities(granted_caps)
    missing = []
    for req in required_caps:
        found = any(cap_matches(g, req) for g in expanded)
        if not found:
            missing.append(req)
    return (len(missing) == 0, missing)


def get_primary_tools_for_caps(caps) -> Set[str]:
    """Determine which primary tools (execute/search/load/sign) are needed.

    Parses each capability, extracts the primary tool name.
    Returns set of primary tool names.
    """
    expanded = expand_capabilities(caps)
    primaries: Set[str] = set()
    for cap in expanded:
        parsed = parse_capability(cap)
        if not parsed:
            continue
        if parsed["primary"] == "*":
            primaries.update(PRIMARY_TOOLS)
        else:
            primaries.add(parsed["primary"])
    return primaries


# System capability prefix (cannot be overridden by projects)
SYSTEM_PREFIXES = ["rye."]


def is_system_capability(cap: str) -> bool:
    """Check if capability is a system primitive (cannot be overridden)."""
    return any(cap.startswith(prefix) for prefix in SYSTEM_PREFIXES)


def load_capabilities(project_path: Path) -> Tuple[Dict[Tuple, str], Dict[str, List[str]]]:
    """
    Load capability definitions from YAML files.
    
    Search order: system → user → project (project overrides)
    
    Override protection: System capabilities can only be defined in ryeos space.
    Projects can only ADD capabilities under rye.mcp.<name>.* namespace.
    
    Args:
        project_path: Path to project root
        
    Returns:
        Tuple of (permissions_map, hierarchy_map)
        - permissions_map: {(action, resource, target): capability_string}
        - hierarchy_map: {parent_cap: [child_caps]}
    """
    try:
        import yaml
    except ImportError:
        raise ImportError("PyYAML is required for capability loading")
    
    import logging
    logger = logging.getLogger(__name__)
    
    permissions = {}  # {pattern_tuple: capability_string}
    hierarchy = {}    # {parent: [children]}
    
    # Determine search paths
    # From: rye/.ai/tools/rye/agent/permissions/capability_tokens/capability_tokens.py
    # To: rye/.ai/tools/rye/agent/
    tokens_dir = Path(__file__).parent  # capability_tokens/
    permissions_dir = tokens_dir.parent  # permissions/
    agent_dir = permissions_dir.parent  # agent/
    system_caps_dir = agent_dir / "permissions" / "capabilities" / "tools"
    
    project_caps_dir = project_path / AI_DIR / "tools" / "agent" / "permissions" / "capabilities" / "tools"
    
    search_order = [
        (system_caps_dir, True),      # System space (can define system capabilities)
        (project_caps_dir, False),    # Project space (can only define mcp.* capabilities)
    ]
    
    for caps_dir, is_system_space in search_order:
        if not caps_dir.exists():
            continue
            
        for yaml_file in sorted(caps_dir.glob("**/*.yaml")):
            try:
                data = yaml.safe_load(yaml_file.read_text())
                if not data:
                    continue
                
                # Merge permissions
                for perm in data.get("permissions", []):
                    pattern = tuple(perm.get("pattern", []))
                    cap = perm.get("capability")
                    
                    if not pattern or not cap:
                        continue
                    
                    # Override protection: Block non-system from defining system capabilities
                    if not is_system_space and is_system_capability(cap):
                        logger.warning(
                            f"BLOCKED: {yaml_file.name} tried to define system capability '{cap}' "
                            f"(only ryeos can define rye.* capabilities)"
                        )
                        continue
                    
                    # Block override of existing system capabilities from user space
                    if not is_system_space and pattern in permissions:
                        existing_cap = permissions[pattern]
                        if is_system_capability(existing_cap):
                            logger.warning(
                                f"BLOCKED: {yaml_file.name} tried to override system capability "
                                f"pattern {pattern} (was '{existing_cap}')"
                            )
                            continue
                    
                    permissions[pattern] = cap
                
                # Merge hierarchy (only from system space for system capabilities)
                for parent, children in data.get("hierarchy", {}).items():
                    if not is_system_space and is_system_capability(parent):
                        logger.warning(
                            f"BLOCKED: {yaml_file.name} tried to define hierarchy for system capability '{parent}'"
                        )
                        continue
                    
                    if parent in hierarchy:
                        # Merge, keeping unique children
                        existing = set(hierarchy[parent])
                        existing.update(children)
                        hierarchy[parent] = sorted(existing)
                    else:
                        hierarchy[parent] = children
                        
            except Exception as e:
                logger.warning(f"Failed to load capabilities from {yaml_file}: {e}")
    
    return permissions, hierarchy
