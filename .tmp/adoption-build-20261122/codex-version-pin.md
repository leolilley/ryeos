# Codex binary version refusal (T3 item 12)

Design call for review: the only existing pin in signed data is the release URL in
bundles/codex/.ai/config/codex/activation.yaml (and schema dir app-server-0.147.0).
The bridge cannot read that file, so each codex structured-session profile
(structured-session, authoring, external-authoring) carries an optional
`expected_server_version` on its `initialize` step ("0.147.0"). That is a second copy;
bundles/codex/test_contract.py asserts it equals the version in activation.yaml.
The bridge compares it to the version token of the initialize response `userAgent`
and refuses with both versions named. Profile schema_version not bumped (optional key).
Unknown-notification rule untouched.
