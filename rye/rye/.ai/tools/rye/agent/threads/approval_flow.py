"""
Phase 3: Human-in-the-Loop Approval Flow.

File-based approval request/response pattern for deployment gates and human decisions.
Uses .ai/threads/{thread_id}/approvals/{request_id}.{request|response}.json files.

Key design:
- Filesystem is the message bus (no IPC/database)
- Timeout-based polling for response files
- Atomic writes for request/response files
"""

import json
import logging
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Dict, Optional, Any

logger = logging.getLogger(__name__)


class ApprovalRequest:
    """Represents a human approval request."""
    
    def __init__(
        self,
        request_id: str,
        prompt: str,
        thread_id: str,
        timeout_seconds: int = 300,
    ):
        """
        Create an approval request.
        
        Args:
            request_id: Unique request identifier (e.g., "approval-1739012650")
            prompt: Human-readable approval prompt (e.g., "Deploy to production?")
            thread_id: Associated thread ID
            timeout_seconds: How long to wait for response (default 5 minutes)
        """
        self.request_id = request_id
        self.prompt = prompt
        self.thread_id = thread_id
        self.timeout_seconds = timeout_seconds
        self.created_at = datetime.now(timezone.utc).isoformat()
    
    def to_dict(self) -> Dict[str, Any]:
        """Serialize request to dict."""
        return {
            "id": self.request_id,
            "prompt": self.prompt,
            "thread_id": self.thread_id,
            "created_at": self.created_at,
            "timeout_seconds": self.timeout_seconds,
        }


class ApprovalResponse:
    """Represents a human approval response."""
    
    def __init__(
        self,
        approved: bool,
        message: str = "",
        request_id: Optional[str] = None,
    ):
        """
        Create an approval response.
        
        Args:
            approved: Whether approval was granted
            message: Optional message/reasoning
            request_id: Associated request ID (optional)
        """
        self.approved = approved
        self.message = message
        self.request_id = request_id
        self.responded_at = datetime.now(timezone.utc).isoformat()
    
    def to_dict(self) -> Dict[str, Any]:
        """Serialize response to dict."""
        result = {
            "approved": self.approved,
            "message": self.message,
            "responded_at": self.responded_at,
        }
        if self.request_id:
            result["request_id"] = self.request_id
        return result


def request_approval(
    thread_id: str,
    prompt: str,
    project_path: Path,
    timeout_seconds: int = 300,
) -> str:
    """
    Create a human approval request and write it to the filesystem.
    
    File structure:
    ```
    .ai/threads/{thread_id}/approvals/
        {request_id}.request.json
        {request_id}.response.json  (created later by human/approver)
    ```
    
    Args:
        thread_id: Thread ID requesting approval
        prompt: Human-readable approval prompt
        project_path: Project root
        timeout_seconds: Response timeout (default 5 minutes)
        
    Returns:
        request_id for later polling
        
    Raises:
        Exception: If write fails
    """
    # Generate request ID using timestamp
    timestamp = int(time.time())
    request_id = f"approval-{timestamp}"
    
    # Create approval directory
    approval_dir = project_path / ".ai" / "threads" / thread_id / "approvals"
    approval_dir.mkdir(parents=True, exist_ok=True)
    
    # Create request object
    request = ApprovalRequest(
        request_id=request_id,
        prompt=prompt,
        thread_id=thread_id,
        timeout_seconds=timeout_seconds,
    )
    
    # Write request atomically
    request_path = approval_dir / f"{request_id}.request.json"
    tmp_path = request_path.with_suffix(".json.tmp")
    
    try:
        with open(tmp_path, "w", encoding="utf-8") as f:
            json.dump(request.to_dict(), f, indent=2)
        tmp_path.rename(request_path)
        logger.info(f"Created approval request {request_id} for thread {thread_id}")
    except Exception as e:
        if tmp_path.exists():
            tmp_path.unlink()
        logger.error(f"Failed to create approval request: {e}")
        raise
    
    return request_id


def wait_for_approval(
    request_id: str,
    thread_id: str,
    project_path: Path,
    timeout_seconds: Optional[int] = None,
) -> Dict[str, Any]:
    """
    Poll for approval response, blocking until timeout or response received.
    
    This function polls the filesystem for a response file:
    ```
    .ai/threads/{thread_id}/approvals/{request_id}.response.json
    ```
    
    Args:
        request_id: Request ID to wait for
        thread_id: Associated thread ID
        project_path: Project root
        timeout_seconds: Max seconds to wait (overrides request's timeout)
        
    Returns:
        Dict with keys:
        - approved: bool
        - message: str (optional message)
        - request_id: str (the original request_id)
        - responded_at: ISO timestamp
        
    Raises:
        TimeoutError: If response not received within timeout
        ValueError: If response file is malformed
        FileNotFoundError: If original request not found
    """
    approval_dir = project_path / ".ai" / "threads" / thread_id / "approvals"
    request_path = approval_dir / f"{request_id}.request.json"
    response_path = approval_dir / f"{request_id}.response.json"
    
    # Validate request exists
    if not request_path.exists():
        raise FileNotFoundError(f"Approval request not found: {request_path}")
    
    # Load request to get timeout if not overridden
    if timeout_seconds is None:
        try:
            request_data = json.loads(request_path.read_text())
            timeout_seconds = request_data.get("timeout_seconds", 300)
        except Exception as e:
            logger.warning(f"Failed to read request timeout: {e}, using default 300s")
            timeout_seconds = 300
    
    # Poll for response
    start_time = time.time()
    poll_interval = 1.0  # Check every second
    
    while True:
        elapsed = time.time() - start_time
        
        # Check if response file exists
        if response_path.exists():
            try:
                response_data = json.loads(response_path.read_text())
                response_data["request_id"] = request_id
                return response_data
            except json.JSONDecodeError as e:
                raise ValueError(f"Malformed approval response: {e}")
        
        # Check timeout
        if elapsed > timeout_seconds:
            raise TimeoutError(
                f"Approval request {request_id} timed out after {timeout_seconds} seconds"
            )
        
        # Wait before next poll
        time.sleep(min(poll_interval, timeout_seconds - elapsed))
    

def poll_approval(
    request_id: str,
    thread_id: str,
    project_path: Path,
) -> Optional[Dict[str, Any]]:
    """
    Non-blocking check for approval response.
    
    Returns response if available, None if not yet responded.
    
    Args:
        request_id: Request ID to check
        thread_id: Associated thread ID
        project_path: Project root
        
    Returns:
        Response dict if available, None otherwise
        
    Raises:
        ValueError: If response file is malformed
    """
    approval_dir = project_path / ".ai" / "threads" / thread_id / "approvals"
    response_path = approval_dir / f"{request_id}.response.json"
    
    if not response_path.exists():
        return None
    
    try:
        response_data = json.loads(response_path.read_text())
        response_data["request_id"] = request_id
        return response_data
    except json.JSONDecodeError as e:
        raise ValueError(f"Malformed approval response: {e}")


def write_approval_response(
    request_id: str,
    thread_id: str,
    approved: bool,
    message: str,
    project_path: Path,
) -> None:
    """
    Write an approval response to the filesystem (for approvers/testers).
    
    Args:
        request_id: Original request ID
        thread_id: Associated thread ID
        approved: Whether to approve
        message: Optional reasoning/message
        project_path: Project root
        
    Raises:
        Exception: If write fails
    """
    approval_dir = project_path / ".ai" / "threads" / thread_id / "approvals"
    approval_dir.mkdir(parents=True, exist_ok=True)
    
    response = ApprovalResponse(
        approved=approved,
        message=message,
        request_id=request_id,
    )
    
    response_path = approval_dir / f"{request_id}.response.json"
    tmp_path = response_path.with_suffix(".json.tmp")
    
    try:
        with open(tmp_path, "w", encoding="utf-8") as f:
            json.dump(response.to_dict(), f, indent=2)
        tmp_path.rename(response_path)
        logger.info(f"Wrote approval response {request_id}: approved={approved}")
    except Exception as e:
        if tmp_path.exists():
            tmp_path.unlink()
        logger.error(f"Failed to write approval response: {e}")
        raise


def list_pending_approvals(
    thread_id: str,
    project_path: Path,
) -> list[Dict[str, Any]]:
    """
    List all pending approval requests for a thread.
    
    Returns all .request.json files that don't have a corresponding .response.json.
    
    Args:
        thread_id: Thread ID
        project_path: Project root
        
    Returns:
        List of pending request dicts
    """
    approval_dir = project_path / ".ai" / "threads" / thread_id / "approvals"
    
    if not approval_dir.exists():
        return []
    
    pending = []
    for request_file in approval_dir.glob("*.request.json"):
        request_id = request_file.stem.replace(".request", "")
        response_file = approval_dir / f"{request_id}.response.json"
        
        # Only include if no response yet
        if not response_file.exists():
            try:
                request_data = json.loads(request_file.read_text())
                pending.append(request_data)
            except Exception as e:
                logger.warning(f"Failed to read request {request_file}: {e}")
    
    return pending
