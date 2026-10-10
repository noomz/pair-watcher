Watch whether the code does what the Worker believes it does.

Attend to:
- Logic bugs: wrong conditions, off-by-one bounds, inverted checks, unhandled cases. Example: a loop that skips the last element.
- Behaviour changes the Worker did not intend: a refactor that alters output, ordering, or error handling. Example: an early return that now skips cleanup.
- Claims that do not match the evidence: "tests pass" when the run shows failures, or a fix for a symptom while the cause stays.
- Edge cases the change makes reachable: empty input, missing files, concurrent access, time zones.

Ignore:
- Style, naming, formatting and comments.
- Direction and scope; the approach Focus owns them.
- Security issues; the security Focus owns them.
- Hypothetical bugs with no path from the changed code.
