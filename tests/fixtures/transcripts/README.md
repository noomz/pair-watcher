# Transcript fixtures (§T3)

Captured live transcripts, scrubbed with `scrub.jq` (`jq -c -f scrub.jq <capture> > <fixture>`). The scrub keeps every line and its order, ids, types and timestamps, plus the fields the tailer reads; everything else on a line is dropped, strings are clipped to 300 chars, and the capturing user's home directory becomes `/home/dev`.

| Fixture | Source (captured 2026-10-05 unless noted) |
|---|---|
| `claude.jsonl` | Claude Code 2.1.289 Worker, idle-delivery check 13 (11 turns, 4 tool calls incl. a `PermissionRequest` dialog) |
| `rollout-…-04dc-….jsonl` | Codex 0.160.0 Worker rollout, check 13 (4 turns, 2 escalated `exec` calls) |
| `rollout-…-053f-….jsonl` | Its guardian sibling rollout (`parent_thread_id` = Worker id, 2 reviews, `outcome: allow`) |
| `pi.jsonl` | pi session (format v3) in a throwaway project, 2026-08-07 (3 tool calls, one `isError`) |

Not captured: Claude compaction, Codex `turn_aborted`, guardian `deny` (T21).
