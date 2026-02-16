# rye:signed:2026-02-16T05:32:36Z:b0ab4444a145139c4c0e5a7da3e231e0b1c65748522c29eb89d6ece4595eb9ef:-RiRtiZI0evboWRXOqc7cew2Tw1rNmAr3Maj9eLmVjdo8j-0J1eeJFAbshL0Y9vAdL_yYDvEJFtBdCGE9a6NBA==:440443d0858f0199
__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/security"
__tool_description__ = "Security manager for thread permission enforcement"

import hashlib
import secrets
from datetime import datetime, timedelta
from pathlib import Path
from typing import Any, Dict, List, Optional, Set


class SecurityManager:
    """Capability tokens, redaction, and security utilities."""

    def __init__(self, project_path: Path):
        self.project_path = project_path
        self._token_store: Dict[str, Dict] = {}
        self._redaction_patterns: List[str] = []

    def generate_capability_token(
        self, capabilities: Set[str], expires_in: int = 3600
    ) -> str:
        """Generate a capability token with expiration."""
        token = secrets.token_urlsafe(32)
        expires_at = datetime.utcnow() + timedelta(seconds=expires_in)
        self._token_store[token] = {
            "capabilities": capabilities,
            "expires_at": expires_at.isoformat(),
        }
        return token

    def validate_token(self, token: str, required_capability: str) -> bool:
        """Validate a token has the required capability."""
        if token not in self._token_store:
            return False

        data = self._token_store[token]
        expires_at = datetime.fromisoformat(data["expires_at"])
        if datetime.utcnow() > expires_at:
            del self._token_store[token]
            return False

        return required_capability in data["capabilities"]

    def revoke_token(self, token: str) -> bool:
        """Revoke a token."""
        if token in self._token_store:
            del self._token_store[token]
            return True
        return False

    def add_redaction_pattern(self, pattern: str) -> None:
        """Add a regex pattern for sensitive data redaction."""
        self._redaction_patterns.append(pattern)

    def redact(self, text: str) -> str:
        """Redact sensitive patterns from text."""
        import re

        result = text
        for pattern in self._redaction_patterns:
            result = re.sub(pattern, "[REDACTED]", result)
        return result

    def redact_dict(self, data: Dict) -> Dict:
        """Recursively redact strings in a dict."""
        result = {}
        for key, value in data.items():
            if isinstance(value, str):
                result[key] = self.redact(value)
            elif isinstance(value, dict):
                result[key] = self.redact_dict(value)
            elif isinstance(value, list):
                result[key] = [
                    self.redact(item)
                    if isinstance(item, str)
                    else self.redact_dict(item)
                    if isinstance(item, dict)
                    else item
                    for item in value
                ]
            else:
                result[key] = value
        return result

    def hash_sensitive(self, value: str) -> str:
        """Hash a sensitive value for logging."""
        return hashlib.sha256(value.encode()).hexdigest()[:16]
