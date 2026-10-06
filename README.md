# pair-watcher

A second agent that watches a coding agent while it works.

pair-watcher follows one **Worker** (Claude Code, Codex, or pi) and speaks only when it would change what happens next:

- **Commentary**: evaluative Concerns in a side pane, for the human only. It never notifies.
- **Challenge and Debate**: for approach-level objections only (work that would have to be redone, not patched), a Challenge goes into the Worker's next turn, followed by a bounded Debate of at most two exchanges.
- **Escalation**: an unresolved Debate, or a question the Worker is blocked on, goes to the human with one notification.

Watchers never edit the repo, never block the Worker, and treat silence as success. Companion mode (one Watcher, every Focus) is the default; Swarm (one Watcher per Focus) is opt-in. A local event log feeds ratio metrics and a silent-session audit, so the Watcher can be measured and improved without rewarding volume.

## Status

Specification stage: no runnable binary yet.

| file | what |
|---|---|
| [`SPEC.md`](SPEC.md) | v1 build contract: goals, constraints, interfaces, 99 invariants, ordered tasks |
| [`CONTEXT.md`](CONTEXT.md) | domain vocabulary (Worker, Watcher, Focus, Concern, Challenge, Debate, Ruling, Escalation, ...) |
| [`BRIEF.md`](BRIEF.md) | original handoff and landscape survey (historical) |
| [`.scratch/pair-watcher/prototypes/`](.scratch/pair-watcher/prototypes/) | throwaway prototypes: pane layout (`node .scratch/pair-watcher/prototypes/15-pane/pane.mjs --variant=B`) and ruling logic (`22-ruling/ruling.html`) |

Planned shape: one Rust binary (Watcher, CLI, hook helper), with a TypeScript Claude Code mod and a pi extension embedded in it. Hooks and transcript tailers form the core; [Herdr](https://herdr.dev) is an optional front-end.

## License

[MIT](LICENSE)
