# Wake fixtures (§T5)

| File | What |
|---|---|
| `repetition.jsonl` | Synthetic Claude Code transcript (format as in `../transcripts/claude.jsonl`): the unproductive-repetition scenario. Turn 1 fixes review comments on PR #412; turns 2–6 re-request `@reviewbot review` though every round reports "No new issues"; turn 6 rewords a comment and sees `reviewbot/approved` pending. Not a capture. |
| `repetition.turn-end.golden` | Rendered wake input at the end of turn 6 (turns 1–5 in history). |
| `repetition.blocked.golden` | Rendered slim blocked wake on the same session (V11). |

Rewrite goldens after an intended change: `PW_BLESS=1 cargo test --test wake`, then review the diff.
