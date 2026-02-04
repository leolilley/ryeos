"""Pydantic models for Registry API requests and responses."""

from datetime import datetime
from typing import Any, Dict, List, Literal, Optional

from pydantic import BaseModel, Field


# Request Models


class PushRequest(BaseModel):
    """Request body for pushing an item to the registry."""

    item_type: Literal["directive", "tool", "knowledge"]
    item_id: str = Field(..., min_length=1, max_length=128)
    content: str = Field(..., min_length=1)
    version: str = Field(..., pattern=r"^\d+\.\d+\.\d+$")
    changelog: Optional[str] = None
    metadata: Optional[Dict[str, Any]] = None


class SearchRequest(BaseModel):
    """Query parameters for searching the registry."""

    query: str = Field(..., min_length=1, max_length=256)
    item_type: Optional[Literal["directive", "tool", "knowledge"]] = None
    category: Optional[str] = None
    author: Optional[str] = None
    limit: int = Field(default=20, ge=1, le=100)
    offset: int = Field(default=0, ge=0)


# Response Models


class SignatureInfo(BaseModel):
    """Signature information for an item."""

    timestamp: str
    hash: str
    registry_username: Optional[str] = None


class PushResponse(BaseModel):
    """Response for successful push operation."""

    status: Literal["published"]
    item_type: str
    item_id: str
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
    version: str
    content: str
    author: str
    signature: SignatureInfo
    created_at: datetime


class SearchResultItem(BaseModel):
    """Single item in search results."""

    item_type: str
    item_id: str
    name: str
    description: Optional[str] = None
    version: str
    author: str
    category: Optional[str] = None
    download_count: int = 0
    created_at: datetime


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
