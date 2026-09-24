# Proxy fixture classification

The pinned CLI schema has a connect-token response containing `method`,
`token`, `uri`, `executionId`, and `expiresAt`. Its example URI is illustrative;
it does not define a validated origin or path grammar. The adapter must not
parse this response or contact any proxy URL until W3 has an authoritative
origin/path and operation-scope policy. No example token or proxy host is
included in fixtures.
