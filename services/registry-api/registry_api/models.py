"""Pydantic models for Registry API requests and responses.

Identity model:
- item_id = "{namespace}/{category}/{name}" (canonical, derived)
- namespace: owner (no slashes)
- category: folder path (may contain slashes)  
- name: basename (no slashes)

Parsing: first segment = namespace, last segment = name, middle = category
"""

from datetime import datetime
from typing import Any, Dict, List, Literal, Optional

from pydantic import BaseModel, Field, field_validator, model_validator


def parse_item_id(item_id: str) -> tuple[str, str, str]:
    """Parse item_id into (namespace, category, name).
    
    Format: namespace/category/name where category may contain slashes.
    Minimum 3 segments required.
    """
    segments = item_id.split("/")
    if len(segments) < 3:
        raise ValueError(
            f"item_id must have at least 3 segments (namespace/category/name), got: {item_id}"
        )
    namespace = segments[0]
    name = segments[-1]
    category = "/".join(segments[1:-1])
    return namespace, category, name


def build_item_id(namespace: str, category: str, name: str) -> str:
    """Build item_id from components."""
    return f"{namespace}/{category}/{name}"


# Request Models


class PushRequest(BaseModel):
    """Request body for pushing an item to the registry.
    
    item_id format: namespace/category/name
    Example: leolilley/core/bootstrap
    """

    item_type: Literal["directive", "tool", "knowledge"]
    item_id: str = Field(..., min_length=5, max_length=256)
    content: str = Field(..., min_length=1)
    version: str = Field(..., pattern=r"^\d+\.\d+\.\d+$")
    changelog: Optional[str] = None
    metadata: Optional[Dict[str, Any]] = None
    
    # Parsed identity fields (set by validator)
    namespace: Optional[str] = None
    category: Optional[str] = None
    name: Optional[str] = None
    
    @model_validator(mode="after")
    def parse_identity(self):
        """Parse item_id into namespace/category/name."""
        namespace, category, name = parse_item_id(self.item_id)
        self.namespace = namespace
        self.category = category
        self.name = name
        return self


class SearchRequest(BaseModel):
    """Query parameters for searching the registry."""

    query: str = Field(..., min_length=1, max_length=256)
    item_type: Optional[Literal["directive", "tool", "knowledge"]] = None
    namespace: Optional[str] = None
    category: Optional[str] = None
    limit: int = Field(default=20, ge=1, le=100)
    offset: int = Field(default=0, ge=0)


# Response Models


class SignatureInfo(BaseModel):
    """Signature information for an item."""

    timestamp: str
    hash: str
    registry_username: Optional[str] = None


class ItemIdentity(BaseModel):
    """Standard identity fields returned for all items."""
    
    item_type: str
    item_id: str  # canonical: namespace/category/name
    namespace: str
    category: str
    name: str


class PushResponse(BaseModel):
    """Response for successful push operation."""

    status: Literal["published"]
    item_type: str
    item_id: str
    namespace: str
    category: str
    name: str
    version: str
    signature: SignatureInfo
    signed_content: str


class PushErrorResponse(BaseModel):
    """Response for failed push operation."""

    status: Literal["error"]
    error: str
    issues: Optional[List[str]] = None


class PullResponse(BaseModel):
    """Response for pull operation."""

    item_type: str
    item_id: str
    namespace: str
    category: str
    name: str
    version: str
    content: str
    author: str
    signature: SignatureInfo
    created_at: datetime


class SearchResultItem(BaseModel):
    """Single item in search results."""

    item_type: str
    item_id: str  # canonical: namespace/category/name
    namespace: str
    category: str
    name: str
    description: Optional[str] = None
    version: str
    author: Optional[str] = None
    download_count: int = 0
    created_at: Optional[datetime] = None


class SearchResponse(BaseModel):
    """Response for search operation."""

    results: List[SearchResultItem]
    total: int
    limit: int
    offset: int


class HealthResponse(BaseModel):
    """Health check response."""

    status: Literal["healthy"]
    version: str
    database: Literal["connected", "error"]


class DeleteResponse(BaseModel):
    """Response for delete operation."""

    status: Literal["deleted"]
    item_type: str
    item_id: str
    version: Optional[str] = None
    deleted_versions: int = 0


class VisibilityResponse(BaseModel):
    """Response for visibility change operation."""

    status: Literal["updated"]
    item_type: str
    item_id: str
    visibility: str
    previous_visibility: str


# Bundle Models


class BundlePushRequest(BaseModel):
    """Request body for pushing a bundle to the registry."""

    bundle_id: str = Field(..., min_length=1, max_length=256)
    version: Optional[str] = None
    manifest: str = Field(..., min_length=1)
    files: Dict[str, Any] = Field(..., description="Map of rel_path -> {content, sha256, inline_signed}")


class BundlePushResponse(BaseModel):
    """Response for successful bundle push."""

    status: Literal["pushed"]
    bundle_id: str
    version: str
    file_count: int


class BundlePullResponse(BaseModel):
    """Response for bundle pull."""

    bundle_id: str
    version: str
    manifest: str
    files: Dict[str, Any]
    author: str
    created_at: datetime


# API Key Models


class CreateApiKeyRequest(BaseModel):
    """Request to create an API key."""

    name: str = Field(..., min_length=1, max_length=128)
    scopes: List[str] = Field(default=["registry:read", "registry:write"])
    expires_in_days: Optional[int] = Field(default=None, ge=1, le=365)


class CreateApiKeyResponse(BaseModel):
    """Response with the created API key (shown only once)."""

    key: str
    name: str
    key_prefix: str
    scopes: List[str]
    expires_at: Optional[datetime] = None
    created_at: datetime


class ApiKeyInfo(BaseModel):
    """API key info (without the secret)."""

    id: str
    name: str
    key_prefix: str
    scopes: List[str]
    created_at: datetime
    expires_at: Optional[datetime] = None
    last_used_at: Optional[datetime] = None
    revoked: bool = False


class ListApiKeysResponse(BaseModel):
    """Response listing API keys."""

    keys: List[ApiKeyInfo]
    count: int


class RevokeApiKeyResponse(BaseModel):
    """Response for API key revocation."""

    status: Literal["revoked"]
    name: str
