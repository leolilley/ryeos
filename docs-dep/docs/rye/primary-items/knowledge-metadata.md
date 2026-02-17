# Knowledge Metadata Reference

Lean metadata specification for knowledge entries in Rye OS.

## Overview

Knowledge entries are markdown documents with YAML frontmatter that store learnings, patterns, specifications, and reference information. They form the searchable knowledge base integrated with directives and tools.

Entries are hash-validated and cryptographically signed on update. Metadata supports deterministic knowledge graph traversal via explicit link relationships.

---

## Required Fields

### `id`

**Type:** string (kebab-case)  
**Required:** Yes

Unique identifier for the knowledge entry. Used in cross-references and searches.

```yaml
id: authentication-patterns
```

**Convention:** Use kebab-case, hierarchical when appropriate:

- `oauth-integration`
- `kubernetes/deployment-strategies`
- `patterns/retry-logic`

### `title`

**Type:** string  
**Required:** Yes

Human-readable title for the knowledge entry.

```yaml
title: Authentication Patterns and Best Practices
```

### `category`

**Type:** string (non-empty)  
**Required:** Yes

Categorizes the knowledge entry. Must match the directory path relative to the `knowledge/` parent directory.

```yaml
category: security/authentication
```

**Examples:**

For file at `.ai/knowledge/security/authentication/oauth.md`:

```yaml
category: security/authentication
```

For file at `.ai/knowledge/patterns/retry-logic.md`:

```yaml
category: patterns
```

For file at `.ai/knowledge/reference.md` (root level):

```yaml
category: reference
```

### `version`

**Type:** semantic version string (X.Y.Z)  
**Required:** Yes

Version of the knowledge entry following semantic versioning. Not git-controlled; tracks changes to entry content.

```yaml
version: "2.1.0"
```

### `author`

**Type:** string  
**Required:** Yes

Original author or creator of the knowledge entry.

```yaml
author: security-team
```

### `created_at`

**Type:** ISO 8601 datetime string  
**Required:** Yes

When the entry was created.

```yaml
created_at: 2025-12-15T10:30:00Z
```

---

## Optional Fields

### `tags`

**Type:** list of strings  
**Purpose:** Searchable tags for discovery

```yaml
tags:
  - authentication
  - oauth2
  - security
```

**Best practices:**

- Use lowercase, kebab-case
- 3-5 tags per entry
- Include problem domain, not just solution

### `references`

**Type:** list of knowledge IDs or external URLs  
**Purpose:** Link to related or referenced knowledge

```yaml
references:
  - oauth-overview
  - token-validation
  - "https://oauth.net/2/"
  - "https://tools.ietf.org/html/rfc6749"
```

**Format:**

- Internal: `knowledge-id` (no prefix)
- External: Full URLs starting with `http://` or `https://`

**Graph navigation:** References are part of the outbound knowledge graph. Use these to traverse forward dependencies.

### `extends`

**Type:** list of knowledge IDs  
**Purpose:** Declare what knowledge this entry builds upon

```yaml
extends:
  - authentication-basics
  - http-security
```

**Graph navigation:** Extends creates an inheritance/dependency chain. Use to traverse upward to foundational concepts.

### `used_by`

**Type:** list of directive or tool IDs  
**Purpose:** Track where this knowledge is applied in the system

```yaml
used_by:
  - setup-oauth-provider
  - secure-api-endpoint
```

**Graph navigation:** Helps understand impact of changes and trace which automation uses this knowledge.

---

## Knowledge Graph Navigation

Knowledge entries form a deterministic graph through explicit link relationships:

- **`extends`** → Navigate upward to foundational concepts
- **`references`** → Navigate to related or referenced knowledge
- **`used_by`** → Traverse inbound to directives/tools that depend on this entry
- **Backlinks** (derived) → Automatically generated from other entries' `references` and `extends` fields pointing to this entry

No external system needed; the graph is fully derivable from metadata.

---

## File Structure

Knowledge entries use markdown with YAML frontmatter:

```markdown
---
id: oauth2-implementation
title: OAuth2 Implementation Guide
category: guide
version: "1.0.0"
author: security-team
created_at: 2025-12-15T10:30:00Z

tags:
  - oauth2
  - authentication
  - implementation

references:
  - jwt-overview
  - "https://tools.ietf.org/html/rfc6749"

extends:
  - authentication-basics

used_by:
  - setup-oauth-provider
---

# OAuth2 Implementation Guide

Content starts here...
```

---

## Complete Example

````yaml
---
id: jwt-token-validation
title: JWT Token Validation Patterns
category: pattern
version: "2.1.0"
author: security-team
created_at: 2025-10-01T08:00:00Z

tags:
  - jwt
  - tokens
  - validation
  - security

references:
  - jwt-overview
  - cryptographic-algorithms
  - "https://tools.ietf.org/html/rfc7519"

extends:
  - authentication-basics
  - cryptographic-signatures

used_by:
  - api-authentication
  - service-authorization
---

# JWT Token Validation Patterns

Best practices for validating JWT tokens in production systems.

## Key Implementation Points

- Token signature verified with public key
- Expiration time always validated
- Algorithm whitelist enforced
- Claims validated according to spec
- None algorithm never accepted

## Common Mistakes to Avoid

- Trusting unverified tokens
- Accepting None algorithm
- Using wrong key for signature validation
- Not checking token expiration
- Weak key management practices

## Examples

### Python

```python
import jwt
from cryptography.hazmat.primitives import serialization

with open('public_key.pem', 'rb') as f:
    public_key = serialization.load_pem_public_key(f.read())

try:
    payload = jwt.decode(
        token,
        public_key,
        algorithms=['RS256'],
        audience='api:prod',
        issuer='https://auth.example.com'
    )
except jwt.InvalidTokenError as e:
    # Handle invalid token
    pass
````

### Node.js

```javascript
const jwt = require("jsonwebtoken");
const fs = require("fs");

const publicKey = fs.readFileSync("public_key.pem");

try {
  const decoded = jwt.verify(token, publicKey, {
    algorithms: ["RS256"],
    audience: "api:prod",
    issuer: "https://auth.example.com",
  });
} catch (err) {
  // Handle invalid token
}
```

```

---

## Validation Rules

1. **Required fields:** `id`, `title`, `category`, `version`, `author`, `created_at`
2. **`id`** must be kebab-case alphanumeric
3. **`category`** must match the file path relative to `knowledge/` directory
4. **`version`** must be semantic version (X.Y.Z)
5. **`created_at`** must be ISO 8601 format
6. **`references`** and graph links use knowledge IDs or full URLs

---

## Best Practices

### Writing Knowledge

- **Title:** Clear, descriptive, avoids jargon
- **Content:** Well-organized with headings and examples
- **Length:** Focused on one topic; split if exceeds 2000 words
- **Examples:** Include code samples, screenshots, diagrams in markdown
- **Clarity:** Use plain language, avoid ambiguity

### Graph Organization

- Use **`extends`** to declare dependencies on foundational knowledge
- Use **`references`** to link to related knowledge or external sources
- Use **`used_by`** to document which directives/tools apply this knowledge
- Backlinks are automatically derived; no need to maintain manually

### Metadata

- Keep `tags` focused (3-5 items)
- Use kebab-case for all IDs
- Version on content changes, not metadata tweaks

---

## References

- [YAML Specification](https://yaml.org/)
- [Markdown Guide](https://www.markdownguide.org/)
```
