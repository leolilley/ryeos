"""Tests for server-side validation."""

import pytest

from registry_api.validation import (
    strip_signature,
    validate_content,
    sign_with_registry,
    verify_registry_signature,
)


class TestStripSignature:
    """Tests for strip_signature function."""

    def test_strip_directive_signature(self):
        """Strip HTML comment signature from directive."""
        content = """<!-- rye:validated:2026-02-04T10:00:00Z:abc123def456789012345678901234567890123456789012345678901234 -->
<directive name="test" version="1.0.0">
  <metadata>
    <description>Test directive</description>
  </metadata>
</directive>"""
        
        result = strip_signature(content, "directive")
        assert not result.startswith("<!-- rye:validated")
        assert "<directive" in result

    def test_strip_tool_signature(self):
        """Strip line comment signature from tool."""
        content = """# rye:validated:2026-02-04T10:00:00Z:abc123def456789012345678901234567890123456789012345678901234
__version__ = "1.0.0"
__tool_type__ = "python"
"""
        
        result = strip_signature(content, "tool")
        assert not result.startswith("# rye:validated")
        assert '__version__ = "1.0.0"' in result

    def test_strip_registry_signature(self):
        """Strip registry signature with username suffix."""
        content = """<!-- rye:validated:2026-02-04T10:00:00Z:abc123def456789012345678901234567890123456789012345678901234|registry@leo -->
<directive name="test" version="1.0.0">
</directive>"""
        
        result = strip_signature(content, "directive")
        assert "|registry@" not in result
        assert "<directive" in result


class TestSignWithRegistry:
    """Tests for sign_with_registry function."""

    def test_sign_directive_with_registry(self):
        """Sign directive content with registry provenance."""
        content = """<directive name="test" version="1.0.0">
  <metadata>
    <description>Test directive</description>
  </metadata>
</directive>"""
        
        signed, sig_info = sign_with_registry(content, "directive", "testuser")
        
        assert "|registry@testuser" in signed
        assert sig_info["registry_username"] == "testuser"
        assert len(sig_info["hash"]) == 64
        assert "T" in sig_info["timestamp"]

    def test_sign_tool_with_registry(self):
        """Sign tool content with registry provenance."""
        content = '''__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "test"
__tool_description__ = "Test tool"
'''
        
        signed, sig_info = sign_with_registry(content, "tool", "leo")
        
        assert "|registry@leo" in signed
        assert signed.startswith("# rye:validated:")
        assert sig_info["registry_username"] == "leo"


class TestVerifyRegistrySignature:
    """Tests for verify_registry_signature function."""

    def test_verify_valid_signature(self):
        """Verify a valid registry signature."""
        content = """<directive name="test" version="1.0.0">
</directive>"""
        
        # Sign it first
        signed, _ = sign_with_registry(content, "directive", "testuser")
        
        # Verify
        is_valid, error, sig_info = verify_registry_signature(
            signed, "directive", "testuser"
        )
        
        assert is_valid
        assert error is None
        assert sig_info["registry_username"] == "testuser"

    def test_verify_username_mismatch(self):
        """Detect username mismatch in signature."""
        content = """<directive name="test" version="1.0.0">
</directive>"""
        
        # Sign as one user
        signed, _ = sign_with_registry(content, "directive", "user1")
        
        # Verify expecting different user
        is_valid, error, sig_info = verify_registry_signature(
            signed, "directive", "user2"
        )
        
        assert not is_valid
        assert "mismatch" in error.lower()

    def test_verify_tampered_content(self):
        """Detect tampered content."""
        content = """<directive name="test" version="1.0.0">
</directive>"""
        
        # Sign it
        signed, _ = sign_with_registry(content, "directive", "testuser")
        
        # Tamper with content (change version)
        tampered = signed.replace("1.0.0", "2.0.0")
        
        # Verify should fail
        is_valid, error, _ = verify_registry_signature(
            tampered, "directive", "testuser"
        )
        
        assert not is_valid
        assert "hash" in error.lower() or "integrity" in error.lower()
