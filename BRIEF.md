# Pair-watcher: an agent that watches the agent

Handoff brief. Written 2026-10-05 by a Hermes session at the user's request.
Intended reader: a fresh session that will build or install this.

> Historical input. The design in §6 was one candidate; the decided design is [`SPEC.md`](SPEC.md),
> which wins wherever they differ. Local install details and raw evidence were kept private.

---

## 1. The idea (as stated)

> "Imagine one harness — one model can go, but we put another agent there to watch what he did
> and be aware on changes or questions or approaches."

Sharpened: a second agent runs alongside a working agent, follows the work *as it happens*,
and reacts to three classes of event:

- **changes** — files/diffs the worker produced, not just its prose
- **questions** — the worker is blocked waiting on an answer or approval
- **approaches** — the worker chose a strategy, and the watcher may push back on it

The watcher is a peer, not a gatekeeper. It should volunteer observations, not wait to be asked.
The worker keeps working; nothing blocks unless the human wants it to.

## 2. What "done" looks like

1. A worker (any harness: pi, Claude Code, Codex) runs normally, untouched, no extra tool calls.
2. A watcher with its own model/prompt profile sees the worker's activity with bounded lag.
3. Observations reach *someone*: the worker's next turn, and/or a notification to the human.
4. The watcher costs little when nothing is happening (no busy polling of a full transcript).
5. On `blocked` (question/approval UI), the watcher is woken — this is the highest-value trigger.
6. Harness-agnostic: pi driver + Codex watcher, or vice versa, must be possible.

## 3. The gap (verified 2026-10-05)

Nobody occupies **autonomous watcher + maintained + harness-agnostic** at once:

- The autonomous one (`ratacat/extra-eyes`) is unlicensed, ⭐20, and dormant since 2026-05-29.
- The maintained, popular ones (`herdr-reviewr` ⭐832, `herdr-annotate` ⭐619) are
  **human-in-the-loop**: the human is the reviewer.
- In-harness options (omc `critic`/`verifier`, `claude-watchdog`) are pull-based or per-turn.

The building blocks already exist (see §5).

## 4. Landscape, with how each line was verified

Stars/licence/last-push below were read from the live GitHub API on 2026-10-05,
not quoted from blog posts.

| project | ★ | licence | last push | mechanism | fit to the idea |
|---|---|---|---|---|---|
| `ratacat/extra-eyes` | 20 | none | 2026-05-29 | Rust daemon + per-harness hooks; N headless watcher AIs; hooks capture conversation + fs/git diff; feedback injected as `additionalContext` on the driver's next hook | **exactly the idea, autonomous**; dormant, unlicensed |
| `persiyanov/herdr-reviewr` | 832 | MIT | 2026-10-05 | Herdr pane shows worker's diff/files; line comments go back into Claude Code / Codex / OpenCode / pi | review loop, **human reviews** |
| `plannotator/herdr-annotate` | 619 | MIT | 2026-09-29 | annotate terminal text / agent replies → feedback to the agent | human annotates |
| `timwuhaotian/the-pair` | 378 | Apache-2.0 | 2026-10-04 | Mentor + Executor cross-check; desktop app + `pair-code` CLI | own dual-agent harness, not a watcher on your session |
| `phiat/claude-esp` | 155 | MIT | 2026-09-22 | tails Claude Code logs → live TUI of thinking/tool calls/subagents | observe-only, human-facing |
| `Temikus/claude-watchdog` | 12 | Apache-2.0 | 2026-10-04 | Stop hook → post-mortem subagent diffing request vs actual repo change | per-turn, not live |
| `StructuPath/herdr-swarm` | 9 | — | 2026-10-04 | N agents in worktrees + status pane of *actual changes* + review-first harvest, resolver agent | fleet awareness, human approves |
| `flupke/herdr-progressive-reviewer` | 3 | — | 2026-10-04 | one-file-at-a-time incremental review with **persistent threads and agent replies**; `prefix+d` | closest to a *conversation*, tiny project |
| `TonyMilton/claude-pair` | 1 | none | 2026-03-19 | Navigator (read-only) + Driver on Claude Code Agent Teams in tmux | right shape, dormant, single harness |
| `zhuravel/magnum` | 0 | — | 2026-10-04 | daemon watches PRs, reviews each push with several AI agents in Herdr panes | multi-agent review, PR-scoped |

Discovery query for more — the Herdr marketplace auto-indexes any public repo carrying the
`herdr-plugin` topic: **1,536 repos** as of 2026-10-05 (the captured page holds the top 100 by
stars):
`api.github.com/search/repositories?q=topic:herdr-plugin&sort=updated`

## 5. Building blocks

- **Herdr 0.9.3**, plugin API = *the entire CLI plus the local socket*; plugins are not sandboxed
  and may address **any** pane/agent in the session.
- **`hhdebb/herdr-radar` is working prior art**: a resident daemon subscribing to agent events,
  driving sidebar rows, with actions/panes/startup hooks. Copy its structure.
- **`ExaDev/agent-comms`** (Claude Code plugin). Cross-harness mesh: rooms, DMs, presence, push
  delivery over TCP localhost:19876, bridges for pi / Claude Code / Codex / OpenCode, MCP bridge
  for anything else. This is the ready-made channel if the watcher and worker need to *talk*
  rather than one injecting into the other.
- **Herdr primitives to build on** (from `SKILL.md` + plugin docs):
  - read state: `herdr agent list`, `herdr agent get <name|pane>`, statuses
    `working|blocked|done|idle|unknown`
  - read content: `herdr agent read <target> --source recent-unwrapped|visible|detection [--lines N]`,
    `herdr pane read`, live stream via `herdr terminal session observe`
  - wake on change: socket `events.subscribe` — `pane.agent_status_changed`, `pane.output_matched`,
    `pane.agent_detected`, `pane.scroll_changed` (plus non-terminal plugin UI is **not** in v1)
  - write back: `herdr agent prompt <target> <text> --wait`, `agent send-keys`,
    `notification show <title> --body <...> --sound request`
  - manifest `herdr-plugin.toml`: `[[actions]]`, `[[panes]]` (overlay|popup|split|tab|zoomed),
    `[[events]]`, `[[startup]]`, `[[build]]`, `[[link_handlers]]`; env
    `HERDR_BIN_PATH`, `HERDR_SOCKET_PATH`, `HERDR_PLUGIN_ID`, `HERDR_PLUGIN_STATE_DIR`
  - no transcript API: read the pane buffer, or use the `agent_session` reference from
    `agent get` to find the harness's own transcript file (e.g. Claude Code JSONL).
- **Watcher models available headless**: `pi -p`, `claude -p`, `codex exec`.

## 6. Design sketch (if building the Herdr plugin)

Name suggestion: `noomz.pair-watcher` (plugin ids use dots; local action ids may not).

1. **Manifest** — one `[[startup]]` that launches a resident watcher daemon under
   `HERDR_PLUGIN_STATE_DIR`; one `[[panes]]` entry (`split`) showing the watcher's running notes;
   one `[[actions]]` to toggle/mute it; optionally `[[events]]` on `pane.agent_status_changed`.
2. **Daemon loop** — subscribe to `pane.agent_status_changed` and `pane.output_matched` for the
   watched pane, then act on three triggers:
   - status becomes `blocked` → **wake watcher immediately** (question/approval pending)
   - a settled diff/git change appears → summarise what changed
   - N seconds of activity since last note → periodic "approach" check
3. **Context handed to the watcher** — never the whole transcript: the worker's last turn text
   (`agent read --source recent-unwrapped`), the diff since the watcher's last cursor
   (`git diff` in the pane's `foreground_cwd`), plus a stable watcher persona prompt. Keep a
   per-session cursor so nothing is re-sent (extra-eyes' trick; cheap and correct).
4. **Delivery** — two channels, both already available:
   - into the worker: `herdr agent prompt <worker> "<note>"` (visible in its next turn) or, if it
     is mid-turn, queue the note until `idle`
   - to the human: `herdr notification show "Watcher" --body "<note>" --sound request`
5. **Guardrails that matter**
   - Watcher must be read-only w.r.t. the repo; it observes and speaks.
   - Rate-limit: max one note per trigger window, coalesce; never inject while the worker is
     mid-tool-call (extra-eyes' failure mode is noise, not corruption — but noise kills adoption).
   - Bound the context it sees (bytes, not lines) so a long session cannot balloon.
   - Fail open: if the watcher model errors, the worker must be unaffected.
6. **Reuse before writing** — read `hhdebb/herdr-radar`
   (daemon + event subscription + config.toml sidebar block) and
   `ogulcancelik/herdr-plugin-examples` (canonical manifest examples).

## 7. Cheaper alternatives, in order

1. **Trial `extra-eyes`** — fastest route to a live autonomous watcher:
   `cargo install --git https://github.com/ratacat/extra-eyes.git --locked --force`,
   then `eyes install claude-code` (or `codex` / pi extension). Watch it on one seat only, and
   read its source first: no license, dormant. Its design (hook capture + cursor + injection)
   is worth copying even if the binary is not adopted.
2. **Human-in-the-loop today** — install `herdr-reviewr` (832★, MIT, active) for the review
   loop, and optionally enable `agent-comms` so worker and watcher can DM across harnesses.
3. **Build the plugin** (§6) — the only route to autonomous + maintained + harness-agnostic.

## 8. Decisions the next session should get from the user

- **Trigger appetite**: woken only on `blocked` + landed diffs (low noise), or continuous
  commentary (high noise, needs a mute)?
- **Delivery target**: interrupt the worker's context, notify the human, or both?
- **Watcher model**: which harness/model for the watcher, and must it differ from the worker's
  (different-vendor critique catches more, costs more)?
- **Scope**: one watched worker (simple) or the whole seat fleet (radar-like sidebar)?

## 9. Verification status — read this before trusting anything above

**Verified by this session, directly:**
- Herdr version (0.9.3)
- Herdr's plugin/agent CLI surface and the plugin manifest fields (via `herdr --help`,
  `herdr plugin --help`, installed manifests, and the vendor docs URLs below)
- `agent-comms` description/README
- every star/licence/last-push number in §4 (GitHub API)
- headless watcher-model availability (`claude -p` exercised)

**Second-hand (from delegated web research, not independently exercised):**
- the exact hook names extra-eyes installs, its injection points, and its source line numbers
- mechanism descriptions for `magnum`, `herdr-swarm`, `herdr-progressive-reviewer`,
  `claude-esp`, `the-pair`

**Not done at all:** nothing here was installed, built, or run end-to-end. No Herdr plugin was
written. `zhuravel/magnum`'s raw README was 404 (its description comes from the GitHub API only).
This session was **not** inside Herdr (`HERDR_ENV` unset), so plugin install/link and any visual
check must happen in a Herdr session, not from an outside shell.

## 10. Sources

- Herdr plugin + socket + agent-automation docs: <https://herdr.dev/docs/plugins/>,
  <https://herdr.dev/docs/socket-api/>, <https://herdr.dev/docs/agent-automation/>,
  <https://herdr.dev/docs/cli-reference/>, <https://herdr.dev/docs/marketplace/>
- Plugin marketplace index: <https://herdr.dev/plugins/> · agent guide: <https://herdr.dev/agent-guide.md>
- Repos: <https://github.com/ratacat/extra-eyes> · <https://github.com/persiyanov/herdr-reviewr> ·
  <https://github.com/plannotator/herdr-annotate> · <https://github.com/hhdebb/herdr-radar> ·
  <https://github.com/ogulcancelik/herdr-plugin-examples> · <https://github.com/ExaDev/agent-comms>
