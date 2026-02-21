"""
Logger utility for RYE.

Implements rotating file logs in user space ({$USER_SPACE or ~}/.ai/logs/):
- rye.log: Main log with 5MB rotation, keeps 3 backups
- rye.errors.log: Errors only, 2MB rotation, keeps 2 backups
- rye.json: Structured JSON, 5MB rotation, keeps 2 backups

USER_SPACE defaults to ~ (home dir). AI_DIR (.ai/) is appended.
"""

import logging
import json
import sys
import os
from datetime import datetime, timezone
from logging.handlers import RotatingFileHandler
from pathlib import Path
from typing import Optional

from rye.constants import AI_DIR
from rye.utils.path_utils import get_user_space


class JsonFormatter(logging.Formatter):
    """JSON formatter for structured logging."""

    def format(self, record: logging.LogRecord) -> str:
        log_data = {
            "timestamp": datetime.now(timezone.utc).isoformat(),
            "level": record.levelname,
            "logger": record.name,
            "message": record.getMessage(),
        }

        if hasattr(record, "extra") and record.extra:
            log_data["extra"] = record.extra

        if record.exc_info:
            log_data["exception"] = self.formatException(record.exc_info)

        return json.dumps(log_data)


def _get_log_dir() -> Path:
    """Get log directory from user space."""
    log_dir = get_user_space() / AI_DIR / "logs"
    log_dir.mkdir(parents=True, exist_ok=True)
    return log_dir


def get_logger(name: str, level: Optional[int] = None) -> logging.Logger:
    """
    Get or create a logger with rotating file handlers.

    Logs to:
    - stderr (console) - warnings and errors only
    - {USER_SPACE}/.ai/logs/rye.log (rotating, 5MB max, 3 backups)
    - {USER_SPACE}/.ai/logs/rye.errors.log (errors only, 2MB max, 2 backups)
    - {USER_SPACE}/.ai/logs/rye.json (structured JSON, 5MB max, 2 backups)

    Args:
        name: Logger name
        level: Optional logging level (defaults to DEBUG)

    Returns:
        Configured logger instance
    """
    logger = logging.getLogger(name)

    if not logger.handlers:
        text_formatter = logging.Formatter(
            "%(asctime)s [%(levelname)s] %(name)s: %(message)s",
            datefmt="%Y-%m-%d %H:%M:%S",
        )

        # Console handler (stderr) - minimal output
        console_handler = logging.StreamHandler(sys.stderr)
        console_handler.setLevel(logging.WARNING)
        console_handler.setFormatter(text_formatter)
        logger.addHandler(console_handler)

        # File handlers (user space)
        try:
            log_dir = _get_log_dir()

            # Main log file
            main_log = log_dir / "rye.log"
            main_handler = RotatingFileHandler(
                main_log, maxBytes=5 * 1024 * 1024, backupCount=3, encoding="utf-8"
            )
            main_handler.setLevel(logging.DEBUG)
            main_handler.setFormatter(text_formatter)
            logger.addHandler(main_handler)

            # Error log file
            error_log = log_dir / "rye.errors.log"
            error_handler = RotatingFileHandler(
                error_log, maxBytes=2 * 1024 * 1024, backupCount=2, encoding="utf-8"
            )
            error_handler.setLevel(logging.ERROR)
            error_handler.setFormatter(text_formatter)
            logger.addHandler(error_handler)

            # JSON structured log
            json_log = log_dir / "rye.json"
            json_handler = RotatingFileHandler(
                json_log, maxBytes=5 * 1024 * 1024, backupCount=2, encoding="utf-8"
            )
            json_handler.setLevel(logging.INFO)
            json_handler.setFormatter(JsonFormatter())
            logger.addHandler(json_handler)

        except Exception:
            pass  # Fail silently if file logging unavailable

    if level is not None:
        logger.setLevel(level)
    elif not logger.level:
        logger.setLevel(logging.DEBUG)

    return logger


def cleanup_old_logs(days: int = 30) -> int:
    """
    Remove log files older than specified days.

    Args:
        days: Remove logs older than this many days

    Returns:
        Number of files removed
    """
    try:
        log_dir = _get_log_dir()
        cutoff = datetime.now().timestamp() - (days * 24 * 60 * 60)
        removed = 0

        for log_file in log_dir.glob("rye*.log*"):
            if log_file.stat().st_mtime < cutoff:
                log_file.unlink()
                removed += 1

        return removed
    except Exception:
        return 0
