# Recorded external-agent transcripts

Each `*.ndjson` file is a recorded frame transcript of a real external agent
session, one JSON object per line: `transcript_start`, then `expect_outbound`
(what the client sent) and `emit_inbound` (what the agent sent), then
`runtime_exit`. The replay tests in `src/replay_tests.rs` feed the inbound
notification frames through the real bridge and the real signed profile.

`codex/simple-0.156.1.ndjson` was recorded against Codex app-server 0.156.1 by
the T3 Code project (MIT licence, Copyright (c) 2026 T3 Tools Inc.) and copied
unchanged. The recorder scrubbed host paths and ids.
