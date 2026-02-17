# Capability Tokens

Directives declare the permissions they need. At runtime, these declarations are minted into a `CapabilityToken` — an Ed25519-signed token that scopes what the directive's thread can do.

Implemented in [`rye/rye/.ai/tools/rye/agent/permissions/capability_tokens/capability_tokens.py`](rye/rye/.ai/tools/rye/agent/permissions/capability_tokens/capability_tokens.py).

## Permission Declarations

Permissions are declared in the directive's `<metadata>` block using hierarchical XML:

```xml
<metadata>
  <permissions>
    <execute>
      <tool>rye.file-system.*</tool>
      <directive>rye.agent.*</directive>
    </execute>
    <search>
      <knowledge>*</knowledge>
    </search>
  </permissions>
</metadata>
```

The parser ([`rye/rye/.ai/tools/rye/core/parsers/markdown_xml.py`](rye/rye/.ai/tools/rye/core/parsers/markdown_xml.py)) normalizes these into capability strings:

```
rye.execute.tool.rye.file-system.*
rye.execute.directive.rye.agent.*
rye.search.knowledge.*
```

### Capability Format

```
rye.{primary}.{item_type}.{specifics}
```

| Component   | Values                                             |
| ----------- | -------------------------------------------------- |
| `primary`   | `execute`, `search`, `load`, `sign`                |
| `item_type` | `tool`, `directive`, `knowledge`                   |
| `specifics` | Item ID with `/` converted to `.`, or `*` wildcard |

### Wildcards

```xml
<!-- Grant all permissions -->
<permissions>*</permissions>

<!-- Grant all execute permissions -->
<permissions>
  <execute>*</execute>
</permissions>
```

### Capability Hierarchy

The `execute` primary implies `search` and `load`. The `sign` primary implies `load`.

```python
PRIMARY_IMPLIES = {
    "execute": ["search", "load"],
    "sign": ["load"],
}
```

A directive with `rye.execute.tool.rye.file-system.*` can also search and load those tools without declaring separate permissions.

## Token Structure

```python
@dataclass
class CapabilityToken:
    caps: List[str]           # Granted capabilities
    aud: str                  # Audience ("rye-execute")
    exp: datetime             # Expiry (UTC, default 1 hour)
    directive_id: str         # Source directive
    thread_id: str            # Owning thread
    parent_id: Optional[str]  # Parent token (for delegation)
    signature: Optional[str]  # Ed25519 signature
    token_id: str             # Unique ID (UUID)
```

Tokens serialize to JWT-like base64 strings via `to_jwt()` / `from_jwt()`.

## Token Minting

When a thread starts, `_mint_token_from_permissions()` in [`thread_directive.py`](rye/rye/.ai/tools/rye/agent/threads/thread_directive.py) extracts capability strings from the parsed directive permissions and creates a token:

```python
token = CapabilityToken(
    caps=caps,
    aud="rye-execute",
    exp=datetime.now(timezone.utc) + timedelta(hours=1),
    directive_id=directive_name,
    thread_id=f"{directive_name}-root",
)
```

The token is then passed to the `SafetyHarness`, which validates every tool call against it.

## Token Attenuation

When a thread spawns a child thread, `attenuate_token()` computes the intersection of the parent's capabilities and the child directive's declared capabilities:

```python
def attenuate_token(parent_token, child_declared_caps):
    parent_caps = set(parent_token.caps)
    child_caps = set(child_declared_caps)
    attenuated_caps = list(parent_caps & child_caps)
    return CapabilityToken(
        caps=sorted(attenuated_caps),
        aud=parent_token.aud,
        exp=parent_token.exp,       # Inherits parent expiry
        parent_id=parent_token.token_id,
        ...
    )
```

A child thread can never have more capabilities than its parent. It can only have fewer or equal.

## Token Verification

Tokens are signed with the same Ed25519 keypair used for content signing (`~/.ai/keys/`). The `SafetyHarness` checks:

1. Token is not expired (`is_expired()`)
2. Token grants the required capability (`has_capability()` with prefix/glob matching)
3. Ed25519 signature is valid (`verify_token()`)

If any check fails, the tool call is denied with a `permission_denied` error that reports the missing capabilities.
