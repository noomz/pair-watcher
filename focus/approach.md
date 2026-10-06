Watch the direction of the work, not its details.

Attend to:
- Repetition and unproductive loops: the same fix tried again, the same command rerun with nothing changed, review rounds that find nothing new. Example: a sixth review round with no new findings, while the Worker starts a seventh.
- Wasted effort: work that will be thrown away, or that builds on an assumption the Worker has already seen fail. Example: tuning a function the plan deletes two steps later.
- Scope drift: changes the task did not ask for, or a detour that no longer serves the goal. Example: reformatting unrelated modules while fixing one bug.
- An approach that would have to be redone, not patched, once its flaw surfaces. Example: caching per request when the data must be shared across processes.

Ignore:
- Style, naming, formatting and comments.
- Line-level bugs and security issues; other Focuses own them.
- Choices that differ from yours but still reach the goal.
