# Data-Driven Budget Ledger Schema

> SQLite schema and configuration for hierarchical budget tracking
>
> **Location:** `.ai/threads/registry.db` (budget_ledger table)

## Overview

The budget ledger tracks hierarchical budget allocation across thread trees using SQLite. The schema and transaction behavior are data-driven from YAML configuration.

## Schema Configuration

```yaml
# budget_ledger_schema.yaml
schema_version: "1.0.0"

ledger:
  # Table definition
  table:
    name: "budget_ledger"
    description: "Hierarchical budget tracking for thread trees"

    columns:
      - name: thread_id
        type: TEXT
        nullable: false
        primary_key: true
        description: "Unique thread identifier"

      - name: parent_thread_id
        type: TEXT
        nullable: true
        foreign_key:
          table: budget_ledger
          column: thread_id
          on_delete: CASCADE
        description: "Parent thread in hierarchy"

      - name: reserved_spend
        type: REAL
        nullable: false
        default: 0.0
        description: "Budget reserved for this thread (atomically checked)"

      - name: actual_spend
        type: REAL
        nullable: false
        default: 0.0
        description: "Actual spend reported by thread"

      - name: max_spend
        type: REAL
        nullable: true
        description: "Maximum allowed spend (null = unlimited)"

      - name: status
        type: TEXT
        nullable: false
        default: "active"
        enum: [active, completed, cancelled]
        description: "Thread budget status"

      - name: updated_at
        type: TEXT
        nullable: false
        description: "ISO 8601 timestamp of last update"

    indexes:
      - name: idx_budget_parent
        columns: [parent_thread_id]
        description: "Fast lookup of child threads"

      - name: idx_budget_status
        columns: [status]
        description: "Fast filtering by status"

# Transaction Configuration
transactions:
  # Isolation level for budget operations
  isolation_level: "IMMEDIATE"

  # Timeout for acquiring lock
  lock_timeout_seconds: 5

  # Retry on lock conflict
  retry:
    enabled: true
    max_attempts: 3
    backoff_ms: [10, 50, 100]

# Operations
operations:
  register:
    description: "Register a new thread's budget"
    sql: |
      INSERT INTO budget_ledger 
        (thread_id, parent_thread_id, max_spend, status, updated_at)
      VALUES 
        (?, ?, ?, 'active', datetime('now'))

  reserve:
    description: "Atomically reserve budget for a child"
    sql: |
      BEGIN IMMEDIATE;

      -- Get parent's remaining budget
      WITH parent_budget AS (
        SELECT 
          max_spend,
          actual_spend,
          (SELECT COALESCE(SUM(reserved_spend), 0) 
           FROM budget_ledger 
           WHERE parent_thread_id = ? AND status = 'active') as children_reserved,
          (SELECT COALESCE(SUM(actual_spend), 0) 
           FROM budget_ledger 
           WHERE parent_thread_id = ? AND status != 'active') as children_actual
        FROM budget_ledger
        WHERE thread_id = ?
      )
      SELECT 
        COALESCE(max_spend, 0) - actual_spend - children_reserved - children_actual as remaining
      FROM parent_budget;

      -- Insert child reservation (only if remaining >= requested)
      INSERT INTO budget_ledger 
        (thread_id, parent_thread_id, reserved_spend, max_spend, status, updated_at)
      SELECT ?, ?, ?, ?, 'active', datetime('now')
      WHERE (SELECT remaining FROM parent_budget) >= ?;

      COMMIT;

    returns: "rows_inserted" # 1 = success, 0 = insufficient budget

  report_actual:
    description: "Report actual spend and release reservation"
    sql: |
      UPDATE budget_ledger
      SET 
        actual_spend = MIN(?, reserved_spend),  -- Clamp to reserved
        reserved_spend = 0,
        status = 'completed',
        updated_at = datetime('now')
      WHERE thread_id = ?

  check_remaining:
    description: "Calculate remaining budget for a thread"
    sql: |
      SELECT 
        COALESCE(max_spend, 0) 
          - actual_spend 
          - (SELECT COALESCE(SUM(reserved_spend), 0) 
             FROM budget_ledger 
             WHERE parent_thread_id = ? AND status = 'active')
          - (SELECT COALESCE(SUM(actual_spend), 0) 
             FROM budget_ledger 
             WHERE parent_thread_id = ? AND status != 'active')
        as remaining
      FROM budget_ledger
      WHERE thread_id = ?

# Invariants
invariants:
  - name: "child_budget_lte_parent"
    description: "Child max_spend cannot exceed parent's remaining budget"
    check: |
      -- Enforced by reserve() operation

  - name: "actual_lte_reserved"
    description: "Actual spend cannot exceed reserved amount"
    check: |
      -- Enforced by MIN(?, reserved_spend) in report_actual

  - name: "non_negative"
    description: "All spend values must be non-negative"
    check: |
      reserved_spend >= 0 AND actual_spend >= 0

# Constraints
constraints:
  parent_exists:
    description: "Parent thread must exist when registering child"
    enforce: false  -- Allow orphans, checked at runtime

  no_cycles:
    description: "Thread cannot be its own ancestor"
    enforce: true   -- Prevent circular references

# Cleanup
cleanup:
  # Archive old records
  archive:
    enabled: true
    after_days: 30
    archive_table: "budget_ledger_archive"

  # Vacuum interval
  vacuum:
    enabled: true
    interval_days: 7
```

## Usage

```python
# Load ledger config
config = load_config("budget_ledger_schema.yaml", project_path)

# Initialize table
async def init_ledger(db_path):
    schema = config.ledger.table

    # Create table from schema
    columns = []
    for col in schema.columns:
        col_def = f"{col.name} {col.type}"
        if not col.nullable:
            col_def += " NOT NULL"
        if col.default is not None:
            col_def += f" DEFAULT {col.default}"
        columns.append(col_def)

    sql = f"""
    CREATE TABLE IF NOT EXISTS {schema.name} (
        {', '.join(columns)},
        PRIMARY KEY ({', '.join(c.name for c in schema.columns if c.primary_key)})
    )
    """

    # Create indexes
    for idx in schema.indexes:
        sql += f"""
        CREATE INDEX IF NOT EXISTS {idx.name}
        ON {schema.name} ({', '.join(idx.columns)});
        """

    async with aiosqlite.connect(db_path) as db:
        await db.executescript(sql)

# Register thread budget
async def register_budget(thread_id, parent_id, max_spend):
    op = config.operations.register

    async with aiosqlite.connect(db_path) as db:
        await db.execute(op.sql, (thread_id, parent_id, max_spend))
        await db.commit()

# Reserve budget for child (atomic)
async def reserve_budget(parent_id, child_id, amount):
    op = config.operations.reserve

    async with aiosqlite.connect(db_path) as db:
        # Use IMMEDIATE isolation
        await db.execute("BEGIN IMMEDIATE")

        # Execute reservation SQL
        cursor = await db.execute(op.sql,
            (parent_id, parent_id, parent_id,  # For parent_budget CTE
             child_id, parent_id, amount, amount, amount))  # For INSERT

        # Check if insert succeeded
        await db.commit()

        # Query if row was inserted
        cursor = await db.execute(
            "SELECT 1 FROM budget_ledger WHERE thread_id = ?",
            (child_id,)
        )
        row = await cursor.fetchone()
        return row is not None  # True = reservation succeeded
```

## Migration

Schema changes use versioned migrations:

```yaml
# migrations/v1.1.0_add_turns_budget.yaml
version: "1.1.0"
description: "Add turns and tokens budget tracking"

changes:
  - type: add_column
    table: budget_ledger
    column:
      name: max_turns
      type: INTEGER
      nullable: true

  - type: add_column
    table: budget_ledger
    column:
      name: actual_turns
      type: INTEGER
      nullable: false
      default: 0
```

## Testing

```python
class TestBudgetLedgerSchema:
    test_table_created_with_all_columns
    test_indexes_created
    test_foreign_key_constraint
    test_reserve_atomic
    test_report_actual_clamps_to_reserved
    test_check_remaining_calculation
```
