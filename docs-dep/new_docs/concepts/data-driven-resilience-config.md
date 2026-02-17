# Data-Driven Resilience Configuration

> Configuration for retry policies, checkpoints, limits, and recovery
>
> **Location:** `rye/rye/.ai/tools/rye/agent/threads/config/resilience.yaml`

## Overview

All resilience behavior is data-driven from YAML:
- Retry policies (exponential backoff, fixed delays)
- Checkpoint intervals and persistence
- Budget limits and escalation
- Error classification patterns

## Configuration Schema

```yaml
# resilience.yaml
schema_version: "1.0.0"

resilience:
  # Retry Configuration
  retry:
    # Default retry policy
    default_policy: "exponential"
    
    # Global max retries across all policies
    max_retries: 3
    
    # Retry policies
    policies:
      exponential:
        type: exponential
        base: 2.0                   # Initial delay in seconds
        multiplier: 2.0             # Multiply by this each attempt
        max_delay: 120.0            # Cap at 2 minutes
        formula: "min(base * (multiplier ** attempt), max_delay)"
        
      fixed:
        type: fixed
        delay: 60.0                 # Fixed 60 second delay
        
      rate_limited:
        type: use_header
        header: "retry-after"       # Use Retry-After header value
        fallback:
          policy: exponential
          base: 5.0
          
      quota_exceeded:
        type: fixed
        delay: 3600.0               # 1 hour for quota issues
    
    # Per-error-category retry rules
    rules:
      transient:
        retryable: true
        policy: exponential
        max_retries: 3
        
      rate_limited:
        retryable: true
        policy: rate_limited
        max_retries: 5
        
      quota:
        retryable: true
        policy: quota_exceeded
        max_retries: 2
        
      permanent:
        retryable: false
        
      limit_hit:
        retryable: false
        action: escalate
        
      budget:
        retryable: false
        action: escalate
        
      cancelled:
        retryable: false
        action: abort

  # Checkpoint Configuration
  checkpoint:
    # When to save state
    triggers:
      pre_turn:
        enabled: true
        description: "Before LLM call"
        
      post_llm:
        enabled: true
        description: "After LLM response"
        
      post_tools:
        enabled: true
        description: "After tool execution"
        
      on_error:
        enabled: true
        description: "When error occurs"
    
    # Persistence settings
    persistence:
      format: "json"
      compression: false
      atomic_write: true          # Write to .tmp then rename
      
    # Retention
    retention:
      max_checkpoints: 10         # Keep last N checkpoints
      cleanup_interval_minutes: 60

  # Budget & Limits Configuration
  budget:
    # Default limits for new threads
    defaults:
      spend: 1.0                  # $1.00 default
      turns: 10
      tokens: 100000
      spawns: 5
      duration_minutes: 30
    
    # Hierarchical budget
    hierarchical:
      enabled: true
      reservation_mode: "pessimistic"  # Reserve full child budget upfront
      
    # Limit escalation
    escalation:
      enabled: true
      
      # Proposed limit calculation
      proposal:
        strategy: "double"        # proposed = current * 2
        max_multiplier: 10        # Cap at 10x original
        
      # Approval request
      request:
        timeout_minutes: 60
        auto_approve_threshold: 5.0  # Auto-approve if proposed < $5
        
      # Retry after approval
      retry:
        max_escalations: 3        # Max times to escalate per thread
        reset_counters_on_escalation: true

  # Crash Recovery Configuration
  recovery:
    # Orphan detection
    orphan_detection:
      enabled: true
      check_interval_minutes: 5
      
      # Criteria for orphaned thread
      criteria:
        status_stuck_for_minutes: 10
        no_heartbeat_for_minutes: 5
        process_not_running: true
    
    # Recovery options
    options:
      auto_resume: false          # Require user confirmation
      max_recovery_attempts: 3
      
    # State reconstruction
    reconstruction:
      from_transcript: true       # Rebuild from transcript events
      from_checkpoint: true       # Prefer checkpoint if available
      max_events_to_replay: 1000

  # Cancellation Configuration
  cancellation:
    # Poison file settings
    poison_file:
      name: "cancel.requested"
      poll_interval_seconds: 1
      
    # Graceful shutdown
    graceful_shutdown:
      enabled: true
      timeout_seconds: 30
      
      # Cleanup on cancel
      cleanup:
        stop_managed_processes: true
        close_sinks: true
        save_final_state: true

# Event Configuration
events:
  retry_attempt:
    criticality: critical
    schema:
      type: object
      required: [attempt, max_attempts, delay, error_category]
      
  checkpoint_saved:
    criticality: droppable
    schema:
      type: object
      required: [checkpoint_id, turn, timestamp]
      
  limit_escalation_requested:
    criticality: critical
    schema:
      type: object
      required: [limit_code, current_value, proposed_max]
      
  thread_recovered:
    criticality: critical
    schema:
      type: object
      required: [original_thread_id, recovery_reason, state_source]
```

## Usage Examples

```python
# Load resilience config
config = load_config("resilience.yaml", project_path)

# Retry with configured policy
async def call_llm_with_retry(...):
    policy = config.retry.policies[config.retry.default_policy]
    
    for attempt in range(config.retry.max_retries):
        try:
            return await call_llm(...)
        except TransientError as e:
            delay = calculate_delay(policy, attempt)
            await asyncio.sleep(delay)

# Check if should retry
def should_retry(error_category, config):
    rule = config.retry.rules.get(error_category)
    return rule.retryable if rule else False
```

## Project Overrides

```yaml
# .ai/config/resilience.yaml
extends: "rye/agent/threads/config/resilience.yaml"

resilience:
  retry:
    max_retries: 5              # More retries for flaky API
    policies:
      exponential:
        max_delay: 300.0        # Up to 5 minutes
        
  budget:
    defaults:
      spend: 5.0                # Higher default budget
      
  recovery:
    orphan_detection:
      check_interval_minutes: 1  # Faster detection
```
