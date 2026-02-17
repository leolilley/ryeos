# Lilux Microkernel Principles

## What is Lilux?

Lilux is a **microkernel** for AI agent systems. It provides generic execution primitives without any domain intelligence. Like a CPU provides instruction execution, Lilux provides tool execution—nothing more.

## Core Principle: Separation of Concerns

### Lilux's Responsibility (Microkernel Layer)

Lilux provides **execution primitives only**:

- Execute subprocess commands in isolated environments
- Make HTTP requests with retry logic and authentication
- Manage file locks and concurrency control
- Store credentials securely in keychains

**NO content intelligence.** Lilux never touches:

- XML parsing
- Metadata understanding
- Schema validation
- Configuration loading
- Tool discovery

## Installation and Usage

Lilux is installed as a library dependency:

```bash
# Lilux is installed as a library dependency
pip install lilux
```

Lilux is **not a standalone server**. It's a library used by other systems. To use Lilux:

```python
# Example usage in external system
from lilux.primitives import SubprocessPrimitive
from lilux.primitives import HttpClientPrimitive
from lilux.runtime import AuthStore, EnvResolver

# Use primitives directly
subprocess_prim = SubprocessPrimitive()
result = subprocess_prim.execute(command="echo hello")
```

## Package Installation

Lilux can be installed from package repositories:

```bash
# Install from PyPI
pip install lilux

# Install from local source
cd /path/to/lilux
pip install -e .
```

## Dependencies

```
lilux/
├── No external dependencies for core primitives
└── Optional: keychain integration (keyring)
```

## Design Principles

### 1. Minimal Microkernel

Each component in Lilux is intentionally simple:

- SubprocessPrimitive - Just executes shell commands
- HttpClientPrimitive - Just makes HTTP requests
- AuthStore - Just manages keychain storage
- EnvResolver - Just resolves environment variables

### 2. Generic Primitives

All primitives are generic and reusable:

- No tool-specific logic
- No domain knowledge
- Framework-agnostic
- Can be used by any orchestrator

### 3. Library API

Lilux provides a clean, well-documented API:

- Clear primitive interfaces
- Consistent error handling
- Comprehensive configuration options
- Self-contained functionality

### 4. Security

Security is handled through:

- OS keychain integration for credentials
- Lockfile management for concurrency
- Cryptographic primitives for hashing and signing (pure functions only—verification workflows and caching are orchestrator responsibility)
- Isolated execution environments

## What Lilux Does NOT Do

Lilux does NOT provide:

- XML parsing
- Metadata understanding
- Schema validation
- Configuration loading
- Tool discovery
- Content parsing
- Orchestration

These capabilities are provided by orchestrator systems that use Lilux as a dependency.

## Core Components

### Primitives

Core execution primitives:

- **SubprocessPrimitive** - Execute shell commands
- **HttpClientPrimitive** - Make HTTP requests
- **LockfileManager** - Manage lockfiles
- **Integrity** - Pure cryptographic functions (hashing, signing)

### Runtime Services

Infrastructure services:

- **AuthStore** - Keychain integration
- **EnvResolver** - Environment variable resolution

### Schemas

JSON Schema definitions (basic validation only—schema extraction and interpretation are orchestrator responsibility):

- **ToolSchema** - JSON Schema type definitions

---

## Related Documentation

- **API Reference:** `[[lilux/api-reference]]` - Complete API signatures
- **Primitives:** `[[lilux/primitives/overview]]`
- **Runtime Services:** `[[lilux/runtime-services/overview]]`
- **Package Structure:** `[[lilux/package/structure]]`
