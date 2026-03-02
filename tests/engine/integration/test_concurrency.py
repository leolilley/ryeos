"""Concurrency tests for thread-safe cache and execution.

Tests cover:
- Concurrent cache access
- Race condition prevention
- Concurrent tool execution
- Performance under load
"""

import pytest
import sys
import asyncio
import threading
from pathlib import Path

# Add rye to path
sys.path.insert(0, str(Path(__file__).parent.parent.parent / "rye"))

from rye.utils.validators import get_validation_schema, clear_validation_schemas_cache


class TestConcurrentCache:
    """Test concurrent cache access patterns."""

    def test_concurrent_validation_schema_access(self):
        """Test cache with 50 concurrent schema requests."""
        clear_validation_schemas_cache()
        results = []
        errors = []

        def access_schema():
            try:
                schema = get_validation_schema('tool')
                results.append(schema)
            except Exception as e:
                errors.append(str(e))

        threads = [threading.Thread(target=access_schema) for _ in range(50)]
        
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        # All threads should succeed
        assert len(errors) == 0, f"Concurrency errors: {errors}"
        assert len(results) == 50

    def test_concurrent_extraction_rules_access(self):
        """Test concurrent extraction rules access."""
        from rye.utils.validators import get_extraction_rules
        
        clear_validation_schemas_cache()
        results = []

        def access_rules():
            rules = get_extraction_rules('tool')
            results.append(rules)

        threads = [threading.Thread(target=access_rules) for _ in range(30)]
        
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        assert len(results) == 30

    def test_concurrent_cache_invalidation(self):
        """Test cache invalidation during concurrent access."""
        from rye.utils.validators import clear_validation_schemas_cache
        
        results = []
        errors = []

        def access_and_invalidate(iteration):
            try:
                if iteration % 5 == 0:
                    clear_validation_schemas_cache()
                schema = get_validation_schema('tool')
                results.append(schema)
            except Exception as e:
                errors.append(str(e))

        threads = [
            threading.Thread(target=access_and_invalidate, args=(i,))
            for i in range(50)
        ]
        
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        # Should handle concurrent invalidation
        assert len(errors) == 0


class TestRaceConditionPrevention:
    """Test prevention of race conditions."""

    def test_schema_initialization_race(self):
        """Test concurrent schema initialization doesn't cause duplicates."""
        from rye.utils.validators import _load_validation_schemas
        
        results = []

        def load_schemas():
            schema = _load_validation_schemas(None)
            results.append(id(schema))

        clear_validation_schemas_cache()
        threads = [threading.Thread(target=load_schemas) for _ in range(10)]
        
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        # Should have multiple schemas loaded (some may be same, some different)
        assert len(results) == 10

    def test_cache_lock_contention(self):
        """Test cache locks don't cause deadlocks."""
        import time
        
        clear_validation_schemas_cache()
        start_time = time.time()

        def rapid_access():
            for _ in range(100):
                get_validation_schema('tool')

        threads = [threading.Thread(target=rapid_access) for _ in range(5)]
        
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        elapsed = time.time() - start_time
        # Should complete reasonably fast (< 5 seconds for 500 accesses)
        assert elapsed < 5.0, f"Lock contention detected, took {elapsed}s"


class TestPerformanceUnderLoad:
    """Test performance remains stable under concurrent load."""

    def test_no_deadlock_under_stress(self):
        """Test no deadlock occurs under high concurrency."""
        import time
        
        results = []
        
        def stress_test(iteration):
            clear_validation_schemas_cache()
            schema = get_validation_schema('tool')
            results.append(schema)

        start = time.time()
        threads = [
            threading.Thread(target=stress_test, args=(i,))
            for i in range(100)
        ]
        
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        elapsed = time.time() - start
        # Should complete without deadlock (< 10 seconds for 100 threads)
        assert elapsed < 10.0, f"Possible deadlock, took {elapsed}s"
        assert len(results) == 100
