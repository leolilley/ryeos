"""RYE Registry API - Server-side validation and signing.

This FastAPI service handles registry push/pull operations with:
- Server-side validation using the rye package's validators
- Registry signing (adds |registry@username suffix)
- Supabase database integration

The service imports and reuses the rye package's data-driven validation,
ensuring consistent rules between client and server.
"""

import logging
from contextlib import asynccontextmanager
from typing import Union

from fastapi import Depends, FastAPI, HTTPException, status
from fastapi.middleware.cors import CORSMiddleware
from supabase import create_client, Client

from registry_api import __version__
from registry_api.auth import User, get_current_user
from registry_api.config import Settings, get_settings
from registry_api.models import (
    HealthResponse,
    PullResponse,
    PushErrorResponse,
    PushRequest,
    PushResponse,
    SearchRequest,
    SearchResponse,
    SearchResultItem,
    SignatureInfo,
    UnpublishResponse,
)
from registry_api.validation import (
    sign_with_registry,
    strip_signature,
    validate_content,
    verify_registry_signature,
)

# Configure logging
logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

# Supabase client (initialized on startup)
_supabase: Client = None


def get_supabase() -> Client:
    """Get Supabase client instance."""
    global _supabase
    if _supabase is None:
        settings = get_settings()
        _supabase = create_client(
            settings.supabase_url,
            settings.supabase_service_key,
        )
    return _supabase


@asynccontextmanager
async def lifespan(app: FastAPI):
    """Application lifespan - initialize on startup, cleanup on shutdown."""
    # Startup
    settings = get_settings()
    logger.info(f"Starting Registry API v{__version__}")
    logger.info(f"Supabase URL: {settings.supabase_url}")
    
    # Initialize Supabase client
    get_supabase()
    
    yield
    
    # Shutdown
    logger.info("Shutting down Registry API")


# Create FastAPI app
app = FastAPI(
    title="RYE Registry API",
    description="Server-side validation and signing for the RYE registry",
    version=__version__,
    lifespan=lifespan,
)

# CORS middleware
app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],  # Configure via settings in production
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)


# =============================================================================
# HEALTH CHECK
# =============================================================================


@app.get("/health", response_model=HealthResponse)
async def health_check():
    """Health check endpoint."""
    try:
        supabase = get_supabase()
        # Quick DB check
        supabase.table("users").select("id").limit(1).execute()
        db_status = "connected"
    except Exception as e:
        logger.warning(f"Database health check failed: {e}")
        db_status = "error"
    
    return HealthResponse(
        status="healthy",
        version=__version__,
        database=db_status,
    )


# =============================================================================
# PUSH - Validate and publish an item
# =============================================================================


@app.post("/v1/push", response_model=Union[PushResponse, PushErrorResponse])
async def push_item(
    request: PushRequest,
    user: User = Depends(get_current_user),
):
    """Validate and publish an item to the registry.
    
    Flow:
    1. Strip any existing signature from content
    2. Validate content using rye validators (same as client-side sign tool)
    3. If validation fails, return error with issues
    4. If validation passes, sign with registry provenance (|registry@username)
    5. Insert/update in database
    6. Return signed content to client
    """
    item_type = request.item_type
    item_id = request.item_id
    content = request.content
    version = request.version
    
    logger.info(f"Push request: {item_type}/{item_id} v{version} from @{user.username}")
    
    # 1. Strip existing signature
    content_clean = strip_signature(content, item_type)
    
    # 2. Validate using rye validators
    is_valid, validation_result = validate_content(content_clean, item_type, item_id)
    
    if not is_valid:
        logger.warning(f"Validation failed for {item_type}/{item_id}: {validation_result['issues']}")
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail={
                "status": "error",
                "error": "Validation failed",
                "issues": validation_result["issues"],
            },
        )
    
    # 3. Sign with registry provenance
    signed_content, signature_info = sign_with_registry(content_clean, item_type, user.username)
    
    # 4. Insert/update in database
    try:
        await _upsert_item(
            item_type=item_type,
            item_id=item_id,
            version=version,
            content=signed_content,
            content_hash=signature_info["hash"],
            user_id=user.id,
            username=user.username,
            changelog=request.changelog,
            metadata=request.metadata,
        )
    except Exception as e:
        logger.error(f"Database error for {item_type}/{item_id}: {e}")
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail={"status": "error", "error": f"Database error: {str(e)}"},
        )
    
    logger.info(f"Published {item_type}/{item_id} v{version} by @{user.username}")
    
    return PushResponse(
        status="published",
        item_type=item_type,
        item_id=item_id,
        version=version,
        signature=SignatureInfo(**signature_info),
        signed_content=signed_content,
    )


async def _upsert_item(
    item_type: str,
    item_id: str,
    version: str,
    content: str,
    content_hash: str,
    user_id: str,
    username: str,
    changelog: str = None,
    metadata: dict = None,
):
    """Insert or update item and version in database.
    
    Uses the same table naming convention as the registry tool:
    - Items: {item_type}s (e.g., directives, tools, knowledge)
    - Versions: {item_type}_versions (e.g., directive_versions)
    """
    supabase = get_supabase()
    metadata = metadata or {}
    
    # Table names follow rye convention (knowledge is singular)
    table = "knowledge" if item_type == "knowledge" else f"{item_type}s"
    version_table = f"{item_type}_versions"
    
    # Ensure user exists
    await _ensure_user(user_id, username)
    
    # Check if item exists
    result = supabase.table(table).select("id").eq("name", item_id).execute()
    
    if result.data:
        # Item exists - add new version
        item_uuid = result.data[0]["id"]
    else:
        # Create new item
        item_data = {
            "name": item_id,
            "author_id": user_id,
            "category": metadata.get("category", ""),
            "description": metadata.get("description", ""),
        }
        
        # Add type-specific fields
        if item_type == "tool":
            item_data["tool_id"] = item_id
            item_data["tool_type"] = metadata.get("tool_type", "python")
            item_data["visibility"] = metadata.get("visibility", "public")
        elif item_type == "knowledge":
            item_data["title"] = metadata.get("title", item_id)
            item_data["entry_type"] = metadata.get("entry_type", "reference")
            item_data["visibility"] = metadata.get("visibility", "public")
        else:
            # directive
            item_data["visibility"] = metadata.get("visibility", "public")
        
        create_result = supabase.table(table).insert(item_data).execute()
        item_uuid = create_result.data[0]["id"]
    
    # Set existing versions to not latest
    supabase.table(version_table).update({"is_latest": False}).eq(
        f"{item_type}_id", item_uuid
    ).execute()
    
    # Create new version
    version_data = {
        f"{item_type}_id": item_uuid,
        "version": version,
        "content": content,
        "content_hash": content_hash,
        "is_latest": True,
    }
    if changelog:
        version_data["changelog"] = changelog
    
    supabase.table(version_table).insert(version_data).execute()


async def _ensure_user(user_id: str, username: str):
    """Ensure user exists in users table."""
    supabase = get_supabase()
    
    result = supabase.table("users").select("id").eq("id", user_id).execute()
    if not result.data:
        supabase.table("users").insert({
            "id": user_id,
            "username": username,
        }).execute()


# =============================================================================
# PULL - Download an item with signature verification
# =============================================================================


@app.get("/v1/pull/{item_type}/{item_id:path}", response_model=PullResponse)
async def pull_item(
    item_type: str,
    item_id: str,
    version: str = None,
):
    """Pull an item from the registry.
    
    Returns the item content with registry signature, allowing clients
    to verify integrity and provenance.
    """
    if item_type not in ["directive", "tool", "knowledge"]:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail={"error": f"Invalid item_type: {item_type}"},
        )
    
    supabase = get_supabase()
    
    # Table names follow rye convention (knowledge is singular)
    table = "knowledge" if item_type == "knowledge" else f"{item_type}s"
    version_table = f"{item_type}_versions"
    
    # Query item first
    item_result = supabase.table(table).select("*").eq("name", item_id).execute()
    
    if not item_result.data:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail={"error": f"{item_type.title()} not found: {item_id}"},
        )
    
    item = item_result.data[0]
    item_uuid = item["id"]
    
    # Query versions separately
    versions_result = supabase.table(version_table).select("*").eq(
        f"{item_type}_id", item_uuid
    ).order("created_at", desc=True).execute()
    
    versions = versions_result.data or []
    
    if not versions:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail={"error": f"No versions found for {item_type}: {item_id}"},
        )
    
    # Get requested version or latest
    if version:
        target_version = next((v for v in versions if v["version"] == version), None)
        if not target_version:
            raise HTTPException(
                status_code=status.HTTP_404_NOT_FOUND,
                detail={"error": f"Version {version} not found for {item_id}"},
            )
    else:
        # Get latest (is_latest=true or most recent)
        target_version = next((v for v in versions if v.get("is_latest")), versions[0])
    
    # Get author username via separate query
    author_username = "unknown"
    if item.get("author_id"):
        user_result = supabase.table("users").select("username").eq("id", item["author_id"]).execute()
        if user_result.data:
            author_username = user_result.data[0].get("username", "unknown")
    
    content = target_version["content"]
    
    # Extract signature info for response
    from rye.utils.metadata_manager import MetadataManager
    strategy = MetadataManager.get_strategy(item_type)
    sig_info = strategy.extract_signature(content)
    
    return PullResponse(
        item_type=item_type,
        item_id=item_id,
        version=target_version["version"],
        content=content,
        author=author_username,
        signature=SignatureInfo(
            timestamp=sig_info.get("timestamp", ""),
            hash=sig_info.get("hash", ""),
            registry_username=sig_info.get("registry_username"),
        ) if sig_info else SignatureInfo(timestamp="", hash=""),
        created_at=target_version["created_at"],
    )


# =============================================================================
# DELETE - Remove an item from the registry
# =============================================================================


@app.delete("/v1/delete/{item_type}/{item_id:path}")
async def delete_item(
    item_type: str,
    item_id: str,
    version: str = None,
    user: User = Depends(get_current_user),
):
    """Remove an item from the registry.
    
    Only the author can unpublish their items.
    If version is specified, only that version is deleted.
    Otherwise, all versions and the item itself are deleted.
    """
    if item_type not in ["directive", "tool", "knowledge"]:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail={"error": f"Invalid item_type: {item_type}"},
        )
    
    supabase = get_supabase()
    
    # Table names follow rye convention (knowledge is singular)
    table = "knowledge" if item_type == "knowledge" else f"{item_type}s"
    version_table = f"{item_type}_versions"
    
    # Find the item
    result = supabase.table(table).select("id, author_id").eq("name", item_id).execute()
    
    if not result.data:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail={"error": f"{item_type.title()} not found: {item_id}"},
        )
    
    item = result.data[0]
    
    # Check ownership
    if item["author_id"] != user.id:
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail={"error": "You can only unpublish your own items"},
        )
    
    item_uuid = item["id"]
    deleted_versions = 0
    
    if version:
        # Delete specific version
        del_result = supabase.table(version_table).delete().eq(
            f"{item_type}_id", item_uuid
        ).eq("version", version).execute()
        deleted_versions = len(del_result.data) if del_result.data else 0
        
        if deleted_versions == 0:
            raise HTTPException(
                status_code=status.HTTP_404_NOT_FOUND,
                detail={"error": f"Version {version} not found for {item_id}"},
            )
        
        # Check if any versions remain
        remaining = supabase.table(version_table).select("id").eq(
            f"{item_type}_id", item_uuid
        ).execute()
        
        if not remaining.data:
            # No versions left, delete the item
            supabase.table(table).delete().eq("id", item_uuid).execute()
    else:
        # Delete all versions
        del_result = supabase.table(version_table).delete().eq(
            f"{item_type}_id", item_uuid
        ).execute()
        deleted_versions = len(del_result.data) if del_result.data else 0
        
        # Delete the item
        supabase.table(table).delete().eq("id", item_uuid).execute()
    
    logger.info(f"Deleted {item_type}/{item_id} (v={version or 'all'}) by @{user.username}")
    
    return {
        "status": "deleted",
        "item_type": item_type,
        "item_id": item_id,
        "version": version,
        "deleted_versions": deleted_versions,
    }


# =============================================================================
# VISIBILITY - Set item visibility (publish/unpublish)
# =============================================================================


@app.post("/v1/visibility/{item_type}/{item_id:path}")
async def set_visibility(
    item_type: str,
    item_id: str,
    body: dict,
    user: User = Depends(get_current_user),
):
    """Set item visibility (public/private/unlisted).
    
    Used by publish (visibility='public') and unpublish (visibility='private').
    """
    visibility = body.get("visibility")
    if visibility not in ["public", "private", "unlisted"]:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail={"error": f"Invalid visibility: {visibility}. Must be public, private, or unlisted."},
        )
    
    if item_type not in ["directive", "tool", "knowledge"]:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail={"error": f"Invalid item_type: {item_type}"},
        )
    
    supabase = get_supabase()
    
    # Table names follow rye convention (knowledge is singular)
    table = "knowledge" if item_type == "knowledge" else f"{item_type}s"
    
    # Find the item
    result = supabase.table(table).select("id, author_id, visibility").eq("name", item_id).execute()
    
    if not result.data:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail={"error": f"{item_type.title()} not found: {item_id}"},
        )
    
    item = result.data[0]
    
    # Check ownership
    if item["author_id"] != user.id:
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail={"error": "You can only change visibility of your own items"},
        )
    
    old_visibility = item.get("visibility", "private")
    
    # Update visibility
    supabase.table(table).update({"visibility": visibility}).eq("id", item["id"]).execute()
    
    logger.info(f"Changed visibility {item_type}/{item_id}: {old_visibility} -> {visibility} by @{user.username}")
    
    return {
        "status": "updated",
        "item_type": item_type,
        "item_id": item_id,
        "visibility": visibility,
        "previous_visibility": old_visibility,
    }


# =============================================================================
# SEARCH - Search registry items
# =============================================================================


@app.get("/v1/search", response_model=SearchResponse)
async def search_items(
    query: str,
    item_type: str = None,
    category: str = None,
    author: str = None,
    limit: int = 20,
    offset: int = 0,
):
    """Search for items in the registry."""
    supabase = get_supabase()
    results = []
    total = 0
    
    # Search each item type (or specific type if provided)
    types_to_search = [item_type] if item_type else ["directive", "tool", "knowledge"]
    
    for itype in types_to_search:
        try:
            # knowledge table is singular, others are plural
            table = "knowledge" if itype == "knowledge" else f"{itype}s"
            
            # Build query - simple select without join for reliability
            q = supabase.table(table).select("*", count="exact")
            
            # Text search on name and description (consistent across all types now)
            q = q.or_(f"name.ilike.%{query}%,description.ilike.%{query}%")
            
            if category:
                q = q.eq("category", category)
            
            result = q.range(offset, offset + limit - 1).execute()
            
            for item in result.data:
                results.append(SearchResultItem(
                    item_type=itype,
                    item_id=item.get("name") or item.get("tool_id") or item.get("zettel_id"),
                    name=item.get("name", ""),
                    description=item.get("description"),
                    version=item.get("latest_version") or "0.0.0",
                    author=None,  # Skip author lookup for now
                    category=item.get("category"),
                    download_count=item.get("download_count") or 0,
                    created_at=item.get("created_at"),
                ))
            
            total += result.count or 0
        except Exception as e:
            logger.error(f"Search error for {itype}: {e}")
            continue
    
    return SearchResponse(
        results=results[:limit],
        total=total,
        limit=limit,
        offset=offset,
    )


# =============================================================================
# MAIN
# =============================================================================


if __name__ == "__main__":
    import uvicorn
    
    settings = get_settings()
    uvicorn.run(
        "registry_api.main:app",
        host=settings.host,
        port=settings.port,
        reload=True,
        log_level=settings.log_level.lower(),
    )
