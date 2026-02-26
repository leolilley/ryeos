"""RYE Registry API - Server-side validation and signing.

Identity model:
- item_id = "{namespace}/{category}/{name}" (canonical)
- namespace: owner (no slashes)
- category: folder path (may contain slashes)
- name: basename (no slashes)

This FastAPI service handles registry push/pull operations with:
- Server-side validation using the rye package's validators
- Registry signing (adds |registry@username suffix)
- Supabase database integration
"""

import logging
from contextlib import asynccontextmanager
from typing import Optional, Union

from fastapi import Depends, FastAPI, HTTPException, status
from fastapi.middleware.cors import CORSMiddleware
from supabase import create_client, Client

from registry_api import __version__
from registry_api.auth import API_KEY_PREFIX, User, get_current_user, get_current_user_optional, _hash_api_key
from registry_api.config import Settings, get_settings
from registry_api.models import (
    build_item_id,
    parse_item_id,
    BundlePullResponse,
    BundlePushRequest,
    BundlePushResponse,
    DeleteResponse,
    HealthResponse,
    PullResponse,
    PushErrorResponse,
    PushRequest,
    PushResponse,
    SearchResponse,
    SearchResultItem,
    SignatureInfo,
    VisibilityResponse,
    CreateApiKeyRequest,
    CreateApiKeyResponse,
    ApiKeyInfo,
    ListApiKeysResponse,
    RevokeApiKeyResponse,
)
from registry_api.validation import (
    get_registry_public_key,
    sign_with_registry,
    strip_signature,
    validate_content,
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
    settings = get_settings()
    logger.info(f"Starting Registry API v{__version__}")
    logger.info(f"Supabase URL: {settings.supabase_url}")
    get_supabase()
    yield
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
    allow_origins=["*"],
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)


# =============================================================================
# HELPERS
# =============================================================================


def get_table_name(item_type: str) -> str:
    """Get table name for item type."""
    return "knowledge" if item_type == "knowledge" else f"{item_type}s"


def get_version_table_name(item_type: str) -> str:
    """Get version table name for item type."""
    return f"{item_type}_versions"


async def _ensure_user(user_id: str, username: str):
    """Ensure user exists in users table."""
    supabase = get_supabase()
    result = supabase.table("users").select("id").eq("id", user_id).execute()
    if not result.data:
        supabase.table("users").insert({
            "id": user_id,
            "username": username,
        }).execute()


async def _get_author_username(author_id: str) -> str:
    """Get username for author_id."""
    if not author_id:
        return "unknown"
    supabase = get_supabase()
    result = supabase.table("users").select("username").eq("id", author_id).execute()
    return result.data[0].get("username", "unknown") if result.data else "unknown"


# =============================================================================
# HEALTH CHECK
# =============================================================================


@app.get("/health", response_model=HealthResponse)
async def health_check():
    """Health check endpoint."""
    try:
        supabase = get_supabase()
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
# PUBLIC KEY - Expose registry Ed25519 public key for TOFU pinning
# =============================================================================


@app.get("/v1/public-key")
async def get_public_key():
    """Return the registry's Ed25519 public key for client-side TOFU pinning."""
    from fastapi.responses import Response

    public_key = get_registry_public_key()
    if public_key is None:
        raise HTTPException(
            status_code=status.HTTP_503_SERVICE_UNAVAILABLE,
            detail="Registry keypair not available",
        )
    return Response(content=public_key, media_type="application/x-pem-file")


# =============================================================================
# PUSH - Validate and publish an item
# =============================================================================


@app.post("/v1/push", response_model=Union[PushResponse, PushErrorResponse])
async def push_item(
    request: PushRequest,
    user: User = Depends(get_current_user),
):
    """Validate and publish an item to the registry.
    
    item_id format: namespace/category/name
    Example: leolilley/core/bootstrap
    """
    item_type = request.item_type
    item_id = request.item_id
    namespace = request.namespace
    category = request.category
    name = request.name
    content = request.content
    version = request.version
    
    logger.info(f"Push: {item_type} {item_id} v{version} by @{user.username}")
    
    # Verify namespace matches authenticated user
    if namespace != user.username:
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail={"error": f"Cannot push to namespace '{namespace}'. You can only push to your own namespace '{user.username}'."},
        )
    
    # Strip existing signature and validate
    content_clean = strip_signature(content, item_type)
    is_valid, validation_result = validate_content(content_clean, item_type, name)
    
    if not is_valid:
        logger.warning(f"Validation failed: {validation_result['issues']}")
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail={
                "status": "error",
                "error": "Validation failed",
                "issues": validation_result["issues"],
            },
        )
    
    # Sign with registry provenance
    signed_content, signature_info = sign_with_registry(content_clean, item_type, user.username)
    
    # Upsert to database
    try:
        await _upsert_item(
            item_type=item_type,
            namespace=namespace,
            category=category,
            name=name,
            version=version,
            content=signed_content,
            content_hash=signature_info["hash"],
            user_id=user.id,
            username=user.username,
            changelog=request.changelog,
            metadata=request.metadata,
        )
    except Exception as e:
        logger.error(f"Database error: {e}")
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail={"status": "error", "error": f"Database error: {str(e)}"},
        )
    
    logger.info(f"Published: {item_type} {item_id} v{version}")
    
    return PushResponse(
        status="published",
        item_type=item_type,
        item_id=item_id,
        namespace=namespace,
        category=category,
        name=name,
        version=version,
        signature=SignatureInfo(**signature_info),
        signed_content=signed_content,
    )


async def _upsert_item(
    item_type: str,
    namespace: str,
    category: str,
    name: str,
    version: str,
    content: str,
    content_hash: str,
    user_id: str,
    username: str,
    changelog: str = None,
    metadata: dict = None,
):
    """Insert or update item and version in database."""
    supabase = get_supabase()
    metadata = metadata or {}
    
    table = get_table_name(item_type)
    version_table = get_version_table_name(item_type)
    
    await _ensure_user(user_id, username)
    
    # Check if item exists (unique on namespace, category, name)
    result = supabase.table(table).select("id").eq(
        "namespace", namespace
    ).eq("category", category).eq("name", name).execute()
    
    if result.data:
        item_uuid = result.data[0]["id"]
    else:
        # Create new item
        item_data = {
            "namespace": namespace,
            "category": category,
            "name": name,
            "author_id": user_id,
            "description": metadata.get("description", ""),
            "visibility": metadata.get("visibility", "private"),
        }
        
        # Type-specific fields
        if item_type == "tool":
            item_data["tool_type"] = metadata.get("tool_type", "python")
        elif item_type == "knowledge":
            item_data["title"] = metadata.get("title", name)
            item_data["entry_type"] = metadata.get("entry_type", "reference")
        
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
    
    # Update latest_version on item
    supabase.table(table).update({"latest_version": version}).eq("id", item_uuid).execute()


# =============================================================================
# PULL - Download an item
# =============================================================================


@app.get("/v1/pull/{item_type}/{item_id:path}", response_model=PullResponse)
async def pull_item(
    item_type: str,
    item_id: str,
    version: str = None,
):
    """Pull an item from the registry.
    
    item_id format: namespace/category/name
    """
    if item_type not in ["directive", "tool", "knowledge"]:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail={"error": f"Invalid item_type: {item_type}"},
        )
    
    # Parse item_id
    try:
        namespace, category, name = parse_item_id(item_id)
    except ValueError as e:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail={"error": str(e)},
        )
    
    supabase = get_supabase()
    table = get_table_name(item_type)
    version_table = get_version_table_name(item_type)
    
    # Query item
    item_result = supabase.table(table).select("*").eq(
        "namespace", namespace
    ).eq("category", category).eq("name", name).execute()
    
    if not item_result.data:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail={"error": f"{item_type.title()} not found: {item_id}"},
        )
    
    item = item_result.data[0]
    item_uuid = item["id"]
    
    # Query versions
    versions_result = supabase.table(version_table).select("*").eq(
        f"{item_type}_id", item_uuid
    ).order("created_at", desc=True).execute()
    
    versions = versions_result.data or []
    if not versions:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail={"error": f"No versions found for {item_id}"},
        )
    
    # Get requested version or latest
    if version:
        target_version = next((v for v in versions if v["version"] == version), None)
        if not target_version:
            raise HTTPException(
                status_code=status.HTTP_404_NOT_FOUND,
                detail={"error": f"Version {version} not found"},
            )
    else:
        target_version = next((v for v in versions if v.get("is_latest")), versions[0])
    
    author_username = await _get_author_username(item.get("author_id"))
    content = target_version["content"]
    
    # Extract signature info
    from rye.utils.metadata_manager import MetadataManager
    strategy = MetadataManager.get_strategy(item_type)
    sig_info = strategy.extract_signature(content) or {}
    
    return PullResponse(
        item_type=item_type,
        item_id=item_id,
        namespace=namespace,
        category=category,
        name=name,
        version=target_version["version"],
        content=content,
        author=author_username,
        signature=SignatureInfo(
            timestamp=sig_info.get("timestamp", ""),
            hash=sig_info.get("hash", ""),
            registry_username=sig_info.get("registry_username"),
        ),
        created_at=target_version["created_at"],
    )


# =============================================================================
# DELETE - Remove an item
# =============================================================================


@app.delete("/v1/delete/{item_type}/{item_id:path}", response_model=DeleteResponse)
async def delete_item(
    item_type: str,
    item_id: str,
    version: str = None,
    user: User = Depends(get_current_user),
):
    """Delete an item from the registry."""
    if item_type not in ["directive", "tool", "knowledge"]:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail={"error": f"Invalid item_type: {item_type}"},
        )
    
    try:
        namespace, category, name = parse_item_id(item_id)
    except ValueError as e:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail={"error": str(e)},
        )
    
    supabase = get_supabase()
    table = get_table_name(item_type)
    version_table = get_version_table_name(item_type)
    
    # Find item
    result = supabase.table(table).select("id, author_id").eq(
        "namespace", namespace
    ).eq("category", category).eq("name", name).execute()
    
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
            detail={"error": "You can only delete your own items"},
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
                detail={"error": f"Version {version} not found"},
            )
        
        # Delete item if no versions remain
        remaining = supabase.table(version_table).select("id").eq(
            f"{item_type}_id", item_uuid
        ).execute()
        if not remaining.data:
            supabase.table(table).delete().eq("id", item_uuid).execute()
    else:
        # Delete all versions and item
        del_result = supabase.table(version_table).delete().eq(
            f"{item_type}_id", item_uuid
        ).execute()
        deleted_versions = len(del_result.data) if del_result.data else 0
        supabase.table(table).delete().eq("id", item_uuid).execute()
    
    logger.info(f"Deleted: {item_type} {item_id} by @{user.username}")
    
    return DeleteResponse(
        status="deleted",
        item_type=item_type,
        item_id=item_id,
        version=version,
        deleted_versions=deleted_versions,
    )


# =============================================================================
# VISIBILITY - Set item visibility
# =============================================================================


@app.post("/v1/visibility/{item_type}/{item_id:path}", response_model=VisibilityResponse)
async def set_visibility(
    item_type: str,
    item_id: str,
    body: dict,
    user: User = Depends(get_current_user),
):
    """Set item visibility (public/private/unlisted)."""
    visibility = body.get("visibility")
    if visibility not in ["public", "private", "unlisted"]:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail={"error": f"Invalid visibility: {visibility}"},
        )
    
    if item_type not in ["directive", "tool", "knowledge"]:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail={"error": f"Invalid item_type: {item_type}"},
        )
    
    try:
        namespace, category, name = parse_item_id(item_id)
    except ValueError as e:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail={"error": str(e)},
        )
    
    supabase = get_supabase()
    table = get_table_name(item_type)
    
    # Find item
    result = supabase.table(table).select("id, author_id, visibility").eq(
        "namespace", namespace
    ).eq("category", category).eq("name", name).execute()
    
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
    supabase.table(table).update({"visibility": visibility}).eq("id", item["id"]).execute()
    
    logger.info(f"Visibility: {item_type} {item_id} {old_visibility} -> {visibility}")
    
    return VisibilityResponse(
        status="updated",
        item_type=item_type,
        item_id=item_id,
        visibility=visibility,
        previous_visibility=old_visibility,
    )


# =============================================================================
# SEARCH - Search registry items
# =============================================================================


@app.get("/v1/search", response_model=SearchResponse)
async def search_items(
    query: str,
    item_type: str = None,
    namespace: str = None,
    category: str = None,
    include_mine: bool = False,
    limit: int = 20,
    offset: int = 0,
    user: Optional[User] = Depends(get_current_user_optional),
):
    """Search for items in the registry.
    
    By default, only shows public items. If include_mine=true and authenticated,
    also includes your own private items.
    """
    supabase = get_supabase()
    results = []
    total = 0
    
    types_to_search = [item_type] if item_type else ["directive", "tool", "knowledge"]
    
    for itype in types_to_search:
        try:
            table = get_table_name(itype)
            
            q = supabase.table(table).select("*", count="exact")
            
            # Text search on name and description
            q = q.or_(f"name.ilike.%{query}%,description.ilike.%{query}%")
            
            if namespace:
                q = q.eq("namespace", namespace)
            if category:
                q = q.ilike("category", f"{category}%")  # Prefix match for nested
            
            # Visibility filter: public OR (include_mine AND owned by user)
            if include_mine and user:
                # Show public items OR user's own items (any visibility)
                q = q.or_(f"visibility.eq.public,author_id.eq.{user.id}")
            else:
                # Only show public items
                q = q.eq("visibility", "public")
            
            result = q.range(offset, offset + limit - 1).execute()
            
            for item in result.data:
                item_id = build_item_id(
                    item["namespace"],
                    item["category"],
                    item["name"],
                )
                results.append(SearchResultItem(
                    item_type=itype,
                    item_id=item_id,
                    namespace=item["namespace"],
                    category=item["category"],
                    name=item["name"],
                    description=item.get("description"),
                    version=item.get("latest_version") or "0.0.0",
                    author=item["namespace"],  # namespace is the author
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
# BUNDLE PUSH - Upload a bundle (manifest + files)
# =============================================================================


@app.post("/v1/bundle/push", response_model=BundlePushResponse)
async def push_bundle(
    request: BundlePushRequest,
    user: User = Depends(get_current_user),
):
    """Push a bundle to the registry.

    Stores the signed manifest and all bundle files as a versioned unit.
    The bundle_id is scoped to the authenticated user's namespace.
    """
    bundle_id = request.bundle_id
    manifest = request.manifest
    files = request.files

    # Parse version from manifest if not provided
    version = request.version
    if not version:
        try:
            import yaml
            manifest_data = yaml.safe_load(manifest)
            version = manifest_data.get("bundle", {}).get("version", "0.1.0")
        except Exception:
            version = "0.1.0"

    logger.info(f"Bundle push: {bundle_id} v{version} by @{user.username} ({len(files)} files)")

    supabase = get_supabase()
    namespace = user.username

    await _ensure_user(user.id, user.username)

    # Upsert bundle record
    result = supabase.table("bundles").select("id").eq(
        "namespace", namespace
    ).eq("bundle_id", bundle_id).execute()

    if result.data:
        bundle_uuid = result.data[0]["id"]
    else:
        create_result = supabase.table("bundles").insert({
            "bundle_id": bundle_id,
            "namespace": namespace,
            "name": bundle_id.split("/")[-1] if "/" in bundle_id else bundle_id,
            "description": "",
            "author_id": user.id,
            "visibility": "private",
        }).execute()
        bundle_uuid = create_result.data[0]["id"]

    # Mark existing versions as not latest
    supabase.table("bundle_versions").update({"is_latest": False}).eq(
        "bundle_id", bundle_uuid
    ).execute()

    # Compute content hash from manifest
    import hashlib
    content_hash = hashlib.sha256(manifest.encode()).hexdigest()

    # Store files as JSONB (content + sha256 per path)
    supabase.table("bundle_versions").insert({
        "bundle_id": bundle_uuid,
        "version": version,
        "manifest": manifest,
        "files": files,
        "content_hash": content_hash,
        "is_latest": True,
    }).execute()

    # Update latest_version
    supabase.table("bundles").update({"latest_version": version}).eq(
        "id", bundle_uuid
    ).execute()

    logger.info(f"Bundle published: {bundle_id} v{version}")

    return BundlePushResponse(
        status="pushed",
        bundle_id=bundle_id,
        version=version,
        file_count=len(files),
    )


# =============================================================================
# BUNDLE PULL - Download a bundle (manifest + files)
# =============================================================================


@app.get("/v1/bundle/pull/{bundle_id:path}", response_model=BundlePullResponse)
async def pull_bundle(
    bundle_id: str,
    version: str = None,
    namespace: str = None,
    user: User = Depends(get_current_user),
):
    """Pull a bundle from the registry.

    Returns the signed manifest and all bundle files.
    If namespace is not specified, defaults to the authenticated user's namespace.
    """
    if not namespace:
        namespace = user.username

    supabase = get_supabase()

    # Find bundle
    result = supabase.table("bundles").select("*").eq(
        "namespace", namespace
    ).eq("bundle_id", bundle_id).execute()

    if not result.data:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail={"error": f"Bundle not found: {namespace}/{bundle_id}"},
        )

    bundle = result.data[0]
    bundle_uuid = bundle["id"]

    # Get versions
    versions_result = supabase.table("bundle_versions").select("*").eq(
        "bundle_id", bundle_uuid
    ).order("created_at", desc=True).execute()

    versions = versions_result.data or []
    if not versions:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail={"error": f"No versions found for bundle {bundle_id}"},
        )

    if version:
        target = next((v for v in versions if v["version"] == version), None)
        if not target:
            raise HTTPException(
                status_code=status.HTTP_404_NOT_FOUND,
                detail={"error": f"Version {version} not found"},
            )
    else:
        target = next((v for v in versions if v.get("is_latest")), versions[0])

    # Increment download count
    supabase.table("bundles").update({
        "download_count": (bundle.get("download_count") or 0) + 1,
    }).eq("id", bundle_uuid).execute()

    author_username = await _get_author_username(bundle.get("author_id"))

    return BundlePullResponse(
        bundle_id=bundle_id,
        version=target["version"],
        manifest=target["manifest"],
        files=target["files"],
        author=author_username,
        created_at=target["created_at"],
    )


# =============================================================================
# API KEYS - Create, list, and revoke API keys for non-interactive auth
# =============================================================================


@app.post("/v1/api-keys", response_model=CreateApiKeyResponse)
async def create_api_key(
    request: CreateApiKeyRequest,
    user: User = Depends(get_current_user),
):
    """Create a new API key for non-interactive authentication.

    The raw key is returned only once in the response. Store it securely.
    """
    import secrets
    from datetime import datetime, timedelta, timezone

    supabase = get_supabase()

    await _ensure_user(user.id, user.username)

    # Generate key: rye_sk_ + 32 bytes of randomness as url-safe base64
    raw_secret = secrets.token_urlsafe(32)
    raw_key = f"{API_KEY_PREFIX}{raw_secret}"
    key_prefix = raw_secret[:8]
    key_hash = _hash_api_key(raw_key)

    expires_at = None
    if request.expires_in_days:
        expires_at = (
            datetime.now(timezone.utc) + timedelta(days=request.expires_in_days)
        ).isoformat()

    try:
        result = supabase.table("api_keys").insert({
            "user_id": user.id,
            "name": request.name,
            "key_prefix": key_prefix,
            "key_hash": key_hash,
            "scopes": request.scopes,
            "expires_at": expires_at,
        }).execute()
    except Exception as e:
        error_msg = str(e)
        if "idx_api_keys_user_name" in error_msg or "duplicate" in error_msg.lower():
            raise HTTPException(
                status_code=status.HTTP_409_CONFLICT,
                detail=f"API key with name '{request.name}' already exists. Revoke it first.",
            )
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"Failed to create API key: {error_msg}",
        )

    record = result.data[0]
    logger.info(f"API key created: {request.name} for @{user.username}")

    return CreateApiKeyResponse(
        key=raw_key,
        name=request.name,
        key_prefix=key_prefix,
        scopes=request.scopes,
        expires_at=record.get("expires_at"),
        created_at=record["created_at"],
    )


@app.get("/v1/api-keys", response_model=ListApiKeysResponse)
async def list_api_keys(
    user: User = Depends(get_current_user),
):
    """List all API keys for the authenticated user."""
    supabase = get_supabase()

    result = (
        supabase.table("api_keys")
        .select("id, name, key_prefix, scopes, created_at, expires_at, last_used_at, revoked_at")
        .eq("user_id", user.id)
        .order("created_at", desc=True)
        .execute()
    )

    keys = [
        ApiKeyInfo(
            id=row["id"],
            name=row["name"],
            key_prefix=row["key_prefix"],
            scopes=row["scopes"],
            created_at=row["created_at"],
            expires_at=row.get("expires_at"),
            last_used_at=row.get("last_used_at"),
            revoked=row.get("revoked_at") is not None,
        )
        for row in result.data
    ]

    return ListApiKeysResponse(keys=keys, count=len(keys))


@app.delete("/v1/api-keys/{name}", response_model=RevokeApiKeyResponse)
async def revoke_api_key(
    name: str,
    user: User = Depends(get_current_user),
):
    """Revoke an API key by name."""
    supabase = get_supabase()

    # Find the key
    result = (
        supabase.table("api_keys")
        .select("id, revoked_at")
        .eq("user_id", user.id)
        .eq("name", name)
        .execute()
    )

    if not result.data:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=f"API key '{name}' not found",
        )

    if result.data[0].get("revoked_at"):
        raise HTTPException(
            status_code=status.HTTP_409_CONFLICT,
            detail=f"API key '{name}' is already revoked",
        )

    supabase.table("api_keys").update(
        {"revoked_at": "now()"}
    ).eq("id", result.data[0]["id"]).execute()

    logger.info(f"API key revoked: {name} for @{user.username}")

    return RevokeApiKeyResponse(status="revoked", name=name)


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
