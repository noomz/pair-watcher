# Repository Guidelines

## Project structure and module organization

Pair-watcher is a specification workspace: the v1 build contract plus throwaway prototypes. No production code yet.

- `SPEC.md` is the v1 build contract (§G goals, §C constraints, §I interfaces, §V invariants, §T tasks, §B bugs). It wins over every other document.
- `CONTEXT.md` defines domain vocabulary such as Worker, Watcher, Concern, Challenge, and Escalation.
- `BRIEF.md` is the original handoff and dated landscape research; historical input only.
- `.scratch/pair-watcher/prototypes/15-pane/` contains `pane.mjs` and its canned `repetition.jsonl` event fixture.
- `.scratch/pair-watcher/prototypes/22-ruling/ruling.html` is a self-contained ruling-logic demo; open it in a browser.
- The decision trail (wayfinder map, tickets, research, captured evidence, `.planning/`) lives in the maintainer's checkout and is gitignored. `SPEC.md` cites ticket file names as provenance; the rules themselves are complete in `SPEC.md`.

## Development and verification commands

Run these commands from the workspace root:

```sh
# Open the interactive pane comparison.
node .scratch/pair-watcher/prototypes/15-pane/pane.mjs --variant=B

# Render a reproducible frame without a terminal UI.
node .scratch/pair-watcher/prototypes/15-pane/pane.mjs --dump --variant=B --at=31 --width=64 --height=30

# Check prototype JavaScript syntax.
node --check .scratch/pair-watcher/prototypes/15-pane/pane.mjs
```

No package manifest, production build command, or automated test suite is configured.

## Style and naming conventions

Use concise Markdown with descriptive headings and relative links. Follow `CONTEXT.md` terminology rather than introducing synonyms. Keep numbered ticket names descriptive, such as `20-fail-open.md`, and link findings to their ticket. Match the prototype's two-space indentation and ES module syntax. No formatter or linter is configured.

## Testing guidelines

For pane changes, run syntax checks and render the repetition fixture at 48, 64, and 96 columns. Inspect wrapping, pending Challenges, and Escalations. Record the command, observed result, and limitations in the relevant ticket. Separate captured facts from proposals and unverified assumptions. No coverage threshold exists.

## Commit and pull request guidelines

Git repository initialised 2026-10-06 on `main`. Use concise imperative subjects. Describe the changed behavior or decision, link the relevant ticket, and include verification results. Include before/after terminal captures for pane changes.

## Configuration and evidence

Keep credentials and personal configuration out of evidence. Date new captures and record their source. Preserve the runtime Watcher's read-only role and queue Challenges until the Worker reaches idle or turn end.
